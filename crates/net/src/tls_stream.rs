//! Re-exports of the TLS stream types.
//!
//! Callers get these from here rather than depending on `tokio-rustls` directly,
//! so the rustls and tokio-rustls versions are pinned in exactly one place. A
//! mismatch between two crates' rustls versions produces type errors that look
//! nothing like a version problem.

pub use rustls::pki_types::ServerName;
pub use tokio_rustls::{
    client::TlsStream as ClientTlsStream, server::TlsStream as ServerTlsStream,
};
pub use tokio_rustls::{TlsAcceptor, TlsConnector};
