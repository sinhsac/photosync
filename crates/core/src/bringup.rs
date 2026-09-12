//! Platform bring-up self check.
//!
//! Exercises the parts of the engine that depend on the host platform rather
//! than on Rust alone: that the bundled SQLite actually linked, that migrations
//! run, and that the derived candidate query (§12.1) executes. Everything else
//! in this crate is pure computation and cannot fail differently per platform.
//!
//! Called across the FFI boundary during Android and iOS bring-up, where a
//! silent linker problem otherwise shows up much later as an unexplained crash.
//! Cheap enough to run at start-up on a real device.

use crate::catalog::{self, FilterArgs, ScannedAsset};
use crate::db;
use crate::error::Result;
use crate::model::{Hash32, MediaType};

/// Outcome of [`self_check`]. All three values are fixed by the fixture below,
/// so any deviation means the platform is misbehaving, not the data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SelfCheck {
    /// Should equal [`db::SCHEMA_VERSION`].
    pub schema_version: i64,
    /// Should be 1: two assets catalogued, one already sent.
    pub candidates: usize,
    /// Should equal `candidates`, or §12.1's invariant is broken on this
    /// platform.
    pub remaining: u64,
}

impl SelfCheck {
    /// Whether every expectation held.
    pub fn is_ok(&self) -> bool {
        self.schema_version == db::SCHEMA_VERSION
            && self.candidates == 1
            && self.remaining == self.candidates as u64
    }
}

/// Builds a two-asset library in memory, confirms one as sent, and checks that
/// exactly one candidate remains.
///
/// The fixture deliberately gives both assets the **same quick hash** and
/// different full hashes. That is the collision case from §11.3, and it is the
/// one that regressed once already (§26.10), so it is worth asserting on every
/// platform rather than trusting that the SQL behaves identically everywhere.
pub fn self_check() -> Result<SelfCheck> {
    let conn = db::open_in_memory()?;
    let schema_version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

    let peer = catalog::upsert_peer(
        &conn,
        &Hash32::from_bytes([0x5A; 32]),
        Some("self-check"),
        None,
    )?;

    let quick = Hash32::from_bytes([0x11; 32]);
    let full_a = Hash32::from_bytes([0xA1; 32]);

    for (idx, pid) in ["self-check-a", "self-check-b"].iter().enumerate() {
        let outcome = catalog::upsert(
            &conn,
            &ScannedAsset {
                platform_asset_id: (*pid).to_string(),
                size: 1024 + idx as u64,
                media_type: MediaType::Image,
                mime: "image/jpeg".to_string(),
                created_at: 1_700_000_000_000 + idx as i64,
                modified_at: 1_700_000_000_000 + idx as i64,
                width: Some(4032),
                height: Some(3024),
                duration_ms: None,
                display_name: Some((*pid).to_string()),
                resource_group_id: None,
                is_local: true,
            },
        )?;
        // Both share one quick hash on purpose.
        catalog::set_quick_hash(&conn, outcome.id, &quick)?;
    }

    // Confirm only the first asset. Keyed on the full hash, so the second must
    // survive despite sharing a quick hash.
    let first = catalog::candidates(
        &conn,
        FilterArgs {
            peer_id: peer,
            include_videos: true,
        },
        1,
    )?;
    if let Some(c) = first.first() {
        catalog::set_full_hash(&conn, c.id, &full_a)?;
        catalog::mark_sent(&conn, peer, &full_a, &quick)?;
    }

    let filter = FilterArgs {
        peer_id: peer,
        include_videos: true,
    };
    let candidates = catalog::candidates(&conn, filter, 100)?.len();
    let remaining = catalog::counters(&conn, filter)?.remaining;

    Ok(SelfCheck {
        schema_version,
        candidates,
        remaining,
    })
}
