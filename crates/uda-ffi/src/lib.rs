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
//!   [`uda_free_string`], including [`uda_last_error_message`]. The only
//!   exception is [`uda_status_message`] (a static string), which must never be
//!   freed.
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
pub use error::{status_message, UdaStatus};

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
/// `path` accepts a plain filesystem path on every backend. A `file://` URI is
/// additionally accepted on GNOME and KDE, which convert it before handing the
/// value to their shell; other backends (Windows, Hyprland, Sway, X11) require
/// a path and receive the string unchanged. An empty path is rejected as an
/// invalid argument rather than being sent to the backend.
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

/// Read the metadata of the active media player.
///
/// The three strings are allocated by Rust and must be released with
/// [`uda_free_string`]. An unpublished field is written as a null pointer, or
/// zero for a duration, so each pointer must be checked before it is read. A
/// field published as an empty string stays an empty C string where the backend
/// can tell the two apart (Linux); on Windows an empty read also surfaces as
/// null, because SMTC has no "unset" marker on the wire, so a host must treat
/// null as "skip this field" everywhere. With no player running every
/// out-parameter is set to null/zero and [`UDA_OK`] is still returned.
///
/// `out_position_ms` is optional: pass null to skip it.
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
        let metadata = media::active_metadata()?.unwrap_or_default();

        // The documented empty answer: an unpublished field is NULL rather
        // than an empty C string, "no player" (which the backends already
        // normalize into `None`) is NULL everywhere, and a zero duration
        // means "unknown" - all with a UDA_OK status, so a now-playing card
        // renders as empty instead of as a failure.
        let title = c_string_or_null(metadata.title.as_deref());
        let artist = c_string_or_null(metadata.artist.as_deref());
        let album = c_string_or_null(metadata.album.as_deref());
        let duration_ms = metadata.duration_ms.unwrap_or(0);
        let position_ms = metadata.position_ms.unwrap_or(0);

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
/// reported with [`UDA_OK`]: it is an answer, not a failure. A negative status
/// code means the backend itself could not be reached: on Linux, connecting to
/// the session bus or building the player proxy fails with
/// `UDA_ERR_DETECTION_FAILED`; on Windows, acquiring the SMTC session manager
/// fails with `UDA_ERR_NOT_SUPPORTED` or `UDA_ERR_DETECTION_FAILED`. A player
/// session that *is* selected but whose state cannot be read is not a
/// failure: that reports [`UDA_OK`] with `UDA_MEDIA_UNKNOWN`. On any negative
/// status `*out_status` is left untouched.
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
/// `command` is one of [`UDA_MEDIA_CMD_PLAY`], [`UDA_MEDIA_CMD_PAUSE`],
/// [`UDA_MEDIA_CMD_TOGGLE`], [`UDA_MEDIA_CMD_NEXT`],
/// [`UDA_MEDIA_CMD_PREVIOUS`] or [`UDA_MEDIA_CMD_STOP`]. An unknown code returns
/// `UDA_ERR_INVALID_ARGUMENT` and nothing is sent.
///
/// `UDA_ERR_NOT_SUPPORTED` is returned
/// when the player refuses the command and when no player is running.
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
/// Writes a bitmask made of the `UDA_SESSION_CAP_*` constants to
/// `*out_capabilities`; `0` means "no receiver for any session action is
/// reachable on this target". The query is side-effect-free - it never touches
/// the machine's power state. On Linux the first call runs a one-shot service
/// probe: every D-Bus await is bounded, and the session lookup starts after the
/// presence check, so the worst case is about 35 seconds before the answer is
/// cached; later calls are immediate. A UI therefore calls it once, ideally off
/// the main thread, before drawing a menu - and must call it before drawing one
/// that could shut the machine down.
///
/// A set bit means a receiver for the action was reachable when the
/// capabilities were queried; the attempt can still fail (hibernation
/// switched off, polkit refusal) and then fails with `UDA_ERR_NOT_SUPPORTED`.
/// On Linux `LOCK` needs the ScreenSaver service or a resolvable logind
/// `auto` session (the caller's session, with the user's display session as
/// the fallback logind itself applies), while `LOGOUT` and
/// `SESSION_MANAGEMENT` need the caller's *own* logind session - resolved
/// with `GetSession("")`, which never falls back to a display session - or a
/// GNOME/KDE/XFCE desktop logout manager whose bus name is owned or
/// activatable. A daemon that runs sessionless (or can only see other users'
/// sessions, as on a systemd-enabled WSL2 host) therefore answers with the
/// power bits, plus `LOCK` when `auto` resolves. Discovery never activates a
/// desktop manager; activation or policy can still refuse an attempted
/// action. Windows reboot and shutdown can likewise fail at attempt time
/// when the caller lacks the SeShutdownPrivilege.
///
/// # Safety
///
/// `out_capabilities` must point at a writable `uint32_t` location.
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

