//! Engine errors.
//!
//! These are *internal* names. They must never reach the screen: §20 requires
//! human-readable, actionable copy, and explicitly forbids showing an error
//! code. The UI layer maps these to strings.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The stored schema is newer than this build understands. Downgrading is
    /// not supported; the user must update the app.
    #[error("database schema version {found} is newer than supported version {supported}")]
    SchemaTooNew { found: i64, supported: i64 },

    /// A value read back from SQLite did not match the shape the model expects.
    /// Always a bug or external tampering, never user error.
    #[error("corrupt row in {table}.{column}: {detail}")]
    CorruptRow {
        table: &'static str,
        column: &'static str,
        detail: String,
    },

    /// Whole-file hash mismatch after a transfer completed (§13.5).
    #[error("hash mismatch: expected {expected}, computed {actual}")]
    HashMismatch { expected: String, actual: String },

    /// A zero-byte asset arrived. Never valid, never committed (§13.2).
    #[error("empty asset")]
    EmptyAsset,

    /// The peer sent something malformed, oversized, or out of order.
    ///
    /// Includes a peer declaring a length beyond the protocol limits, which is
    /// refused before any allocation (§18).
    #[error("protocol: {detail}")]
    Protocol { detail: String },

    /// Code-bound authentication failed (§9.3). Deliberately carries no detail:
    /// telling a peer *why* its proof was rejected leaks information about the
    /// code.
    #[error("authentication failed")]
    AuthFailed,
}

pub type Result<T> = std::result::Result<T, Error>;
