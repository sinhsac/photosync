//! Transport for PhotoSync: TCP, TLS 1.3, and frame I/O.
//!
//! Separate from `photosync-core` so the engine keeps no dependency on tokio or
//! rustls and stays testable without a network (`app_info.md` §17).

pub mod frame;
pub mod identity;
pub mod link;
pub mod orchestrator;
pub mod tls;
pub mod tls_stream;

pub use identity::Identity;
pub use tls::{ObservedPeer, Pinning};
