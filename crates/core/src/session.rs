//! Sessions and in-flight transfers (`app_info.md` §13.6, §14).
//!
//! The division of labour here is deliberate and easy to get wrong:
//!
//! * **The backlog is a query** ([`crate::catalog::candidates`]). It is not
//!   stored, so it cannot drift and needs no crash reconciliation (§12.1).
//! * **`transfer` holds only in-flight rows.** A row exists while an asset is
//!   mid-flight and is deleted the moment it reaches a terminal state, at which
//!   point `sent_log` gains a row instead. Tens of rows, not hundreds of
//!   thousands.
//!
//! So "what is left to do" is derived, and "where exactly was I in this asset"
//! is persisted. Only the second needs a byte offset.

use crate::db::now_millis;
use crate::error::{Error, Result};
use crate::model::{SessionRole, SessionState, TransferState};
use rusqlite::{named_params, Connection, OptionalExtension};
use uuid::Uuid;

/// A sync session. Survives app death so it can be resumed (§20).
#[derive(Clone, Debug)]
pub struct Session {
    pub id: String,
    pub role: SessionRole,
    pub peer_id: Option<i64>,
    pub state: SessionState,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub items_total: u64,
    pub items_done: u64,
    pub items_skipped: u64,
    pub items_failed: u64,
    pub bytes_total: u64,
    pub bytes_done: u64,
}

impl Session {
    /// Items that reached any terminal state, successful or not.
    pub const fn items_settled(&self) -> u64 {
        self.items_done + self.items_skipped + self.items_failed
    }

    /// What §19.3 shows: "1,284 of 12,431". Skips count as progress, because to
    /// the user an asset already on the other phone is done (§18).
    pub const fn items_progress(&self) -> u64 {
        self.items_done + self.items_skipped
    }
}

/// One in-flight asset.
#[derive(Clone, Debug)]
pub struct Transfer {
    pub session_id: String,
    pub local_asset_id: i64,
    pub state: TransferState,
    pub bytes_transferred: u64,
    pub total_bytes: u64,
    pub retry_count: u32,
}

impl Transfer {
    /// Resume offset for `ASSET_BEGIN` (§18). Always a verified, flushed chunk
    /// boundary, because that is the only value ever persisted (§13.4).
    pub const fn resume_offset(&self) -> u64 {
        self.bytes_transferred
    }
}

/// Maximum attempts per asset before it is marked failed (§13.5).
pub const MAX_ASSET_ATTEMPTS: u32 = 3;

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

/// Opens a new session in state `active`.
pub fn begin(
    conn: &Connection,
    role: SessionRole,
    peer_id: Option<i64>,
    items_total: u64,
    bytes_total: u64,
) -> Result<Session> {
    let id = Uuid::new_v4().to_string();
    let started_at = now_millis();

    conn.execute(
        "INSERT INTO session
           (id, role, peer_id, state, started_at, items_total, bytes_total)
         VALUES (:id, :role, :peer_id, :state, :started_at, :items_total, :bytes_total)",
        named_params! {
            ":id": &id,
            ":role": role.as_str(),
            ":peer_id": peer_id,
            ":state": SessionState::Active.as_str(),
            ":started_at": started_at,
            ":items_total": items_total as i64,
            ":bytes_total": bytes_total as i64,
        },
    )?;

    Ok(Session {
        id,
        role,
        peer_id,
        state: SessionState::Active,
        started_at,
        finished_at: None,
        items_total,
        items_done: 0,
        items_skipped: 0,
        items_failed: 0,
        bytes_total,
        bytes_done: 0,
    })
}

/// Moves a session to a new state, stamping `finished_at` for terminal ones.
pub fn set_state(conn: &Connection, session_id: &str, state: SessionState) -> Result<()> {
    let finished = matches!(
        state,
        SessionState::Done | SessionState::Cancelled | SessionState::Interrupted
    );
    conn.execute(
        "UPDATE session
            SET state = :state,
                finished_at = CASE WHEN :finished THEN :now ELSE finished_at END
          WHERE id = :id",
        named_params! {
            ":state": state.as_str(),
            ":finished": finished,
            ":now": now_millis(),
            ":id": session_id,
        },
    )?;
    Ok(())
}

/// Marks every `active` session as `interrupted`.
///
/// **Call this once at startup, before anything else touches the database.** A
/// session left `active` means the process died mid-sync: nothing had the chance
/// to write a terminal state. Without this sweep such a session is
/// indistinguishable from a live one, and §20's Resume offer would never appear.
///
/// Returns how many sessions were swept, which is a useful signal to log.
pub fn sweep_stale_active(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "UPDATE session
            SET state = :interrupted, finished_at = :now
          WHERE state = :active",
        named_params! {
            ":interrupted": SessionState::Interrupted.as_str(),
            ":active": SessionState::Active.as_str(),
            ":now": now_millis(),
        },
    )?;
    if n > 0 {
        tracing::info!(count = n, "swept sessions left active by an unclean exit");
    }
    Ok(n)
}

