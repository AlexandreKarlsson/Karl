//! Karl core: everything the daemon, the CLI and a future agent share.
//!
//! - [`model`]: one `Item` type for mail, chat messages, calendar events and tasks.
//! - [`store`]: the SQLite cache every source syncs into and every command reads from.
//! - [`policy`]: the daemon config (`karl.toml`): sources, profiles, modes, limits, hide and redact rules.
//! - [`render`]: token-minimal text lines; the only thing a model ever sees.
//! - [`commands`]: the command table, one source of truth for parsing, help and the index.
//! - [`proto`]: the unix-socket wire format between `karl` (client) and `karld` (daemon).
//! - [`text`]: HTML stripping, quote/signature removal, truncation.
//!
//! The crate has no network and no async: a pure library, testable in memory.

pub mod commands;
pub mod model;
pub mod policy;
pub mod proto;
pub mod render;
pub mod store;
pub mod text;

pub use model::{Item, Kind};
pub use store::Store;
