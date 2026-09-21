use thiserror::Error;

/// Unified error type for all UDA operations.
#[derive(Error, Debug)]
pub enum UdaError {
    #[error("Feature not supported: {0}")]
    NotSupported(String),

    #[error("Detection failed: {0}")]
    DetectionFailed(String),

    #[error("Command failed: {0}")]
    CommandFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Internal error: {0}")]
    Internal(String),
}
