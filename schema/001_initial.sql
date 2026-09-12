-- PhotoSync — initial schema (revision 2, app_info.md §14)
--
-- One database per device. Catalog and state only, never media binaries.
-- A single device may act as sender and receiver at different times, so both
-- sides' tables live here; only one role is active per session (§5).
--
-- Applied as migration step 1. Migrations are stepwise and transactional:
-- disable foreign_keys, run steps, re-enable in a finally block, and run
-- PRAGMA foreign_key_check in debug builds.

-- ---------------------------------------------------------------------------
-- Connection pragmas (§14). Set on every connection, not stored in the file.
--   PRAGMA journal_mode = WAL;        -- persistent, survives reopen
--   PRAGMA synchronous  = NORMAL;
--   PRAGMA foreign_keys = ON;         -- per-connection, must be re-set
--   PRAGMA cache_size   = -32000;     -- 32 MB
--   PRAGMA temp_store   = MEMORY;
-- ---------------------------------------------------------------------------

PRAGMA user_version = 1;

-- ===========================================================================
-- Shared
-- ===========================================================================

-- A peer is identified by its certificate fingerprint and nothing else (§9.1).
-- SHA-256 over the certificate DER, stored as the raw 32 bytes.
CREATE TABLE peer (
  id               INTEGER PRIMARY KEY,
  cert_fingerprint BLOB    NOT NULL UNIQUE,
  device_name      TEXT,
  platform         TEXT,                       -- 'ios' | 'android'
  paired_at        INTEGER NOT NULL,           -- unix millis
  last_seen_at     INTEGER
);

-- Sessions outlive the connection so a sync can be resumed (§9.5, §9.6, §20).
CREATE TABLE session (
  id            TEXT    PRIMARY KEY,           -- uuid
  role          TEXT    NOT NULL CHECK (role IN ('sender', 'receiver')),
  peer_id       INTEGER REFERENCES peer(id) ON DELETE SET NULL,
  state         TEXT    NOT NULL CHECK (state IN
                    ('active', 'interrupted', 'done', 'cancelled')),
  started_at    INTEGER NOT NULL,
  finished_at   INTEGER,
  items_total   INTEGER NOT NULL DEFAULT 0,
  items_done    INTEGER NOT NULL DEFAULT 0,
  items_skipped INTEGER NOT NULL DEFAULT 0,
  items_failed  INTEGER NOT NULL DEFAULT 0,
  bytes_total   INTEGER NOT NULL DEFAULT 0,
  bytes_done    INTEGER NOT NULL DEFAULT 0
);

-- Resumable sessions are found on launch by this index (§14.3).
CREATE INDEX idx_session_interrupted ON session(started_at)
  WHERE state = 'interrupted';

-- Library change-detection checkpoints (§15.1): MediaStore version and
-- per-volume generation on Android, PHPersistentChangeToken on iOS.
-- Also holds device name and other small app state.
CREATE TABLE app_kv (
  key        TEXT PRIMARY KEY,
  value      BLOB,
  updated_at INTEGER NOT NULL
);

-- ===========================================================================
-- Sender side
-- ===========================================================================

-- The library catalog.
--
-- quick_hash IS NULL is the hashing work queue (§15.3). Nothing else tracks
-- what still needs hashing.
--
-- full_hash IS NULL until the asset has been streamed at least once; once set
-- it is never recomputed and it enables exact dedup on later syncs (§11.3).
CREATE TABLE local_asset (
  id                INTEGER PRIMARY KEY,
  platform_asset_id TEXT    NOT NULL UNIQUE,   -- PHAsset.localIdentifier /
                                               -- MediaStore _ID.
                                               -- LOCAL CACHE KEY ONLY (§11).
  quick_hash        BLOB,                      -- SHA-256, 32 bytes
  full_hash         BLOB,                      -- SHA-256, 32 bytes
  size              INTEGER NOT NULL,
  media_type        TEXT    NOT NULL CHECK (media_type IN ('image', 'video')),
  mime              TEXT    NOT NULL,
  created_at        INTEGER NOT NULL,
  modified_at       INTEGER NOT NULL,
  width             INTEGER,
  height            INTEGER,
  duration_ms       INTEGER,
  display_name      TEXT,
  resource_group_id TEXT,                      -- Live Photo pairing (§16)
  is_local          INTEGER NOT NULL DEFAULT 1 -- 0 = original not on device,
                                               -- e.g. iCloud offload (§27.2)
                      CHECK (is_local IN (0, 1)),
  scanned_at        INTEGER NOT NULL
);

