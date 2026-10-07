/*
 * uda.h - UniDesktop API (UDA) C-ABI header.
 *
 * This header describes the stable C interface exported by `crates/uda-ffi`:
 *   - Linux   -> libuda_ffi.so
 *   - Windows -> uda_ffi.dll
 *
 * Every function returns an `int32_t` status code (`0` = success, negative =
 * failure) except `uda_free_string`, which returns nothing. Human-readable
 * diagnostics for the most recent failure are available from
 * `uda_last_error_message()`.
 *
 * Memory ownership
 * ----------------
 *   - Strings RETURNED by UDA are allocated by the library and must be released
 *     with `uda_free_string()`. Passing null to `uda_free_string()` is a no-op,
 *     so callers may free unconditionally. The two exceptions are
 *     `uda_last_error_message()` (a borrow of library-owned thread-local
 *     storage) and `uda_status_message()` (a static string): both must NOT be
 *     freed.
 *   - Strings PASSED IN are borrowed for the duration of the call only; the
 *     caller keeps ownership and must keep them alive until the call returns.
 *   - Wake-lock handles are plain `uint64_t` values owned by this process.
 *     Release each one exactly once with `uda_wakelock_release()`.
 *
 * Thread safety
 * -------------
 * All functions are free of global state apart from the wake-lock registry
 * (internally synchronised) and the thread-local last-error slot, so they may
 * be called concurrently from several threads. The last-error message is
 * per-thread: one thread's failure never overwrites another's diagnosis.
 *
 * ABI stability
 * -------------
 * The exported symbol set is append-only. New capabilities are added as new
 * functions; existing signatures, constants and status codes do not change.
 */

#ifndef UDA_H_
#define UDA_H_

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * @file uda.h
 *
 * C-ABI surface of the UniDesktop API.
 *
 * @section ownership Ownership in one paragraph
 *
 * String handles returned by this library are allocated by Rust and must be
 * released with uda_free_string(); the exceptions (uda_last_error_message()
 * and uda_status_message()) are documented on their own declarations. Every
 * string passed *into* a call is borrowed for the duration of that call only:
 * the caller keeps ownership and may free it immediately afterwards. Tray
 * icons and menus are opaque handles into process-local tables; the caller
 * owns the *handle* and must destroy it exactly once with uda_tray_destroy() /
 * uda_tray_menu_destroy(). Callback pointers registered on a menu row are
 * retained by UDA for the lifetime of that row, and the menu (not the
 * callback) is the unit of destruction.
 */

/* ------------------------------------------------------------------------- */
/* Status codes                                                              */
/* ------------------------------------------------------------------------- */

/** The call succeeded. */
#define UDA_OK 0
/** Null pointer, invalid UTF-8, or an out-of-range enum code. */
#define UDA_ERR_INVALID_ARGUMENT (-1)
/** The current platform or session cannot provide the feature. */
#define UDA_ERR_NOT_SUPPORTED (-2)
/** Detecting the environment or OS release failed. */
#define UDA_ERR_DETECTION_FAILED (-3)
/** A filesystem or process-spawn error occurred. */
#define UDA_ERR_IO (-4)
/** An unexpected internal failure occurred. */
#define UDA_ERR_INTERNAL (-5)
/** A panic was contained at the FFI boundary (should never be observed). */
#define UDA_ERR_PANIC (-6)

/* ------------------------------------------------------------------------- */
/* Theme codes (uda_detect_theme)                                            */
/* ------------------------------------------------------------------------- */

/** The theme could not be determined. */
#define UDA_THEME_UNKNOWN 0
/** Dark appearance. */
#define UDA_THEME_DARK 1
/** Light appearance. */
#define UDA_THEME_LIGHT 2

/* ------------------------------------------------------------------------- */
/* Fill modes (uda_set_wallpaper)                                            */
/* ------------------------------------------------------------------------- */

/** Crop to fill while preserving the aspect ratio. */
#define UDA_FILL_CROP 0
/** Fill the screen, ignoring the aspect ratio. */
#define UDA_FILL_FILL 1
/** Fit inside the screen while preserving the aspect ratio. */
#define UDA_FILL_FIT 2
/** Stretch to fill the screen. */
#define UDA_FILL_STRETCH 3

/* ------------------------------------------------------------------------- */
/* Wake-lock types (uda_wakelock_acquire)                                    */
/* ------------------------------------------------------------------------- */

