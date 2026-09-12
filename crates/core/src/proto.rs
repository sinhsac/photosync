//! Wire protocol: messages and framing (`app_info.md` §18).
//!
//! Pure encoding, no I/O, so the transport can be swapped (§8) and the frames can
//! be exercised without a socket.
//!
//! Every limit in this module exists because the LAN is untrusted (§9). A peer
//! that has completed TLS is still not trusted to send sane lengths, and a
//! length prefix read from the network must never be used to size an allocation
//! without a bound. That is the classic way a protocol like this becomes a
//! one-packet denial of service.

use crate::chunk::CHUNK_LEN;
use crate::error::{Error, Result};
use crate::model::{AssetDescriptor, Hash32, HaveVerdict, SessionRole};
use serde::{Deserialize, Serialize};

/// Protocol major version. A mismatch is refused with a human-readable message,
/// never a crash (§18).
pub const PROTOCOL_MAJOR: u16 = 1;

/// Protocol minor version. Peers with equal majors must interoperate.
pub const PROTOCOL_MINOR: u16 = 0;

/// Largest permitted header. Headers are small JSON objects; the biggest is a
/// `HaveQuery` batch of 500 entries (§12.2), which is well under this.
pub const MAX_HEADER_LEN: u32 = 1024 * 1024;

/// Largest permitted payload. Only `Chunk` carries one, and it is exactly
/// [`CHUNK_LEN`], so the bound is tight on purpose.
pub const MAX_PAYLOAD_LEN: u32 = CHUNK_LEN as u32;

