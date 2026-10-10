//! UniDesktop API (UDA) C-ABI export layer.
//!
//! This crate builds the shared library every non-Rust language links against:
//! `libuda_ffi.so` on Linux, `uda_ffi.dll` on Windows. The matching C header is
//! [`include/uda.h`](../../include/uda.h), **generated** by cbindgen from the
//! exported functions below and the surface types in [`abi`] - never edit the
//! header by hand, run `scripts/gen-header.sh` instead. Ready-made bindings
//! live under `examples/` (Python `ctypes`, Node.js `koffi`).
//!
//! # Contract
//!
//! Every exported function returns an `int32_t` status code: `0` success, `-1`
//! invalid argument (null pointer, bad UTF-8, unknown enum), `-2` feature not
//! supported, `-3` detection failed, `-4` I/O error, `-5` internal error, `-6` a
//! panic was contained at the boundary.
//!
//! # Memory ownership
//!
//! - Strings **returned** by UDA are allocated by Rust and must be freed with
//!   [`uda_free_string`]. The one exception is [`status_message`], whose
//!   static return must **not** be freed.
//! - Strings **passed in** are borrowed for the duration of the call only.
//! - Wake-lock handles are `uint64_t` values owned by this process; release each
//!   exactly once with [`uda_wakelock_release`].
//!
//! # Safety guarantees
//!
//! Every exported body runs inside [`std::panic::catch_unwind`], so a panic can
//! never unwind across the `extern "C"` boundary. No pointer is dereferenced
//! before a null check, and no buffer length is assumed: C strings are read to
//! their null terminator and validated as UTF-8.
//!
//! # Thread safety
//!
//! The exports are free functions with no global mutable state except the
//! wake-lock registry, the tray handle registry (both internally
//! synchronised) and the thread-local last-error slot.

pub mod abi;

mod dispatch;
mod error;
mod media;
mod notify;
mod session;
mod tray;
mod util;
mod wakelocks;

use std::os::raw::{c_char, c_void};

use uda_core::session::SessionAction;

pub use abi::*;
pub use error::status_message;

/// Detect the current system colour scheme.
///
/// Writes `UDA_THEME_UNKNOWN`, `UDA_THEME_DARK` or `UDA_THEME_LIGHT` to
/// `*out_theme`. On failure a negative status code is returned and
/// `*out_theme` is left untouched.
///
/// # Safety
///
/// `out_theme` must point to a writable, non-null `int32_t` location.
#[no_mangle]
pub unsafe extern "C" fn uda_detect_theme(out_theme: *mut i32) -> i32 {
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
/// `path` is a null-terminated UTF-8 filesystem path; on Linux a `file://`
/// URI is also accepted, while Windows requires a plain filesystem path. An
/// empty path is rejected as an invalid argument rather than being sent to
/// the backend. `fill_mode` is one of the `UDA_FILL_*` codes; anything else
/// is rejected.
///
/// On failure a negative status code is returned and no wallpaper is changed.
///
/// # Safety
///
/// `path` must be a valid, readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_set_wallpaper(path: *const c_char, fill_mode: i32) -> i32 {
    if path.is_null() {
        util::set_last_message("`path` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: null was rejected above; the conversion walks to the null
        // terminator and rejects invalid UTF-8 instead of reading past it.
        let path = unsafe { util::owned_string_from(path, "path") }?;
        if path.is_empty() {
            return Err(error::Failure::InvalidArgument(
                "`path` must not be empty".to_string(),
            ));
        }
        let fill_mode = fill_mode_from_c(fill_mode)?;
        dispatch::set_wallpaper(&path, fill_mode)
    })
}

/// Read the current wallpaper path.
///
/// On success `*out_path` receives a heap C string the caller must release
/// with `uda_free_string()`. When no wallpaper is configured (or the platform
/// cannot report one), `*out_path` is set to NULL while the call still
/// returns `UDA_OK` - check the pointer rather than the status to detect "no
/// wallpaper".
///
/// # Safety
///
/// `out_path` must point to a writable, non-null pointer location.
#[no_mangle]
pub unsafe extern "C" fn uda_get_wallpaper(out_path: *mut *mut c_char) -> i32 {
    if out_path.is_null() {
        util::set_last_message("`out_path` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let path = dispatch::get_wallpaper_path()?;

        // SAFETY: null was rejected above, and the caller guarantees a writable
        // pointer slot at this address.
        unsafe {
            *out_path = match &path {
                Some(path) => util::c_string_from(path),
                None => std::ptr::null_mut(),
            };
        }
        Ok(())
    })
}

