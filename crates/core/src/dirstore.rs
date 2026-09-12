//! Filesystem-backed library providers for the harness.
//! A photo library that is just a directory.
//!
//! Stands in for PhotoKit and MediaStore. Not a mock: it satisfies the same
//! contract the real adapters must, including the one rule that matters most —
//! `commit` is the only call that makes an asset visible, and staging lives
//! somewhere the "library" never looks (§13.3).
//!
//! Lives in `core` rather than in the dev harness because it is the desktop
//! implementation, not a test double. A PC has no MediaStore and no PhotoKit; a
//! folder *is* its photo library. The same code therefore backs both the harness and
//! the desktop binary, which is the point — two copies had already drifted apart
//! once, and the copy the sender used reported every file as
//! `application/octet-stream`.

use crate::catalog::ScannedAsset;
use crate::model::{AssetDescriptor, MediaType};
use crate::provider::{FileStaging, LibrarySink, LibrarySource, ReadSeek, StagingFile};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Maps a file extension to a media type and MIME type.
///
/// `None` means "not media", which is how the harness skips its own database and
/// any stray file in the library directory.
///
/// This mapping is the harness's stand-in for what MediaStore and PhotoKit report,
/// and it has to be right for a reason that is not obvious: the receiver inserts
/// into a MediaStore *collection* chosen from the media type, and MediaStore then
/// refuses any MIME the collection does not accept. A sender that reports
/// `application/octet-stream` for a JPEG gets its bytes transferred, verified, and
/// then rejected at publish — the asset is lost at the last step, with the failure
/// appearing on the receiver.
pub fn media_kind(path: &Path) -> Option<(MediaType, &'static str)> {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => (MediaType::Image, "image/jpeg"),
        "png" => (MediaType::Image, "image/png"),
        "heic" | "heif" => (MediaType::Image, "image/heic"),
        "webp" => (MediaType::Image, "image/webp"),
        "gif" => (MediaType::Image, "image/gif"),
        "mp4" | "m4v" => (MediaType::Video, "video/mp4"),
        "mov" => (MediaType::Video, "video/quicktime"),
        // The synthetic fixtures the harness generates. Kept because a transfer of
        // opaque bytes is still a valid transfer test, and because it is the case
        // that exercises the receiver's MIME fallback.
        "bin" => (MediaType::Image, "application/octet-stream"),
        _ => return None,
    })
}

/// Builds a [`ScannedAsset`] from a real file.
///
/// The absolute path stands in for the platform asset id, which is exactly how it
/// is used: a local key only (§11). Returns `None` for an empty file or one that is
/// not recognisable media — neither is a gallery entry.
pub fn describe(path: &Path) -> Option<ScannedAsset> {
    let meta = fs::metadata(path).ok()?;
    if meta.len() == 0 {
        return None;
    }
    let (media_type, mime) = media_kind(path)?;

    // The filesystem has no "date taken", so mtime stands in for it. That is what
    // §16 carries to the receiver, and it is why the fixtures set mtime explicitly.
    let modified = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    Some(ScannedAsset {
        platform_asset_id: path.to_string_lossy().into_owned(),
        size: meta.len(),
        media_type,
        mime: mime.to_string(),
        created_at: modified,
        modified_at: modified,
        width: None,
        height: None,
        duration_ms: None,
        display_name: path.file_name().map(|s| s.to_string_lossy().into_owned()),
        resource_group_id: None,
        is_local: true,
    })
}

/// Reads originals from a directory. `platform_asset_id` is the absolute path,
/// which is exactly how a platform id is used: a local key only (§11).
pub struct DirSource;

impl LibrarySource for DirSource {
    fn open_original(&self, platform_asset_id: &str) -> io::Result<Box<dyn ReadSeek + Send>> {
        Ok(Box::new(fs::File::open(platform_asset_id)?))
    }
}

/// Writes into a directory, staging in a sibling `.staging` folder.
pub struct DirSink {
    library: PathBuf,
    staging: PathBuf,
}

impl DirSink {
    pub fn new(library: impl AsRef<Path>) -> io::Result<Self> {
        let library = library.as_ref().to_path_buf();
        let staging = library.join(".staging");
        fs::create_dir_all(&library)?;
        fs::create_dir_all(&staging)?;
        Ok(Self { library, staging })
    }

    /// Visible assets only.
    ///
    /// Dotfiles are excluded, not just the `.staging` directory. The harness puts
    /// its database beside the library, and counting it as an asset made a
    /// successful 5-asset transfer report "8 files" — which reads as a bug in the
    /// transfer rather than in the reporting. A hidden file is not a gallery
    /// entry, the same way a pending MediaStore row is invisible.
    pub fn visible(&self) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.library)? {
            let entry = entry?;
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            if hidden {
                continue;
            }
            if entry.file_type()?.is_file() {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn staging_dir(&self) -> &Path {
        &self.staging
    }
}

impl LibrarySink for DirSink {
    fn staging(
        &self,
        descriptor: &AssetDescriptor,
    ) -> io::Result<(String, Box<dyn StagingFile + Send>)> {
        // Named by content key so reopening after a restart finds the same bytes,
        // which is what resume across a process death depends on (§13.4).
        //
        // A directory has no collections and no MIME registry, so the rest of the
        // descriptor is genuinely not needed here.
        let path = self
            .staging
            .join(format!("{}.part", descriptor.quick_hash.to_hex()));
        let file = FileStaging::create(&path)?;
        Ok((path.to_string_lossy().into_owned(), Box::new(file)))
    }

    fn commit(&self, staging_ref: &str, descriptor: &AssetDescriptor) -> io::Result<String> {
        let name = descriptor
            .display_name
            .clone()
            .unwrap_or_else(|| format!("{}.bin", descriptor.quick_hash.short()));

        // Never overwrite: two distinct assets can legitimately share a display
        // name, and silently clobbering one would be exactly the data loss the
        // rest of the design works to avoid.
        let mut dest = self.library.join(&name);
        let mut n = 1u32;
        while dest.exists() {
            let stem = Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "asset".into());
            let ext = Path::new(&name)
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy()))
                .unwrap_or_default();
            dest = self.library.join(format!("{stem}-{n}{ext}"));
            n += 1;
        }

        // Rename, not copy: same volume, and it is atomic. §13.3 prefers this,
        // with copy-and-reverify only where a rename is impossible.
        fs::rename(staging_ref, &dest)?;

        // The creation date comes from the source, so the receiver's timeline is
        // not all "today" (§16). Only mtime is settable portably; the real
        // adapters set the asset's creation date through the platform API.
        let mtime = std::time::UNIX_EPOCH
            + std::time::Duration::from_millis(descriptor.created_at.max(0) as u64);
        let _ = fs::File::options()
            .write(true)
            .open(&dest)
            .and_then(|f| f.set_modified(mtime));

        Ok(dest.to_string_lossy().into_owned())
    }

    fn abandon(&self, staging_ref: &str) -> io::Result<()> {
        match fs::remove_file(staging_ref) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}