/// The most recent resumable session, if any. Drives the launch-time Resume
/// offer (§14.3, §20).
pub fn latest_resumable(conn: &Connection) -> Result<Option<Session>> {
    let row = conn
        .query_row(
            "SELECT id, role, peer_id, state, started_at, finished_at,
                    items_total, items_done, items_skipped, items_failed,
                    bytes_total, bytes_done
               FROM session
              WHERE state = ?1
              ORDER BY started_at DESC
              LIMIT 1",
            [SessionState::Interrupted.as_str()],
            session_from_row,
        )
        .optional()?;
    row.transpose()
}

/// Loads one session by id.
pub fn get(conn: &Connection, session_id: &str) -> Result<Option<Session>> {
    let row = conn
        .query_row(
            "SELECT id, role, peer_id, state, started_at, finished_at,
                    items_total, items_done, items_skipped, items_failed,
                    bytes_total, bytes_done
               FROM session WHERE id = ?1",
            [session_id],
            session_from_row,
        )
        .optional()?;
    row.transpose()
}

// ---------------------------------------------------------------------------
// In-flight transfers
// ---------------------------------------------------------------------------

/// Puts an asset in flight, or returns the existing row if it already is.
///
/// Idempotent on purpose: after a reconnect the sender re-offers assets it had
/// started, and the existing `bytes_transferred` is exactly what must survive.
pub fn enqueue(
    conn: &Connection,
    session_id: &str,
    local_asset_id: i64,
    total_bytes: u64,
) -> Result<Transfer> {
    conn.execute(
        "INSERT INTO transfer
           (session_id, local_asset_id, state, total_bytes, updated_at)
         VALUES (:sid, :aid, :state, :total, :now)
         ON CONFLICT(session_id, local_asset_id) DO NOTHING",
        named_params! {
            ":sid": session_id,
            ":aid": local_asset_id,
            ":state": TransferState::Queued.as_str(),
            ":total": total_bytes as i64,
            ":now": now_millis(),
        },
    )?;

    find(conn, session_id, local_asset_id)?.ok_or_else(|| Error::CorruptRow {
        table: "transfer",
        column: "-",
        detail: "row vanished immediately after insert".into(),
    })
}

/// Reads one in-flight row.
pub fn find(
    conn: &Connection,
    session_id: &str,
    local_asset_id: i64,
) -> Result<Option<Transfer>> {
    let row = conn
        .query_row(
            "SELECT session_id, local_asset_id, state, bytes_transferred,
                    total_bytes, retry_count
               FROM transfer
              WHERE session_id = ?1 AND local_asset_id = ?2",
            rusqlite::params![session_id, local_asset_id],
            transfer_from_row,
        )
        .optional()?;
    row.transpose()
}

/// Every in-flight row for a session, oldest first.
///
/// Loaded on resume so the sender knows which assets already have a byte offset
/// worth continuing from (§13.4).
pub fn in_flight(conn: &Connection, session_id: &str) -> Result<Vec<Transfer>> {
    let mut stmt = conn.prepare(
        "SELECT session_id, local_asset_id, state, bytes_transferred,
                total_bytes, retry_count
           FROM transfer
          WHERE session_id = ?1
          ORDER BY updated_at ASC",
    )?;
    let rows = stmt.query_map([session_id], transfer_from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row??);
    }
    Ok(out)
}

/// Advances progress within an asset.
///
/// `bytes` must be a **verified, flushed** chunk boundary. Persisting a value
/// that has not been flushed means a resume would skip bytes that never reached
/// storage, and the whole-file hash would fail at the end with no obvious cause
/// (§13.4).
///
/// Monotonic by construction: a lower value is ignored rather than written, so a
/// late-arriving duplicate acknowledgement cannot rewind progress.
pub fn advance(
    conn: &Connection,
    session_id: &str,
    local_asset_id: i64,
    bytes: u64,
) -> Result<()> {
    conn.execute(
        "UPDATE transfer
            SET state = :state,
                bytes_transferred = MAX(bytes_transferred, :bytes),
                updated_at = :now
          WHERE session_id = :sid AND local_asset_id = :aid",
        named_params! {
            ":state": TransferState::Transferring.as_str(),
            ":bytes": bytes as i64,
            ":now": now_millis(),
            ":sid": session_id,
            ":aid": local_asset_id,
        },
    )?;
    Ok(())
}

