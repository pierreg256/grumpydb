use uuid::Uuid;

/// Central error type for GrumpyDB.
#[derive(Debug, thiserror::Error)]
pub enum GrumpyError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("page {0} not found")]
    PageNotFound(u32),

    #[error("page {0} is full")]
    PageFull(u32),

    #[error("key {0} already exists")]
    DuplicateKey(Uuid),

    #[error("key {0} not found")]
    KeyNotFound(Uuid),

    #[error("checksum mismatch on page {page_id}: expected {expected:#010x}, got {actual:#010x}")]
    ChecksumMismatch {
        page_id: u32,
        expected: u32,
        actual: u32,
    },

    #[error("WAL corrupted at LSN {0}")]
    WalCorrupted(u64),

    #[error("buffer pool exhausted: all frames are pinned")]
    BufferPoolExhausted,

    #[error("document too large: {size} bytes (max: {max})")]
    DocumentTooLarge { size: usize, max: usize },

    #[error("codec error: {0}")]
    Codec(String),
}

/// Convenience Result type for GrumpyDB operations.
pub type Result<T> = std::result::Result<T, GrumpyError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_display() {
        let err = GrumpyError::PageNotFound(42);
        assert_eq!(err.to_string(), "page 42 not found");
    }

    #[test]
    fn test_error_from_io() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let err: GrumpyError = io_err.into();
        assert!(matches!(err, GrumpyError::Io(_)));
    }

    #[test]
    fn test_error_checksum_display() {
        let err = GrumpyError::ChecksumMismatch {
            page_id: 5,
            expected: 0xDEADBEEF,
            actual: 0xCAFEBABE,
        };
        let msg = err.to_string();
        assert!(msg.contains("page 5"));
        assert!(msg.contains("0xdeadbeef"));
    }
}