/// Release a string previously returned by this library.
///
/// Passing NULL is a no-op, so callers may free unconditionally.
///
/// # Safety
///
/// `s` must be null or a pointer obtained from a UDA export that hands out
/// strings. Freeing a pointer owned by the caller is undefined behaviour.
#[no_mangle]
pub unsafe extern "C" fn uda_free_string(s: *mut c_char) {
    let _ = util::catch_boundary(|| {
        // SAFETY: see the function's safety contract.
        unsafe { util::free_c_string(s) };
        Ok(())
    });
}

/// Acquire a wake lock.
///
/// `lock_type` is `UDA_WAKELOCK_DISPLAY` or `UDA_WAKELOCK_SYSTEM`; `reason`
/// is a null-terminated UTF-8 description used for diagnostics. On success
/// `*out_handle` receives a non-zero handle to pass to
/// `uda_wakelock_release()`; on failure it is left untouched.
///
/// # Safety
///
/// `out_handle` must point to a writable, non-null `uint64_t` location and
/// `reason` must be a readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_wakelock_acquire(
    lock_type: i32,
    reason: *const c_char,
    out_handle: *mut u64,
) -> i32 {
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

/// Release a wake lock previously obtained from `uda_wakelock_acquire()`.
///
/// `handle` must be a value returned by `uda_wakelock_acquire()`; 0 is
/// rejected with `UDA_ERR_INVALID_ARGUMENT`, and so is any handle that is
/// not a live lock in this process (already released, or never issued
/// here).
#[no_mangle]
pub extern "C" fn uda_wakelock_release(handle: u64) -> i32 {
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
/// list of `key\nlabel` records. Any string may be NULL, which is treated as
/// the empty string.
///
/// `app_name` is not cosmetic: on Windows it is the AppUserModelID the toast
/// is addressed to, and an unpackaged process has none. UDA registers it as
/// the process's explicit AUMID before the first toast is shown, which is
/// what lets a plain `node script.js` display a native toast. Passing NULL
/// or "" selects the generic "UniDesktop.Notification" identity.
///
/// On Windows, toast *buttons* still require a packaged (MSIX) identity, so
/// `actions` is accepted for parity but not surfaced; the toast itself
/// displays normally. Use `WindowsNotificationManager::availability()` to
/// probe further.
///
/// A trailing `key` without its `label` is dropped rather than rendered as a
/// blank button. The remaining FreeDesktop fields keep their defaults:
/// `replaces_id` is 0 (a new notification), the expiry is the server
/// default, and the urgency is normal. Callers needing those must use the
/// Rust API.
///
/// On success `*out_id` receives the id the notification server assigned; it
/// is left untouched on failure.
///
/// # Safety
///
/// `out_id` must point to a writable, non-null `uint32_t` location. The five
/// strings must each be NULL or readable, null-terminated UTF-8.
#[no_mangle]
pub unsafe extern "C" fn uda_notify(
    app_name: *const c_char,
    title: *const c_char,
    body: *const c_char,
    icon: *const c_char,
    actions: *const c_char,
    out_id: *mut u32,
) -> i32 {
    if out_id.is_null() {
        util::set_last_message("`out_id` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
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

/// Read the system accent colour as four channels.
///
/// Writes R, G, B, A (each in the 0-255 range) to the four bytes at
/// `out_rgba`. A platform that exposes no accent colour - most Linux
/// desktops - leaves the bytes untouched and still returns `UDA_OK`, so a
/// zeroed slot means "no accent", not failure.
///
/// # Safety
///
/// `out_rgba` must point to four writable `uint8_t` values.
#[no_mangle]
pub unsafe extern "C" fn uda_get_accent_color(out_rgba: *mut u8) -> i32 {
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
        // four writable bytes.
        unsafe {
            *out_rgba = color.r;
            *out_rgba.add(1) = color.g;
            *out_rgba.add(2) = color.b;
            *out_rgba.add(3) = color.a;
        }
        Ok(())
    })
}

/// Read the now-playing metadata of the active media player.
///
/// On Linux the backend scans the session bus for an
/// `org.mpris.MediaPlayer2.*` service; on Windows it asks the Global System
/// Media Transport Controls session manager. Both answer "no player" when
/// none is running, which this function turns into: `*out_title`,
/// `*out_artist` and `*out_album` set to NULL, and `*out_duration_ms` /
/// `*out_position_ms` set to 0 - with a `UDA_OK` status. A now-playing card
/// therefore renders as empty rather than as a failure. If the platform has
/// no media backend at all, a negative status code is returned instead and
/// the out-parameters are left untouched.
///
/// The three strings are allocated by the library and must each be released
/// with `uda_free_string()`. Freeing NULL is a no-op, so callers may free
/// unconditionally. A field the player does not publish (a radio stream with
/// no album, say) is NULL rather than an empty string, which lets a binding
/// skip it, while a field published as an empty string stays an empty string -
/// the two cases remain distinguishable. The artist list is already joined
/// with ", " when the player publishes several artists.
///
/// `out_duration_ms` receives the track length in milliseconds, or 0 when
/// unknown (a live stream). `out_position_ms` is optional: pass NULL to skip
/// it; it receives the playback position in milliseconds, or 0 when the
/// backend cannot report it.
///
/// # Safety
///
/// `out_title`, `out_artist` and `out_album` must each point at a writable
/// `char *` location; `out_duration_ms` must point at a writable `uint64_t`;
/// `out_position_ms` must be NULL or point at a writable `uint64_t`.
#[no_mangle]
pub unsafe extern "C" fn uda_media_get_metadata(
    out_title: *mut *mut c_char,
    out_artist: *mut *mut c_char,
    out_album: *mut *mut c_char,
    out_duration_ms: *mut u64,
    out_position_ms: *mut u64,
) -> i32 {
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
        let metadata = media::active_metadata()?;

        // The documented empty answer: an unpublished field is NULL rather
        // than an empty C string, "no player" (which the backends already
        // normalize into `None`) is NULL everywhere, and a zero duration
        // means "unknown" - all with a UDA_OK status, so a now-playing card
        // renders as empty instead of as a failure.
        let (title, artist, album, duration_ms, position_ms) = match metadata {
            Some(ref metadata) => (
                c_string_or_null(metadata.title.as_deref()),
                c_string_or_null(metadata.artist.as_deref()),
                c_string_or_null(metadata.album.as_deref()),
                metadata.duration_ms.unwrap_or(0),
                metadata.position_ms.unwrap_or(0),
            ),
            None => (
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                0,
                0,
            ),
        };

        // SAFETY: all four pointers were validated non-null and writable above;
        // `out_position_ms` is checked before the optional write.
        unsafe {
            *out_title = title;
            *out_artist = artist;
            *out_album = album;
            *out_duration_ms = duration_ms;
            if !out_position_ms.is_null() {
                *out_position_ms = position_ms;
            }
        }
        Ok(())
    })
}

/// Read the playback status of the active media player.
///
/// Writes `UDA_MEDIA_PLAYING`, `UDA_MEDIA_PAUSED`, `UDA_MEDIA_STOPPED` or
/// `UDA_MEDIA_UNKNOWN` to `*out_status`. `UDA_MEDIA_UNKNOWN` covers both "no
/// player is running" and "the state could not be determined", and is
/// reported with `UDA_OK`: it is an answer, not a failure. A negative status
/// code means the platform has no media backend at all.
///
/// # Safety
///
/// `out_status` must point to a writable, non-null `int32_t` location.
#[no_mangle]
pub unsafe extern "C" fn uda_media_get_status(out_status: *mut i32) -> i32 {
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
/// `command` is one of `UDA_MEDIA_CMD_PLAY`, `UDA_MEDIA_CMD_PAUSE`,
/// `UDA_MEDIA_CMD_TOGGLE`, `UDA_MEDIA_CMD_NEXT`, `UDA_MEDIA_CMD_PREVIOUS` or
/// `UDA_MEDIA_CMD_STOP`. An unrecognised code returns
/// `UDA_ERR_INVALID_ARGUMENT` and nothing is sent.
///
/// A machine with no player running reports `UDA_ERR_NOT_SUPPORTED`, and any
/// non-`UDA_OK` status means the command was not delivered, so a caller can
/// tell "not delivered" from "delivered" without inspecting the player.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_media_send_command(command: i32) -> i32 {
    util::catch_boundary(|| {
        let command = media::command_from_c(command)?;
        media::send_command(command)
    })
}

/// Report which session and power actions this platform can perform.
///
/// Writes a bitmask made of the `UDA_SESSION_CAP_*` flags to
/// `*out_capabilities`; 0 means "no session backend exists on this target".
///
/// The query is static and side-effect-free - it never touches the machine's
/// power state - so a host may call it freely to decide which menu entries to
/// draw, and *must* call it before drawing one that could shut the machine
/// down.
///
/// A set bit means "the code path exists", not "the account is allowed": a
/// machine with hibernation switched off still reports
/// `UDA_SESSION_CAP_HIBERNATE`, and the attempt then fails with
/// `UDA_ERR_NOT_SUPPORTED`. Likewise, Windows reboot and shutdown need the
/// SeShutdownPrivilege, which is a runtime answer.
///
/// # Safety
///
/// `out_capabilities` must point to a writable, non-null `uint32_t` location.
#[no_mangle]
pub unsafe extern "C" fn uda_session_capabilities(out_capabilities: *mut u32) -> i32 {
    if out_capabilities.is_null() {
        util::set_last_message("`out_capabilities` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // SAFETY: null was rejected above, and the caller guarantees a writable
        // `uint32_t` at this address.
        unsafe { *out_capabilities = session::capabilities().bits() };
        Ok(())
    })
}

/// Lock the session.
///
/// Linux: `org.freedesktop.ScreenSaver.Lock()` on the session bus, falling
/// back to `loginctl lock-session`. Windows: `LockWorkStation()`.
///
/// This is the only session action that is safe to automate: it is reversible
/// (the user unlocks with their password) and it destroys nothing. The other
/// five `uda_session_*` actions must be gated behind an explicit user
/// confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_lock() -> i32 {
    util::catch_boundary(|| session::perform(SessionAction::Lock))
}