/// Lock the session, leaving every running program alone.
///
/// This is the **only** action safe to automate: it is reversible and destroys
/// nothing. The other five exports must be gated behind an explicit user
/// confirmation.
///
/// Returns `UDA_ERR_NOT_SUPPORTED` when
/// the platform advertises no lock capability at all.
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
/// **This action logs the user out.** Unsaved work is lost unless the desktop
/// refuses to comply. Never call it without an explicit confirmation.
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
/// **This action changes the machine's power state.** Never call it without an
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
/// **This action changes the machine's power state.** Unsaved work is lost.
/// Never call it without an explicit user confirmation.
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
/// On Windows this needs an elevated process or a local administrator account
/// for `SeShutdownPrivilege`; without it the call fails with
/// `UDA_ERR_NOT_SUPPORTED`.
///
/// **This action restarts the machine.** Unsaved work is lost. Never call it
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
/// Same elevation requirement as [`uda_session_reboot`].
///
/// **This action shuts the machine down.** Unsaved work is lost. Never call it
/// without an explicit user confirmation.
///
/// # Safety
///
/// This function takes no pointers; there is nothing to validate.
#[no_mangle]
pub unsafe extern "C" fn uda_session_shutdown() -> i32 {
    util::catch_boundary(|| session::perform(SessionAction::Shutdown))
}

/// Read the current wallpaper path.
///
/// On success `*out_path` receives a heap C string the caller must release with
/// [`uda_free_string`]. When no wallpaper is configured (or the platform cannot
/// report one), `*out_path` is set to null and the call still returns
/// [`UDA_OK`] - check the pointer, not the status, to detect "no wallpaper". An
/// empty value is never handed back as a zero-length string; it is reported as
/// null too.
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
/// # Safety
///
/// `s` must be null or a pointer obtained from an export that transfers string
/// ownership to the caller: [`uda_get_wallpaper`], [`uda_media_get_metadata`],
/// [`uda_tray_support_reason`] or [`uda_last_error_message`]. Freeing any other
/// pointer is undefined behaviour. Passing null is a no-op, so callers may free
/// unconditionally.
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
/// On Linux, when the native IPC tiers are unavailable, the fallback holds
/// the lock by running `systemd-inhibit` for at most 3600 seconds. That
/// fallback lock expires silently at the cap and must be re-acquired; the
/// native tiers are not capped. A successful acquire records no error message
/// of its own, but it does not clear an unread message left by an earlier
/// failure on the same thread; read `uda_last_error_message()` only after a
/// non-`UDA_OK` status.
///
/// A lock acquired through the CLI fallback (no native IPC available) is
/// bounded to roughly one hour: past that deadline the lock is retired and its
/// handle stops being live, so a host that needs a longer lock must re-acquire
/// it.
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
/// Returns `UDA_ERR_INVALID_ARGUMENT` when the handle is not a live lock in
/// this process (already released, never issued here, or a fallback lock
/// retired after its bounded lifetime elapsed).
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
/// displays normally. Rust callers can probe further with
/// `WindowsNotificationManager::availability()`; the C-ABI surface has no
/// notification-availability probe.
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

