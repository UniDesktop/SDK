//! C-ABI status codes and the mapping from Rust errors.
//!
//! Every exported function in this crate reports its outcome as an `i32`:
//!
//! | Code | Name | Meaning |
//! |------|------|---------|
//! | `0`  | [`UDA_OK`] | Success |
//! | `-1` | [`UDA_ERR_INVALID_ARGUMENT`] | Null pointer, malformed string or unknown enum code |
//! | `-2` | [`UDA_ERR_NOT_SUPPORTED`] | The platform cannot deliver the feature |
//! | `-3` | [`UDA_ERR_DETECTION_FAILED`] | Environment/OS detection failed |
//! | `-4` | [`UDA_ERR_IO`] | Filesystem or process-spawn failure |
//! | `-5` | [`UDA_ERR_INTERNAL`] | Unexpected internal failure |
//! | `-6` | [`UDA_ERR_PANIC`] | A panic was caught at the FFI boundary |
//!
//! Human-readable diagnostics for the failing call are available through
//! `uda_last_error_message()` until the next UDA call on the same thread.

use uda_core::error::UdaError;

/// Status code returned by every exported C function.
pub type UdaStatus = i32;

/// The call succeeded.
pub const UDA_OK: UdaStatus = 0;

/// A caller-supplied argument was null, not UTF-8, or out of range.
pub const UDA_ERR_INVALID_ARGUMENT: UdaStatus = -1;

/// The current platform or session cannot provide the requested feature.
pub const UDA_ERR_NOT_SUPPORTED: UdaStatus = -2;

/// Detecting the environment or OS release failed.
pub const UDA_ERR_DETECTION_FAILED: UdaStatus = -3;

/// An I/O or process-spawn error occurred.
pub const UDA_ERR_IO: UdaStatus = -4;

/// An unexpected internal failure occurred.
pub const UDA_ERR_INTERNAL: UdaStatus = -5;

/// A panic escaped the Rust implementation and was contained by the boundary.
pub const UDA_ERR_PANIC: UdaStatus = -6;

/// A failure that the C ABI can describe without borrowing [`UdaError`].
///
/// Keeping the invalid-argument case separate from [`UdaError`] matters: a
/// caller passing a null pointer is a contract violation on *their* side, and
/// collapsing that into a generic internal error would hide the real problem.
#[derive(Debug)]
pub(crate) enum Failure {
    /// The caller violated the C ABI contract.
    InvalidArgument(String),
    /// A typed [`UdaError`] surfaced from a platform backend.
    Uda(UdaError),
}

impl Failure {
    /// The status code to hand back to the caller.
    pub(crate) fn status(&self) -> UdaStatus {
        match self {
            Failure::InvalidArgument(_) => UDA_ERR_INVALID_ARGUMENT,
            Failure::Uda(error) => status_of_uda_error(error),
        }
    }

    /// A human-readable description for `uda_last_error_message()`.
    pub(crate) fn message(&self) -> String {
        match self {
            Failure::InvalidArgument(message) => message.clone(),
            Failure::Uda(error) => error.to_string(),
        }
    }
}

impl From<UdaError> for Failure {
    fn from(error: UdaError) -> Self {
        Failure::Uda(error)
    }
}

/// Map a typed [`UdaError`] onto its C status code.
fn status_of_uda_error(error: &UdaError) -> UdaStatus {
    match error {
        UdaError::NotSupported(_) => UDA_ERR_NOT_SUPPORTED,
        UdaError::DetectionFailed(_) => UDA_ERR_DETECTION_FAILED,
        UdaError::CommandFailed(_) => UDA_ERR_INTERNAL,
        UdaError::Io(_) => UDA_ERR_IO,
        UdaError::Internal(_) => UDA_ERR_INTERNAL,
    }
}

/// Describe a status code with a static, allocation-free string.
///
/// Used by bindings that want to render a failure without making a second
/// FFI round-trip. Unknown codes report themselves so future codes stay
/// diagnosable from older clients.
pub fn status_message(status: UdaStatus) -> &'static str {
    match status {
        UDA_OK => "success",
        UDA_ERR_INVALID_ARGUMENT => "invalid argument",
        UDA_ERR_NOT_SUPPORTED => "feature not supported on this platform",
        UDA_ERR_DETECTION_FAILED => "environment detection failed",
        UDA_ERR_IO => "I/O error",
        UDA_ERR_INTERNAL => "internal error",
        UDA_ERR_PANIC => "panic contained at the FFI boundary",
        _ => "unrecognized status code",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_are_distinct_and_negative_on_failure() {
        let failures = [
            UDA_ERR_INVALID_ARGUMENT,
            UDA_ERR_NOT_SUPPORTED,
            UDA_ERR_DETECTION_FAILED,
            UDA_ERR_IO,
            UDA_ERR_INTERNAL,
            UDA_ERR_PANIC,
        ];
        for (index, code) in failures.iter().enumerate() {
            assert!(*code < 0, "failure codes must be negative: {code}");
            for other in &failures[index + 1..] {
                assert_ne!(code, other, "failure codes must be unique");
            }
        }
    }

    #[test]
    fn invalid_argument_is_reported_before_the_typed_errors() {
        // Callers rely on `-1` being reserved for contract violations.
        assert_eq!(UDA_ERR_INVALID_ARGUMENT, -1);
    }

    #[test]
    fn uda_errors_map_to_the_documented_codes() {
        let cases = [
            (UdaError::NotSupported("x".into()), UDA_ERR_NOT_SUPPORTED),
            (
                UdaError::DetectionFailed("x".into()),
                UDA_ERR_DETECTION_FAILED,
            ),
            (UdaError::CommandFailed("x".into()), UDA_ERR_INTERNAL),
            (UdaError::Internal("x".into()), UDA_ERR_INTERNAL),
            (
                UdaError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "x")),
                UDA_ERR_IO,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(Failure::Uda(error).status(), expected);
        }
    }

    #[test]
    fn failure_messages_are_never_empty() {
        let failures = [
            Failure::InvalidArgument("`path` must not be null".to_string()),
            Failure::Uda(UdaError::NotSupported("no backend".to_string())),
        ];
        for failure in failures {
            assert!(!failure.message().is_empty());
        }
    }

    #[test]
    fn every_documented_code_has_a_message() {
        for code in [
            UDA_OK,
            UDA_ERR_INVALID_ARGUMENT,
            UDA_ERR_NOT_SUPPORTED,
            UDA_ERR_DETECTION_FAILED,
            UDA_ERR_IO,
            UDA_ERR_INTERNAL,
            UDA_ERR_PANIC,
        ] {
            assert!(!status_message(code).is_empty());
        }
        assert_eq!(status_message(1234), "unrecognized status code");
    }
}
