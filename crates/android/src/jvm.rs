//! JVM plumbing.
//!
//! Two things have to be reachable from any Rust thread: the `JavaVM`, so a
//! thread can attach, and the application `Context`, because every MediaStore
//! call needs one.
//!
//! The engine runs on its own thread (a `rusqlite::Connection` is not `Sync`, so
//! a session owns one thread — see `crates/dev/src/sync.rs`). That thread is not
//! the one Java called in on, so it must attach itself before any JNI call.

use jni::objects::{GlobalRef, JClass, JObject};
use jni::{AttachGuard, JavaVM};
use std::sync::OnceLock;

static VM: OnceLock<JavaVM> = OnceLock::new();
static CONTEXT: OnceLock<GlobalRef> = OnceLock::new();
static HELPER_CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// Records the `JavaVM`. Called from `JNI_OnLoad`, which is the only place it is
/// available without an env.
pub fn set_vm(vm: JavaVM) {
    let _ = VM.set(vm);
}

/// Records the application `Context` as a global reference.
///
/// A local reference would be invalid the moment the calling Java method returns,
/// so this must be promoted. The `Context` outlives every session, so a leak here
/// is bounded and deliberate.
pub fn set_context(context: GlobalRef) {
    let _ = CONTEXT.set(context);
}

/// Attaches the current thread and returns a guard.
///
/// `attach_current_thread` detaches on drop, which is what we want: the engine
/// thread should not stay attached across its whole lifetime holding a JNI frame
/// open.
pub fn attach() -> Result<AttachGuard<'static>, String> {
    let vm = VM.get().ok_or("JNI_OnLoad has not run")?;
    vm.attach_current_thread()
        .map_err(|e| format!("cannot attach this thread to the JVM: {e}"))
}

/// The application `Context`.
pub fn context() -> Result<&'static JObject<'static>, String> {
    CONTEXT
        .get()
        .map(|g| g.as_obj())
        .ok_or_else(|| "the Context has not been set; call Native.init first".to_string())
}

/// Records `app.photosync.PhotoStore` as a global reference.
///
/// # Why this is not optional
///
/// A thread attached with `AttachCurrentThread` gets the *system* class loader,
/// not the application's. `find_class("app/photosync/PhotoStore")` from an engine
/// thread therefore fails with `ClassNotFoundException`, even though the class is
/// plainly in the APK — the search path is `/system/lib64` and nothing else.
///
/// Resolving the class once from a thread Java called in on, and keeping a global
/// reference to it, sidesteps the class loader entirely: afterwards every call is
/// made against an object, and no name lookup happens on an engine thread.
///
/// The alternative — stashing the app's `ClassLoader` and invoking `loadClass`
/// reflectively — costs a reflective call per lookup and solves no extra problem.
pub fn set_helper_class(class: GlobalRef) {
    let _ = HELPER_CLASS.set(class);
}

/// The cached helper class, usable from any attached thread.
///
/// Safe to reconstruct from the raw pointer because the [`GlobalRef`] behind it
/// lives in a `OnceLock` for the life of the process, and a `JClass` built this
/// way is a borrow that deletes nothing on drop.
pub fn helper_class() -> Result<JClass<'static>, String> {
    let global = HELPER_CLASS
        .get()
        .ok_or("PhotoStore has not been resolved; call Native.init first")?;
    Ok(unsafe { JClass::from_raw(global.as_raw()) })
}