/** Prevent the display from sleeping. */
#define UDA_WAKELOCK_DISPLAY 0
/** Prevent the system from idling or suspending. */
#define UDA_WAKELOCK_SYSTEM 1

/** Media playback is progressing. */
#define UDA_MEDIA_PLAYING 0
/** A track is loaded and halted. */
#define UDA_MEDIA_PAUSED 1
/** Nothing is loaded, or playback reached the end. */
#define UDA_MEDIA_STOPPED 2
/**
 * The state could not be determined. This is ALSO the code for "no media player
 * is running", which is not an error: uda_media_get_status() reports it with a
 * UDA_OK status. Never render it as a paused track.
 */
#define UDA_MEDIA_UNKNOWN 3

/** Start or resume playback. */
#define UDA_MEDIA_CMD_PLAY 0
/** Halt playback, keeping the position. */
#define UDA_MEDIA_CMD_PAUSE 1
/** Switch between playing and paused. */
#define UDA_MEDIA_CMD_TOGGLE 2
/** Advance to the next track. */
#define UDA_MEDIA_CMD_NEXT 3
/** Return to the previous track. */
#define UDA_MEDIA_CMD_PREVIOUS 4
/** Stop playback and unload the track. */
#define UDA_MEDIA_CMD_STOP 5

/**
 * A session backend exists on this platform.
 *
 * This bit says nothing by itself: it only means at least one of the six actions
 * below is reachable. Test the action's own bit before offering it.
 */
#define UDA_SESSION_CAP_MANAGEMENT 0x00010000u
/** The session can be locked - the only action safe to automate. */
#define UDA_SESSION_CAP_LOCK 0x00020000u
/** The calling user's session can be ended. */
#define UDA_SESSION_CAP_LOGOUT 0x00040000u
/** The machine can be suspended to RAM. */
#define UDA_SESSION_CAP_SUSPEND 0x00080000u
/** The machine can be hibernated to disk. */
#define UDA_SESSION_CAP_HIBERNATE 0x00100000u
/** The machine can be rebooted. */
#define UDA_SESSION_CAP_REBOOT 0x00200000u
/** The machine can be powered off. */
#define UDA_SESSION_CAP_SHUTDOWN 0x00400000u

/**
 * A tray backend exists on this platform.
 *
 * This bit says nothing by itself: test the feature bits below before relying
 * on a specific tray behaviour.
 */
#define UDA_TRAY_CAP_SYSTEM_TRAY 0x00000080u
/** The tray icon can be shown, hidden, and swapped at runtime. */
#define UDA_TRAY_CAP_ICON 0x00000100u
/** The tray exposes hover text. */
#define UDA_TRAY_CAP_TOOLTIP 0x00000200u
/** The tray reports a single primary click. */
#define UDA_TRAY_CAP_CLICK 0x00000400u
/**
 * The tray reports a native double click. Never set on Linux (SNI has no
 * double-click signal); a host there synthesises it from two clicks.
 */
#define UDA_TRAY_CAP_DOUBLE_CLICK 0x00000800u
/** The tray exposes a context menu. */
#define UDA_TRAY_CAP_CONTEXT_MENU 0x00001000u
/** Menu rows can render a checkbox state. */
#define UDA_TRAY_CAP_CHECKBOX 0x00002000u
/** Menu rows can be added, removed, or relabelled at runtime. */
#define UDA_TRAY_CAP_DYNAMIC_MENU 0x00004000u

/* ------------------------------------------------------------------------- */
/* Functions                                                                 */
/* ------------------------------------------------------------------------- */

/**
 * Detect the current system colour scheme.
 *
 * @param out_theme  Receives UDA_THEME_UNKNOWN, UDA_THEME_DARK or
 *                   UDA_THEME_LIGHT. Must not be null.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_detect_theme(int32_t *out_theme);

/**
 * Set the desktop wallpaper.
 *
 * Accepts both a plain filesystem path and a `file://` URI. An empty (or
 * whitespace-only) path is rejected with UDA_ERR_INVALID_ARGUMENT before any
 * backend is consulted, on every platform.
 *
 * @param path       Null-terminated UTF-8 filesystem path. Must not be null.
 * @param fill_mode  One of the UDA_FILL_* codes.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_set_wallpaper(const char *path, int32_t fill_mode);

/**
 * Read the current wallpaper path.
 *
 * On success *out_path receives a heap C string the caller must release with
 * `uda_free_string()`. When no wallpaper is configured (or the platform cannot
 * report one), *out_path is set to NULL while the call still returns UDA_OK, so
 * check the pointer rather than the status to detect "no wallpaper". An empty
 * value is never returned as a zero-length string; it is reported as NULL too.
 *
 * @param out_path  Receives the newly allocated string, or NULL. Must not be
 *                  null itself.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_get_wallpaper(char **out_path);

/**
 * Release a string previously returned by this library.
 *
 * Passing NULL is a no-op, so callers may free unconditionally.
 *
 * @param s  A pointer obtained from `uda_get_wallpaper()`, or NULL.
 */
