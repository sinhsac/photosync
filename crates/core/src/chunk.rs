//! Chunked transfer, resume, and verification (`app_info.md` §13).
//!
//! This has no prior art in either reference project: Immich uploads whole files
//! and deletes the partial on disconnect, which is the opposite of what resume
//! needs (§2). So the invariants are stated here explicitly rather than assumed.
//!
//! Three rules govern everything below:
//!
//! 1. **Sequential within an asset**, so resume is a single byte offset.
//! 2. **`bytes_received` only ever names bytes that are durably on disk.** It is
//!    written after the flush, never before. Getting this backwards produces a
//!    resume that skips bytes, and the only symptom is a whole-file hash failure
//!    minutes later with nothing pointing at the cause.
//! 3. **Two digests, two jobs.** The per-chunk hash protects the transfer and
//!    makes a retry cost 4 MB. The whole-file hash proves the result and is the
//!    only thing allowed to authorise a commit (§11.3, §26.7).

use crate::error::{Error, Result};
use crate::identity::{chunk_hash, FullHasher};
use crate::model::Hash32;
use crate::provider::StagingFile;
// `write_all`, `read_exact` and `sync` reach us through the `StagingFile` and
// `Read` bounds, so those traits do not need importing here.
use std::io::{Read, Seek, SeekFrom};

/// Fixed chunk size (§13.1). Part of the wire contract; changing it changes
/// resume arithmetic on both sides, so it may not be negotiated per session.
pub const CHUNK_LEN: usize = 4 * 1024 * 1024;

/// Attempts per chunk before the asset itself is failed (§13.5).
pub const MAX_CHUNK_ATTEMPTS: u32 = 3;

/// One `CHUNK` frame (§18).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Chunk {
    /// Byte offset of this chunk within the asset. Carried explicitly rather
    /// than as an index so the receiver can reject a gap without inferring
    /// anything from a multiplication.
    pub offset: u64,
    pub payload: Vec<u8>,
    pub hash: Hash32,
}

impl Chunk {
    /// Offset immediately past this chunk.
    pub fn end(&self) -> u64 {
        self.offset + self.payload.len() as u64
    }
}

/// What the receiver says about one chunk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChunkVerdict {
    /// Written and flushed. `bytes_received` is now this value.
    Accepted { bytes_received: u64 },
    /// The payload does not match its declared hash. Resend this chunk only
    /// (§13.5). Nothing was written.
    Corrupt,
    /// The chunk does not start where the receiver expects. The sender must
    /// resume from `expected_offset`. Nothing was written.
    ///
    /// This is not an error condition: it is the normal answer when a sender
    /// reconnects with a stale idea of its own progress.
    Misaligned { expected_offset: u64 },
}

/// Outcome of `ASSET_END`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FinishVerdict {
    /// Hashes agree. The staging file is complete and may be committed.
    Verified { full_hash: Hash32 },
    /// Hashes disagree. Discard the staging file and retry the whole asset
    /// (§13.5); a chunk-level retry cannot help, because every chunk already
    /// passed its own check.
    Mismatch { expected: Hash32, actual: Hash32 },
    /// Fewer bytes arrived than the descriptor promised.
    Short { received: u64, expected: u64 },
}

// ---------------------------------------------------------------------------
// Receiver
// ---------------------------------------------------------------------------

/// Receives one asset into a staging file.
///
/// Holds no database handle on purpose. Persisting `bytes_received` is the
/// caller's job, and it must happen **after** [`ChunkVerdict::Accepted`] is
/// returned, because that is the point at which the bytes are durable.
pub struct InboundAsset<S: StagingFile> {
    staging: S,
    hasher: FullHasher,
    total_bytes: u64,
    /// Verified and durably written. Equals `hasher.bytes()` at all times.
    bytes_received: u64,
}

impl<S: StagingFile> InboundAsset<S> {
    /// Starts a fresh asset, discarding anything already in the staging file.
    pub fn begin(mut staging: S, total_bytes: u64) -> Result<Self> {
        staging.set_len(0)?;
        staging.seek(SeekFrom::Start(0))?;
        Ok(Self {
            staging,
            hasher: FullHasher::new(),
            total_bytes,
            bytes_received: 0,
        })
    }

    /// Resumes an interrupted asset from a persisted offset (§13.4).
    ///
    /// Two steps, in this order, and the order matters:
    ///
    /// 1. **Truncate to `bytes_received`.** The file may be longer, because a
    ///    write can land without its flush completing. Those trailing bytes are
    ///    not covered by the persisted offset and must go.
    /// 2. **Re-read `[0, bytes_received)` to rebuild the digest.** Platform
    ///    crypto APIs cannot import a partially-consumed hash state, so there is
    ///    no shortcut (§26.8). This is local flash I/O, no network cost, and it
    ///    is why the UI shows "Resuming…" rather than appearing to stall
    ///    (§19.3).
    pub fn resume(mut staging: S, total_bytes: u64, bytes_received: u64) -> Result<Self> {
        let on_disk = staging.len()?;
        if bytes_received > on_disk {
            // The persisted offset claims more than exists. Refuse to guess:
            // starting over is correct and cheap compared to a silent hole.
            return Err(Error::CorruptRow {
                table: "inbound_transfer",
                column: "bytes_received",
                detail: format!("offset {bytes_received} exceeds staging length {on_disk}"),
            });
        }
        if on_disk > bytes_received {
            staging.set_len(bytes_received)?;
        }

        let hasher = FullHasher::resume_from(&mut staging, bytes_received)?;
        debug_assert_eq!(hasher.bytes(), bytes_received);

        staging.seek(SeekFrom::Start(bytes_received))?;
        Ok(Self {
            staging,
            hasher,
            total_bytes,
            bytes_received,
        })
    }

