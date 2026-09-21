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

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

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

#ifdef __cplusplus
}
#endif

#endif /* UDA_H_ */