-- Serves both the dedup lookup and the "WHERE quick_hash IS NULL" hashing
-- queue: SQLite answers IS NULL from this index as a covering search, so a
-- separate partial index on the unhashed rows is redundant. Verified with
-- EXPLAIN QUERY PLAN.
CREATE INDEX idx_local_asset_quick    ON local_asset(quick_hash);
CREATE INDEX idx_local_asset_group    ON local_asset(resource_group_id)
  WHERE resource_group_id IS NOT NULL;
-- Candidate query orders by created_at DESC (§12.1).
CREATE INDEX idx_local_asset_created  ON local_asset(created_at DESC);

-- Confirmed-sent index, per peer. Drives the anti-join in §12.1.
-- Inserting here is what makes an asset fall out of the candidate set.
--
-- KEYED ON full_hash, NEVER ON quick_hash.
--
-- quick_hash is a candidate filter and is allowed to collide by design (§11.1);
-- two distinct files can share one. Keying this table on it means confirming
-- one such file silently removes the other from the candidate set forever, so
-- it is never transferred. That is the exact data-loss failure mode §11.3
-- exists to prevent, and it applies to the sender's own bookkeeping just as
-- much as to the receiver's answer.
--
-- Every path that reaches this table already knows the full hash: a committed
-- asset was just streamed, and a skipped-as-exact asset was skipped precisely
-- because we supplied its full hash. So full_hash is NOT NULL here.
-- quick_hash is retained for diagnostics only and is not part of any key.
CREATE TABLE sent_log (
  peer_id    INTEGER NOT NULL REFERENCES peer(id) ON DELETE CASCADE,
  full_hash  BLOB    NOT NULL,
  quick_hash BLOB    NOT NULL,
  sent_at    INTEGER NOT NULL,
  PRIMARY KEY (peer_id, full_hash)
) WITHOUT ROWID;

-- IN-FLIGHT ONLY (§12.1, §14.1).
--
-- A row exists while an asset is mid-transfer and is deleted on commit, at
-- which point sent_log gains a row. Tens of rows, not hundreds of thousands.
-- Do not use this table to represent the backlog; the backlog is a query.
CREATE TABLE transfer (
  session_id        TEXT    NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  local_asset_id    INTEGER NOT NULL REFERENCES local_asset(id) ON DELETE CASCADE,
  state             TEXT    NOT NULL CHECK (state IN
                        ('queued', 'transferring', 'verifying', 'committed',
                         'failed', 'skipped_already_present', 'cancelled')),
  bytes_transferred INTEGER NOT NULL DEFAULT 0,
  total_bytes       INTEGER NOT NULL,
  retry_count       INTEGER NOT NULL DEFAULT 0,
  error_code        TEXT,
  updated_at        INTEGER NOT NULL,
  PRIMARY KEY (session_id, local_asset_id)
);

-- ===========================================================================
-- Receiver side
-- ===========================================================================

-- Everything ever accepted. full_hash is the authoritative identity; quick_hash
-- is indexed for the candidate lookup that can only answer "probable" (§11.3).
CREATE TABLE received_asset (
  full_hash         BLOB    PRIMARY KEY,       -- SHA-256, 32 bytes
  quick_hash        BLOB    NOT NULL,
  size              INTEGER NOT NULL,
  platform_asset_id TEXT,                      -- assigned at commit
  peer_id           INTEGER REFERENCES peer(id) ON DELETE SET NULL,
  received_at       INTEGER NOT NULL
) WITHOUT ROWID;

CREATE INDEX idx_received_quick ON received_asset(quick_hash);

-- In-flight inbound assets. This table is what makes resume possible (§13.4).
--
-- bytes_received is advanced ONLY to a chunk boundary that has been verified
-- and flushed, and only after the flush. On resume, truncate staging_ref to
-- this offset before accepting further chunks, then re-read it to rebuild the
-- SHA-256 state (§13.4, §26.8).
CREATE TABLE inbound_transfer (
  session_id     TEXT    NOT NULL REFERENCES session(id) ON DELETE CASCADE,
  quick_hash     BLOB    NOT NULL,
  staging_ref    TEXT    NOT NULL,             -- temp path, or pending
                                               -- MediaStore URI (§10.4)
  bytes_received INTEGER NOT NULL DEFAULT 0,
  total_bytes    INTEGER NOT NULL,
  updated_at     INTEGER NOT NULL,
  PRIMARY KEY (session_id, quick_hash)
);
