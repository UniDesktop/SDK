//! UniDesktop API (UDA) C-ABI export layer.
//!
//! See [`tray`] for the system-tray surface, which is the newest addition.
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
mod media;
mod notify;
mod session;
mod tray;
mod util;
mod wakelocks;

use std::os::raw::{c_char, c_int, c_void};

use uda_core::session::SessionAction;

pub use error::{status_message, UdaStatus};

// Re-exported from the media module: these numbers are part of the C ABI (they
// appear verbatim in include/uda.h), so a binding should read them from the
// library rather than hard-coding its own copies.
pub use media::{
    UDA_MEDIA_CMD_NEXT, UDA_MEDIA_CMD_PAUSE, UDA_MEDIA_CMD_PLAY, UDA_MEDIA_CMD_PREVIOUS,
    UDA_MEDIA_CMD_STOP, UDA_MEDIA_CMD_TOGGLE, UDA_MEDIA_PAUSED, UDA_MEDIA_PLAYING,
    UDA_MEDIA_STOPPED, UDA_MEDIA_UNKNOWN,
};

// Re-exported from the session module: the capability bitmask is part of the C
// ABI as well, because a caller must be able to ask "which actions exist here?"
// *before* drawing a menu that could shut the machine down.
pub use session::{
    UDA_SESSION_CAP_HIBERNATE, UDA_SESSION_CAP_LOCK, UDA_SESSION_CAP_LOGOUT,
    UDA_SESSION_CAP_MANAGEMENT, UDA_SESSION_CAP_REBOOT, UDA_SESSION_CAP_SHUTDOWN,
    UDA_SESSION_CAP_SUSPEND,
};

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
/// `path` accepts both a plain filesystem path and a `file://` URI; an empty path
/// is rejected as an invalid argument rather than being sent to the backend.
///
/// On failure a negative status code is returned and no wallpaper is changed.
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

/// Read the metadata of the active media player.
///
/// Writes three owned strings - `*out_title`, `*out_artist`, `*out_album` - and
/// the track length to `*out_duration_ms`. A field the player does not publish is
/// written as a null pointer (title, artist, album) or zero (duration), so a
/// caller must check each pointer before reading it rather than assuming the
/// struct is fully populated.
///
/// The returned strings are allocated by Rust and must be released with
/// [`uda_free_string`]. Freeing a null pointer is a no-op.
///
/// When no player is running (the normal case on a desktop with no media app),
/// all four out-parameters are set to null/zero and [`UDA_OK`] is returned: an
/// empty now-playing card, not a failure.
///
/// `out_position_ms` is optional: pass null to skip it. When supplied it receives
/// the playback position in milliseconds, or zero when the backend cannot report
/// it (MPRIS on a player that has never been queried, SMTC on a session with no
/// timeline).
///
/// # Safety
///
/// `out_title`, `out_artist` and `out_album` must each point at a writable
/// `char *` location; `out_duration_ms` must point at a writable `uint64_t`;
/// `out_position_ms` must be null or point at a writable `uint64_t`.
#[no_mangle]
pub unsafe extern "C" fn uda_media_get_metadata(
    out_title: *mut *mut c_char,
    out_artist: *mut *mut c_char,
    out_album: *mut *mut c_char,
    out_duration_ms: *mut u64,
    out_position_ms: *mut u64,
) -> c_int {
    if out_title.is_null()
        || out_artist.is_null()
        || out_album.is_null()
        || out_duration_ms.is_null()
    {
        util::set_last_message(
            "`out_title`, `out_artist`, `out_album` and `out_duration_ms` must not be null",
        );
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let metadata = media::active_metadata()?.unwrap_or_default();

        // Each field becomes its own allocation, so a caller can free them one at
        // a time (or free null, which is a no-op). Writing null rather than an
        // empty string lets a binding use `x is None` to skip the field.
        let title = util::c_string_from(&metadata.title);
        let artist = util::c_string_from(&metadata.artist);
        let album = util::c_string_from(&metadata.album);

        // SAFETY: all four pointers were validated non-null and writable above;
        // `out_position_ms` is checked before the optional write.
        unsafe {
            *out_title = title;
            *out_artist = artist;
            *out_album = album;
            *out_duration_ms = metadata.duration_ms.unwrap_or(0);
            if !out_position_ms.is_null() {
                *out_position_ms = metadata.position_ms.unwrap_or(0);
            }
        }
        Ok(())
    })
}