void uda_free_string(char *s);

/**
 * Acquire a wake lock.
 *
 * A lock acquired through the CLI fallback (no native IPC available) is bounded
 * to roughly one hour: past that deadline the lock is retired and its handle
 * stops being live, so a host that needs a longer lock must re-acquire it.
 *
 * @param lock_type   UDA_WAKELOCK_DISPLAY or UDA_WAKELOCK_SYSTEM.
 * @param reason      Null-terminated UTF-8 description used for diagnostics.
 *                    Must not be null.
 * @param out_handle  Receives a non-zero handle to pass to
 *                    `uda_wakelock_release()`. Must not be null.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_wakelock_acquire(int32_t lock_type, const char *reason, uint64_t *out_handle);

/**
 * Release a wake lock obtained from `uda_wakelock_acquire()`.
 *
 * Returns UDA_ERR_INVALID_ARGUMENT when the handle is not a live lock in this
 * process (already released, never issued here, or a fallback lock retired
 * after its bounded lifetime elapsed).
 *
 * @param handle  A non-zero handle from `uda_wakelock_acquire()`.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_wakelock_release(uint64_t handle);

/* ------------------------------------------------------------------------- */
/* Notifications                                                            */
/* ------------------------------------------------------------------------- */

/**
 * Send a system notification.
 *
 * The five strings cover what a notification needs: the sending app's
 * `app_name`, a one-line `title`, a multi-line `body`, an optional `icon`
 * (path or URI; empty means none), and `actions` as a flat newline-separated
 * list of `key\nlabel` records. Any string may be null, which is treated as the
 * empty string.
 *
 * `app_name` is not cosmetic: on Windows it is the AppUserModelID the toast is
 * addressed to, and an unpackaged process has none. UDA registers it as the
 * process's explicit AUMID before the first toast is shown, which is what lets
 * a plain `node script.js` display a native toast. Passing null or "" selects
 * the generic "UniDesktop.Notification" identity.
 *
 * The remaining FreeDesktop fields keep their defaults: `replaces_id` is 0 (a
 * new notification), the expiry is the server default, and the urgency is
 * normal. Callers needing those must use the Rust API.
 *
 * A trailing `key` without its `label` is dropped rather than rendered as a
 * blank button.
 *
 * On Windows, toast *buttons* still require a packaged (MSIX) identity, so
 * `actions` is accepted for parity but not surfaced; the toast itself displays
 * normally. Use `WindowsNotificationManager::availability()` to probe further.
 *
 * @param app_name  Sending application name; the Windows toast identity.
 *                  Null or "" selects "UniDesktop.Notification".
 * @param title     One-line summary. Null is treated as "".
 * @param body      Multi-line detail. Null is treated as "".
 * @param icon      Path or URI for the notification image, or null.
 * @param actions   Flat "key\nlabel" records separated by '\n', or null.
 * @param out_id    Receives the id the notification server assigned. Must not
 *                  be null. Left untouched on failure.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_notify(const char *app_name,
                   const char *title,
                   const char *body,
                   const char *icon,
                   const char *actions,
                   uint32_t *out_id);

/**
 * Read the system accent colour as four channels.
 *
 * Writes R, G, B, A (each 0..=255) to the four bytes at `out_rgba`. A platform
 * that exposes no accent colour - most Linux desktops - leaves the bytes
 * untouched and still returns UDA_OK, so a zeroed slot means "no accent",
 * not failure.
 *
 * @param out_rgba  Pointer to four writable `uint8_t` values. Must not be null.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_get_accent_color(uint8_t *out_rgba);

/**
 * Read the now-playing metadata of the active media player.
 *
 * On Linux the backend scans the session bus for an `org.mpris.MediaPlayer2.*`
 * service; on Windows it asks the Global System Media Transport Controls session
 * manager. Both answer `Ok(None)` when no player is running, which this function
 * turns into: `*out_title`, `*out_artist` and `*out_album` set to NULL, and
 * `*out_duration_ms` / `*out_position_ms` set to 0 - with a UDA_OK status. A
 * now-playing card therefore renders as empty rather than as a failure.
 *
 * The three strings are allocated by the library and must each be released with
 * uda_free_string(). Freeing NULL is a no-op, so callers may free
 * unconditionally. A field the player does not publish (a radio stream with no
 * album, say) is NULL rather than an empty string - as is a field the player
 * publishes as the empty string, so a binding's `if (ptr)` check always sees
 * "no value" the same way.
 *
 * @param out_title         Receives the track title, or NULL. Must not be null.
 * @param out_artist        Receives the artist(s), already joined with ", " when
 *                          the player publishes several, or NULL. Must not be
 *                          null.
 * @param out_album         Receives the album name, or NULL. Must not be null.
 * @param out_duration_ms   Receives the track length in milliseconds, or 0 when
 *                          unknown (a live stream). Must not be null.
 * @param out_position_ms   Optional; pass NULL to skip. Receives the playback
 *                          position in milliseconds, or 0 when the backend
 *                          cannot report it.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_media_get_metadata(char **out_title,
                               char **out_artist,
                               char **out_album,
                               uint64_t *out_duration_ms,
                               uint64_t *out_position_ms);

/**
 * Read the playback status of the active media player.
 *
 * Writes UDA_MEDIA_PLAYING, UDA_MEDIA_PAUSED, UDA_MEDIA_STOPPED or
 * UDA_MEDIA_UNKNOWN to `*out_status`. UDA_MEDIA_UNKNOWN covers both "no player is
 * running" and "the state could not be determined", and is reported with UDA_OK:
 * it is an answer, not a failure. A negative status code means the platform has
 * no media backend at all.
 *
 * @param out_status  Receives one of the UDA_MEDIA_* status codes. Must not be
 *                    null.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_media_get_status(int32_t *out_status);

/**
 * Send a transport command to the active media player.
 *
 * `command` is one of UDA_MEDIA_CMD_PLAY, UDA_MEDIA_CMD_PAUSE,
 * UDA_MEDIA_CMD_TOGGLE, UDA_MEDIA_CMD_NEXT, UDA_MEDIA_CMD_PREVIOUS or
 * UDA_MEDIA_CMD_STOP. An unrecognised code returns UDA_ERR_INVALID_ARGUMENT and
 * nothing is sent.
 *
 * A player that refuses the command (an app that disables "next track") and a
 * machine with no player running both report UDA_ERR_NOT_SUPPORTED, so a caller
 * can tell "not delivered" from "delivered" without inspecting the player.
 *
 * @param command  One of the UDA_MEDIA_CMD_* codes.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_media_send_command(int32_t command);

/**
 * Report which session and power actions this platform can perform.
 *
 * Writes a bitmask made of the UDA_SESSION_CAP_* flags to `*out_capabilities`;
 * 0 means "no session backend exists on this target".
 *
 * The query is static and side-effect-free - it never touches the machine's
 * power state - so a host may call it freely to decide which menu entries to
 * draw, and *must* call it before drawing one that could shut the machine down.
 *
 * A set bit means "the code path exists", not "the account is allowed": a
 * machine with hibernation switched off still reports UDA_SESSION_CAP_HIBERNATE,
 * and the attempt then fails with UDA_ERR_NOT_SUPPORTED. Likewise, Windows
 * reboot and shutdown need the SeShutdownPrivilege, which is a runtime answer.
 *
 * @param out_capabilities  Receives the bitmask. Must not be null.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_session_capabilities(uint32_t *out_capabilities);

/**
 * Lock the session.
 *
 * Linux: `org.freedesktop.ScreenSaver.Lock()` on the session bus, falling back to
 * `loginctl lock-session`. Windows: `LockWorkStation()`.
 *
 * This is the only session action that is safe to automate: it is reversible
 * (the user unlocks with their password) and it destroys nothing. The other five
 * `uda_session_*` actions below must be gated behind an explicit user
 * confirmation.
 *
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_session_lock(void);

/**
 * End the calling user's session.
 *
 * Linux: `org.freedesktop.login1.Manager.TerminateSession("")` on the system
 * bus, falling back to the desktop's own session manager (GNOME, KDE, XFCE).
 * Windows: `ExitWindowsEx(EWX_LOGOFF, 0)`.
 *
 * WARNING: this logs the user out. Unsaved work in applications that do not
 * refuse is lost. Never call it without an explicit user confirmation.
 *
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_session_logout(void);

/**
 * Suspend the machine to RAM.
 *
 * Linux: `org.freedesktop.login1.Manager.Suspend(false)`. Windows:
 * `SetSuspendState(false, ...)`.
 *
 * WARNING: this changes the machine's power state. Never call it without an
 * explicit user confirmation.
 *
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_session_suspend(void);

/**
 * Hibernate the machine to disk.
 *
 * Linux: `org.freedesktop.login1.Manager.Hibernate(false)`. Windows:
 * `SetSuspendState(true, ...)`, which the platform rejects with
 * ERROR_FILE_NOT_FOUND when hibernation is disabled - reported as
 * UDA_ERR_NOT_SUPPORTED.
 *
 * WARNING: this changes the machine's power state. Never call it without an
 * explicit user confirmation.
 *
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_session_hibernate(void);

/**
 * Restart the machine.
 *
 * Linux: `org.freedesktop.login1.Manager.Reboot(false)`. Windows:
 * `ExitWindowsEx(EWX_REBOOT | EWX_FORCEIFHUNG, 0)` after enabling
 * SeShutdownPrivilege, which needs an elevated process or an administrator
 * account; without it the call fails with UDA_ERR_NOT_SUPPORTED rather than
 * half-rebooting.
 *
 * WARNING: this restarts the machine and unsaved work is lost. Never call it
 * without an explicit user confirmation.
 *
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_session_reboot(void);

/**
 * Power the machine off.
 *
 * Linux: `org.freedesktop.login1.Manager.PowerOff(false)`. Windows:
 * `ExitWindowsEx(EWX_POWEROFF | EWX_FORCEIFHUNG, 0)` after enabling
 * SeShutdownPrivilege, with the same elevation requirement as
 * uda_session_reboot().
 *
 * WARNING: this shuts the machine down and unsaved work is lost. Never call it
 * without an explicit user confirmation.
 *
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_session_shutdown(void);

/**
 * Return the message describing the most recent failure on the calling thread.
 *
 * The returned pointer borrows library-owned storage: it stays valid until the
 * next UDA call on the same thread replaces the message, and it must NOT be
 * passed to `uda_free_string()` (freeing it is undefined behaviour). Copy the
 * text if it must outlive that. Returns NULL when no failure has been recorded
 * on this thread yet.
 *
 * @return A borrowed, null-terminated C string owned by the library, or NULL.
 */