/// End the calling user's session.
///
/// Linux: `org.freedesktop.login1.Manager.TerminateSession("")` on the system
/// bus, falling back to the desktop's own session manager (GNOME, KDE, XFCE).
/// Windows: `ExitWindowsEx(EWX_LOGOFF, 0)`.
///
/// WARNING: this logs the user out. Unsaved work in applications that do not
/// refuse is lost. Never call it without an explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_logout() -> i32 {
    util::catch_boundary(|| session::perform(SessionAction::Logout))
}

/// Suspend the machine to RAM.
///
/// Linux: `org.freedesktop.login1.Manager.Suspend(false)`. Windows:
/// `SetSuspendState(false, ...)`.
///
/// WARNING: this changes the machine's power state. Never call it without an
/// explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_suspend() -> i32 {
    util::catch_boundary(|| session::perform(SessionAction::Suspend))
}

/// Hibernate the machine to disk.
///
/// Linux: `org.freedesktop.login1.Manager.Hibernate(false)`. Windows:
/// `SetSuspendState(true, ...)`, which the platform rejects with
/// ERROR_FILE_NOT_FOUND when hibernation is disabled - reported as
/// `UDA_ERR_NOT_SUPPORTED`.
///
/// WARNING: this changes the machine's power state. Never call it without an
/// explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_hibernate() -> i32 {
    util::catch_boundary(|| session::perform(SessionAction::Hibernate))
}