/// Records a failed attempt and reports whether any remain (§13.5).
///
/// Returns `true` if the asset should be retried, `false` if it has exhausted
/// [`MAX_ASSET_ATTEMPTS`] and must be settled as failed. One failed asset never
/// fails a session.
pub fn record_attempt_failure(
    conn: &Connection,
    session_id: &str,
    local_asset_id: i64,
    error_code: &str,
) -> Result<bool> {
    let retry_count: i64 = conn.query_row(
        "UPDATE transfer
            SET retry_count = retry_count + 1,
                bytes_transferred = 0,
                state = :state,
                error_code = :code,
                updated_at = :now
          WHERE session_id = :sid AND local_asset_id = :aid
        RETURNING retry_count",
        named_params! {
            ":state": TransferState::Queued.as_str(),
            ":code": error_code,
            ":now": now_millis(),
            ":sid": session_id,
            ":aid": local_asset_id,
        },
        |r| r.get(0),
    )?;

    // bytes_transferred is reset because a whole-file hash mismatch invalidates
    // the entire staged file, not just the last chunk (§13.5). A *chunk* hash
    // mismatch is retried in the transport layer and never reaches here.
    Ok((retry_count as u32) < MAX_ASSET_ATTEMPTS)
}

/// Settles an asset: updates the session counters and **deletes** the in-flight
/// row, in one transaction.
///
/// Deleting is the point. It keeps `transfer` proportional to concurrency rather
/// than to library size (§21), and it makes the invariant easy to state: a row
/// in `transfer` means work in progress, always.
///
/// The caller is responsible for the durable record of success — on the sender
/// that is [`crate::catalog::mark_sent`] plus
/// [`crate::catalog::set_full_hash`]. Both must happen in the same transaction
/// as this call, or a crash in between would lose the fact that the asset was
/// delivered and it would be sent again.
pub fn settle(
    conn: &Connection,
    session_id: &str,
    local_asset_id: i64,
    outcome: TransferState,
    bytes_credited: u64,
) -> Result<()> {
    debug_assert!(
        outcome.is_terminal(),
        "settle() requires a terminal state, got {outcome:?}"
    );

    let (done, skipped, failed) = match outcome {
        TransferState::Committed => (1, 0, 0),
        TransferState::SkippedAlreadyPresent => (0, 1, 0),
        TransferState::Failed => (0, 0, 1),
        // A cancelled asset is not an outcome the user is told about per-item;
        // the session itself reports cancellation.
        TransferState::Cancelled => (0, 0, 0),
        _ => (0, 0, 0),
    };

    conn.execute(
        "UPDATE session
            SET items_done    = items_done    + :done,
                items_skipped = items_skipped + :skipped,
                items_failed  = items_failed  + :failed,
                bytes_done    = bytes_done    + :bytes
          WHERE id = :sid",
        named_params! {
            ":done": done,
            ":skipped": skipped,
            ":failed": failed,
            ":bytes": bytes_credited as i64,
            ":sid": session_id,
        },
    )?;

    conn.execute(
        "DELETE FROM transfer WHERE session_id = ?1 AND local_asset_id = ?2",
        rusqlite::params![session_id, local_asset_id],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------

fn session_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Session>> {
    let role_raw: String = r.get(1)?;
    let state_raw: String = r.get(3)?;

    let Some(role) = SessionRole::parse(&role_raw) else {
        return Ok(Err(Error::CorruptRow {
            table: "session",
            column: "role",
            detail: role_raw,
        }));
    };
    let Some(state) = SessionState::parse(&state_raw) else {
        return Ok(Err(Error::CorruptRow {
            table: "session",
            column: "state",
            detail: state_raw,
        }));
    };

    Ok(Ok(Session {
        id: r.get(0)?,
        role,
        peer_id: r.get(2)?,
        state,
        started_at: r.get(4)?,
        finished_at: r.get(5)?,
        items_total: r.get::<_, i64>(6)? as u64,
        items_done: r.get::<_, i64>(7)? as u64,
        items_skipped: r.get::<_, i64>(8)? as u64,
        items_failed: r.get::<_, i64>(9)? as u64,
        bytes_total: r.get::<_, i64>(10)? as u64,
        bytes_done: r.get::<_, i64>(11)? as u64,
    }))
}

fn transfer_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Transfer>> {
    let state_raw: String = r.get(2)?;
    let Some(state) = TransferState::parse(&state_raw) else {
        return Ok(Err(Error::CorruptRow {
            table: "transfer",
            column: "state",
            detail: state_raw,
        }));
    };
    Ok(Ok(Transfer {
        session_id: r.get(0)?,
        local_asset_id: r.get(1)?,
        state,
        bytes_transferred: r.get::<_, i64>(3)? as u64,
        total_bytes: r.get::<_, i64>(4)? as u64,
        retry_count: r.get::<_, i64>(5)? as u32,
    }))
}
