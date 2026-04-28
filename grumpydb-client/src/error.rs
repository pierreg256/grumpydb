//! Client error types.

/// Errors from the GrumpyDB client driver.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// TCP/TLS connection error.
    #[error("connection error: {0}")]
    Connection(#[from] std::io::Error),

    /// Authentication failed.
    #[error("authentication failed: {0}")]
    Auth(String),

    /// Protocol parsing error.
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Server returned an error.
    #[error("server error: {0}")]
    Server(String),
}