/// Restart the machine.
///
/// Linux: `org.freedesktop.login1.Manager.Reboot(false)`. Windows:
/// `ExitWindowsEx(EWX_REBOOT | EWX_FORCEIFHUNG, 0)` after enabling
/// SeShutdownPrivilege, which needs an elevated process or an administrator
/// account; without it the call fails with `UDA_ERR_NOT_SUPPORTED` rather
/// than half-rebooting.
///
/// WARNING: this restarts the machine and unsaved work is lost. Never call it
/// without an explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_reboot() -> i32 {
    util::catch_boundary(|| session::perform(SessionAction::Reboot))
}

/// Power the machine off.
///
/// Linux: `org.freedesktop.login1.Manager.PowerOff(false)`. Windows:
/// `ExitWindowsEx(EWX_POWEROFF | EWX_FORCEIFHUNG, 0)` after enabling
/// SeShutdownPrivilege, with the same elevation requirement as
/// `uda_session_reboot()`.
///
/// WARNING: this shuts the machine down and unsaved work is lost. Never call
/// it without an explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_shutdown() -> i32 {
    util::catch_boundary(|| session::perform(SessionAction::Shutdown))
}

/// Return the message describing the most recent failure on the calling
/// thread.
///
/// Returns a newly allocated, null-terminated string that the caller must
/// release with `uda_free_string()` (cast away `const` at the call), or NULL
/// when no failure has been recorded yet. Reading consumes the message: the
/// next call returns NULL until a new failure is recorded on the same
/// thread.
#[no_mangle]
pub extern "C" fn uda_last_error_message() -> *const c_char {
    let Some(message) = util::take_last_message() else {
        return std::ptr::null();
    };

    // Leaked on purpose: the caller frees it with `uda_free_string`, keeping one
    // allocation policy for every string the library hands out. `UdaError`'s
    // `Display` never emits an interior nul, so the failure arm is unreachable.
    match std::ffi::CString::new(message) {
        Ok(c_string) => c_string.into_raw().cast_const(),
        Err(_) => std::ptr::null(),
    }
}

