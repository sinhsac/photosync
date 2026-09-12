//! Receiver-side state (`app_info.md` §11.3, §12.2, §13.4, §14.2).
//!
//! Two responsibilities:
//!
//! * Answering "do you already have this?" — and answering it in a way that can
//!   never silently lose an asset (§11.3).
//! * Remembering how far each in-flight asset got, so a reconnect resumes rather
//!   than restarts (§13.4).

use crate::db::now_millis;
use crate::error::{Error, Result};
use crate::model::{Hash32, HaveVerdict};
use rusqlite::{named_params, Connection, OptionalExtension};

/// One entry of a `HAVE_QUERY` batch (§12.2).
///
/// `id` is the sender's own identifier, echoed back untouched so the two sides
/// never depend on ordering.
#[derive(Clone, Debug)]
pub struct HaveQueryItem {
    pub id: u64,
    pub quick_hash: Hash32,
    /// Present only when the sender has streamed this asset before. Its presence
    /// is what upgrades the answer from `Probable` to a definite `Skip`.
    pub full_hash: Option<Hash32>,
}

/// One entry of a `HAVE_RESPONSE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HaveResponseItem {
    pub id: u64,
    pub verdict: HaveVerdict,
}

/// Answers one batch of the manifest exchange.
///
/// The rule that matters, and the one that regressed once already (§26.10):
///
/// * `full_hash` supplied and known → [`HaveVerdict::Skip`]. Exact, safe.
/// * `full_hash` supplied and unknown → [`HaveVerdict::Send`]. Also exact.
/// * `full_hash` absent, `quick_hash` unknown → [`HaveVerdict::Send`].
/// * `full_hash` absent, `quick_hash` known → [`HaveVerdict::Probable`].
///
/// That last case is the important one. A quick key may collide by design
/// (§11.1), so it can only ever say "maybe". The asset is streamed and the
/// decision is made against the arriving full hash at commit time. Bytes may be
/// wasted; an asset is never lost.
pub fn answer_have_query(
    conn: &Connection,
    batch: &[HaveQueryItem],
) -> Result<Vec<HaveResponseItem>> {
    let mut by_full = conn.prepare("SELECT 1 FROM received_asset WHERE full_hash = ?1")?;
    let mut by_quick = conn.prepare("SELECT 1 FROM received_asset WHERE quick_hash = ?1 LIMIT 1")?;

    let mut out = Vec::with_capacity(batch.len());
    for item in batch {
        let verdict = match item.full_hash {
            Some(full) => {
                let known: Option<i64> = by_full
                    .query_row([&full.as_bytes()[..]], |r| r.get(0))
                    .optional()?;
                if known.is_some() {
                    HaveVerdict::Skip
                } else {
                    HaveVerdict::Send
                }
            }
            None => {
                let known: Option<i64> = by_quick
                    .query_row([&item.quick_hash.as_bytes()[..]], |r| r.get(0))
                    .optional()?;
                if known.is_some() {
                    HaveVerdict::Probable
                } else {
                    HaveVerdict::Send
                }
            }
        };
        out.push(HaveResponseItem {
            id: item.id,
            verdict,
        });
    }
    Ok(out)
}

/// Whether this exact content is already held.
///
/// Called at commit time for an asset that was streamed on a `Probable` verdict.
/// A hit means the transfer ends as `skipped_already_present`, which is a
/// success, not an error (§18).
pub fn holds_full_hash(conn: &Connection, full_hash: &Hash32) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM received_asset WHERE full_hash = ?1",
            [&full_hash.as_bytes()[..]],
            |r| r.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Records a committed asset. Keyed on the full hash (§14.2).
