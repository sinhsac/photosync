//! The Android native library.
//!
//! Holds no engine logic. Its whole job is to be the boundary: translate MediaStore
//! into the provider traits (`store`), keep the JVM reachable from engine threads
//! (`jvm`), and expose a handful of entry points to Kotlin.
//!
//! Every exported function catches panics. Unwinding across the JNI boundary is
//! undefined behaviour, and a Rust panic reaching the JVM is a process abort with
//! no useful diagnostic — so a panic becomes an error string instead.

mod jvm;
mod log;
mod session;
mod store;

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jint, jstring};
use jni::{JNIEnv, JavaVM};

/// Records the `JavaVM` the moment the library is loaded.
///
/// The only place a `JavaVM` is available without an env, and the reason an engine
/// thread can attach itself later.
#[no_mangle]
pub extern "system" fn JNI_OnLoad(vm: JavaVM, _reserved: *mut std::ffi::c_void) -> jint {
    jvm::set_vm(vm);
    jni::sys::JNI_VERSION_1_6
}

/// Turns a Rust `String` into a Java string, or returns null on failure.
fn to_jstring(env: &mut JNIEnv, s: String) -> jstring {
    env.new_string(s)
        .map(|j| j.into_raw())
        .unwrap_or(std::ptr::null_mut())
}

/// Reads a Java string, with a fallback so a null never panics.
fn from_jstring(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s)
        .map(|v| v.into())
        .unwrap_or_else(|_| String::new())
}

/// Runs `f`, converting a panic into an error string rather than letting it cross
/// the boundary.
fn guarded(what: &str, f: impl FnOnce() -> String + std::panic::UnwindSafe) -> String {
    match std::panic::catch_unwind(f) {
        Ok(s) => s,
        Err(_) => {
            let msg = format!("{{\"ok\":false,\"error\":\"{what} panicked\"}}");
            log::line(format!("{what} panicked"));
            msg
        }
    }
}

/// Stores the application `Context`. Must be called before anything else.
#[no_mangle]
pub extern "system" fn Java_app_photosync_Native_nativeInit(
    mut env: JNIEnv,
    _class: JClass,
    context: JObject,
) -> jstring {
    let result = match init_globals(&mut env, &context) {
        Ok(()) => {
            log::line("native library ready");
            "{\"ok\":true}".to_string()
        }
        Err(e) => format!("{{\"ok\":false,\"error\":\"{e}\"}}"),
    };
    to_jstring(&mut env, result)
}

/// Promotes the `Context` and resolves the MediaStore helper class.
///
/// Both must happen here, on the thread Java called in on. The `Context` because a
/// local reference dies when this method returns; the class because this is the
/// only thread with the *application* class loader. Engine threads attach later
/// with the system loader and cannot resolve anything from the APK by name, so a
/// lookup deferred to first use fails with `ClassNotFoundException` at the worst
/// possible moment — mid-transfer, on a background thread, as an unhandled Java
/// exception that takes the process down.
fn init_globals(env: &mut JNIEnv, context: &JObject) -> Result<(), String> {
    let ctx = env
        .new_global_ref(context)
        .map_err(|e| format!("cannot hold the Context: {e}"))?;
    jvm::set_context(ctx);

    let class = env
        .find_class("app/photosync/PhotoStore")
        .map_err(|e| format!("cannot find app.photosync.PhotoStore: {e}"))?;
    let class = env
        .new_global_ref(&class)
        .map_err(|e| format!("cannot hold PhotoStore: {e}"))?;
    jvm::set_helper_class(class);

    Ok(())
}

/// Engine self check (§22.3): proves SQLite linked, migrations run, and the
/// candidate query behaves — including the quick-hash collision case.
#[no_mangle]
pub extern "system" fn Java_app_photosync_Native_nativeSelfCheck(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let out = guarded("selfCheck", || {
        match photosync_core::bringup::self_check() {
            Ok(c) => format!(
                "{{\"ok\":{},\"schema\":{},\"candidates\":{},\"remaining\":{}}}",
                c.is_ok(),
                c.schema_version,
                c.candidates,
                c.remaining
            ),
            Err(e) => format!("{{\"ok\":false,\"error\":\"{e}\"}}"),
        }
    });
    to_jstring(&mut env, out)
}

/// Catalogues and hashes the real photo library. Call off the UI thread.
#[no_mangle]
pub extern "system" fn Java_app_photosync_Native_nativeScanLibrary(
    mut env: JNIEnv,
    _class: JClass,
    db_path: JString,
) -> jstring {
    let path = from_jstring(&mut env, &db_path);
    let out = guarded("scanLibrary", || session::scan_library(&path));
    to_jstring(&mut env, out)
}

/// Starts receiving. Returns immediately with the pairing code (§6).
#[no_mangle]
pub extern "system" fn Java_app_photosync_Native_nativeStartReceiver(
    mut env: JNIEnv,
    _class: JClass,
    db_path: JString,
) -> jstring {
    let path = from_jstring(&mut env, &db_path);
    let out = guarded("startReceiver", || session::start_receiver(&path));
    to_jstring(&mut env, out)
}

/// Starts sending to whoever answers `code`. `addr` may be empty to discover.
#[no_mangle]
pub extern "system" fn Java_app_photosync_Native_nativeStartSender(
    mut env: JNIEnv,
    _class: JClass,
    db_path: JString,
    code: JString,
    addr: JString,
) -> jstring {
    let path = from_jstring(&mut env, &db_path);
    let code = from_jstring(&mut env, &code);
    let addr = from_jstring(&mut env, &addr);
    let addr = if addr.trim().is_empty() {
        None
    } else {
        Some(addr)
    };
    let out = guarded("startSender", || session::start_sender(&path, &code, addr));
    to_jstring(&mut env, out)
}

/// Session status as JSON, for polling.
#[no_mangle]
pub extern "system" fn Java_app_photosync_Native_nativeStatus(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let out = guarded("status", session::status_json);
    to_jstring(&mut env, out)
}

/// Drains buffered log lines.
#[no_mangle]
pub extern "system" fn Java_app_photosync_Native_nativeDrainLog(
    mut env: JNIEnv,
    _class: JClass,
) -> jstring {
    let out = guarded("drainLog", log::drain);
    to_jstring(&mut env, out)
}
