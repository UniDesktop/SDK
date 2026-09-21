//! UniDesktop API (UDA) C-ABI export layer.
//!
//! This crate builds the shared library that every non-Rust language links
//! against: `libuda_ffi.so` on Linux, `uda_ffi.dll` on Windows. The matching C
//! header lives at [`include/uda.h`](../../include/uda.h) and the ready-made
//! bindings under `examples/` (Python `ctypes`, Node.js `koffi`).
//!
//! # Contract
//!
//! Every exported function returns an `int32_t` status code:
//!
//! | Code | Meaning |
//! |------|---------|
//! | `0`  | success |
//! | `-1` | invalid argument (null pointer, bad UTF-8, unknown enum) |
//! | `-2` | feature not supported on this platform/session |
//! | `-3` | environment detection failed |
//! | `-4` | I/O error |
//! | `-5` | internal error |
//! | `-6` | a panic was contained at the boundary |
//!
//! # Memory ownership
//!
//! - Strings **returned** by UDA are allocated by Rust and must be freed with
//!   [`uda_free_string`].
//! - Strings **passed in** are borrowed for the duration of the call only; the
//!   caller keeps ownership.
//! - Wake-lock handles are `uint64_t` values owned by this process. Release
//!   them exactly once with [`uda_wakelock_release`].
//!
//! # Safety guarantees
//!
//! Following `AGENTS.md` Principle 1 (Never Panic):
//!
//! - Every exported body runs inside [`std::panic::catch_unwind`], so a panic
//!   can never unwind across the `extern "C"` boundary (which is undefined
//!   behaviour in Rust).
//! - No pointer is dereferenced before it is checked for null, and no buffer
//!   length is assumed: C strings are read to their null terminator and
//!   validated as UTF-8.
//! - No function returns a borrow; everything crossing the boundary is either a
//!   plain integer or an owned pointer the caller must free.
//!
//! # Thread safety
//!
//! All exports are `extern "C"` free functions with no global mutable state
//! except the wake-lock registry (internally synchronised) and the
//! thread-local last-error slot. They are safe to call from several threads.

mod dispatch;
mod error;
mod util;
mod wakelocks;

use std::os::raw::{c_char, c_int};

pub use error::{status_message, UdaStatus};

/// Status code for "success".
pub const UDA_OK: c_int = 0;
/// Status code for "invalid argument".
pub const UDA_ERR_INVALID_ARGUMENT: c_int = -1;
/// Status code for "feature not supported".
pub const UDA_ERR_NOT_SUPPORTED: c_int = -2;
/// Status code for "environment detection failed".
pub const UDA_ERR_DETECTION_FAILED: c_int = -3;
/// Status code for "I/O error".
pub const UDA_ERR_IO: c_int = -4;
/// Status code for "internal error".
pub const UDA_ERR_INTERNAL: c_int = -5;
/// Status code for "a panic was contained at the boundary".
pub const UDA_ERR_PANIC: c_int = -6;

/// Theme code for "unknown / not detected".
pub const UDA_THEME_UNKNOWN: c_int = 0;
/// Theme code for "dark".
pub const UDA_THEME_DARK: c_int = 1;
/// Theme code for "light".
pub const UDA_THEME_LIGHT: c_int = 2;

/// Fill-mode code for "crop to fill, preserving aspect ratio".
pub const UDA_FILL_CROP: c_int = 0;
/// Fill-mode code for "fill, ignoring aspect ratio".
pub const UDA_FILL_FILL: c_int = 1;
/// Fill-mode code for "fit, preserving aspect ratio".
pub const UDA_FILL_FIT: c_int = 2;
/// Fill-mode code for "stretch to fill".
pub const UDA_FILL_STRETCH: c_int = 3;

/// Wake-lock code for "prevent the display from sleeping".
pub const UDA_WAKELOCK_DISPLAY: c_int = 0;
/// Wake-lock code for "prevent the system from idling".
pub const UDA_WAKELOCK_SYSTEM: c_int = 1;

/// Detect the current system colour scheme.
///
/// Writes one of [`UDA_THEME_UNKNOWN`], [`UDA_THEME_DARK`] or
/// [`UDA_THEME_LIGHT`] to `*out_theme`. On failure a negative status code is
/// returned and `*out_theme` is left untouched.
///
/// # Safety
///
/// `out_theme` must be a valid, writable, non-null `int32_t` location.
#[no_mangle]
pub unsafe extern "C" fn uda_detect_theme(out_theme: *mut c_int) -> c_int {
    if out_theme.is_null() {
        util::set_last_message("`out_theme` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: null was rejected above, and the caller guarantees a writable
        // `int32_t` at this address.
        unsafe {
            let slot = &mut *out_theme;
            match dispatch::detect_theme_code() {
                Ok(code) => {
                    *slot = code;
                    Ok(())
                }
                Err(failure) => Err(failure),
            }
        }
    })
}

