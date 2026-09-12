//! The sender-side catalog: library rows, peers, and the confirmed-sent index.
//!
//! This module owns the two queries the whole sync design rests on (§12.1):
//! [`candidates`] and [`counters`]. They must agree on every filter, so they are
//! written next to each other and share [`FilterArgs`]. Adding a filter to one
//! without the other makes the progress bar stop short of 100%.

use crate::db::now_millis;
use crate::error::{Error, Result};
use crate::model::{AssetDescriptor, Hash32, LibraryCounters, MediaType};
use rusqlite::{named_params, Connection, OptionalExtension, Row};

/// One asset as reported by the platform photo library (§10). Contains no hash:
/// hashing happens later, driven by the `quick_hash IS NULL` queue (§15.3).
#[derive(Clone, Debug)]
pub struct ScannedAsset {
    /// `PHAsset.localIdentifier` or the MediaStore `_ID`. A local cache key
    /// only, never cross-device identity (§11).
    pub platform_asset_id: String,
    pub size: u64,
    pub media_type: MediaType,
    pub mime: String,
    pub created_at: i64,
    pub modified_at: i64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_ms: Option<u64>,
    pub display_name: Option<String>,
    pub resource_group_id: Option<String>,
    /// `false` when the original is not on the device, e.g. offloaded to iCloud
    /// (§27.2). Such assets are catalogued but never queued.
    pub is_local: bool,
}

/// What [`upsert`] did, so a scan can report progress without a second query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpsertOutcome {
    pub id: i64,
    /// `true` when the row now has no quick hash, either because it is new or
    /// because its content markers changed (§15.2). Either way it is in the
    /// hashing queue.
    pub needs_hash: bool,
}

/// An asset waiting to be hashed (§15.3).
#[derive(Clone, Debug)]
pub struct PendingHash {
    pub id: i64,
    pub platform_asset_id: String,
    pub size: u64,
}

/// A queued asset, ready to offer to a peer.
#[derive(Clone, Debug)]
pub struct Candidate {
    pub id: i64,
    pub platform_asset_id: String,
    pub descriptor: AssetDescriptor,
}

/// The filters shared by [`candidates`] and [`counters`].
///
/// Any new filter goes here, which forces both queries to be updated together.
#[derive(Clone, Copy, Debug)]
pub struct FilterArgs {
    pub peer_id: i64,
    /// The only user-facing filter in MVP (§19.1).
    pub include_videos: bool,
}

// ---------------------------------------------------------------------------
// Peers
// ---------------------------------------------------------------------------

/// Records a peer, or refreshes what we know about an existing one.
///
/// Identity is the certificate fingerprint and nothing else (§9.1), so that is
/// the conflict target.
pub fn upsert_peer(
    conn: &Connection,
    cert_fingerprint: &Hash32,
    device_name: Option<&str>,
    platform: Option<&str>,
) -> Result<i64> {
    let now = now_millis();
    let id = conn.query_row(
        "INSERT INTO peer (cert_fingerprint, device_name, platform, paired_at, last_seen_at)
         VALUES (:fp, :name, :platform, :now, :now)
         ON CONFLICT(cert_fingerprint) DO UPDATE SET
           device_name  = COALESCE(excluded.device_name, peer.device_name),
           platform     = COALESCE(excluded.platform, peer.platform),
           last_seen_at = excluded.last_seen_at
         RETURNING id",
        named_params! {
            ":fp": &cert_fingerprint.as_bytes()[..],
            ":name": device_name,
            ":platform": platform,
            ":now": now,
        },
        |r| r.get(0),
    )?;
    Ok(id)
}