const char *uda_last_error_message(void);

/**
 * Describe a status code with a static string.
 *
 * The returned pointer is valid for the lifetime of the library and must not be
 * freed. Useful for rendering a failure without a second FFI round-trip.
 *
 * @param status  A status code returned by any UDA function.
 * @return A static, null-terminated description.
 */
const char *uda_status_message(int32_t status);

/* ------------------------------------------------------------------------- */
/* System tray                                                               */
/* ------------------------------------------------------------------------- */

/*
 * Tray icons and menus are process-local resources identified by opaque
 * uint64_t handles. They are NOT references, NOT pointers, and are not valid in
 * another process: each handle indexes a table owned by the shared library.
 *
 * HANDLE LIFETIME
 *
 *   - A handle is single-use. Destroying it removes the table entry, so a
 *     successful destroy followed by another call with the same value is
 *     reported as UDA_ERR_INVALID_ARGUMENT rather than acting on a stale
 *     resource.
 *   - Handle 0 is never a live resource. A zeroed out-parameter therefore
 *     unambiguously means "the call failed".
 *   - A menu handle stays valid after uda_tray_set_menu(): the icon holds its
 *     own reference, so the menu keeps working whether or not the handle is
 *     destroyed later. Destroying the menu handle (uda_tray_menu_destroy())
 *     detaches the menu from every icon still showing it, so its callbacks
 *     stop firing immediately; the icons themselves stay alive.
 *
 * CALLBACK THREADING MODEL (read before writing a handler)
 *
 * Every menu callback is invoked on the tray worker thread that the platform
 * backend owns. It is NOT the thread that called uda_tray_menu_add_text() and
 * NOT your UI thread. The callback therefore must:
 *
 *   - return as soon as possible (a slow handler stalls every later menu
 *     interaction);
 *   - not block, sleep, or wait on a lock the main thread may hold;
 *   - not touch GUI toolkit state directly - post an event into the host's own
 *     loop instead;
 *   - be prepared to fire after the row was removed, if the shell was already
 *     mid-dispatch.
 *
 * `user_data` is handed back to the callback verbatim. UDA never dereferences
 * it; keeping it alive is the host's responsibility.
 *
 * The callback may be NULL, which yields a silent row that still renders and
 * still reports its state in later calls.
 */