pub fn record_received(
    conn: &Connection,
    full_hash: &Hash32,
    quick_hash: &Hash32,
    size: u64,
    platform_asset_id: Option<&str>,
    peer_id: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO received_asset
           (full_hash, quick_hash, size, platform_asset_id, peer_id, received_at)
         VALUES (:full, :quick, :size, :pid, :peer, :now)
         ON CONFLICT(full_hash) DO UPDATE SET
           platform_asset_id = COALESCE(excluded.platform_asset_id,
                                        received_asset.platform_asset_id),
           received_at = excluded.received_at",
        named_params! {
            ":full": &full_hash.as_bytes()[..],
            ":quick": &quick_hash.as_bytes()[..],
            ":size": size as i64,
            ":pid": platform_asset_id,
            ":peer": peer_id,
            ":now": now_millis(),
        },
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// In-flight inbound assets
// ---------------------------------------------------------------------------

/// A partially received asset (§14.2).
#[derive(Clone, Debug)]
pub struct InboundState {
    pub session_id: String,
    pub quick_hash: Hash32,
    pub staging_ref: String,
    /// Verified and durably written. Never anything else (§13.4).
    pub bytes_received: u64,
    pub total_bytes: u64,
}

/// Registers an inbound asset, or returns the existing row so a reconnect keeps
/// its offset.
pub fn open(
    conn: &Connection,
    session_id: &str,
    quick_hash: &Hash32,
    staging_ref: &str,
    total_bytes: u64,
) -> Result<InboundState> {
    conn.execute(
        "INSERT INTO inbound_transfer
           (session_id, quick_hash, staging_ref, total_bytes, updated_at)
         VALUES (:sid, :quick, :ref, :total, :now)
         ON CONFLICT(session_id, quick_hash) DO NOTHING",
        named_params! {
            ":sid": session_id,
            ":quick": &quick_hash.as_bytes()[..],
            ":ref": staging_ref,
            ":total": total_bytes as i64,
            ":now": now_millis(),
        },
    )?;

    find(conn, session_id, quick_hash)?.ok_or_else(|| Error::CorruptRow {
        table: "inbound_transfer",
        column: "-",
        detail: "row vanished immediately after insert".into(),
    })
}

/// Reads one inbound row.
pub fn find(
    conn: &Connection,
    session_id: &str,
    quick_hash: &Hash32,
) -> Result<Option<InboundState>> {
    let row = conn
        .query_row(
            "SELECT session_id, quick_hash, staging_ref, bytes_received, total_bytes
               FROM inbound_transfer
              WHERE session_id = ?1 AND quick_hash = ?2",
            rusqlite::params![session_id, &quick_hash.as_bytes()[..]],
            inbound_from_row,
        )
        .optional()?;
    row.transpose()
}

/// Every inbound row for a session. Loaded on reconnect so the receiver can
/// report each asset's resume offset (§13.4).
pub fn in_flight(conn: &Connection, session_id: &str) -> Result<Vec<InboundState>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, quick_hash, staging_ref, bytes_received, total_bytes
           FROM inbound_transfer
          WHERE session_id = ?1
          ORDER BY updated_at ASC",
    )?;
    let rows = stmt.query_map([session_id], inbound_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// Persists a new resume offset.
///
/// **Call only after the chunk was flushed**, i.e. once
/// [`crate::chunk::ChunkVerdict::Accepted`] has been returned. Writing this
/// before the flush is the one mistake in the whole resume design that produces
/// silent corruption instead of an error (§13.4).
///
/// Monotonic: a lower value is ignored, so a duplicate acknowledgement cannot
/// rewind progress.
pub fn advance(
    conn: &Connection,
    session_id: &str,
    quick_hash: &Hash32,
    bytes_received: u64,
) -> Result<()> {
    conn.execute(
        "UPDATE inbound_transfer
            SET bytes_received = MAX(bytes_received, :bytes),
                updated_at = :now
          WHERE session_id = :sid AND quick_hash = :quick",
        named_params! {
            ":bytes": bytes_received as i64,
            ":now": now_millis(),
            ":sid": session_id,
            ":quick": &quick_hash.as_bytes()[..],
        },
    )?;
    Ok(())
}

/// Resets an asset to zero after a whole-file hash mismatch (§13.5).
///
/// The staging file must be truncated by the caller. A chunk-level retry cannot
/// help here: every chunk already passed its own hash, so the corruption is
/// somewhere the per-chunk check cannot see.
pub fn reset(conn: &Connection, session_id: &str, quick_hash: &Hash32) -> Result<()> {
    conn.execute(
        "UPDATE inbound_transfer
            SET bytes_received = 0, updated_at = :now
          WHERE session_id = :sid AND quick_hash = :quick",
        named_params! {
            ":now": now_millis(),
            ":sid": session_id,
            ":quick": &quick_hash.as_bytes()[..],
        },
    )?;
    Ok(())
}

/// Drops the in-flight row once the asset reaches a terminal state.
///
/// Same reasoning as the sender side: a row here means work in progress, always
/// (§14.2).
pub fn close(conn: &Connection, session_id: &str, quick_hash: &Hash32) -> Result<()> {
    conn.execute(
        "DELETE FROM inbound_transfer WHERE session_id = ?1 AND quick_hash = ?2",
        rusqlite::params![session_id, &quick_hash.as_bytes()[..]],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------

fn inbound_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<InboundState>> {
    let quick_raw: Vec<u8> = r.get(1)?;
    let Some(quick_hash) = Hash32::from_slice(&quick_raw) else {
        return Ok(Err(Error::CorruptRow {
            table: "inbound_transfer",
            column: "quick_hash",
            detail: format!("{} bytes, expected 32", quick_raw.len()),
        }));
    };
    Ok(Ok(InboundState {
        session_id: r.get(0)?,
        quick_hash,
        staging_ref: r.get(2)?,
        bytes_received: r.get::<_, i64>(3)? as u64,
        total_bytes: r.get::<_, i64>(4)? as u64,
    }))
}