/// Return the message describing the most recent failure on this thread.
///
/// The returned string is heap-allocated by this call and its ownership moves
/// to the caller: release it with [`uda_free_string`] and it stays valid until
/// then, no matter how many other UDA calls run in between. Returns null when
/// no failure has been recorded on this thread yet.
///
/// **Reading consumes the message.** The slot is emptied before the string is
/// built, so an immediate second call returns null; a binding that needs the
/// text twice must copy it or keep the returned allocation alive.
#[no_mangle]
pub extern "C" fn uda_last_error_message() -> *mut c_char {
    util::last_error_pointer()
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

/// Report which tray features the active platform backend advertises.
///
/// Writes a bitmask made of the `UDA_TRAY_CAP_*` constants to
/// `*out_capabilities`; `0` means "no tray backend exists on this target", and
/// a feature the backend cannot deliver has its bit cleared. The query is
/// side-effect-free - it never registers anything with the shell - so a host
/// may call it freely to decide whether to build tray UI at all, and what to
/// degrade gracefully.
///
/// `UDA_TRAY_CAP_CLICK` and `UDA_TRAY_CAP_DOUBLE_CLICK` are always cleared:
/// the C ABI has no click-callback registration, so activation is delivered
/// only through menu rows. The constants stay defined for a future click API;
/// a host must not infer click events from this bitmask today.
///
/// # Safety
///
/// `out_capabilities` must point at a writable `uint32_t` location.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_capabilities(out_capabilities: *mut u32) -> i32 {
    if out_capabilities.is_null() {
        util::set_last_message("`out_capabilities` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        // The C ABI cannot deliver click callbacks, so the backend's click bits
        // are masked off rather than advertised. See the doc comment above.
        // The bit values live in `crate::abi` with the rest of the C ABI.
        const UNDELIVERABLE_BY_C_ABI: u32 = UDA_TRAY_CAP_CLICK | UDA_TRAY_CAP_DOUBLE_CLICK;
        // SAFETY: null was rejected above, and the caller guarantees a writable
        // `uint32_t` at this address.
        unsafe { *out_capabilities = tray::capabilities().bits() & !UNDELIVERABLE_BY_C_ABI };
        Ok(())
    })
}

/// Report how completely this icon's tray registration succeeded.
///
/// Writes one of the `UDA_TRAY_SUPPORT_LEVEL_*` codes to `*out_level`:
/// `UDA_TRAY_SUPPORT_LEVEL_FULL` when nothing degraded,
/// `UDA_TRAY_SUPPORT_LEVEL_PARTIAL` when the icon is live but its registration
/// recorded a degradation - on Linux, no `StatusNotifierWatcher` is reachable
/// or the watcher refuses the registration, so the item is exported yet the
/// shell may never display it - and `UDA_TRAY_SUPPORT_LEVEL_NONE` only when the
/// backend never claimed a tray at all (not reachable through handles this
/// library issued).
///
/// The answer is per icon, not per platform: `uda_tray_capabilities()` reports
/// which code paths exist, this reports what actually happened to *this* icon,
/// and `uda_tray_support_reason()` explains a `PARTIAL` answer.
///
/// On Windows the answer is always `UDA_TRAY_SUPPORT_LEVEL_FULL` for a live
/// icon: the Win32 worker has no watcher step, so there is no degradation path.
///
/// # Safety
///
/// `out_level` must point at a writable `int32_t` location.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_support_level(handle: u64, out_level: *mut i32) -> i32 {
    if out_level.is_null() {
        util::set_last_message("`out_level` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let level = tray::support_level(handle)?;
        // SAFETY: null was rejected above, and the caller guarantees a writable
        // `int32_t` at this address.
        unsafe { *out_level = level };
        Ok(())
    })
}

/// Explain why a tray icon's support level is partial.
///
/// On success `*out_reason` receives a heap C string the caller must release
/// with [`uda_free_string`], or NULL when the icon is not degraded - check the
/// pointer, not the status. A degraded icon (see `uda_tray_support_level`)
/// always carries a non-empty reason naming what is missing or refused, e.g. a
/// session with no `StatusNotifierWatcher` or a watcher that rejects the item;
/// a fully supported icon reports NULL.
///
/// # Safety
///
/// `out_reason` must point at a writable pointer location.
#[no_mangle]
pub unsafe extern "C" fn uda_tray_support_reason(handle: u64, out_reason: *mut *mut c_char) -> i32 {
    if out_reason.is_null() {
        util::set_last_message("`out_reason` must not be null");
        return UDA_ERR_INVALID_ARGUMENT;
    }

    util::catch_boundary(|| {
        let reason = tray::support_reason(handle)?;

        // SAFETY: null was rejected above, and the caller guarantees a writable
        // pointer slot at this address.
        unsafe {
            *out_reason = match &reason {
                Some(text) => util::c_string_from(text),
                None => std::ptr::null_mut(),
            };
        }
        Ok(())
    })
}