/**
 * Create a tray icon.
 *
 * @param name        Application name used for registration (D-Bus bus name on
 *                    Linux, window class on Windows). An empty string selects
 *                    this library's default name. Must be null-terminated
 *                    UTF-8, or NULL for the default.
 * @param tooltip     Hover text; may be NULL or empty. Text longer than 127
 *                    characters is clamped (the call still succeeds).
 * @param out_handle  Receives a non-zero handle on success. Must not be null.
 *                    Left untouched on failure.
 * @return UDA_OK on success, otherwise a negative status code.
 *
 * @note The icon has no image until uda_tray_set_icon_path() or
 *       uda_tray_set_icon_rgba() supplies one, so it can be prepared and only
 *       made visible once built.
 */
int32_t uda_tray_create(const char *name, const char *tooltip, uint64_t *out_handle);

/**
 * Replace a tray icon's tooltip.
 *
 * @param handle   A handle from uda_tray_create().
 * @param tooltip  Null-terminated UTF-8 text, or NULL to clear it.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_set_tooltip(uint64_t handle, const char *tooltip);

/**
 * Replace a tray icon's image from a file or icon-theme name.
 *
 * @param handle  A handle from uda_tray_create().
 * @param path    Null-terminated UTF-8 path. Linux also accepts a freedesktop
 *                icon-theme name here; Windows requires a file path.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_set_icon_path(uint64_t handle, const char *path);

/**
 * Replace a tray icon's image from raw pixels.
 *
 * The buffer is borrowed: only `stride * height` bytes are copied and the
 * caller keeps ownership of `data`. Pixels are top-down RGBA, four bytes each.
 * A buffer shorter than `stride * height` is rejected before any pixel is read.
 *
 * @param handle  A handle from uda_tray_create().
 * @param width   Pixel width, non-zero.
 * @param height  Pixel height, non-zero.
 * @param stride  Bytes per row; must be at least `width * 4`.
 * @param data    Pointer to the pixels, or NULL when `len` is zero.
 * @param len     Readable byte count at `data`.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_set_icon_rgba(uint64_t handle,
                               uint32_t width,
                               uint32_t height,
                               uint32_t stride,
                               const uint8_t *data,
                               size_t len);

/**
 * Show or hide the icon without unregistering it.
 *
 * @param handle   A handle from uda_tray_create().
 * @param visible  0 hides, any other value shows.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_set_visible(uint64_t handle, int32_t visible);

/**
 * Destroy a tray icon and unregister it from the shell.
 *
 * Terminal: the handle cannot be reused afterwards. Menus previously attached
 * keep their own handles and stay valid.
 *
 * @param handle  A handle from uda_tray_create().
 * @return UDA_OK on success, UDA_ERR_INVALID_ARGUMENT when the handle is not a
 *         live tray icon in this process.
 */
