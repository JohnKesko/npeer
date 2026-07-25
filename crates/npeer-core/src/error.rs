//! Error types shared across the engine.

/// Convenience alias used throughout the engine.
pub type Result<T> = std::result::Result<T, Error>;

/// All recoverable failures the engine can report.
///
/// The CLI is expected to render these as human-readable messages, so every
/// variant carries enough context to stand on its own.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid bencode data: {0}")]
    Bencode(String),

    #[error("invalid torrent: {0}")]
    InvalidTorrent(String),

    #[error("invalid magnet link: {0}")]
    InvalidMagnet(String),

    #[error("tracker error: {0}")]
    Tracker(String),

    #[error("peer protocol error: {0}")]
    Protocol(String),

    #[error("could not fetch torrent metadata: {0}")]
    Metadata(String),

    #[error("storage error: {0}")]
    Storage(String),

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("timed out {0}")]
    Timeout(String),

    #[error("operation was cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Helper for ad-hoc protocol violations.
    pub fn protocol(msg: impl Into<String>) -> Self {
        Error::Protocol(msg.into())
    }
}
