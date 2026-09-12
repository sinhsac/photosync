//! MediaStore-backed photo library (`app_info.md` §10.3, §10.4).
//!
//! The engine never sees any of this — it sees `LibrarySource` and `LibrarySink`
//! (§17). What lives here is the translation to Android, and it is deliberately
//! thin: all ContentResolver work is done by a small Java helper, and this module
//! only calls six static methods on it.
//!
//! Doing ContentResolver, ContentValues and Uri manipulation in raw JNI is a
//! large amount of error-prone boilerplate for no benefit. Six calls with simple
//! signatures is a surface small enough to reason about.
//!
//! # Why a raw file descriptor
//!
//! The pending MediaStore item *is* the staging file (§10.4), so no copy happens
//! at commit — publishing is a metadata update. To make that work the engine
//! needs random access to it, which is why [`StagingFile`] requires
//! `Read + Write + Seek`: on resume it has to truncate and re-read (§13.4).
//!
//! Java hands over an owned descriptor from `ParcelFileDescriptor.detachFd()`, and
//! `File::from_raw_fd` adopts it. Ownership transfers exactly once; Java must not
//! close it afterwards.

use crate::jvm;
use jni::objects::{JObject, JString, JValue};
use photosync_core::model::{AssetDescriptor, Hash32, MediaType};
use photosync_core::provider::{LibrarySink, LibrarySource, ReadSeek, StagingFile};
use std::io;
use std::os::fd::FromRawFd;

const HELPER: &str = "app/photosync/PhotoStore";

/// Turns a JNI failure into an `io::Error`, since that is what the provider
/// traits speak.
fn jni_err(what: &str, e: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{what}: {e}"))
}

/// Calls a static Java method returning `String`.
fn call_static_string(method: &str, sig: &str, args: &[JValue]) -> io::Result<String> {
    let mut env = jvm::attach().map_err(|e| jni_err("attach", e))?;
    let ctx = jvm::context().map_err(|e| jni_err("context", e))?;

    // Every helper method takes the Context first; passing it explicitly keeps
    // the Java side free of static state.
    let mut full: Vec<JValue> = Vec::with_capacity(args.len() + 1);
    let ctx_obj = unsafe { JObject::from_raw(ctx.as_raw()) };
    full.push(JValue::Object(&ctx_obj));
    full.extend_from_slice(args);

    let out = env
        .call_static_method(HELPER, method, sig, &full)
        .map_err(|e| jni_err(method, e))?;
    let obj = out.l().map_err(|e| jni_err(method, e))?;
    if obj.is_null() {
        return Err(io::Error::other(format!("{method} returned null")));
    }
    let s: String = env
        .get_string(&JString::from(obj))
        .map_err(|e| jni_err(method, e))?
        .into();
    Ok(s)
}

/// Calls a static Java method returning `int`.
fn call_static_int(method: &str, sig: &str, args: &[JValue]) -> io::Result<i32> {
    let mut env = jvm::attach().map_err(|e| jni_err("attach", e))?;
    let ctx = jvm::context().map_err(|e| jni_err("context", e))?;

    let mut full: Vec<JValue> = Vec::with_capacity(args.len() + 1);
    let ctx_obj = unsafe { JObject::from_raw(ctx.as_raw()) };
    full.push(JValue::Object(&ctx_obj));
    full.extend_from_slice(args);

    let out = env
        .call_static_method(HELPER, method, sig, &full)
        .map_err(|e| jni_err(method, e))?;
    out.i().map_err(|e| jni_err(method, e))
}

/// Calls a static Java method returning `void`.
fn call_static_void(method: &str, sig: &str, args: &[JValue]) -> io::Result<()> {
    let mut env = jvm::attach().map_err(|e| jni_err("attach", e))?;
    let ctx = jvm::context().map_err(|e| jni_err("context", e))?;

    let mut full: Vec<JValue> = Vec::with_capacity(args.len() + 1);
    let ctx_obj = unsafe { JObject::from_raw(ctx.as_raw()) };
    full.push(JValue::Object(&ctx_obj));
    full.extend_from_slice(args);

    env.call_static_method(HELPER, method, sig, &full)
        .map_err(|e| jni_err(method, e))?;
    Ok(())
}

/// Wraps an owned descriptor handed over by Java.
///
/// Adopting the descriptor means Java has already detached it and must not close
/// it. Dropping this closes it exactly once.
fn file_from_fd(fd: i32) -> io::Result<std::fs::File> {
    if fd < 0 {
        return Err(io::Error::other("Java returned an invalid descriptor"));
    }
    Ok(unsafe { std::fs::File::from_raw_fd(fd) })
}

// ---------------------------------------------------------------------------
// Write side (§10.4)
// ---------------------------------------------------------------------------

/// Writes into MediaStore.
///
/// The commit sequence is exactly §10.4's: insert with `IS_PENDING = 1`, stream
/// into the returned URI, verify, then clear the flag. A crash at any point leaves
/// an invisible pending row, never a corrupt gallery entry.
pub struct MediaStoreSink;