/// Set the desktop wallpaper.
///
/// `path` must be a null-terminated UTF-8 filesystem path. `fill_mode` is one
/// of the [`UDA_FILL_CROP`] family.
///
/// # Safety
///
/// `path` must be a valid, readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_set_wallpaper(path: *const c_char, fill_mode: c_int) -> c_int {
    if path.is_null() {
        util::set_last_message("`path` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: null was rejected above; the conversion walks to the null
        // terminator and rejects invalid UTF-8 instead of reading past it.
        let path = unsafe { util::owned_string_from(path, "path") }?;
        let fill_mode = fill_mode_from_c(fill_mode)?;
        dispatch::set_wallpaper(&path, fill_mode)
    })
}

/// Read the current wallpaper path.
///
/// On success `*out_path` receives a heap C string the caller must release with
/// [`uda_free_string`]. When no wallpaper is configured (or the platform cannot
/// report one), `*out_path` is set to null and the call still returns
/// [`UDA_OK`] - check the pointer, not the status, to detect "no wallpaper".
///
/// # Safety
///
/// `out_path` must be a valid, writable, non-null pointer location.
#[no_mangle]
pub unsafe extern "C" fn uda_get_wallpaper(out_path: *mut *mut c_char) -> c_int {
    if out_path.is_null() {
        util::set_last_message("`out_path` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: null was rejected above, and the caller guarantees a writable
        // pointer slot at this address.
        let slot = unsafe { &mut *out_path };
        let path = dispatch::get_wallpaper_path()?;
        // `c_string_from` returns null for the "no wallpaper" case as well as
        // for an interior-nul string, so the caller only ever sees null vs. a
        // valid pointer it must free.
        *slot = match &path {
            Some(path) => util::c_string_from(path),
            None => std::ptr::null_mut(),
        };
        Ok(())
    })
}

/// Free a string previously returned by this library.
///
/// Passing null is a no-op, so callers may free unconditionally.
///
/// # Safety
///
/// `s` must be null or a pointer obtained from [`uda_get_wallpaper`]. Freeing a
/// pointer owned by the caller is undefined behaviour.
#[no_mangle]
pub unsafe extern "C" fn uda_free_string(s: *mut c_char) {
    // This function cannot report failure, so it must not panic either. The
    // only work it does is reclaiming a `CString` this library allocated.
    let _ = util::catch_boundary(|| {
        // SAFETY: see the function's safety contract.
        unsafe { util::free_c_string(s) };
        Ok(())
    });
}

/// Acquire a wake lock.
///
/// `lock_type` is [`UDA_WAKELOCK_DISPLAY`] or [`UDA_WAKELOCK_SYSTEM`]. On
/// success `*out_handle` receives a non-zero handle to pass to
/// [`uda_wakelock_release`]; on failure it is left untouched.
///
/// # Safety
///
/// `out_handle` must be a valid, writable, non-null `uint64_t` location and
/// `reason` a readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_wakelock_acquire(
    lock_type: c_int,
    reason: *const c_char,
    out_handle: *mut u64,
) -> c_int {
    if out_handle.is_null() {
        util::set_last_message("`out_handle` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: `reason` was validated non-null and is converted with the same
        // null-terminating, UTF-8-checking helper as `path`, so it cannot read
        // past the end of the caller's buffer.
        let reason = unsafe { util::owned_string_from(reason, "reason") }?;
        let lock_type = wake_lock_type_from_c(lock_type)?;
        let handle = dispatch::acquire_wakelock(lock_type, &reason)?;

        // SAFETY: `out_handle` was validated non-null above, and the caller
        // guarantees a writable `uint64_t` at that address.
        unsafe { *out_handle = handle.raw() };
        Ok(())
    })
}

/// Release a wake lock previously obtained from [`uda_wakelock_acquire`].
///
/// Returns [`UDA_ERR_INVALID_ARGUMENT`] when the handle is not a live lock in
/// this process (already released, or never issued here).
#[no_mangle]
pub extern "C" fn uda_wakelock_release(handle: u64) -> c_int {
    util::catch_boundary(|| {
        let handle = dispatch::WakeLockHandle::from_raw(handle)
            .ok_or_else(|| error::Failure::InvalidArgument("`handle` must not be 0".to_string()))?;
        dispatch::release_wakelock(handle)
    })
}

