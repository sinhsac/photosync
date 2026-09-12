//! JVM plumbing.
//!
//! Two things have to be reachable from any Rust thread: the `JavaVM`, so a
//! thread can attach, and the application `Context`, because every MediaStore
//! call needs one.
//!
//! The engine runs on its own thread (a `rusqlite::Connection` is not `Sync`, so
//! a session owns one thread — see `crates/dev/src/sync.rs`). That thread is not
//! the one Java called in on, so it must attach itself before any JNI call.

use jni::objects::{GlobalRef, JObject};
use jni::{AttachGuard, JavaVM};
use std::sync::OnceLock;

static VM: OnceLock<JavaVM> = OnceLock::new();
static CONTEXT: OnceLock<GlobalRef> = OnceLock::new();

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