int32_t uda_tray_destroy(uint64_t handle);

/**
 * Create an empty context menu.
 *
 * @param out_menu_handle  Receives a non-zero handle on success. Must not be
 *                         null. Left untouched on failure.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_menu_create(uint64_t *out_menu_handle);

/**
 * Callback type for a plain text menu row.
 *
 * @param item_id   The row's id, as returned by uda_tray_menu_add_text().
 * @param user_data The pointer registered alongside this callback.
 */
typedef void (*UdaTrayTextCallback)(uint64_t item_id, void *user_data);

/**
 * Callback type for a checkbox menu row.
 *
 * @param item_id   The row's id, as returned by uda_tray_menu_add_checkbox().
 * @param checked   The NEW state after the toggle: 0 or 1. The row's own stored
 *                  value has already been updated, so this is what the shell
 *                  renders next.
 * @param user_data The pointer registered alongside this callback.
 */
typedef void (*UdaTrayCheckboxCallback)(uint64_t item_id, int32_t checked, void *user_data);

/**
 * Append a plain text row to a menu.
 *
 * @param menu_handle  A handle from uda_tray_menu_create().
 * @param label        Row text; a blank label is rejected with
 *                     UDA_ERR_INVALID_ARGUMENT because it would render
 *                     invisibly.
 * @param callback     Invoked on the tray worker thread when the row is
 *                     activated, or NULL for a silent row.
 * @param user_data    Handed back to `callback` untouched.
 * @param out_item_id  Receives the row's stable, non-zero id on success. The
 *                     callback receives the same value. Must not be null.
 * @return UDA_OK on success; UDA_ERR_INVALID_ARGUMENT for a blank label or a
 *         handle that is not a live menu; otherwise a negative status code.
 */