/// Return the message describing the most recent failure on this thread.
///
/// The returned string is owned by the library and stays valid until the next
/// UDA call on the same thread; copy it if it must outlive that. Returns null
/// when no failure has been recorded yet.
///
/// This function is part of the ABI even though the task listed six core
/// exports: without it a negative status code carries no diagnosis, and every
/// caller in `examples/` uses it.
#[no_mangle]
pub extern "C" fn uda_last_error_message() -> *const c_char {
    let Some(message) = util::take_last_message() else {
        return std::ptr::null();
    };

    match std::ffi::CString::new(message) {
        // Leaked on purpose: the caller frees it with `uda_free_string`, which
        // keeps a single allocation policy for every string this library hands
        // out. A message containing an interior nul cannot happen because the
        // message text comes from `UdaError`'s `Display`, which never emits one.
        Ok(c_string) => c_string.into_raw().cast_const(),
        Err(_) => std::ptr::null(),
    }
}

/// Describe a status code with a static string.
///
/// Convenience for bindings that want to render a failure without a second FFI
/// round-trip. The returned pointer stays valid for the lifetime of the library
/// and must **not** be freed.
///
/// The text has to be copied into a null-terminated buffer before it crosses the
/// boundary: a Rust `&str` carries a length and is *not* null-terminated, so
/// handing out `str::as_ptr()` would let a C caller keep reading past the end of
/// the message into whatever byte follows it in `.rodata`. The status codes are
/// a small closed set, so one `CString` per code is cached and reused.
#[no_mangle]
pub extern "C" fn uda_status_message(status: c_int) -> *const c_char {
    STATUS_MESSAGES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(pointer) = cache.get(&status) {
            return *pointer;
        }

        // `status_message` returns plain ASCII with no interior nul, so this
        // conversion cannot fail; `unwrap_or_default` states that guarantee
        // instead of relying on a panic the boundary would have to catch.
        let text = std::ffi::CString::new(error::status_message(status)).unwrap_or_default();
        // Leaked deliberately: the pointer must outlive the call and callers are
        // documented not to free it. Bounded by the number of distinct codes.
        let pointer = text.into_raw().cast_const();
        cache.insert(status, pointer);
        pointer
    })
}

