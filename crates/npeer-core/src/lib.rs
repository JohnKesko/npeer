//! # npeer-core
//!
//! The BitTorrent engine behind the `npeer` client: bencoding, torrent and
//! magnet parsing, tracker communication (HTTP and UDP), the peer wire
//! protocol with BEP 10 extensions, piece scheduling, storage, verification
//! and session management.
//!
//! This crate is deliberately free of any command-line or terminal concerns
//! so it can power other frontends (a TUI, a daemon, tests) unchanged.

pub mod bencode;
pub mod error;
pub mod magnet;
pub mod metadata;
pub mod metainfo;
pub mod peer;
pub mod picker;
pub mod ratelimit;
pub mod session;
pub mod stats;
pub mod storage;
pub mod tracker;
pub mod types;
pub mod util;
pub mod verify;

pub use error::{Error, Result};
pub use magnet::MagnetLink;
pub use metainfo::Metainfo;
pub use picker::BLOCK_SIZE;
pub use types::{InfoHash, PeerId};

/// Engine version, taken from the crate version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