int32_t uda_tray_menu_add_text(uint64_t menu_handle,
                               const char *label,
                               UdaTrayTextCallback callback,
                               void *user_data,
                               uint64_t *out_item_id);

/**
 * Append a visual separator to a menu.
 *
 * A separator has no label, no callback and no id, so there is no out-parameter.
 *
 * @param menu_handle  A handle from uda_tray_menu_create().
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_menu_add_separator(uint64_t menu_handle);

/**
 * Append a checkbox row to a menu.
 *
 * When a callback is supplied, the row's stored value is inverted *before* it
 * runs, so the `checked` argument is the new state and the menu cannot drift
 * out of sync with the shell. A NULL callback installs no handler at all: the
 * row renders with its initial state and never toggles, which is what a host
 * that drives the checkbox through its own UI wants.
 *
 * @param menu_handle  A handle from uda_tray_menu_create().
 * @param label        Row text; a blank label is rejected with
 *                     UDA_ERR_INVALID_ARGUMENT.
 * @param checked      0 starts unchecked, any other value starts checked.
 * @param callback     Invoked on the tray worker thread when the row is toggled,
 *                     or NULL for a row that never toggles.
 * @param user_data    Handed back to `callback` untouched.
 * @param out_item_id  Receives the row's stable, non-zero id on success. Must
 *                     not be null.
 * @return UDA_OK on success; UDA_ERR_INVALID_ARGUMENT for a blank label or a
 *         handle that is not a live menu; otherwise a negative status code.
 */
int32_t uda_tray_menu_add_checkbox(uint64_t menu_handle,
                                   const char *label,
                                   int32_t checked,
                                   UdaTrayCheckboxCallback callback,
                                   void *user_data,
                                   uint64_t *out_item_id);

/**
 * Attach a menu to a tray icon, replacing any menu set earlier.
 *
 * The menu handle stays valid after this call: the icon holds its own
 * reference, so the menu keeps working whether or not the handle is destroyed
 * later. Destroying the handle (see uda_tray_menu_destroy()) detaches the menu
 * from every icon still showing it.
 *
 * @param tray_handle  A handle from uda_tray_create().
 * @param menu_handle  A handle from uda_tray_menu_create().
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_set_menu(uint64_t tray_handle, uint64_t menu_handle);

/**
 * Destroy a menu handle.
 *
 * The menu is detached from EVERY tray icon that still shows it, so its rows
 * and callbacks stop firing immediately; the icons themselves stay alive and
 * simply have no menu afterwards. This makes destroying a menu handle safe even
 * when the host has already attached it and torn down its callback trampolines.
 * Terminal: the handle cannot be reused afterwards.
 *
 * @param menu_handle  A handle from uda_tray_menu_create().
 * @return UDA_OK on success, UDA_ERR_INVALID_ARGUMENT when the handle is not a
 *         live menu in this process.
 */
int32_t uda_tray_menu_destroy(uint64_t menu_handle);

/**
 * Report which tray features the active platform backend advertises.
 *
 * Writes a bitmask made of the UDA_TRAY_CAP_* flags to `*out_capabilities`;
 * 0 means "no tray backend exists on this target", and a feature the backend
 * cannot deliver has its bit cleared. The query is static and side-effect-free
 * - it never registers anything with the shell - so a host may call it freely
 * to decide whether to build tray UI at all, and what to degrade gracefully.
 *
 * @param out_capabilities  Receives the bitmask. Must not be null.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_capabilities(uint32_t *out_capabilities);

#ifdef __cplusplus
}
#endif

#endif /* UDA_H_ */