impl LibrarySink for MediaStoreSink {
    fn staging(&self, key: &Hash32) -> io::Result<(String, Box<dyn StagingFile + Send>)> {
        // Named by content key so a restart reopens the same pending item, which
        // is what resume across a process death depends on (§13.4).
        //
        // The display name is provisional. `publish` renames it to the real one,
        // because the descriptor is only known after ASSET_BEGIN and MediaStore
        // wants a name at insert time.
        let env = jvm::attach().map_err(|e| jni_err("attach", e))?;
        // `new_string` only needs a shared borrow; `call_static_method` needs a
        // mutable one, which is why the helpers above hold `mut` and these do not.
        let name = env
            .new_string(format!("psync-{}.tmp", key.to_hex()))
            .map_err(|e| jni_err("new_string", e))?;
        drop(env);

        let uri = call_static_string(
            "createPending",
            "(Landroid/content/Context;Ljava/lang/String;)Ljava/lang/String;",
            &[JValue::Object(&JObject::from(name))],
        )?;

        let env = jvm::attach().map_err(|e| jni_err("attach", e))?;
        let juri = env
            .new_string(&uri)
            .map_err(|e| jni_err("new_string", e))?;
        drop(env);

        let fd = call_static_int(
            "openPendingFd",
            "(Landroid/content/Context;Ljava/lang/String;)I",
            &[JValue::Object(&JObject::from(juri))],
        )?;

        Ok((uri, Box::new(FdStaging::new(file_from_fd(fd)?))))
    }

    fn commit(&self, staging_ref: &str, descriptor: &AssetDescriptor) -> io::Result<String> {
        let env = jvm::attach().map_err(|e| jni_err("attach", e))?;
        let juri = env
            .new_string(staging_ref)
            .map_err(|e| jni_err("new_string", e))?;
        let jname = env
            .new_string(
                descriptor
                    .display_name
                    .clone()
                    .unwrap_or_else(|| format!("{}.jpg", descriptor.quick_hash.short())),
            )
            .map_err(|e| jni_err("new_string", e))?;
        let jmime = env
            .new_string(&descriptor.mime)
            .map_err(|e| jni_err("new_string", e))?;
        drop(env);

        // `created_at` is passed so the gallery sorts the asset by when the photo
        // was taken, not when it arrived (§16).
        call_static_string(
            "publish",
            "(Landroid/content/Context;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;JZ)\
             Ljava/lang/String;",
            &[
                JValue::Object(&JObject::from(juri)),
                JValue::Object(&JObject::from(jname)),
                JValue::Object(&JObject::from(jmime)),
                JValue::Long(descriptor.created_at),
                JValue::Bool(u8::from(descriptor.media_type == MediaType::Video)),
            ],
        )
    }

    fn abandon(&self, staging_ref: &str) -> io::Result<()> {
        let env = jvm::attach().map_err(|e| jni_err("attach", e))?;
        let juri = env
            .new_string(staging_ref)
            .map_err(|e| jni_err("new_string", e))?;
        drop(env);

        call_static_void(
            "abandon",
            "(Landroid/content/Context;Ljava/lang/String;)V",
            &[JValue::Object(&JObject::from(juri))],
        )
    }
}

/// [`StagingFile`] over a descriptor from MediaStore.
pub struct FdStaging {
    file: std::fs::File,
}

impl FdStaging {
    pub fn new(file: std::fs::File) -> Self {
        Self { file }
    }
}

impl io::Read for FdStaging {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(&mut self.file, buf)
    }
}

impl io::Write for FdStaging {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        io::Write::write(&mut self.file, buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.file)
    }
}

impl io::Seek for FdStaging {
    fn seek(&mut self, pos: io::SeekFrom) -> io::Result<u64> {
        io::Seek::seek(&mut self.file, pos)
    }
}

impl StagingFile for FdStaging {
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        io::Write::flush(&mut self.file)?;
        // Must actually reach the device: `bytes_received` is persisted only after
        // this returns, and resume correctness depends on that ordering (§13.4).
        self.file.sync_all()
    }

    fn len(&mut self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

// ---------------------------------------------------------------------------
// Read side (§10.3)
// ---------------------------------------------------------------------------

/// Reads originals out of MediaStore.
pub struct MediaStoreSource;

impl LibrarySource for MediaStoreSource {
    fn open_original(&self, platform_asset_id: &str) -> io::Result<Box<dyn ReadSeek + Send>> {
        let env = jvm::attach().map_err(|e| jni_err("attach", e))?;
        let jid = env
            .new_string(platform_asset_id)
            .map_err(|e| jni_err("new_string", e))?;
        drop(env);

        let fd = call_static_int(
            "openOriginalFd",
            "(Landroid/content/Context;Ljava/lang/String;)I",
            &[JValue::Object(&JObject::from(jid))],
        )?;
        Ok(Box::new(file_from_fd(fd)?))
    }
}

/// One row of the library, as the Java helper reports it.
#[derive(Debug, serde::Deserialize)]
pub struct ScannedRow {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// Unix millis.
    pub taken: i64,
    pub modified: i64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration: Option<u64>,
    pub video: bool,
}

/// Enumerates images and videos (§10.3).
///
/// Returns JSON rather than a stream of JNI objects: one call and one parse is far
/// cheaper than thousands of JNI round trips, and §21 forbids holding the library
/// in memory only in the sense of *media bytes* — a catalogue row is tiny. For a
/// 500,000-asset library this should become paged, and the Java side already
/// supports a limit and offset for that reason.
pub fn enumerate(limit: i32, offset: i32) -> io::Result<Vec<ScannedRow>> {
    let json = call_static_string(
        "enumerate",
        "(Landroid/content/Context;II)Ljava/lang/String;",
        &[JValue::Int(limit), JValue::Int(offset)],
    )?;
    serde_json::from_str(&json)
        .map_err(|e| io::Error::other(format!("cannot parse the library listing: {e}")))
}
