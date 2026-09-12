//! PhotoSync engine core.
//!
//! Platform-agnostic by contract (`app_info.md` §17): this crate must not
//! reference PhotoKit, MediaStore, or any OS photo or networking API. It knows
//! Asset, Manifest, Transfer, State and Session. Everything platform-specific
//! arrives through a provider trait.
//!
//! Section numbers in doc comments refer to `app_info.md` revision 2, which is
//! the specification of record.

pub mod bringup;
pub mod catalog;
pub mod chunk;
pub mod db;
pub mod error;
pub mod identity;
pub mod inbound;
pub mod model;
pub mod pairing;
pub mod proto;
pub mod provider;
pub mod session;

pub use error::{Error, Result};