/// Create a tray icon.
///
/// `name` is the application name used for registration (the D-Bus bus name on
/// Linux, the window class on Windows); a null `name` selects the library
/// default. `tooltip` may be null or empty. Windows applies the 127-`char`
/// clamp at registration and then fits the result into szTip's 127 UTF-16
/// units (a non-BMP character uses two); Linux SNI forwards the create-time
/// text as-is. `uda_tray_set_tooltip` clamps to 127 `char`s on both platforms,
/// and registration still succeeds either way.
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
/// each. A zero `width` or `height`, a `stride` smaller than `width * 4`, or
/// a `len` smaller than `stride * height` is rejected with
/// `UDA_ERR_INVALID_ARGUMENT` before any pixel is read.
///
/// # Safety
///
/// `data` must not be null and must point to `len` readable bytes.
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
/// `callback` is invoked on the **tray worker thread** when the row is
/// activated. Passing `None` (null) is legal: the row still renders, but
/// activating it does nothing. A text row has no toggle state, and there is no
/// API to read a row's state. `user_data` is handed back to the callback
/// untouched and is never dereferenced by UDA.
///
/// On success `*out_item_id` receives the row's stable, non-zero id; the
/// callback receives the same value, so a host does not need a table of its
/// own.
///
/// # Safety
///
/// `out_item_id` must be a valid, writable, non-null `uint64_t` slot. `label`
/// must be a readable, null-terminated UTF-8 string; a blank label is rejected
/// with `UDA_ERR_INVALID_ARGUMENT`
/// because it would render an invisible row.
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
/// When a `callback` is supplied, the row's own stored value is inverted *before*
/// it runs, so the `checked` argument is the new state the shell will render.
/// A null callback installs no handler at all: the row renders with its initial
/// state and never toggles, which is what a host that drives the checkbox
/// through its own UI wants.
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
/// reference, so the menu keeps working whether or not the handle is destroyed
/// later. Destroying the handle (see [`uda_tray_menu_destroy`]) detaches the
/// menu from every icon still showing it.
#[no_mangle]
pub extern "C" fn uda_tray_set_menu(tray_handle: u64, menu_handle: u64) -> i32 {
    util::catch_boundary(|| tray::set_menu(tray_handle, menu_handle))
}

/// Destroy a menu handle.
///
/// The menu is detached from **every** tray icon that still shows it, so its
/// rows and callbacks stop firing (a snapshot a backend already cloned stays
/// silent instead of calling the host); the icons themselves stay alive and
/// simply have no menu afterwards. This makes destroying a menu handle safe
/// even when the host has already attached it. Terminal: the handle cannot be
/// reused afterwards. Returns `UDA_ERR_INVALID_ARGUMENT` when the handle is not
/// a live menu, and `UDA_ERR_INTERNAL` while one of the menu's callbacks is
/// still running - the menu stays fully intact then, so a host that waits for
/// its own callback to return and retries will succeed; the host may free its
/// callback trampolines only after this call reports success.
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
///
/// This deliberately does not go through [`util::c_string_from`]: that helper
/// folds an empty string into null, honouring the C ABI's "empty means absent"
/// convention for the other exports. A media field needs the two cases kept
/// apart, so an empty published value allocates a real, zero-length C string.
fn c_string_or_null(text: Option<&str>) -> *mut c_char {
    match text {
        // The only failure is an interior NUL, which no backend can produce;
        // reporting it as "not published" is still the correct C answer.
        Some(text) => match std::ffi::CString::new(text) {
            Ok(c_string) => c_string.into_raw(),
            Err(_) => std::ptr::null_mut(),
        },
        None => std::ptr::null_mut(),
    }
}

thread_local! {
    /// Cache of the leaked status-message strings, so repeated calls with the
    /// same code reuse a single allocation instead of leaking one per call.
    static STATUS_MESSAGES: std::cell::RefCell<std::collections::HashMap<i32, *const c_char>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
}

/// Map a C fill-mode code onto the cross-platform [`FillMode`](uda_core::wallpaper::FillMode).
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

