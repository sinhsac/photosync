//! A drainable log buffer.
//!
//! Progress has to cross from a Rust engine thread to the Android UI thread. The
//! obvious approach — a JNI callback into Java — means the Rust thread has to
//! attach, hold a local frame, and find a method id, on every progress update,
//! and a mistake there is a hard crash rather than an error.
//!
//! Appending to a buffer that Java drains on a timer is duller and cannot crash.
//! It is also enough for bring-up: §19's real progress screen reads aggregate
//! counters from SQLite, not a log.

use std::sync::Mutex;

static BUFFER: Mutex<String> = Mutex::new(String::new());

/// Appends a line.
pub fn line(text: impl AsRef<str>) {
    if let Ok(mut buf) = BUFFER.lock() {
        // Bound it. A stuck UI must not turn a long sync into unbounded memory.
        if buf.len() > 64 * 1024 {
            buf.clear();
            buf.push_str("[log truncated]\n");
        }
        buf.push_str(text.as_ref());
        buf.push('\n');
    }
}

/// Takes everything buffered so far, leaving it empty.
pub fn drain() -> String {
    BUFFER
        .lock()
        .map(|mut buf| std::mem::take(&mut *buf))
        .unwrap_or_default()
}
