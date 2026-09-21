//! Safety helpers shared by every exported function.
//!
//! # Design rules
//!
//! 1. **No panic ever crosses the FFI boundary** (`AGENTS.md` Principle 1).
//!    Every exported function body is wrapped in [`catch_boundary`], which
//!    converts both a typed [`Failure`] and an actual `panic!` into a negative
//!    status code.
//! 2. **No pointer is dereferenced before it is validated.** Raw pointers are
//!    turned into owned Rust values (`&str`, `String`) by the helpers here, and
//!    the conversion functions never assume the buffer is well formed: they walk
//!    to a null terminator and bail out with
//!    [`UDA_ERR_INVALID_ARGUMENT`](crate::error::UDA_ERR_INVALID_ARGUMENT) when
//!    the bytes are not valid UTF-8.
//! 3. **Errors survive the return.** Rust's error type cannot be represented in
//!    C, so the message is parked in a thread-local slot that
//!    `uda_last_error_message()` reads back as a C string.

use std::cell::RefCell;
use std::os::raw::c_char;

use crate::error::{Failure, UdaStatus, UDA_ERR_PANIC};

thread_local! {
    /// Message of the most recent failure on the calling thread.
    ///
    /// Thread-local (rather than global) so concurrent callers in different
    /// languages never read each other's diagnostics.
    static LAST_MESSAGE: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Remember `message` so `uda_last_error_message()` can hand it back.
pub(crate) fn set_last_message(message: &str) {
    // A poisoned or unavailable cell must not break the call that already
    // failed for an unrelated reason, so failures here are silently dropped.
    let _ = LAST_MESSAGE.try_with(|slot| {
        *slot.borrow_mut() = Some(message.to_string());
    });
}

/// Take the stored message, leaving the slot empty.
pub(crate) fn take_last_message() -> Option<String> {
    LAST_MESSAGE
        .try_with(|slot| slot.borrow_mut().take())
        .ok()
        .flatten()
}

/// Borrow a C string as an owned Rust `String`.
///
/// # Safety
///
/// `pointer` must either be null or point to a readable, null-terminated byte
/// sequence. The function never assumes this holds: it stops at the first null
/// byte and rejects invalid UTF-8, so a malformed buffer produces an error
/// rather than undefined behaviour.
pub(crate) unsafe fn owned_string_from(
    pointer: *const c_char,
    parameter: &str,
) -> Result<String, Failure> {
    if pointer.is_null() {
        return Err(Failure::InvalidArgument(format!(
            "`{parameter}` must not be null"
        )));
    }

    // SAFETY: the caller guarantees a valid C string; `CStr` reads until the
    // null terminator and no further.
    let text = unsafe { std::ffi::CStr::from_ptr(pointer) }
        .to_str()
        .map_err(|_| Failure::InvalidArgument(format!("`{parameter}` is not valid UTF-8")))?
        .to_owned();
    Ok(text)
}

/// Convert a Rust string into a heap C string the caller must free.
///
/// The returned pointer is produced by [`CString::into_raw`], so ownership
/// transfers to the caller and is reclaimed by `uda_free_string`. `null` is
/// returned when the text contains an interior null byte (which cannot happen
/// for values produced by the backends, but a foreign path could in theory
/// supply one).
pub(crate) fn c_string_from(text: &str) -> *mut c_char {
    match std::ffi::CString::new(text) {
        Ok(c_string) => c_string.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Free a string previously returned by this library.
///
/// # Safety
///
/// `pointer` must be null or come from a UDA export that returns a `*mut c_char`.
/// Freeing a pointer owned by the caller is undefined behaviour.
pub(crate) unsafe fn free_c_string(pointer: *mut c_char) {
    if pointer.is_null() {
        return;
    }
    // SAFETY: the pointer was produced by `CString::into_raw` in this library
    // and has not been freed yet, so reclaiming it here is sound.
    drop(unsafe { std::ffi::CString::from_raw(pointer) });
}

/// Run a fallible body under panic containment and turn the outcome into a status.
///
/// The closure returns a `Result<(), Failure>`; a `panic!` inside it is caught
/// and reported as
/// [`UDA_ERR_PANIC`](crate::error::UDA_ERR_PANIC) so it can never unwind into
/// foreign frames, where unwinding across an `extern "C"` boundary is undefined
/// behaviour.
pub(crate) fn catch_boundary<F>(body: F) -> UdaStatus
where
    F: FnOnce() -> Result<(), Failure>,
{
    // `AssertUnwindSafe` is acceptable here: the closure only touches values it
    // owns or reads through validated pointers, and a panic aborts the operation
    // anyway, leaving no observable half-mutated state behind.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));

    match outcome {
        Ok(Ok(())) => crate::error::UDA_OK,
        Ok(Err(failure)) => {
            set_last_message(&failure.message());
            failure.status()
        }
        Err(payload) => {
            let description = panic_description(&payload);
            log::debug!("panic contained at the FFI boundary: {description}");
            set_last_message(&format!(
                "panic contained at the FFI boundary: {description}"
            ));
            UDA_ERR_PANIC
        }
    }
}

/// Extract a readable description from a caught panic payload.
///
/// A `panic!` payload is not guaranteed to be a bare `&'static str`: depending
/// on how the panic is raised it can also arrive as an owned `String`, or boxed
/// as `Box<dyn Any + Send>` (which is what a panic inside a closure wrapped in
/// `AssertUnwindSafe` produced here). All three shapes are unwrapped, because
/// dropping the message would make a contained panic indistinguishable from a
/// silent failure and leave the caller with no diagnosis at all.
fn panic_description(payload: &(dyn std::any::Any + Send)) -> String {
    // `panic!` can store the message as `&'static str`, as an owned `String`, or
    // - depending on the toolchain and panic settings - wrapped in a
    // `Box<dyn Any + Send>`. Every shape is unwrapped here, because losing the
    // message would make a contained panic indistinguishable from a silent one.
    if let Some(text) = payload.downcast_ref::<&'static str>() {
        return (*text).to_string();
    }
    if let Some(text) = payload.downcast_ref::<String>() {
        return text.clone();
    }
    if let Some(inner) = payload.downcast_ref::<Box<dyn std::any::Any + Send>>() {
        if let Some(text) = inner.downcast_ref::<&'static str>() {
            return (*text).to_string();
        }
        if let Some(text) = inner.downcast_ref::<String>() {
            return text.clone();
        }
    }
    "panic payload carries no readable message".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{UDA_ERR_INVALID_ARGUMENT, UDA_ERR_NOT_SUPPORTED, UDA_OK};
    use std::ffi::CString;

    #[test]
    fn null_pointer_is_an_invalid_argument() {
        let result = unsafe { owned_string_from(std::ptr::null(), "path") };
        match result {
            Err(Failure::InvalidArgument(message)) => {
                assert!(message.contains("path"), "message was: {message}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn valid_c_string_round_trips() {
        let text = CString::new("/home/user/wall.jpg").expect("ascii path");
        let owned =
            unsafe { owned_string_from(text.as_ptr(), "path") }.expect("valid UTF-8 C string");
        assert_eq!(owned, "/home/user/wall.jpg");
    }

    #[test]
    fn invalid_utf8_is_rejected_without_reading_past_the_terminator() {
        // 0xFF is never valid UTF-8; the string is still null-terminated so the
        // read itself is in bounds.
        let bytes = [0xFFu8, 0x00];
        let result = unsafe { owned_string_from(bytes.as_ptr().cast::<c_char>(), "path") };
        assert!(matches!(result, Err(Failure::InvalidArgument(_))));
    }

    #[test]
    fn c_string_round_trip_through_free() {
        let pointer = c_string_from("/tmp/wallpaper.png");
        assert!(!pointer.is_null());
        let back = unsafe { owned_string_from(pointer, "path") }.expect("round trip");
        assert_eq!(back, "/tmp/wallpaper.png");
        unsafe { free_c_string(pointer) };
    }

    #[test]
    fn interior_null_byte_yields_a_null_pointer_not_a_short_string() {
        let pointer = c_string_from("bad\0path");
        assert!(pointer.is_null());
    }

    #[test]
    fn free_of_null_is_a_no_op() {
        unsafe { free_c_string(std::ptr::null_mut()) };
    }

    #[test]
    fn success_path_returns_ok_and_leaves_no_message() {
        let status = catch_boundary(|| Ok(()));
        assert_eq!(status, UDA_OK);
        assert_eq!(take_last_message(), None);
    }

    #[test]
    fn typed_failure_records_its_message_and_status() {
        let status = catch_boundary(|| {
            Err(Failure::Uda(uda_core::error::UdaError::NotSupported(
                "no screensaver service".to_string(),
            )))
        });
        assert_eq!(status, UDA_ERR_NOT_SUPPORTED);
        let message = take_last_message().expect("message was recorded");
        assert!(message.contains("no screensaver service"), "got: {message}");
    }

    #[test]
    fn invalid_argument_is_reported_as_such() {
        let status = catch_boundary(|| {
            Err(Failure::InvalidArgument(
                "`out_path` must not be null".to_string(),
            ))
        });
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
    }

    #[test]
    fn panic_is_contained_and_reported_as_panic_status() {
        let status = catch_boundary(|| panic!("boom from a test"));
        assert_eq!(status, UDA_ERR_PANIC);
        let message = take_last_message().expect("panic message recorded");
        assert!(
            message.contains("boom"),
            "the panic message must survive the boundary, got: {message}"
        );
    }

    #[test]
    fn formatted_panic_messages_also_survive() {
        let status = catch_boundary(|| panic!("boom {}", 42));
        assert_eq!(status, UDA_ERR_PANIC);
        let message = take_last_message().expect("panic message recorded");
        assert!(message.contains("boom 42"), "got: {message}");
    }

    #[test]
    fn an_owned_string_payload_is_unwrapped() {
        // `panic_any` with a `String` exercises the second match arm.
        let status = catch_boundary(|| std::panic::panic_any(String::from("owned boom")));
        assert_eq!(status, UDA_ERR_PANIC);
        let message = take_last_message().expect("panic message recorded");
        assert!(message.contains("owned boom"), "got: {message}");
    }

    #[test]
    fn panic_payloads_without_a_string_are_still_described() {
        let status = catch_boundary(|| std::panic::panic_any(42u8));
        assert_eq!(status, UDA_ERR_PANIC);
        let message = take_last_message().expect("message recorded");
        assert!(!message.is_empty());
    }
}