/// Map a C wake-lock code onto the cross-platform [`WakeLockType`](uda_core::wakelock::WakeLockType).
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
        // SAFETY: the pointer came from `c_string_or_null` in this test.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_str()
            .expect("empty string is valid UTF-8");
        assert_eq!(text, "");
        // SAFETY: same pointer, freed exactly once.
        unsafe { util::free_c_string(pointer) };
        // Published text -> the text, read back verbatim.
        let pointer = c_string_or_null(Some("/music/track.flac"));
        assert!(!pointer.is_null());
        // SAFETY: the pointer came from `c_string_or_null` in this test.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_str()
            .expect("the path is valid UTF-8");
        assert_eq!(text, "/music/track.flac");
        // SAFETY: same pointer, freed exactly once.
        unsafe { util::free_c_string(pointer) };
    }

    #[test]
    fn media_get_metadata_rejects_a_null_out_parameter() {
        // Every string slot and the duration slot are mandatory (the position
        // slot is the only optional one). Validation runs before any backend
        // call, so this needs no player.
        //
        // The slots start at sentinels rather than NULL/0: a rejected call must
        // write nothing, and the documented empty answer is itself NULL/0, so a
        // zeroed slot could not distinguish "untouched" from "overwritten".
        // The sentinel pointer is only ever compared by address, never
        // dereferenced, so it does not have to point at valid memory.
        let sentinel: *mut c_char = std::ptr::NonNull::<c_char>::dangling().as_ptr();
        let mut title = sentinel;
        let mut artist = sentinel;
        let mut album = sentinel;
        let mut duration: u64 = 99;

        // SAFETY: the pointers are live locals; `out_title` is deliberately
        // null, which is exactly the case under test.
        let status = unsafe {
            uda_media_get_metadata(
                std::ptr::null_mut(),
                &mut artist,
                &mut album,
                &mut duration,
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());

        // SAFETY: `out_duration_ms` is deliberately null this time.
        let status = unsafe {
            uda_media_get_metadata(
                &mut title,
                &mut artist,
                &mut album,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);

        // A rejected call writes nothing into the slots it was given: had the
        // null check fallen through into the body, these would be NULL/0 (the
        // "no player" answer) instead of the sentinels.
        assert_eq!(title, sentinel);
        assert_eq!(artist, sentinel);
        assert_eq!(album, sentinel);
        assert_eq!(duration, 99);
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
        if status == UDA_ERR_NOT_SUPPORTED {
            // A host with no appearance backend (e.g. macOS, where this crate
            // ships no backend) answers -2 before touching the slot; the same
            // early-out the wake-lock round trip uses.
            return;
        }
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

    #[test]
    fn tray_capabilities_reject_a_null_out_parameter() {
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe { uda_tray_capabilities(std::ptr::null_mut()) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());
    }

    #[test]
    fn tray_capabilities_report_only_documented_bits() {
        let mut bits: u32 = 0;
        // SAFETY: `bits` is a live, writable `uint32_t` on this stack frame.
        let status = unsafe { uda_tray_capabilities(&mut bits) };
        assert_eq!(status, UDA_OK);

        // The C ABI has no click-callback registration, so those bits are
        // deliberately cleared even when the platform backend can measure
        // clicks.
        const UNDELIVERABLE_BY_C_ABI: u32 = UDA_TRAY_CAP_CLICK | UDA_TRAY_CAP_DOUBLE_CLICK;
        assert_eq!(
            bits & UNDELIVERABLE_BY_C_ABI,
            0,
            "the C ABI delivers activation only through menu rows: {bits:#x}"
        );

        // Every other bit must forward the backend's answer verbatim.
        assert_eq!(bits, tray::capabilities().bits() & !UNDELIVERABLE_BY_C_ABI);
        assert_eq!(
            bits & !abi::DOCUMENTED_TRAY_CAPABILITIES,
            0,
            "undocumented tray capability bits: {bits:#x}"
        );
        if !cfg!(any(target_os = "linux", target_os = "windows")) {
            assert_eq!(bits, 0, "a target with no tray backend must report 0");
        }
    }

    #[test]
    fn tray_support_level_rejects_a_null_out_parameter() {
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe { uda_tray_support_level(1, std::ptr::null_mut()) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());
    }

    #[test]
    fn tray_support_reason_rejects_a_null_out_parameter() {
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe { uda_tray_support_reason(1, std::ptr::null_mut()) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);
        assert!(util::take_last_message().is_some());
    }

    /// The exports the C ABI presents as plain `extern "C"` (no `unsafe`).
    /// Every argument they take is a value - a numeric handle, a status code -
    /// that the library handles as data: a garbage value produces an error,
    /// never undefined behaviour, so a Rust caller may call them without an
    /// `unsafe` block.
    ///
    /// The list pins the classification rather than deriving it: the media
    /// command and session control exports also take only values, yet they are
    /// `unsafe`, as they have been since v0.2.0. Conversely every export that
    /// takes a raw pointer is `unsafe`, because a dangling non-null pointer is
    /// UB that a safe signature cannot accept. Reclassifying an export either
    /// way is a compatibility decision: update this list and the changelog
    /// together.
    const PLAIN_EXTERN_C_EXPORTS: &[&str] = &[
        "uda_last_error_message",
        "uda_status_message",
        "uda_tray_destroy",
        "uda_tray_menu_add_separator",
        "uda_tray_menu_destroy",
        "uda_tray_set_menu",
        "uda_tray_set_visible",
        "uda_wakelock_release",
    ];

    #[test]
    fn export_safety_classification_is_pinned() {
        // The safe/unsafe split of the C ABI surface is a compatibility
        // contract: Rust hosts that link the dylib see it, and a silent
        // reclassification (an export drifting between `extern "C"` and
        // `unsafe extern "C"`) is exactly the kind of surprise change a
        // downstream build cannot absorb. This pins the split; reclassifying an
        // export is a deliberate decision that must update this list and the
        // changelog together.
        let source = include_str!("lib.rs");
        let mut plain: Vec<&str> = Vec::new();
        let mut unsafe_exports: Vec<&str> = Vec::new();
        for line in source.lines() {
            let line = line.trim_start();
            let (bucket, signature) =
                if let Some(rest) = line.strip_prefix("pub unsafe extern \"C\" fn ") {
                    (&mut unsafe_exports, rest)
                } else if let Some(rest) = line.strip_prefix("pub extern \"C\" fn ") {
                    (&mut plain, rest)
                } else {
                    continue;
                };
            // rustfmt keeps `fn NAME(` on one line even when it wraps the
            // parameter list, so the name ends at the first `(`.
            let end = signature.find('(').unwrap_or(signature.len());
            bucket.push(&signature[..end]);
        }

        plain.sort_unstable();
        let mut expected = PLAIN_EXTERN_C_EXPORTS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            plain, expected,
            "the plain export set drifted; update PLAIN_EXTERN_C_EXPORTS and the changelog together"
        );

        unsafe_exports.sort_unstable();
        // No export may appear in both sets, and every export not listed as
        // plain must be `unsafe`: a pointer-taking export classified safe would
        // let a Rust caller pass a dangling pointer without any unsafe block,
        // which is unsound.
        assert!(unsafe_exports.iter().all(|name| !plain.contains(name)));
        assert_eq!(
            unsafe_exports.len() + plain.len(),
            35,
            "the export count changed; the classification list must follow"
        );
    }

    #[test]
    fn last_error_message_returns_a_caller_owned_allocation() {
        let _ = util::take_last_message();
        // SAFETY: passing null is exactly the case under test.
        let status = unsafe { uda_detect_theme(std::ptr::null_mut()) };
        assert_eq!(status, UDA_ERR_INVALID_ARGUMENT);

        let pointer = uda_last_error_message();
        assert!(
            !pointer.is_null(),
            "a failure must leave a readable message"
        );

        // SAFETY: the pointer is a live library allocation owned by this test
        // until it is released below.
        let text = unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_str()
            .expect("the message is valid UTF-8");
        assert!(text.contains("out_theme"), "got: {text}");

        // Reading consumed the message: an immediate second call with no new
        // failure reports "nothing recorded".
        assert!(
            uda_last_error_message().is_null(),
            "the message slot must be empty after a successful read"
        );

        // Ownership moved to the caller: released through the export that frees
        // every other returned string.
        // SAFETY: the pointer came from `uda_last_error_message` above and has
        // not been released yet.
        unsafe { uda_free_string(pointer) };
    }
}