thread_local! {
    /// Cache of the leaked status-message strings, so repeated calls with the
    /// same code reuse a single allocation instead of leaking one per call.
    static STATUS_MESSAGES: std::cell::RefCell<std::collections::HashMap<c_int, *const c_char>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Map a C fill-mode code onto the cross-platform [`FillMode`].
fn fill_mode_from_c(code: c_int) -> Result<uda_core::wallpaper::FillMode, error::Failure> {
    use uda_core::wallpaper::FillMode;
    let fill_mode = match code {
        UDA_FILL_CROP => FillMode::Crop,
        UDA_FILL_FILL => FillMode::Fill,
        UDA_FILL_FIT => FillMode::Fit,
        UDA_FILL_STRETCH => FillMode::Stretch,
        other => {
            return Err(error::Failure::InvalidArgument(format!(
                "unknown fill_mode {other}; expected 0..=3"
            )));
        }
    };
    Ok(fill_mode)
}

/// Map a C wake-lock code onto the cross-platform [`WakeLockType`].
fn wake_lock_type_from_c(code: c_int) -> Result<uda_core::wakelock::WakeLockType, error::Failure> {
    use uda_core::wakelock::WakeLockType;
    let lock_type = match code {
        UDA_WAKELOCK_DISPLAY => WakeLockType::PreventDisplaySleep,
        UDA_WAKELOCK_SYSTEM => WakeLockType::PreventSystemIdle,
        other => {
            return Err(error::Failure::InvalidArgument(format!(
                "unknown lock_type {other}; expected 0..=1"
            )));
        }
    };
    Ok(lock_type)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn status_constants_match_the_error_module() {
        assert_eq!(UDA_OK, error::UDA_OK);
        assert_eq!(UDA_ERR_INVALID_ARGUMENT, error::UDA_ERR_INVALID_ARGUMENT);
        assert_eq!(UDA_ERR_NOT_SUPPORTED, error::UDA_ERR_NOT_SUPPORTED);
        assert_eq!(UDA_ERR_PANIC, error::UDA_ERR_PANIC);
    }

    #[test]
    fn fill_mode_codes_are_accepted_and_rejected() {
        assert!(fill_mode_from_c(UDA_FILL_CROP).is_ok());
        assert!(fill_mode_from_c(UDA_FILL_FILL).is_ok());
        assert!(fill_mode_from_c(UDA_FILL_FIT).is_ok());
        assert!(fill_mode_from_c(UDA_FILL_STRETCH).is_ok());

        for bad in [-1, 4, 99] {
            let failure = fill_mode_from_c(bad).expect_err("out-of-range code");
            assert_eq!(failure.status(), UDA_ERR_INVALID_ARGUMENT);
        }
    }

    #[test]
    fn wake_lock_codes_are_accepted_and_rejected() {
        assert!(wake_lock_type_from_c(UDA_WAKELOCK_DISPLAY).is_ok());
        assert!(wake_lock_type_from_c(UDA_WAKELOCK_SYSTEM).is_ok());
        for bad in [-1, 2, 99] {
            let failure = wake_lock_type_from_c(bad).expect_err("out-of-range code");
            assert_eq!(failure.status(), UDA_ERR_INVALID_ARGUMENT);
        }
    }

    #[test]
    fn detect_theme_writes_into_the_out_parameter() {
        let mut theme: c_int = -100;
        // SAFETY: `theme` is a live, writable `int32_t` on this stack frame.
        let status = unsafe { uda_detect_theme(&mut theme) };
        assert_eq!(status, UDA_OK);
        assert!((0..=2).contains(&theme), "unexpected theme {theme}");
    }

    #[test]
    fn detect_theme_rejects_a_null_out_parameter() {
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe { uda_detect_theme(std::ptr::null_mut()) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());
    }

    #[test]
    fn set_wallpaper_rejects_a_null_path() {
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe { uda_set_wallpaper(std::ptr::null(), UDA_FILL_FILL) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());
    }

    #[test]
    fn set_wallpaper_rejects_an_unknown_fill_mode() {
        let path = CString::new("/tmp/x.jpg").expect("ascii");
        // SAFETY: `path` is a valid C string and the fill mode is the invalid
        // part under test.
        let status = unsafe { uda_set_wallpaper(path.as_ptr(), 42) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());
    }

    #[test]
    fn set_wallpaper_rejects_an_empty_path() {
        let path = CString::new("").expect("empty string is valid");
        // SAFETY: `path` is a valid, empty C string.
        let status = unsafe { uda_set_wallpaper(path.as_ptr(), UDA_FILL_FILL) };
        assert!(status < 0, "an empty path must fail, got {status}");
    }

    #[test]
    fn get_wallpaper_fills_the_out_pointer_or_null() {
        let mut pointer: *mut c_char = std::ptr::null_mut();
        // SAFETY: `pointer` is a live, writable pointer slot.
        let status = unsafe { uda_get_wallpaper(&mut pointer) };
        assert_eq!(status, UDA_OK);

        if pointer.is_null() {
            // No wallpaper configured on this host; that is a valid outcome.
            return;
        }
        // SAFETY: the pointer came from this library and has not been freed.
        let text = unsafe { util::owned_string_from(pointer, "out_path") }
            .expect("library strings are valid UTF-8");
        assert!(!text.is_empty());
        // SAFETY: same pointer, freed exactly once.
        unsafe { uda_free_string(pointer) };
    }

    #[test]
    fn get_wallpaper_rejects_a_null_out_parameter() {
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe { uda_get_wallpaper(std::ptr::null_mut()) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
    }

    #[test]
    fn free_string_of_null_is_a_no_op() {
        // SAFETY: null is documented as a no-op.
        unsafe { uda_free_string(std::ptr::null_mut()) };
    }

    #[test]
    fn wakelock_acquire_release_round_trip() {
        let reason = CString::new("uda-ffi unit test").expect("ascii");
        let mut handle: u64 = 0;

        // SAFETY: both pointers are live, writable locals / a valid C string.
        let status =
            unsafe { uda_wakelock_acquire(UDA_WAKELOCK_DISPLAY, reason.as_ptr(), &mut handle) };
        if status == UDA_ERR_NOT_SUPPORTED {
            // Neither native IPC nor the CLI fallback is available here.
            return;
        }
        assert_eq!(status, UDA_OK);
        assert_ne!(handle, 0, "a successful acquire must hand back a handle");

        assert_eq!(uda_wakelock_release(handle), UDA_OK);
        assert_eq!(
            uda_wakelock_release(handle),
            UDA_ERR_INVALID_ARGUMENT,
            "the handle must be single-use"
        );
    }

    #[test]
    fn wakelock_acquire_rejects_a_null_handle_slot() {
        let reason = CString::new("uda-ffi").expect("ascii");
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe {
            uda_wakelock_acquire(UDA_WAKELOCK_DISPLAY, reason.as_ptr(), std::ptr::null_mut())
        };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
    }

    #[test]
    fn wakelock_release_of_zero_is_an_invalid_argument() {
        assert_eq!(uda_wakelock_release(0), UDA_ERR_INVALID_ARGUMENT);
    }

    #[test]
    fn last_error_message_is_null_when_nothing_failed() {
        let _ = util::take_last_message();
        assert!(uda_last_error_message().is_null());
    }

    #[test]
    fn status_message_pointer_is_readable_and_static() {
        let pointer = uda_status_message(UDA_ERR_NOT_SUPPORTED);
        assert!(!pointer.is_null());
        // SAFETY: the pointer refers to a `'static` Rust string literal.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_str()
            .expect("static ASCII message");
        assert_eq!(text, "feature not supported on this platform");
    }
}