/// Read the playback status of the active media player.
///
/// Writes one of [`UDA_MEDIA_PLAYING`], [`UDA_MEDIA_PAUSED`],
/// [`UDA_MEDIA_STOPPED`] or [`UDA_MEDIA_UNKNOWN`] to `*out_status`.
///
/// [`UDA_MEDIA_UNKNOWN`] covers both "no player is running" and "the state could
/// not be determined"; it is never an error, so [`UDA_OK`] is still returned. A
/// negative status means the platform has no media backend at all.
///
/// # Safety
///
/// `out_status` must point at a writable `int32_t` location.
#[no_mangle]
pub unsafe extern "C" fn uda_media_get_status(out_status: *mut c_int) -> c_int {
    if out_status.is_null() {
        util::set_last_message("`out_status` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let status = media::playback_status()?;

        // SAFETY: the pointer was validated non-null and writable.
        unsafe { *out_status = status.code() };
        Ok(())
    })
}

/// Send a transport command to the active media player.
///
/// `command` is one of [`UDA_MEDIA_CMD_PLAY`], [`UDA_MEDIA_CMD_PAUSE`],
/// [`UDA_MEDIA_CMD_TOGGLE`], [`UDA_MEDIA_CMD_NEXT`],
/// [`UDA_MEDIA_CMD_PREVIOUS`] or [`UDA_MEDIA_CMD_STOP`]. An unknown code returns
/// [`UDA_ERR_INVALID_ARGUMENT`] and nothing is sent, because forwarding a
/// malformed instruction to the user's player is worse than rejecting it.
///
/// A player that refuses the command (an app that disables `Next`) returns
/// [`UDA_ERR_NOT_SUPPORTED`], which is also the answer when no player is running.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_media_send_command(command: c_int) -> c_int {
    util::catch_boundary(|| {
        let command = media::command_from_c(command)?;
        media::send_command(command)
    })
}

/// Report which session and power actions this platform can perform.
///
/// Writes a bitmask made of the [`UDA_SESSION_CAP_*`] constants to
/// `*out_capabilities`. `0` means "no session backend exists on this target".
///
/// This is a *static, side-effect-free* query: it costs one D-Bus connection at
/// most and never touches the machine's power state, so a UI may call it freely
/// to decide which menu entries to draw.
///
/// A set bit means "the code path exists", not "the account is allowed". A
/// machine with hibernation switched off still reports
/// [`UDA_SESSION_CAP_HIBERNATE`]; the attempt then fails with
/// [`UDA_ERR_NOT_SUPPORTED`](crate::error::UDA_ERR_NOT_SUPPORTED), which is what
/// tells the caller the difference.
///
/// # Safety
///
/// `out_capabilities` must point at a writable `uint32_t` location.
#[no_mangle]
pub unsafe extern "C" fn uda_session_capabilities(out_capabilities: *mut u32) -> c_int {
    if out_capabilities.is_null() {
        util::set_last_message("`out_capabilities` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let capabilities = session::capabilities();

        // SAFETY: null was rejected above, and the caller guarantees a writable
        // `uint32_t` at this address.
        unsafe { *out_capabilities = capabilities.bits() };
        Ok(())
    })
}

/// Lock the session, leaving every running program alone.
///
/// Linux: `org.freedesktop.ScreenSaver.Lock()` on the session bus, falling back
/// to `loginctl lock-session`. Windows: `user32!LockWorkStation`.
///
/// This is the **only** action safe to automate: it is reversible (the user
/// unlocks with their password) and destroys nothing. The other five exports
/// must be gated behind an explicit user confirmation.
///
/// Returns [`UDA_ERR_NOT_SUPPORTED`](crate::error::UDA_ERR_NOT_SUPPORTED) when
/// the platform advertises no lock capability at all.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_lock() -> c_int {
    util::catch_boundary(|| session::perform(SessionAction::Lock))
}

/// End the calling user's session.
///
/// Linux: `org.freedesktop.login1.Manager.TerminateSession("")` on the system
/// bus, falling back to the desktop's own session manager. Windows:
/// `ExitWindowsEx(EWX_LOGOFF, 0)`.
///
/// **This action logs the user out.** Unsaved work is lost unless the desktop
/// refuses to comply. Never call it without an explicit confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_logout() -> c_int {
    util::catch_boundary(|| session::perform(SessionAction::Logout))
}

