use thiserror::Error;

/// Crate-wide result alias.
pub type Result<T> = std::result::Result<T, Error>;

/// Errors surfaced by the shared layer. Component crates wrap these in
/// `anyhow::Error` at their boundaries.
#[derive(Debug, Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json (de)serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("noise/crypto error: {0}")]
    Noise(String),

    #[error("protocol violation: {0}")]
    Protocol(String),

    #[error("authentication failed: {0}")]
    Auth(String),

    #[error("peer is offline or unknown: {0}")]
    PeerUnavailable(String),

    #[error("invalid key material: {0}")]
    Key(String),

    #[error("tunnel closed")]
    Closed,
}

impl From<snow::Error> for Error {
    fn from(e: snow::Error) -> Self {
        Error::Noise(e.to_string())
    }
}
