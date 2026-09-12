//! Provider interfaces (`app_info.md` §17).
//!
//! The engine talks only to these. No implementation in this crate may reference
//! PhotoKit, MediaStore, or a socket.

use std::io::{self, Read, Seek, Write};

/// A partially received asset on disk, before it is committed to the photo
/// library (§13.3).
///
/// On iOS and desktop this is an app-private temp file. On Android it is the
/// `IS_PENDING` MediaStore URI opened `"rw"`, which is why random access is part
/// of the contract rather than write-only: the resume path has to seek and
/// truncate (§13.4).
///
/// Nothing here is ever visible to the user. A crash leaves either a temp file
/// or an invisible pending row, never a gallery entry (§10.4).
pub trait StagingFile: Read + Write + Seek {
    /// Truncates or extends to `len`.
    ///
    /// Used on resume to drop bytes past the last verified, flushed chunk
    /// boundary. Those bytes may have been written but not durably, so they are
    /// not trustworthy.
    fn set_len(&mut self, len: u64) -> io::Result<()>;

    /// Flushes to durable storage.
    ///
    /// Must actually reach the device, not just the OS buffer. `bytes_received`
    /// is persisted only after this returns, and the ordering is what makes
    /// resume correct: a value on disk always describes bytes that are also on
    /// disk (§13.4).
    fn sync(&mut self) -> io::Result<()>;

    /// Current length.
    fn len(&mut self) -> io::Result<u64>;

    /// Whether the staging file holds nothing yet.
    fn is_empty(&mut self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// Lets a boxed staging file be used wherever a concrete one can be.
///
/// `Read`, `Write` and `Seek` already forward through `Box` in std; this adds the
/// three methods that are ours. Without it the orchestrator would have to be
/// generic over the staging type, which would push a platform detail up through
/// every layer for no benefit.
impl<T: StagingFile + ?Sized> StagingFile for Box<T> {
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        (**self).set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        (**self).sync()
    }

    fn len(&mut self) -> io::Result<u64> {
        (**self).len()
    }
}

/// Readable, seekable original bytes.
///
/// Seekable because a resumed transfer re-reads its prefix to rebuild the digest
/// (§13.4). On iOS this wraps a `PHAssetResourceManager` stream; a plain
/// `UIImage` round trip would recompress and is forbidden (§10.1).
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// The read side of the photo library (§10.1, §10.3).
pub trait LibrarySource {
    /// Opens an asset's **original** bytes, unmodified (§16).
    fn open_original(&self, platform_asset_id: &str) -> io::Result<Box<dyn ReadSeek + Send>>;
}

/// The write side of the photo library (§10.2, §10.4).
///
/// Split into staging and commit because §13.3 forbids a partially written file
/// from ever becoming a gallery entry. `commit` is the only call that makes an
/// asset visible, and it may only be reached after a whole-file hash match.
pub trait LibrarySink {
    /// Creates or reopens staging for an asset.
    ///
    /// Returns an opaque reference to persist in `inbound_transfer.staging_ref`,
    /// plus the handle. Reopening with the same `descriptor.quick_hash` must return
    /// the same bytes, or resume cannot work across a restart.
    ///
    /// # Why the whole descriptor and not just the key
    ///
    /// The key alone is enough to name a temp file, and that is all the filesystem
    /// implementation uses. MediaStore is different: a pending row is created inside
    /// a *collection* (`Images` or `Video`) with a MIME type, and neither can be
    /// changed afterwards — an `update` that tries to move a row between
    /// collections, or to set a MIME the collection does not accept, is rejected
    /// with `IllegalArgumentException`.
    ///
    /// So the decision has to be made here, at insert time. It can be: `ASSET_BEGIN`
    /// carries the descriptor and arrives before this call, so the media type is
    /// already known. Passing only the key made a value that was available look
    /// unavailable, and pushed an impossible fix-up into `commit`.
    fn staging(
        &self,
        descriptor: &crate::model::AssetDescriptor,
    ) -> io::Result<(String, Box<dyn StagingFile + Send>)>;

    /// Publishes a verified staging file into the library.
    ///
    /// On Android this clears `IS_PENDING`; on iOS it hands the file to
    /// `PHAssetCreationRequest`. Returns the platform asset id.
    ///
    /// Must set the asset's creation date from `descriptor.created_at`, so the
    /// receiver's timeline is not all "today" (§16).
    fn commit(
        &self,
        staging_ref: &str,
        descriptor: &crate::model::AssetDescriptor,
    ) -> io::Result<String>;

    /// Discards staging without publishing. Used on hash mismatch and on cancel.
    fn abandon(&self, staging_ref: &str) -> io::Result<()>;
}

/// [`StagingFile`] over a plain filesystem file. Used on iOS, on desktop, and by
/// the dev harness.
pub struct FileStaging {
    file: std::fs::File,
}

impl FileStaging {
    pub fn create(path: impl AsRef<std::path::Path>) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        Ok(Self { file })
    }
}

impl Read for FileStaging {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for FileStaging {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for FileStaging {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl StagingFile for FileStaging {
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_all()
    }

    fn len(&mut self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}