/// Suspend the machine to RAM.
///
/// Linux: `org.freedesktop.login1.Manager.Suspend(false)`. Windows:
/// `SetSuspendState(false, ...)`.
///
/// **This action changes the machine's power state.** Never call it without an
/// explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_suspend() -> c_int {
    util::catch_boundary(|| session::perform(SessionAction::Suspend))
}

/// Hibernate the machine to disk.
///
/// Linux: `org.freedesktop.login1.Manager.Hibernate(false)`. Windows:
/// `SetSuspendState(true, ...)`, which the platform rejects with
/// `ERROR_FILE_NOT_FOUND` when hibernation is disabled - reported as
/// [`UDA_ERR_NOT_SUPPORTED`](crate::error::UDA_ERR_NOT_SUPPORTED).
///
/// **This action changes the machine's power state.** Never call it without an
/// explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_hibernate() -> c_int {
    util::catch_boundary(|| session::perform(SessionAction::Hibernate))
}

/// Restart the machine.
///
/// Linux: `org.freedesktop.login1.Manager.Reboot(false)`. Windows:
/// `ExitWindowsEx(EWX_REBOOT | EWX_FORCEIFHUNG, 0)` after enabling
/// `SeShutdownPrivilege`, which needs an elevated process or a local
/// administrator account; without it the call fails with
/// [`UDA_ERR_NOT_SUPPORTED`](crate::error::UDA_ERR_NOT_SUPPORTED) rather than
/// half-rebooting.
///
/// **This action restarts the machine.** Unsaved work is lost. Never call it
/// without an explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_reboot() -> c_int {
    util::catch_boundary(|| session::perform(SessionAction::Reboot))
}

/// Power the machine off.
///
/// Linux: `org.freedesktop.login1.Manager.PowerOff(false)`. Windows:
/// `ExitWindowsEx(EWX_POWEROFF | EWX_FORCEIFHUNG, 0)` after enabling
/// `SeShutdownPrivilege`, with the same elevation requirement as
/// [`uda_session_reboot`].
///
/// **This action shuts the machine down.** Unsaved work is lost. Never call it
/// without an explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_shutdown() -> c_int {
    util::catch_boundary(|| session::perform(SessionAction::Shutdown))
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

/// Send a system notification.
///
/// The five strings cover what a notification needs: the sending app's
/// `app_name`, a one-line `title`, a multi-line `body`, an optional `icon`
/// (path or URI; empty means none), and `actions` as a flat newline-separated
/// list of `key\nlabel` records. Any string may be null, which is treated as the
/// empty string.
///
/// `app_name` is not cosmetic: on Windows it is the AppUserModelID the toast is
/// addressed to, and an unpackaged process has none. UDA registers it as the
/// process's explicit AUMID before the first toast is shown, which is what lets
/// a plain `node script.js` display a native toast. Leaving it empty (or null)
/// selects the generic `UniDesktop.Notification` identity.
///
/// On success `*out_id` receives the id the notification server assigned; it is
/// left untouched on failure.
///
/// # Safety
///
/// `out_id` must be a valid, writable, non-null `uint32_t` location. The five
/// strings must each be null or readable, null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn uda_notify(
    app_name: *const c_char,
    title: *const c_char,
    body: *const c_char,
    icon: *const c_char,
    actions: *const c_char,
    out_id: *mut u32,
) -> c_int {
    if out_id.is_null() {
        util::set_last_message("`out_id` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // Every string goes through the same null-terminating, UTF-8-checking
        // conversion, so none of them can read past the caller's buffers; null
        // is accepted and becomes the empty string.
        let app_name = owned_or_empty(app_name, "app_name")?;
        let title = owned_or_empty(title, "title")?;
        let body = owned_or_empty(body, "body")?;
        let icon = owned_or_empty(icon, "icon")?;
        let actions = owned_or_empty(actions, "actions")?;

        let mut id = 0u32;
        notify::notify(&app_name, &title, &body, &icon, &actions, &mut id)?;
        // SAFETY: the slot was validated non-null and is writable.
        unsafe { *out_id = id };
        Ok(())
    })
}

