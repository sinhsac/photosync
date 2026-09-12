//! SQLite access: connection setup and migrations (`app_info.md` §14).

use crate::error::{Error, Result};
use rusqlite::Connection;
use std::path::Path;

/// Highest schema version this build understands.
pub const SCHEMA_VERSION: i64 = 1;

/// Migration steps, applied in order. Index `n` migrates from `user_version == n`
/// to `n + 1`. Each script is responsible for setting `PRAGMA user_version`.
///
/// The SQL lives outside the crate, at the workspace root, because the same
/// schema will be read by the Flutter side and by any future desktop build.
const MIGRATIONS: &[&str] = &[include_str!("../../../schema/001_initial.sql")];

/// Opens (creating if needed) the PhotoSync database and brings it up to date.
pub fn open(path: impl AsRef<Path>) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Opens a private in-memory database. Used by the dev harness (§22.3).
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Applies the per-connection pragmas from §14.
///
/// `foreign_keys` and `cache_size` are per-connection and must be set every
/// time; `journal_mode = WAL` is persistent but setting it again is harmless.
pub fn configure(conn: &Connection) -> Result<()> {
    // Returns the resulting mode as a row, so it cannot go through
    // `pragma_update`.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        // An in-memory database reports "memory" and can never use WAL. Any
        // other value on a real file is worth knowing about.
        tracing::debug!(journal_mode = %mode, "journal_mode is not WAL");
    }

    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // Negative means KiB rather than pages: 32 MB.
    conn.pragma_update(None, "cache_size", -32_000)?;
    Ok(())
}

/// Runs any outstanding migration steps.
///
/// Each step runs in its own transaction with foreign keys disabled, as §14
/// requires, and `foreign_keys` is restored afterwards even if the step fails.
pub fn migrate(conn: &Connection) -> Result<()> {
    let mut version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    if version > SCHEMA_VERSION {
        return Err(Error::SchemaTooNew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }

    while (version as usize) < MIGRATIONS.len() {
        let step = MIGRATIONS[version as usize];
        tracing::info!(from = version, to = version + 1, "applying migration");

        conn.pragma_update(None, "foreign_keys", false)?;
        let outcome = conn
            .execute_batch("BEGIN")
            .and_then(|()| conn.execute_batch(step))
            .and_then(|()| conn.execute_batch("COMMIT"));

        if let Err(e) = outcome {
            let _ = conn.execute_batch("ROLLBACK");
            conn.pragma_update(None, "foreign_keys", true)?;
            return Err(e.into());
        }
        conn.pragma_update(None, "foreign_keys", true)?;

        let applied: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        debug_assert_eq!(
            applied,
            version + 1,
            "migration {} did not set user_version",
            version + 1
        );
        version = applied;
    }

    // Cheap in debug, skipped in release: catches a migration that left a
    // dangling reference behind (§14).
    if cfg!(debug_assertions) {
        let mut stmt = conn.prepare("PRAGMA foreign_key_check")?;
        let mut rows = stmt.query([])?;
        if rows.next()?.is_some() {
            return Err(Error::CorruptRow {
                table: "(foreign_key_check)",
                column: "-",
                detail: "dangling foreign key reference after migration".into(),
            });
        }
    }

    Ok(())
}

/// Current unix time in milliseconds. Every timestamp column uses this unit.
pub fn now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