/// Fixed frame prefix: two big-endian lengths.
pub const FRAME_PREFIX_LEN: usize = 8;

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// One protocol message (§18).
///
/// Tagged with a short `t` field so a capture stays readable while keeping the
/// header small.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Message {
    /// First message on the control stream. Sent by both sides.
    Hello {
        major: u16,
        minor: u16,
        device_name: String,
        platform: String,
        /// SHA-256 of the sender's certificate DER. Under TLS this is
        /// cross-checked against the handshake and dropped on mismatch, so it is
        /// a convenience, never the identity (§9.1).
        cert_fingerprint: Hash32,
    },

    /// Code-bound authentication proof (§9.3).
    ///
    /// The sender proves first. See [`AuthOrder`] for why that direction is not
    /// arbitrary.
    Authenticate {
        proof: Hash32,
    },

    /// Receiver's nonce, sent before either proof so both transcripts agree.
    AuthChallenge {
        nonce: [u8; 32],
    },

    SessionBegin {
        role: SessionRole,
        session_id: String,
        est_items: u64,
        est_bytes: u64,
    },

    /// The receiver's in-flight offsets, sent once after authentication.
    ///
    /// Resolves the tension between §13.4 ("the receiver reports
    /// `bytes_received`") and doing so without a round trip per asset. One
    /// message covers the whole session; the sender then proposes an offset in
    /// [`Message::AssetBegin`] and the receiver corrects it with
    /// [`ChunkOutcome::Misaligned`] if the proposal is wrong.
    ///
    /// The receiver stays authoritative: its correction always wins, and a sender
    /// that ignores it makes no progress.
    SessionResume {
        in_flight: Vec<ResumePoint>,
    },

    /// A batch of the manifest exchange (§12.2).
    HaveQuery {
        items: Vec<HaveQueryEntry>,
    },

    HaveResponse {
        items: Vec<HaveResponseEntry>,
    },

    AssetBegin {
        descriptor: AssetDescriptor,
        /// The sender's *proposal*, taken from [`Message::SessionResume`] or zero
        /// for an asset it has not started. Not authoritative: a wrong value
        /// costs one [`ChunkOutcome::Misaligned`] and nothing else.
        resume_offset: u64,
    },

    /// Header for a chunk. The bytes travel in the frame payload, not in JSON.
    Chunk {
        offset: u64,
        hash: Hash32,
    },

    ChunkAck {
        offset: u64,
        outcome: ChunkOutcome,
    },

    AssetEnd {
        full_hash: Hash32,
    },

    AssetAck {
        outcome: AssetOutcome,
    },

    /// Checkpoint after an applied batch (§12.3).
    BatchAck {
        last_id: u64,
    },

    SessionEnd {
        items_done: u64,
        items_skipped: u64,
        items_failed: u64,
        bytes_done: u64,
    },

    Heartbeat,

    /// Orderly refusal. Carries text meant for a log, never for the screen: §20
    /// requires the UI to supply its own wording.
    Abort {
        reason: AbortReason,
        detail: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaveQueryEntry {
    pub id: u64,
    pub quick_hash: Hash32,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub full_hash: Option<Hash32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HaveResponseEntry {
    pub id: u64,
    pub verdict: HaveVerdict,
}

/// One asset the receiver already holds part of (§13.4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumePoint {
    pub quick_hash: Hash32,
    /// Verified and durably written. Always a chunk boundary.
    pub bytes_received: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkOutcome {
    /// Written and durable. `bytes_received` is the new resume offset.
    Accepted { bytes_received: u64 },
    /// Resend this chunk. 4 MB, not the asset (§13.5).
    Corrupt,
    /// Continue from `expected_offset` instead.
    Misaligned { expected_offset: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetOutcome {
    /// Committed into the photo library.
    Committed,
    /// The receiver already held this exact content. **A success** (§18): it
    /// travels the same path as `Committed` and increments the skipped counter.
    AlreadyPresent,
    /// Whole-file hash disagreed. Retry the asset (§13.5).
    HashMismatch,
    /// Something else went wrong with this asset. The session continues.
    Failed,
}

impl AssetOutcome {
    /// Whether the sender may record this asset as delivered.
    ///
    /// `AlreadyPresent` counts, which is the whole point of §18's rule that a
    /// duplicate is not an error.
    pub const fn is_delivered(self) -> bool {
        matches!(self, AssetOutcome::Committed | AssetOutcome::AlreadyPresent)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AbortReason {
    VersionMismatch,
    AuthFailed,
    TooManyAttempts,
    CodeExpired,
    Busy,
    OutOfSpace,
    Cancelled,
    Internal,
}

/// Who proves knowledge of the code first (§9.3).
///
/// **The sender proves first, and the receiver answers only after verifying.**
/// Not arbitrary: whichever side speaks first hands a brute-forceable value to
/// whoever is listening, and 6 digits is 20 bits, so an offline search over one
/// captured proof is instant. Making the receiver prove first would let anyone
/// who can reach it harvest a proof and recover the code without ever passing
/// the attempt limit — and impersonating the *receiver* is the damaging
/// direction, because that is the side a whole library is streamed to.
///
/// With this ordering an attacker posing as a receiver still learns a proof and
/// can recover the code offline. That residual weakness is inherent to a short
/// code without a PAKE and is documented in §9.4 rather than hidden here.
pub enum AuthOrder {}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

/// A decoded frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub message: Message,
    /// Empty for every message except [`Message::Chunk`].
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(message: Message) -> Self {
        Self {
            message,
            payload: Vec::new(),
        }
    }

    pub fn with_payload(message: Message, payload: Vec<u8>) -> Self {
        Self { message, payload }
    }
}

/// Encodes a frame: `header_len:u32 ‖ payload_len:u32 ‖ header ‖ payload`.
pub fn encode(frame: &Frame) -> Result<Vec<u8>> {
    let header = serde_json::to_vec(&frame.message).map_err(|e| Error::Protocol {
        detail: format!("cannot encode header: {e}"),
    })?;

    if header.len() as u64 > MAX_HEADER_LEN as u64 {
        return Err(Error::Protocol {
            detail: format!("header of {} bytes exceeds limit", header.len()),
        });
    }
    if frame.payload.len() as u64 > MAX_PAYLOAD_LEN as u64 {
        return Err(Error::Protocol {
            detail: format!("payload of {} bytes exceeds limit", frame.payload.len()),
        });
    }

    let mut out = Vec::with_capacity(FRAME_PREFIX_LEN + header.len() + frame.payload.len());
    out.extend_from_slice(&(header.len() as u32).to_be_bytes());
    out.extend_from_slice(&(frame.payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(&frame.payload);
    Ok(out)
}

/// The two lengths from a frame prefix, already bounds-checked.
///
/// Checking **before** allocating is the entire point: these numbers come from a
/// peer we do not trust with our memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLengths {
    pub header_len: u32,
    pub payload_len: u32,
}

impl FrameLengths {
    pub const fn total(&self) -> usize {
        self.header_len as usize + self.payload_len as usize
    }
}

/// Parses and validates a frame prefix.
pub fn parse_prefix(prefix: &[u8; FRAME_PREFIX_LEN]) -> Result<FrameLengths> {
    let header_len = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]);
    let payload_len = u32::from_be_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]);

    if header_len == 0 {
        return Err(Error::Protocol {
            detail: "frame has an empty header".into(),
        });
    }
    if header_len > MAX_HEADER_LEN {
        return Err(Error::Protocol {
            detail: format!("peer declared a {header_len}-byte header, limit is {MAX_HEADER_LEN}"),
        });
    }
    if payload_len > MAX_PAYLOAD_LEN {
        return Err(Error::Protocol {
            detail: format!(
                "peer declared a {payload_len}-byte payload, limit is {MAX_PAYLOAD_LEN}"
            ),
        });
    }
    Ok(FrameLengths {
        header_len,
        payload_len,
    })
}

/// Decodes a frame body whose lengths were validated by [`parse_prefix`].
pub fn decode_body(lengths: FrameLengths, body: &[u8]) -> Result<Frame> {
    let expected = lengths.total();
    if body.len() != expected {
        return Err(Error::Protocol {
            detail: format!("frame body is {} bytes, expected {expected}", body.len()),
        });
    }
    let (header, payload) = body.split_at(lengths.header_len as usize);

    let message: Message = serde_json::from_slice(header).map_err(|e| Error::Protocol {
        detail: format!("cannot decode header: {e}"),
    })?;

    // A payload on anything other than Chunk means the peer is confused or
    // probing. Refuse rather than silently ignore.
    if !matches!(message, Message::Chunk { .. }) && !payload.is_empty() {
        return Err(Error::Protocol {
            detail: "payload present on a message that carries none".into(),
        });
    }

    Ok(Frame {
        message,
        payload: payload.to_vec(),
    })
}

/// Convenience for round-tripping in one call. Not used on the network path,
/// where the prefix and body are read separately.
pub fn decode(bytes: &[u8]) -> Result<Frame> {
    if bytes.len() < FRAME_PREFIX_LEN {
        return Err(Error::Protocol {
            detail: "frame shorter than its prefix".into(),
        });
    }
    let mut prefix = [0u8; FRAME_PREFIX_LEN];
    prefix.copy_from_slice(&bytes[..FRAME_PREFIX_LEN]);
    let lengths = parse_prefix(&prefix)?;
    decode_body(lengths, &bytes[FRAME_PREFIX_LEN..])
}

/// Checks a peer's `Hello` against our own version (§18).
pub fn check_version(major: u16, _minor: u16) -> Result<()> {
    if major != PROTOCOL_MAJOR {
        return Err(Error::Protocol {
            detail: format!("peer speaks protocol {major}, this build speaks {PROTOCOL_MAJOR}"),
        });
    }
    Ok(())
}