/// Drops a peer and, by cascade, its `sent_log`. Backs "Forget this device"
/// (§9.5, §19.7).
pub fn forget_peer(conn: &Connection, peer_id: i64) -> Result<()> {
    conn.execute("DELETE FROM peer WHERE id = ?1", [peer_id])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Library catalog
// ---------------------------------------------------------------------------

/// Content markers compared to decide whether an asset changed (§15.2).
///
/// Deliberately does **not** include the hash: the whole point is to detect a
/// change without reading the file. `IFNULL` sentinels keep NULLs comparable,
/// using values no real asset can have.
const UNCHANGED_PREDICATE: &str = "
       local_asset.size        = excluded.size
   AND local_asset.modified_at = excluded.modified_at
   AND IFNULL(local_asset.width,       -1) = IFNULL(excluded.width,       -1)
   AND IFNULL(local_asset.height,      -1) = IFNULL(excluded.height,      -1)
   AND IFNULL(local_asset.duration_ms, -1) = IFNULL(excluded.duration_ms, -1)
";

/// Inserts or refreshes one catalogued asset.
///
/// When the content markers are unchanged the stored hashes are preserved, which
/// is what makes a rescan nearly free and a second sync cheap. When anything
/// changed, both hashes are reset to `NULL`, which re-enrols the asset in the
/// hashing queue and, once hashed, back into the candidate set. That single
/// write is the entire invalidation mechanism (§15.2).
pub fn upsert(conn: &Connection, asset: &ScannedAsset) -> Result<UpsertOutcome> {
    let sql = format!(
        "INSERT INTO local_asset
           (platform_asset_id, size, media_type, mime, created_at, modified_at,
            width, height, duration_ms, display_name, resource_group_id,
            is_local, scanned_at)
         VALUES
           (:pid, :size, :media_type, :mime, :created_at, :modified_at,
            :width, :height, :duration_ms, :display_name, :group_id,
            :is_local, :now)
         ON CONFLICT(platform_asset_id) DO UPDATE SET
           quick_hash        = CASE WHEN {UNCHANGED_PREDICATE} THEN local_asset.quick_hash END,
           full_hash         = CASE WHEN {UNCHANGED_PREDICATE} THEN local_asset.full_hash  END,
           size              = excluded.size,
           media_type        = excluded.media_type,
           mime              = excluded.mime,
           created_at        = excluded.created_at,
           modified_at       = excluded.modified_at,
           width             = excluded.width,
           height            = excluded.height,
           duration_ms       = excluded.duration_ms,
           display_name      = excluded.display_name,
           resource_group_id = excluded.resource_group_id,
           is_local          = excluded.is_local,
           scanned_at        = excluded.scanned_at
         RETURNING id, quick_hash IS NULL"
    );

    let (id, needs_hash) = conn.query_row(
        &sql,
        named_params! {
            ":pid": &asset.platform_asset_id,
            ":size": asset.size as i64,
            ":media_type": asset.media_type.as_str(),
            ":mime": &asset.mime,
            ":created_at": asset.created_at,
            ":modified_at": asset.modified_at,
            ":width": asset.width,
            ":height": asset.height,
            ":duration_ms": asset.duration_ms.map(|d| d as i64),
            ":display_name": asset.display_name.as_deref(),
            ":group_id": asset.resource_group_id.as_deref(),
            ":is_local": asset.is_local,
            ":now": now_millis(),
        },
        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, bool>(1)?)),
    )?;

    Ok(UpsertOutcome { id, needs_hash })
}

