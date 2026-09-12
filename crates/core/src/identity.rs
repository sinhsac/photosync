//! Asset identity (`app_info.md` §11).
//!
//! Two levels, with a strict division of authority:
//!
//! * [`quick_hash`] is a **candidate filter**. It may cause an asset to be
//!   offered; it may never, on its own, cause an asset to be skipped (§11.3).
//! * [`FullHasher`] produces the **authority**, computed in a single pass while
//!   bytes stream during transfer (§13.2). Nothing ever re-reads a file just to
//!   hash it.

use crate::model::{Hash32, HASH_LEN};
use sha2::{Digest, Sha256};
use std::io::{self, Read, Seek, SeekFrom};

/// Bytes sampled per region by the quick key.
pub const QUICK_SAMPLE_LEN: u64 = 64 * 1024;

/// Number of regions sampled: head, middle, tail.
pub const QUICK_SAMPLE_REGIONS: u64 = 3;

/// Files at or below this size are hashed whole, because three 64 KB windows
/// would overlap and the sampling would be meaningless.
pub const QUICK_WHOLE_FILE_THRESHOLD: u64 = QUICK_SAMPLE_LEN * QUICK_SAMPLE_REGIONS;

/// Computes the quick key: `SHA256(size_be ‖ head ‖ middle ‖ tail)`.
///
/// The exact byte layout is part of the wire contract, since two devices compare
/// these values (§11.4). It must not change without a protocol major bump:
///
/// 1. `size` as 8 bytes, **big-endian**. Fixed explicitly so a little-endian and
///    a big-endian device never disagree.
/// 2. The first [`QUICK_SAMPLE_LEN`] bytes.
/// 3. [`QUICK_SAMPLE_LEN`] bytes centred on `size / 2`.
/// 4. The last [`QUICK_SAMPLE_LEN`] bytes.
///
/// Files of at most [`QUICK_WHOLE_FILE_THRESHOLD`] bytes are hashed in full
/// after the size prefix, so the result stays a function of the whole content.
///
/// Reads at most 192 KB. For an 18 GB library this is the difference between
/// seconds and tens of minutes (§26.5).
///
/// `size` is passed in rather than derived from the reader because the platform
/// library already knows it, and because a mismatch between the declared and
/// actual length is itself a signal worth catching: this function returns
/// [`io::ErrorKind::UnexpectedEof`] if the reader is shorter than `size`.
pub fn quick_hash<R: Read + Seek>(size: u64, reader: &mut R) -> io::Result<Hash32> {
    let mut hasher = Sha256::new();
    hasher.update(size.to_be_bytes());

    if size <= QUICK_WHOLE_FILE_THRESHOLD {
        reader.seek(SeekFrom::Start(0))?;
        let mut buf = vec![0u8; size as usize];
        reader.read_exact(&mut buf)?;
        hasher.update(&buf);
        return Ok(finish(hasher));
    }

    let mut buf = vec![0u8; QUICK_SAMPLE_LEN as usize];

    // Head.
    reader.seek(SeekFrom::Start(0))?;
    reader.read_exact(&mut buf)?;
    hasher.update(&buf);

    // Middle, centred. size > 3 * SAMPLE here, so this window cannot overlap
    // either end.
    let middle_start = size / 2 - QUICK_SAMPLE_LEN / 2;
    reader.seek(SeekFrom::Start(middle_start))?;
    reader.read_exact(&mut buf)?;
    hasher.update(&buf);

    // Tail.
    reader.seek(SeekFrom::Start(size - QUICK_SAMPLE_LEN))?;
    reader.read_exact(&mut buf)?;
    hasher.update(&buf);

    Ok(finish(hasher))
}

/// Streaming SHA-256 over a complete file, plus the byte count.
///
/// Fed from the **same** buffer that is being written to disk, in one pass
/// (§13.2). The byte count is accumulated from the stream rather than trusted
/// from a declared length, and a zero-byte result is rejected at [`finish`].
///
/// This type deliberately has no way to be constructed from a partial digest
/// state: platform crypto APIs cannot import one, so a resumed transfer rebuilds
/// the state by re-reading its own staging file (§13.4, §26.8).
#[derive(Clone)]
pub struct FullHasher {
    hasher: Sha256,
    bytes: u64,
}

impl FullHasher {
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    /// Feeds bytes that have also been (or are about to be) written to storage.
    pub fn update(&mut self, chunk: &[u8]) {
        self.hasher.update(chunk);
        self.bytes += chunk.len() as u64;
    }

    /// Bytes consumed so far. This is the value that becomes `bytes_received`,
    /// but only once the corresponding write has been flushed (§13.4).
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Consumes the hasher.
    ///
    /// Returns `None` for an empty stream: a zero-byte asset is never valid and
    /// must not be committed.
    pub fn finish(self) -> Option<Hash32> {
        if self.bytes == 0 {
            return None;
        }
        Some(finish(self.hasher))
    }

    /// Rebuilds digest state from an existing partial file, for resume (§13.4).
    ///
    /// Reads exactly `up_to` bytes from the start. The caller must have already
    /// truncated the staging file to `up_to`, because any bytes past the last
    /// verified, flushed chunk boundary are not trustworthy.
    pub fn resume_from<R: Read + Seek>(reader: &mut R, up_to: u64) -> io::Result<Self> {
        let mut me = Self::new();
        reader.seek(SeekFrom::Start(0))?;

        // 2 MB, matching the buffer size Immich measured for native hashing
        // (Appendix A). Large enough to keep the digest fed, small enough to
        // keep memory flat on a phone (§21).
        const BUF_LEN: usize = 2 * 1024 * 1024;
        let mut buf = vec![0u8; BUF_LEN];

        while me.bytes < up_to {
            let want = ((up_to - me.bytes) as usize).min(BUF_LEN);
            reader.read_exact(&mut buf[..want])?;
            me.update(&buf[..want]);
        }
        Ok(me)
    }
}

impl Default for FullHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// SHA-256 of a single chunk payload (§13.1).
///
/// Per-chunk digests are what turn a corrupt 8 GB video into a 4 MB retry
/// instead of a full re-transfer (§26.7).
pub fn chunk_hash(payload: &[u8]) -> Hash32 {
    let mut hasher = Sha256::new();
    hasher.update(payload);
    finish(hasher)
}

fn finish(hasher: Sha256) -> Hash32 {
    let out = hasher.finalize();
    let mut bytes = [0u8; HASH_LEN];
    bytes.copy_from_slice(&out);
    Hash32::from_bytes(bytes)
}
