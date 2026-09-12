//! Filesystem-backed library providers for the harness.
//!
//! Stands in for PhotoKit and MediaStore. Not a mock: it satisfies the same
//! contract the real adapters must, including the one rule that matters most —
//! `commit` is the only call that makes an asset visible, and staging lives
//! somewhere the "library" never looks (§13.3).

use photosync_core::model::{AssetDescriptor, Hash32};
use photosync_core::provider::{FileStaging, LibrarySink, LibrarySource, ReadSeek, StagingFile};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

    /// Visible assets only. Staging is a dotted subdirectory and is skipped, the
    /// same way a pending MediaStore row is invisible to the gallery.
    pub fn visible(&self) -> io::Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.library)? {
            let entry = entry?;
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
    fn staging(&self, key: &Hash32) -> io::Result<(String, Box<dyn StagingFile + Send>)> {
        // Named by content key so reopening after a restart finds the same bytes,
        // which is what resume across a process death depends on (§13.4).
        let path = self.staging.join(format!("{}.part", key.to_hex()));
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