/// Read the system accent colour.
///
/// Writes the four 0..=255 channels to `*out_rgba` as R, G, B, A. A platform
/// that exposes no accent colour (most Linux desktops) leaves the slot
/// untouched and still returns [`UDA_OK`], so check the returned status only
/// for hard failures and treat a zeroed slot as "no accent".
///
/// # Safety
///
/// `out_rgba` must point at four writable `uint8_t` values.
#[no_mangle]
pub unsafe extern "C" fn uda_get_accent_color(out_rgba: *mut u8) -> c_int {
    if out_rgba.is_null() {
        util::set_last_message("`out_rgba` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let Some(color) = dispatch::accent_color() else {
            log::debug!("no accent colour reported by the platform");
            return Ok(());
        };

        // SAFETY: the pointer was validated non-null and the caller guarantees
        // four writable bytes. The four writes happen before any other UDA call
        // could observe a partially written slot.
        unsafe {
            *out_rgba = color.r;
            *out_rgba.add(1) = color.g;
            *out_rgba.add(2) = color.b;
            *out_rgba.add(3) = color.a;
        }
        Ok(())
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

// ---------------------------------------------------------------------------
// System tray
// ---------------------------------------------------------------------------

// Ownership of the tray surface.
//
// The C caller cannot hold an `Arc`, so every tray icon and every context menu
// lives in a process-wide table keyed by an opaque `uint64_t`. Two independent
// handle spaces are *not* used - one table with a per-entry kind, so a mistake
// is diagnosed ("handle 3 is a menu, not a tray icon") instead of accidentally
// resolving to the wrong record.
//
// The invariants the exports below uphold:
//
// - Handles start at `1`; `0` means "no handle" and every entry point rejects
//   it without touching a pointer.
// - A handle is single-use: destroying it removes the entry, and a second
//   destroy of the same value is `UDA_ERR_INVALID_ARGUMENT` rather than a
//   silent no-op, so a host cannot "double-release" a shell resource.
// - A menu may be attached to an icon and then destroyed; the icon keeps its
//   own `Arc`, so the tray does not lose its rows.

/// Create a tray icon.
///
/// `name` is the application name used for registration (the D-Bus bus name on
/// Linux, the window class on Windows). `tooltip` may be an empty string; text
/// longer than 127 `char`s is clamped, and the call still succeeds.
///
/// On success `*out_handle` receives a non-zero handle for every other
/// `uda_tray_*` call. On failure it is left untouched.
///
/// # Safety
///
/// `out_handle` must be a valid, writable, non-null `uint64_t` slot.
/// `name` and `tooltip` must be readable, null-terminated UTF-8 strings; a null
/// `tooltip` is treated as the empty string, a null `name` as the library
/// default name.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_create(
    name: *const c_char,
    tooltip: *const c_char,
    out_handle: *mut u64,
) -> c_int {
    if out_handle.is_null() {
        util::set_last_message("`out_handle` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: `out_handle` was validated non-null above and the caller
        // guarantees a writable `uint64_t` there. The two strings are converted
        // with the null-terminating, UTF-8-checking helper, so the conversion
        // cannot read past the caller's buffers.
        let (name, tooltip) = unsafe {
            // `owned_string_from` rejects null, which is the behaviour wanted
            // for `name`; `tooltip` is explicitly optional, so it is defaulted
            // here instead of being rejected.
            let tooltip = if tooltip.is_null() {
                String::new()
            } else {
                util::owned_string_from(tooltip, "tooltip")?
            };
            let name = if name.is_null() {
                String::new()
            } else {
                util::owned_string_from(name, "name")?
            };
            (name, tooltip)
        };

        let handle = tray::create_icon(&name, &tooltip)?;
        // SAFETY: the slot was validated non-null and is writable.
        unsafe { *out_handle = handle };
        Ok(())
    })
}

/// Replace a tray icon's tooltip.
///
/// # Safety
///
/// `tooltip` must be a readable, null-terminated UTF-8 string, or null for the
/// empty string.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_set_tooltip(handle: u64, tooltip: *const c_char) -> c_int {
    util::catch_boundary(|| {
        let tooltip = owned_or_empty(tooltip, "tooltip")?;
        tray::set_tooltip(handle, &tooltip)
    })
}

/// Replace a tray icon's image from a filesystem path or icon-theme name.
///
/// On Linux the value is also accepted as a freedesktop icon-theme name, which
/// is what a themed application wants; on Windows it must be a file path
/// (`.ico`, `.png`, `.bmp`).
///
/// # Safety
///
/// `path` must be a readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_set_icon_path(handle: u64, path: *const c_char) -> c_int {
    if path.is_null() {
        util::set_last_message("`path` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: null was rejected above; the conversion walks to the
        // terminator and rejects invalid UTF-8 instead of over-reading.
        let path = unsafe { util::owned_string_from(path, "path") }?;
        tray::set_icon_path(handle, &path)
    })
}