    /// Offset the sender should send next. Returned in `ASSET_BEGIN` as the
    /// resume offset, and it is receiver-authoritative (§18).
    pub const fn resume_offset(&self) -> u64 {
        self.bytes_received
    }

    pub const fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Verifies, writes, flushes, and hashes one chunk — in that order.
    ///
    /// The digest is fed from the same buffer that goes to storage, in one pass
    /// (§13.2). Nothing is written until the chunk's own hash checks out, so a
    /// corrupt chunk cannot advance the file at all.
    pub fn accept_chunk(&mut self, chunk: &Chunk) -> Result<ChunkVerdict> {
        if chunk.offset != self.bytes_received {
            return Ok(ChunkVerdict::Misaligned {
                expected_offset: self.bytes_received,
            });
        }
        if chunk_hash(&chunk.payload) != chunk.hash {
            return Ok(ChunkVerdict::Corrupt);
        }
        if chunk.end() > self.total_bytes {
            // Would overrun the declared size. Treat as misaligned rather than
            // writing past the end.
            return Ok(ChunkVerdict::Misaligned {
                expected_offset: self.bytes_received,
            });
        }

        self.staging.seek(SeekFrom::Start(self.bytes_received))?;
        self.staging.write_all(&chunk.payload)?;
        // Durable before the offset is reportable. See rule 2 at the top.
        self.staging.sync()?;

        self.hasher.update(&chunk.payload);
        self.bytes_received = self.hasher.bytes();

        Ok(ChunkVerdict::Accepted {
            bytes_received: self.bytes_received,
        })
    }

    /// Compares the streamed digest against the sender's `ASSET_END`.
    ///
    /// A [`FinishVerdict::Verified`] result is the **only** thing that may
    /// authorise a commit into the photo library (§13.3).
    pub fn finish(self, declared: Hash32) -> Result<(FinishVerdict, S)> {
        if self.bytes_received != self.total_bytes {
            return Ok((
                FinishVerdict::Short {
                    received: self.bytes_received,
                    expected: self.total_bytes,
                },
                self.staging,
            ));
        }
        let staging = self.staging;
        let actual = match self.hasher.finish() {
            Some(h) => h,
            None => return Err(Error::EmptyAsset),
        };
        let verdict = if actual == declared {
            FinishVerdict::Verified { full_hash: actual }
        } else {
            FinishVerdict::Mismatch {
                expected: declared,
                actual,
            }
        };
        Ok((verdict, staging))
    }
}

// ---------------------------------------------------------------------------
// Sender
// ---------------------------------------------------------------------------

/// Produces chunks from an asset's original bytes.
///
/// The sender hashes the whole file as it streams, so a resume from a non-zero
/// offset has to rebuild its digest by re-reading the skipped prefix, exactly as
/// the receiver does. The bytes are not re-sent, only re-hashed.
pub struct OutboundAsset<R: Read + Seek> {
    source: R,
    hasher: FullHasher,
    total_bytes: u64,
    offset: u64,
}

impl<R: Read + Seek> OutboundAsset<R> {
    pub fn begin(mut source: R, total_bytes: u64) -> Result<Self> {
        source.seek(SeekFrom::Start(0))?;
        Ok(Self {
            source,
            hasher: FullHasher::new(),
            total_bytes,
            offset: 0,
        })
    }

    /// Starts at `offset`, rebuilding digest state over the skipped prefix.
    ///
    /// `offset` comes from the receiver, never from local memory (§18).
    pub fn resume(mut source: R, total_bytes: u64, offset: u64) -> Result<Self> {
        if offset > total_bytes {
            return Err(Error::CorruptRow {
                table: "(resume)",
                column: "offset",
                detail: format!("offset {offset} exceeds asset size {total_bytes}"),
            });
        }
        let hasher = FullHasher::resume_from(&mut source, offset)?;
        source.seek(SeekFrom::Start(offset))?;
        Ok(Self {
            source,
            hasher,
            total_bytes,
            offset,
        })
    }

    pub const fn offset(&self) -> u64 {
        self.offset
    }

    pub const fn is_complete(&self) -> bool {
        self.offset >= self.total_bytes
    }

    /// Reads the next chunk, or `None` when the asset is fully read.
    ///
    /// Advances the sender's digest as a side effect, which is why a chunk must
    /// not be produced twice: a retry resends the *same* [`Chunk`] value the
    /// caller already holds rather than calling this again.
    pub fn next_chunk(&mut self) -> Result<Option<Chunk>> {
        if self.is_complete() {
            return Ok(None);
        }
        let remaining = self.total_bytes - self.offset;
        let want = remaining.min(CHUNK_LEN as u64) as usize;

        let mut payload = vec![0u8; want];
        self.source.read_exact(&mut payload)?;

        let offset = self.offset;
        self.hasher.update(&payload);
        self.offset = self.hasher.bytes();

        Ok(Some(Chunk {
            offset,
            hash: chunk_hash(&payload),
            payload,
        }))
    }

    /// The value for `ASSET_END`. Available only once every chunk has been read.
    pub fn full_hash(&self) -> Option<Hash32> {
        if !self.is_complete() {
            return None;
        }
        self.hasher.clone().finish()
    }
}

/// Number of chunks an asset of `size` bytes will be split into.
pub const fn chunk_count(size: u64) -> u64 {
    size.div_ceil(CHUNK_LEN as u64)
}