/// Describe a status code with a static string.
///
/// The returned pointer is valid for the lifetime of the library and must
/// not be freed. Useful for rendering a failure without a second FFI
/// round-trip.
#[no_mangle]
pub extern "C" fn uda_status_message(status: i32) -> *const c_char {
    STATUS_MESSAGES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(pointer) = cache.get(&status) {
            return *pointer;
        }

        let text = std::ffi::CString::new(error::status_message(status)).unwrap_or_default();
        let pointer = text.into_raw().cast_const();
        cache.insert(status, pointer);
        pointer
    })
}

// ---------------------------------------------------------------------------
// System tray
// ---------------------------------------------------------------------------

// The C caller cannot hold an `Arc`, so every tray icon and context menu lives
// in a process-wide table keyed by an opaque `uint64_t`. Handles start at `1`;
// `0` means "no handle". A handle can be destroyed exactly once: a second
// destroy of the same value is `UDA_ERR_INVALID_ARGUMENT` rather than a silent
// no-op, so a host cannot double-release a shell resource.

/// Create a tray icon.
///
/// TRAY HANDLES
///
/// Tray icons and menus are process-local resources identified by opaque
/// `uint64_t` handles. They are NOT references, NOT pointers, and are not
/// valid in another process: each handle indexes a table owned by the shared
/// library.
///
/// - A handle can be destroyed exactly once. Destroying it removes the table
///   entry, so a successful destroy followed by another call with the same
///   value is reported as `UDA_ERR_INVALID_ARGUMENT` rather than acting on a
///   stale resource.
/// - Handle 0 is never a live resource. A zeroed out-parameter therefore
///   unambiguously means "the call failed".
/// - A menu handle stays valid after `uda_tray_set_menu()`: the icon holds
///   its own reference, so destroying the menu afterwards leaves the tray
///   working. Destroy the menu explicitly only when the icon will never need
///   it again.
///
/// CALLBACK THREADING MODEL (read before writing a handler)
///
/// Every menu callback is invoked on the tray worker thread that the
/// platform backend owns. It is NOT the thread that called
/// `uda_tray_menu_add_text()` and NOT your UI thread. The callback therefore
/// must:
///
/// - return as soon as possible (a slow handler stalls every later menu
///   interaction);
/// - not block, sleep, or wait on a lock the main thread may hold;
/// - not touch GUI toolkit state directly - post an event into the host's
///   own loop instead;
/// - be prepared to fire after the row was removed, if the shell was already
///   mid-dispatch.
///
/// `user_data` is handed back to the callback verbatim. UDA never
/// dereferences it; keeping it alive is the host's responsibility. The
/// callback may be NULL, which yields a silent row that still renders and
/// still reports its state in later calls.
///
/// ARGUMENTS
///
/// `name` is the application name used for registration (the D-Bus bus name
/// on Linux, the window class on Windows); a NULL or empty `name` selects
/// the library default. `tooltip` may be NULL or empty; text longer than
/// 127 characters is clamped, and the call still succeeds.
///
/// On success `*out_handle` receives a non-zero handle for every other
/// `uda_tray_*` call. On failure it is left untouched. The icon has no image
/// until `uda_tray_set_icon_path()` or `uda_tray_set_icon_rgba()` supplies
/// one, so it can be prepared and only made visible once built.
///
/// # Safety
///
/// `out_handle` must point to a writable, non-null `uint64_t` slot. `name`
/// and `tooltip` must be NULL or readable, null-terminated UTF-8 strings.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_create(
    name: *const c_char,
    tooltip: *const c_char,
    out_handle: *mut u64,
) -> i32 {
    if out_handle.is_null() {
        util::set_last_message("`out_handle` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let (name, tooltip) = unsafe {
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
/// `tooltip` is null-terminated UTF-8 text, or NULL to clear it.
///
/// # Safety
///
/// `tooltip` must be a readable, null-terminated UTF-8 string, or NULL for
/// the empty string.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_set_tooltip(handle: u64, tooltip: *const c_char) -> i32 {
    util::catch_boundary(|| {
        let tooltip = owned_or_empty(tooltip, "tooltip")?;
        tray::set_tooltip(handle, &tooltip)
    })
}

/// Replace a tray icon's image from a file or icon-theme name.
///
/// `path` is a null-terminated UTF-8 path. Linux also accepts a freedesktop
/// icon-theme name here; Windows requires a file path (`.ico`, `.cur`,
/// `.bmp`).
///
/// # Safety
///
/// `path` must be a readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_set_icon_path(handle: u64, path: *const c_char) -> i32 {
    if path.is_null() {
        util::set_last_message("`path` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let path = unsafe { util::owned_string_from(path, "path") }?;
        tray::set_icon_path(handle, &path)
    })
}

/// Replace a tray icon's image from raw pixels.
///
/// The buffer is borrowed: only `stride * height` bytes are copied and the
/// caller keeps ownership of `data`. Pixels are top-down RGBA, four bytes
/// each. A buffer shorter than `stride * height` is rejected before any
/// pixel is read.
///
/// # Safety
///
/// `data` must not be null and must point to `len` readable bytes. `width`
/// and `height` must be non-zero and `stride` at least `width * 4`.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_set_icon_rgba(
    handle: u64,
    width: u32,
    height: u32,
    stride: u32,
    data: *const u8,
    len: usize,
) -> i32 {
    if data.is_null() {
        util::set_last_message("`data` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| tray::set_icon_rgba(handle, width, height, stride, data, len))
}

/// Show or hide the icon without unregistering it.
///
/// `visible` is a C boolean: 0 hides, any other value shows.
#[no_mangle]
pub extern "C" fn uda_tray_set_visible(handle: u64, visible: i32) -> i32 {
    util::catch_boundary(|| tray::set_visible(handle, visible != 0))
}

/// Destroy a tray icon and unregister it from the shell.
///
/// Terminal: the handle cannot be reused afterwards. Menus previously
/// attached keep their own handles and stay valid. Returns
/// `UDA_ERR_INVALID_ARGUMENT` when the handle is not a live tray icon in
/// this process.
#[no_mangle]
pub extern "C" fn uda_tray_destroy(handle: u64) -> i32 {
    util::catch_boundary(|| tray::destroy_icon(handle))
}

/// Create an empty context menu.
///
/// On success `*out_menu_handle` receives a non-zero handle for the
/// `uda_tray_menu_add_*` functions and `uda_tray_set_menu()`. On failure it
/// is left untouched.
///
/// # Safety
///
/// `out_menu_handle` must point to a writable, non-null `uint64_t` slot.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_menu_create(out_menu_handle: *mut u64) -> i32 {
    if out_menu_handle.is_null() {
        util::set_last_message("`out_menu_handle` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let handle = tray::create_menu()?;
        unsafe { *out_menu_handle = handle };
        Ok(())
    })
}

/// Append a plain text row to a menu.
///
/// `callback` is invoked on the tray worker thread when the row is activated,
/// or NULL for a silent row that still renders and still reports its state
/// in later calls. `user_data` is handed back to the callback untouched and
/// is never dereferenced by UDA. `label` must be non-blank: a blank label is
/// rejected with `UDA_ERR_NOT_SUPPORTED` because it would render invisibly.
///
/// On success `*out_item_id` receives the row's stable, non-zero id; the
/// callback receives the same value, so a host does not need a table of its
/// own.
///
/// # Safety
///
/// `out_item_id` must point to a writable, non-null `uint64_t` slot and
/// `label` must be a readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_menu_add_text(
    menu_handle: u64,
    label: *const c_char,
    callback: UdaTrayTextCallback,
    user_data: *mut c_void,
    out_item_id: *mut u64,
) -> i32 {
    if out_item_id.is_null() {
        util::set_last_message("`out_item_id` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }
    if label.is_null() {
        util::set_last_message("`label` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let label = unsafe { util::owned_string_from(label, "label") }?;
        let item_id = tray::menu_add_text(menu_handle, &label, callback, user_data)?;
        unsafe { *out_item_id = item_id };
        Ok(())
    })
}

/// Append a visual separator to a menu.
///
/// A separator has no label, no callback and no id, so there is no
/// out-parameter.
#[no_mangle]
pub extern "C" fn uda_tray_menu_add_separator(menu_handle: u64) -> i32 {
    util::catch_boundary(|| tray::menu_add_separator(menu_handle))
}

/// Append a checkbox row to a menu.
///
/// The row's stored value is inverted *before* `callback` runs, so the
/// `checked` argument is the new state the shell will render and the menu
/// cannot drift out of sync with it. `checked` is 0 to start unchecked, any
/// other value to start checked. `callback` is invoked on the tray worker
/// thread when the row is toggled, or NULL for a silent row. `label` must be
/// non-blank: a blank label is rejected with `UDA_ERR_NOT_SUPPORTED`.
///
/// On success `*out_item_id` receives the row's stable, non-zero id.
///
/// # Safety
///
/// `out_item_id` must point to a writable, non-null `uint64_t` slot and
/// `label` must be a readable, null-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_menu_add_checkbox(
    menu_handle: u64,
    label: *const c_char,
    checked: i32,
    callback: UdaTrayCheckboxCallback,
    user_data: *mut c_void,
    out_item_id: *mut u64,
) -> i32 {
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
/// reference, so destroying the menu afterwards is optional and does not
/// clear the rows.
#[no_mangle]
pub extern "C" fn uda_tray_set_menu(tray_handle: u64, menu_handle: u64) -> i32 {
    util::catch_boundary(|| tray::set_menu(tray_handle, menu_handle))
}

/// Destroy a menu handle.
///
/// Safe to call after `uda_tray_set_menu()`, as documented there. Terminal:
/// the handle cannot be reused afterwards. Returns
/// `UDA_ERR_INVALID_ARGUMENT` when the handle is not a live menu in this
/// process.
#[no_mangle]
pub extern "C" fn uda_tray_menu_destroy(menu_handle: u64) -> i32 {
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

/// Allocate a C string for a media metadata field, or return null.
///
/// `None` means the player did not publish the field and maps to NULL; a
/// published value - including an empty one - maps to a real string, so a
/// host can tell "published empty" from "not published".
fn c_string_or_null(text: Option<&str>) -> *mut c_char {
    match text {
        Some(text) => util::c_string_from(text),
        None => std::ptr::null_mut(),
    }
}

thread_local! {
    /// Cache of the leaked status-message strings, so repeated calls with the
    /// same code reuse a single allocation instead of leaking one per call.
    static STATUS_MESSAGES: std::cell::RefCell<std::collections::HashMap<i32, *const c_char>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Map a C fill-mode code onto the cross-platform [`FillMode`].
fn fill_mode_from_c(code: i32) -> Result<uda_core::wallpaper::FillMode, error::Failure> {
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
fn wake_lock_type_from_c(code: i32) -> Result<uda_core::wakelock::WakeLockType, error::Failure> {
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
    fn metadata_fields_distinguish_published_empty_from_unpublished() {
        // Unpublished -> NULL, the documented "skip this field" answer.
        assert!(c_string_or_null(None).is_null());
        // Published as an empty string -> a real, empty C string, so a host
        // that cares can tell the two apart; the caller frees it like any
        // other library string.
        let pointer = c_string_or_null(Some(""));
        assert!(!pointer.is_null());
        // SAFETY: the pointer came from `c_string_from` in this test.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_str()
            .expect("empty string is valid UTF-8");
        assert_eq!(text, "");
        // SAFETY: same pointer, freed exactly once.
        unsafe { util::free_c_string(pointer) };
        // Published text -> the text.
        let pointer = c_string_or_null(Some("/music/track.flac"));
        assert!(!pointer.is_null());
        // SAFETY: same ownership pattern as above.
        unsafe { util::free_c_string(pointer) };
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
        let mut theme: i32 = -100;
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
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
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