/// Replace a tray icon's image from raw RGBA pixels.
///
/// The buffer is **borrowed**: only `stride * height` bytes are copied, and the
/// caller keeps ownership of `data`. Pixels are top-down, four bytes per pixel
/// (red, green, blue, alpha).
///
/// # Safety
///
/// `data` must be null or point to `len` readable bytes. Anything short of
/// `stride * height` is rejected with `UDA_ERR_INVALID_ARGUMENT` before a pixel
/// is read.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_set_icon_rgba(
    handle: u64,
    width: u32,
    height: u32,
    stride: u32,
    data: *const u8,
    len: usize,
) -> c_int {
    if data.is_null() {
        util::set_last_message("`data` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| tray::set_icon_rgba(handle, width, height, stride, data, len))
}

/// Show or hide the icon without unregistering it.
///
/// `visible` is a C boolean: `0` hides, any other value shows.
#[no_mangle]
pub extern "C" fn uda_tray_set_visible(handle: u64, visible: c_int) -> c_int {
    util::catch_boundary(|| tray::set_visible(handle, visible != 0))
}

/// Destroy a tray icon and unregister it from the shell.
///
/// Returns `UDA_ERR_INVALID_ARGUMENT` when the handle is not a live icon in this
/// process. Destroying is terminal: the handle cannot be reused.
#[no_mangle]
pub extern "C" fn uda_tray_destroy(handle: u64) -> c_int {
    util::catch_boundary(|| tray::destroy_icon(handle))
}

/// Create an empty context menu.
///
/// On success `*out_menu_handle` receives a non-zero handle for
/// `uda_tray_menu_add_*` and `uda_tray_set_menu`.
///
/// # Safety
///
/// `out_menu_handle` must be a valid, writable, non-null `uint64_t` slot.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_menu_create(out_menu_handle: *mut u64) -> c_int {
    if out_menu_handle.is_null() {
        util::set_last_message("`out_menu_handle` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let handle = tray::create_menu()?;
        // SAFETY: validated non-null above, and the caller guarantees a
        // writable `uint64_t` at that address.
        unsafe { *out_menu_handle = handle };
        Ok(())
    })
}

/// Append a plain text row to a menu.
///
/// `callback` is invoked on the **tray worker thread** when the row is
/// activated. Passing `None` (null) is legal: the row toggles silently, which is
/// what a host using a polling model wants. `user_data` is handed back to the
/// callback untouched and is never dereferenced by UDA.
///
/// On success `*out_item_id` receives the row's stable, non-zero id, which the
/// callback also receives so a host does not need its own table.
///
/// # Safety
///
/// `out_item_id` must be a valid, writable, non-null `uint64_t` slot. `label`
/// must be a readable, null-terminated UTF-8 string; a blank label is rejected
/// with `UDA_ERR_NOT_SUPPORTED` because it would render an invisible row.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_menu_add_text(
    menu_handle: u64,
    label: *const c_char,
    callback: Option<tray::TextCallback>,
    user_data: *mut c_void,
    out_item_id: *mut u64,
) -> c_int {
    if out_item_id.is_null() {
        util::set_last_message("`out_item_id` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }
    if label.is_null() {
        util::set_last_message("`label` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: `label` was validated non-null above, and the helper stops at
        // the null terminator and rejects invalid UTF-8 instead of over-reading.
        let label = unsafe { util::owned_string_from(label, "label") }?;
        let item_id = tray::menu_add_text(menu_handle, &label, callback, user_data)?;
        // SAFETY: `out_item_id` was validated non-null and is writable.
        unsafe { *out_item_id = item_id };
        Ok(())
    })
}

/// Append a visual separator to a menu.
///
/// A separator has no label, no callback and no id, so nothing is returned.
#[no_mangle]
pub extern "C" fn uda_tray_menu_add_separator(menu_handle: u64) -> c_int {
    util::catch_boundary(|| tray::menu_add_separator(menu_handle))
}

/// Append a checkbox row to a menu.
///
/// The row's own stored value is inverted *before* `callback` runs, so the
/// `checked` argument is the new state the shell will render - which means the
/// host and the menu cannot disagree about what the checkbox shows.
///
/// `callback` may be null, in which case the row still toggles silently.
///
/// # Safety
///
/// `out_item_id` must be a valid, writable, non-null `uint64_t` slot. `label`
/// must be a readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_menu_add_checkbox(
    menu_handle: u64,
    label: *const c_char,
    checked: c_int,
    callback: Option<tray::CheckboxCallback>,
    user_data: *mut c_void,
    out_item_id: *mut u64,
) -> c_int {
    if out_item_id.is_null() {
        util::set_last_message("`out_item_id` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }
    if label.is_null() {
        util::set_last_message("`label` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: `label` was validated non-null above.
        let label = unsafe { util::owned_string_from(label, "label") }?;
        let item_id =
            tray::menu_add_checkbox(menu_handle, &label, checked != 0, callback, user_data)?;
        // SAFETY: `out_item_id` was validated non-null and is writable.
        unsafe { *out_item_id = item_id };
        Ok(())
    })
}

/// Attach a menu to a tray icon, replacing any menu set earlier.
///
/// The menu handle stays valid after this call: the icon holds its own
/// reference, so `uda_tray_menu_destroy` on the same menu is optional and does
/// not clear the tray's rows.
#[no_mangle]
pub extern "C" fn uda_tray_set_menu(tray_handle: u64, menu_handle: u64) -> c_int {
    util::catch_boundary(|| tray::set_menu(tray_handle, menu_handle))
}

/// Destroy a menu handle.
///
/// Safe to call after `uda_tray_set_menu`, as documented there. Returns
/// `UDA_ERR_INVALID_ARGUMENT` when the handle is not a live menu.
#[no_mangle]
pub extern "C" fn uda_tray_menu_destroy(menu_handle: u64) -> c_int {
    util::catch_boundary(|| tray::destroy_menu(menu_handle))
}

/// Convert a nullable C string to an owned `String`, defaulting to empty.
///
/// Kept separate from [`util::owned_string_from`] so that "null means empty" is
/// explicit at each call site instead of being a hidden property of a shared
/// helper.
fn owned_or_empty(pointer: *const c_char, parameter: &str) -> Result<String, error::Failure> {
    if pointer.is_null() {
        return Ok(String::new());
    }
    // SAFETY: null was rejected above; the helper validates the terminator and
    // the UTF-8 content before returning.
    unsafe { util::owned_string_from(pointer, parameter) }
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

    #[test]
    fn notify_rejects_a_null_out_parameter() {
        let title = CString::new("title").expect("ascii");
        // SAFETY: passing null for the out slot is exactly the case under test.
        let status = unsafe {
            uda_notify(
                title.as_ptr(),
                title.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());
    }

    #[test]
    fn notify_accepts_null_strings_as_empty() {
        // A notification with no app name, body, icon or actions is still a
        // notification. On a host without a notification daemon this reports
        // "not supported" rather than "invalid argument", which is the
        // distinction under test.
        let title = CString::new("UDA test").expect("ascii");
        let mut id: u32 = 0;
        // SAFETY: `id` is a live, writable local; the strings are null, which the
        // contract explicitly allows.
        let status = unsafe {
            uda_notify(
                title.as_ptr(),
                title.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                &mut id,
            )
        };
        assert_ne!(status, UDA_ERR_INVALID_ARGUMENT);
    }

    #[test]
    fn notify_transports_actions_as_a_flat_list() {
        let app = CString::new("UDA Notification Demo").expect("ascii");
        let title = CString::new("标题").expect("ascii");
        let body = CString::new("正文").expect("ascii");
        let actions = CString::new("open\n查看详情\nclose\n关闭").expect("ascii");
        let mut id: u32 = 0;
        // SAFETY: all five are live C strings and `id` a writable local.
        let status = unsafe {
            uda_notify(
                app.as_ptr(),
                title.as_ptr(),
                body.as_ptr(),
                std::ptr::null(),
                actions.as_ptr(),
                &mut id,
            )
        };
        assert_ne!(status, UDA_ERR_INVALID_ARGUMENT);
    }

    #[test]
    fn accent_color_rejects_a_null_out_parameter() {
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe { uda_get_accent_color(std::ptr::null_mut()) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());
    }

    #[test]
    fn accent_color_either_writes_four_channels_or_reports_none() {
        let mut rgba = [0u8; 4];
        // SAFETY: `rgba` is four writable bytes on this stack frame.
        let status = unsafe { uda_get_accent_color(rgba.as_mut_ptr()) };
        assert_eq!(status, UDA_OK);
        // Whatever the platform reports, a zeroed slot is the documented
        // "no accent colour" answer and must not be mistaken for a failure.
        let _ = rgba;
    }
}
