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
 *     so callers may free unconditionally.
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
 * released with uda_free_string(). Every string passed *into* a call is
 * borrowed for the duration of that call only: the caller keeps ownership and
 * may free it immediately afterwards. Tray icons and menus are opaque handles
 * into process-local tables; the caller owns the *handle* and must destroy it
 * exactly once with uda_tray_destroy() / uda_tray_menu_destroy(). Callback
 * pointers registered on a menu row are retained by UDA for the lifetime of
 * that row, and the menu (not the callback) is the unit of destruction.
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
 * check the pointer rather than the status to detect "no wallpaper".
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
 * process (already released, or never issued here).
 *
 * @param handle  A non-zero handle from `uda_wakelock_acquire()`.
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_wakelock_release(uint64_t handle);

/**
 * Return the message describing the most recent failure on the calling thread.
 *
 * The returned string is owned by the library and stays valid until the next
 * UDA call on the same thread; copy it if it must outlive that. Returns NULL
 * when no failure has been recorded yet. Release the copy with
 * `uda_free_string()`.
 *
 * @return A newly allocated C string, or NULL.
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
 *     own reference, so destroying the menu afterwards leaves the tray working.
 *     Destroy the menu explicitly only when the icon will never need it again.
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
 *                     UDA_ERR_NOT_SUPPORTED because it would render invisibly.
 * @param callback     Invoked on the tray worker thread when the row is
 *                     activated, or NULL for a silent row.
 * @param user_data    Handed back to `callback` untouched.
 * @param out_item_id  Receives the row's stable, non-zero id on success. The
 *                     callback receives the same value. Must not be null.
 * @return UDA_OK on success, otherwise a negative status code.
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
 * The row's stored value is inverted *before* `callback` runs, so the `checked`
 * argument is the new state and the menu cannot drift out of sync with the shell.
 *
 * @param menu_handle  A handle from uda_tray_menu_create().
 * @param label        Row text; a blank label is rejected with
 *                     UDA_ERR_NOT_SUPPORTED.
 * @param checked      0 starts unchecked, any other value starts checked.
 * @param callback     Invoked on the tray worker thread when the row is toggled,
 *                     or NULL for a silent row.
 * @param user_data    Handed back to `callback` untouched.
 * @param out_item_id  Receives the row's stable, non-zero id on success. Must
 *                     not be null.
 * @return UDA_OK on success, otherwise a negative status code.
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
 * The menu handle stays valid after this call: the icon holds its own reference,
 * so destroying the menu afterwards is optional and does not clear the rows.
 *
 * @param tray_handle  A handle from uda_tray_create().
 * @param menu_handle  A handle from uda_tray_menu_create().
 * @return UDA_OK on success, otherwise a negative status code.
 */
int32_t uda_tray_set_menu(uint64_t tray_handle, uint64_t menu_handle);

/**
 * Destroy a menu handle.
 *
 * Safe to call after uda_tray_set_menu(), as documented there. Terminal: the
 * handle cannot be reused afterwards.
 *
 * @param menu_handle  A handle from uda_tray_menu_create().
 * @return UDA_OK on success, UDA_ERR_INVALID_ARGUMENT when the handle is not a
 *         live menu in this process.
 */
int32_t uda_tray_menu_destroy(uint64_t menu_handle);

#ifdef __cplusplus
}
#endif

#endif /* UDA_H_ */