/// Whether this platform id is already catalogued. Lets a scan distinguish a new
/// asset from a changed one without a second write.
pub fn was_catalogued(conn: &Connection, platform_asset_id: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM local_asset WHERE platform_asset_id = ?1",
            [platform_asset_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Removes catalog rows whose platform ids no longer exist. Used after a full
/// rescan (§15.1).
pub fn delete_by_platform_id(conn: &Connection, platform_asset_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM local_asset WHERE platform_asset_id = ?1",
        [platform_asset_id],
    )?;
    Ok(n > 0)
}

/// The hashing work queue (§15.3): `WHERE quick_hash IS NULL`. No separate
/// table, no separate state to reconcile.
///
/// Ordered newest first so the assets a user is most likely to care about become
/// sendable soonest.
pub fn pending_hashes(conn: &Connection, limit: u32) -> Result<Vec<PendingHash>> {
    let mut stmt = conn.prepare(
        "SELECT id, platform_asset_id, size
           FROM local_asset
          WHERE quick_hash IS NULL
            AND is_local = 1
          ORDER BY created_at DESC
          LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |r| {
        Ok(PendingHash {
            id: r.get(0)?,
            platform_asset_id: r.get(1)?,
            size: r.get::<_, i64>(2)? as u64,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Error::from)
}

/// Stores a computed quick key. Once set, it is never recomputed unless the
/// asset changes (§15.2).
pub fn set_quick_hash(conn: &Connection, id: i64, quick_hash: &Hash32) -> Result<()> {
    conn.execute(
        "UPDATE local_asset SET quick_hash = ?2 WHERE id = ?1",
        rusqlite::params![id, &quick_hash.as_bytes()[..]],
    )?;
    Ok(())
}

/// Caches the authoritative full hash after an asset has been streamed (§11.2).
pub fn set_full_hash(conn: &Connection, id: i64, full_hash: &Hash32) -> Result<()> {
    conn.execute(
        "UPDATE local_asset SET full_hash = ?2 WHERE id = ?1",
        rusqlite::params![id, &full_hash.as_bytes()[..]],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// The two queries that must agree (§12.1)
// ---------------------------------------------------------------------------

/// Shared `FROM` and `WHERE` fragments, so the filters cannot drift apart.
///
/// The anti-join is on `full_hash`, never on `quick_hash`. A quick key is a
/// filter that may collide by design, so joining on it would let one confirmed
/// asset silently suppress a different one (§11.3). An asset that has never been
/// streamed has `full_hash IS NULL`, the join finds nothing, and it is correctly
/// treated as pending.
const CANDIDATE_FROM: &str = "
  FROM local_asset la
  LEFT JOIN sent_log sl
         ON sl.peer_id = :peer_id AND sl.full_hash = la.full_hash
";

const CANDIDATE_WHERE: &str = "
   la.quick_hash IS NOT NULL
   AND la.is_local = 1
   AND sl.full_hash IS NULL
   AND (:include_videos OR la.media_type = 'image')
";

/// "What is left to send", derived rather than materialised (§12.1).
///
/// Paged with `LIMIT`, because a 500,000-asset library must never be
/// materialised in memory (§21). Call repeatedly: confirming an asset inserts a
/// `sent_log` row and the asset falls out of this result, so the next page is
/// always fresh work with no cursor to maintain.
pub fn candidates(conn: &Connection, filter: FilterArgs, limit: u32) -> Result<Vec<Candidate>> {
    let sql = format!(
        "SELECT la.id, la.platform_asset_id, la.quick_hash, la.full_hash, la.size,
                la.media_type, la.mime, la.created_at, la.modified_at,
                la.width, la.height, la.duration_ms, la.display_name,
                la.resource_group_id
         {CANDIDATE_FROM}
         WHERE {CANDIDATE_WHERE}
         ORDER BY la.created_at DESC
         LIMIT :limit"
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        named_params! {
            ":peer_id": filter.peer_id,
            ":include_videos": filter.include_videos,
            ":limit": limit,
        },
        candidate_from_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Error::from)
}

/// Progress counters (§12.1).
///
/// `remaining` applies exactly the filters in [`CANDIDATE_WHERE`]. If it ever
/// diverges from [`candidates`], the sync appears to stall just short of
/// finishing.
pub fn counters(conn: &Connection, filter: FilterArgs) -> Result<LibraryCounters> {
    let sql = format!(
        "SELECT COUNT(*),
                COUNT(*) FILTER (WHERE la.quick_hash IS NULL),
                COUNT(*) FILTER (WHERE {CANDIDATE_WHERE}),
                COUNT(*) FILTER (WHERE la.is_local = 0)
         {CANDIDATE_FROM}"
    );

    let counters = conn.query_row(
        &sql,
        named_params! {
            ":peer_id": filter.peer_id,
            ":include_videos": filter.include_videos,
        },
        |r| {
            Ok(LibraryCounters {
                total: r.get::<_, i64>(0)? as u64,
                hashing: r.get::<_, i64>(1)? as u64,
                remaining: r.get::<_, i64>(2)? as u64,
                not_on_device: r.get::<_, i64>(3)? as u64,
            })
        },
    )?;
    Ok(counters)
}

// ---------------------------------------------------------------------------
// Confirmed-sent index
// ---------------------------------------------------------------------------

/// Records that a peer now holds this content, which removes it from
/// [`candidates`] permanently (§12.1).
///
/// Called on `committed` **and** on `skipped_already_present`: both mean the
/// receiver has the asset, and a skip is a success (§18).
///
/// `full_hash` is required, not optional. It is the identity this table is keyed
/// on, and every caller already knows it: a committed asset was just streamed,
/// and an asset skipped as an exact match was skipped because we supplied its
/// full hash in the query. Accepting `None` here would mean writing a row that
/// cannot be matched later.
///
/// The caller should also persist the same hash on the library row via
/// [`set_full_hash`], since the candidate anti-join reads it from there.
pub fn mark_sent(
    conn: &Connection,
    peer_id: i64,
    full_hash: &Hash32,
    quick_hash: &Hash32,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sent_log (peer_id, full_hash, quick_hash, sent_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(peer_id, full_hash) DO UPDATE SET
           quick_hash = excluded.quick_hash,
           sent_at    = excluded.sent_at",
        rusqlite::params![
            peer_id,
            &full_hash.as_bytes()[..],
            &quick_hash.as_bytes()[..],
            now_millis(),
        ],
    )?;
    Ok(())
}

/// Whether this peer already holds this exact content.
///
/// Keyed on the full hash, so this is an exact answer, never a probable one.
pub fn was_sent(conn: &Connection, peer_id: i64, full_hash: &Hash32) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sent_log WHERE peer_id = ?1 AND full_hash = ?2",
            rusqlite::params![peer_id, &full_hash.as_bytes()[..]],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

// ---------------------------------------------------------------------------

fn candidate_from_row(r: &Row<'_>) -> rusqlite::Result<Candidate> {
    let quick_raw: Vec<u8> = r.get(2)?;
    let full_raw: Option<Vec<u8>> = r.get(3)?;
    let media_raw: String = r.get(5)?;

    // `candidates` filters on `quick_hash IS NOT NULL`, and the column is
    // written only from a Hash32, so a bad length here means the file was
    // modified outside the app.
    let quick_hash = Hash32::from_slice(&quick_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            quick_raw.len(),
            rusqlite::types::Type::Blob,
            "local_asset.quick_hash is not 32 bytes".into(),
        )
    })?;

    let full_hash = match full_raw {
        Some(raw) => Some(Hash32::from_slice(&raw).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                raw.len(),
                rusqlite::types::Type::Blob,
                "local_asset.full_hash is not 32 bytes".into(),
            )
        })?),
        None => None,
    };

    let media_type = MediaType::parse(&media_raw).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            media_raw.len(),
            rusqlite::types::Type::Text,
            format!("unknown media_type {media_raw:?}").into(),
        )
    })?;

    Ok(Candidate {
        id: r.get(0)?,
        platform_asset_id: r.get(1)?,
        descriptor: AssetDescriptor {
            quick_hash,
            full_hash,
            size: r.get::<_, i64>(4)? as u64,
            media_type,
            mime: r.get(6)?,
            created_at: r.get(7)?,
            modified_at: r.get(8)?,
            width: r.get(9)?,
            height: r.get(10)?,
            duration_ms: r.get::<_, Option<i64>>(11)?.map(|d| d as u64),
            display_name: r.get(12)?,
            resource_group_id: r.get(13)?,
        },
    })
}
