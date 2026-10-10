use thiserror::Error;

/// Unified error type for all UDA operations.
///
/// Marked `#[non_exhaustive]`: new variants may be added in minor releases, so
/// downstream matches need a wildcard arm.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum UdaError {
    #[error("Invalid argument: {0}")]
    InvalidArgument(String),

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
