//! Domain model. Platform-agnostic by construction (`app_info.md` §17): nothing
//! in this module may reference PhotoKit, MediaStore, sockets or SQL.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Length of every hash in the system. Both the quick key and the full hash are
/// SHA-256 (§11).
pub const HASH_LEN: usize = 32;

/// A SHA-256 digest.
///
/// Serialises as a lowercase hex string so the wire format stays readable in a
/// packet capture, and stores in SQLite as the raw 32 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Hash32([u8; HASH_LEN]);

impl Hash32 {
    pub const fn from_bytes(bytes: [u8; HASH_LEN]) -> Self {
        Self(bytes)
    }

    pub fn from_slice(bytes: &[u8]) -> Option<Self> {
        <[u8; HASH_LEN]>::try_from(bytes).ok().map(Self)
    }

    pub const fn as_bytes(&self) -> &[u8; HASH_LEN] {
        &self.0
    }

    pub fn to_hex(self) -> String {
        hex::encode(self.0)
    }

    pub fn from_hex(s: &str) -> Option<Self> {
        let raw = hex::decode(s).ok()?;
        Self::from_slice(&raw)
    }

    /// Short form for logs. Never use this for comparison.
    pub fn short(self) -> String {
        hex::encode(&self.0[..6])
    }
}

impl fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash32({})", self.short())
    }
}

impl fmt::Display for Hash32 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl Serialize for Hash32 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Hash32 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Hash32::from_hex(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid 32-byte hex digest: {s:?}")))
    }
}

/// The two media kinds we carry. String forms match the `CHECK` constraint on
/// `local_asset.media_type` in `schema/001_initial.sql`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Image,
    Video,
}

impl MediaType {
    pub const fn as_str(self) -> &'static str {
        match self {
            MediaType::Image => "image",
            MediaType::Video => "video",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "image" => Some(MediaType::Image),
            "video" => Some(MediaType::Video),
            _ => None,
        }
    }
}

impl fmt::Display for MediaType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What the sender knows about one asset, and what travels on the wire (§11.4).
///
/// `full_hash` is `None` until the asset has been streamed at least once. Its
/// presence is what allows the receiver to answer `Skip` instead of `Probable`
/// (§11.3), so it must be carried whenever it is known.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetDescriptor {
    pub quick_hash: Hash32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub full_hash: Option<Hash32>,
    pub size: u64,
    pub media_type: MediaType,
    pub mime: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub modified_at: i64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub display_name: Option<String>,
    /// Groups the resources of one logical asset, e.g. a Live Photo still and
    /// its paired video (§16).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resource_group_id: Option<String>,
}

/// Persisted transfer state (§13.6).
///
/// `DISCOVERED` from the diagram is not represented here: an asset that has been
/// discovered simply exists in `local_asset` and has no `transfer` row. `RETRY`
/// is a transition, not a state, and is recorded as `retry_count`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferState {
    Queued,
    Transferring,
    Verifying,
    Committed,
    Failed,
    /// A success terminal state, not an error (§18).
    SkippedAlreadyPresent,
    Cancelled,
}

impl TransferState {
    pub const fn as_str(self) -> &'static str {
        match self {
            TransferState::Queued => "queued",
            TransferState::Transferring => "transferring",
            TransferState::Verifying => "verifying",
            TransferState::Committed => "committed",
            TransferState::Failed => "failed",
            TransferState::SkippedAlreadyPresent => "skipped_already_present",
            TransferState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => TransferState::Queued,
            "transferring" => TransferState::Transferring,
            "verifying" => TransferState::Verifying,
            "committed" => TransferState::Committed,
            "failed" => TransferState::Failed,
            "skipped_already_present" => TransferState::SkippedAlreadyPresent,
            "cancelled" => TransferState::Cancelled,
            _ => return None,
        })
    }

    /// Terminal states never transition again. Both success and failure.
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            TransferState::Committed
                | TransferState::Failed
                | TransferState::SkippedAlreadyPresent
                | TransferState::Cancelled
        )
    }

    /// Whether this outcome counts toward the user-visible success total.
    pub const fn is_success(self) -> bool {
        matches!(
            self,
            TransferState::Committed | TransferState::SkippedAlreadyPresent
        )
    }
}

/// A device is never both roles at once (§5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionRole {
    Sender,
    Receiver,
}

impl SessionRole {
    pub const fn as_str(self) -> &'static str {
        match self {
            SessionRole::Sender => "sender",
            SessionRole::Receiver => "receiver",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sender" => Some(SessionRole::Sender),
            "receiver" => Some(SessionRole::Receiver),
            _ => None,
        }
    }
}

/// Session lifecycle. `Interrupted` is the state that makes the launch-time
/// Resume offer possible (§14.3, §20).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionState {
    Active,
    Interrupted,
    Done,
    Cancelled,
}

impl SessionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            SessionState::Active => "active",
            SessionState::Interrupted => "interrupted",
            SessionState::Done => "done",
            SessionState::Cancelled => "cancelled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "active" => Some(SessionState::Active),
            "interrupted" => Some(SessionState::Interrupted),
            "done" => Some(SessionState::Done),
            "cancelled" => Some(SessionState::Cancelled),
            _ => None,
        }
    }
}

/// The receiver's answer for one queried asset (§12.2).
///
/// Three verdicts, not two. `Probable` exists because a quick-key match alone
/// may never cause a skip (§11.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HaveVerdict {
    /// Not present. Transfer it.
    Send,
    /// Exact full-hash match. Skipping is safe.
    Skip,
    /// Quick-key match only. Stream it and decide at commit time.
    Probable,
}

/// Progress counters for the UI (§12.1). Read as one aggregate query, never by
/// walking the candidate set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryCounters {
    /// Every asset in the catalog, including ones that will never be sent.
    pub total: u64,
    /// Still waiting for a quick hash (§15.3).
    pub hashing: u64,
    /// Will be transferred to this peer. Must equal the candidate count for the
    /// same filters, or the progress bar cannot reach 100% (§12.1).
    pub remaining: u64,
    /// Originals not on this device, e.g. offloaded to iCloud (§27.2). Reported
    /// in the completion summary and deliberately not subtracted from `total`.
    pub not_on_device: u64,
}
