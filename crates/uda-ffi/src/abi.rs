//! The C-ABI surface of this crate, in one place.
//!
//! Every constant and callback type that `include/uda.h` exposes is declared
//! here, so the Rust side stays the single source of truth and the header is a
//! generated artifact (see `cbindgen.toml` and `scripts/gen-header.sh`).
//! Declaring a constant here is all it takes to publish it in the header on
//! the next regeneration; nothing else needs updating.
//!
//! The doc comments on this module's items are carried verbatim into the
//! generated C header, so they are written for the C caller: ASCII prose, no
//! Rust intra-doc links, no markdown tables.

use std::os::raw::c_void;

// ---------------------------------------------------------------------------
// Status codes (returned by every exported function)
// ---------------------------------------------------------------------------

/// The call succeeded.
pub const UDA_OK: i32 = 0;
/// Null pointer, invalid UTF-8, or an out-of-range enum code.
pub const UDA_ERR_INVALID_ARGUMENT: i32 = -1;
/// The current platform or session cannot provide the feature.
pub const UDA_ERR_NOT_SUPPORTED: i32 = -2;
/// Detecting the environment or OS release failed.
pub const UDA_ERR_DETECTION_FAILED: i32 = -3;
/// A filesystem or process-spawn error occurred.
pub const UDA_ERR_IO: i32 = -4;
/// An unexpected internal failure occurred.
pub const UDA_ERR_INTERNAL: i32 = -5;
/// A panic was contained at the FFI boundary (should never be observed).
pub const UDA_ERR_PANIC: i32 = -6;

// ---------------------------------------------------------------------------
// Theme codes (uda_detect_theme)
// ---------------------------------------------------------------------------

/// The theme could not be determined.
pub const UDA_THEME_UNKNOWN: i32 = 0;
/// Dark appearance.
pub const UDA_THEME_DARK: i32 = 1;
/// Light appearance.
pub const UDA_THEME_LIGHT: i32 = 2;

// ---------------------------------------------------------------------------
// Fill modes (uda_set_wallpaper)
// ---------------------------------------------------------------------------

/// Crop to fill while preserving the aspect ratio.
pub const UDA_FILL_CROP: i32 = 0;
/// Fill the screen, ignoring the aspect ratio.
pub const UDA_FILL_FILL: i32 = 1;
/// Fit inside the screen while preserving the aspect ratio.
pub const UDA_FILL_FIT: i32 = 2;
/// Stretch to fill the screen.
pub const UDA_FILL_STRETCH: i32 = 3;

// ---------------------------------------------------------------------------
// Wake-lock types (uda_wakelock_acquire)
// ---------------------------------------------------------------------------

/// Prevent the display from sleeping.
pub const UDA_WAKELOCK_DISPLAY: i32 = 0;
/// Prevent the system from idling or suspending.
pub const UDA_WAKELOCK_SYSTEM: i32 = 1;

// ---------------------------------------------------------------------------
// Media playback status (uda_media_get_status)
// ---------------------------------------------------------------------------

/// Media playback is progressing.
pub const UDA_MEDIA_PLAYING: i32 = 0;
/// A track is loaded and halted.
pub const UDA_MEDIA_PAUSED: i32 = 1;
/// Nothing is loaded, or playback reached the end.
pub const UDA_MEDIA_STOPPED: i32 = 2;
/// The state could not be determined. This is ALSO the code for "no media
/// player is running", which is not an error: `uda_media_get_status()` reports
/// it with a UDA_OK status. Never render it as a paused track.
pub const UDA_MEDIA_UNKNOWN: i32 = 3;

// ---------------------------------------------------------------------------
// Media transport commands (uda_media_send_command)
// ---------------------------------------------------------------------------

/// Start or resume playback.
pub const UDA_MEDIA_CMD_PLAY: i32 = 0;
/// Halt playback, keeping the position.
pub const UDA_MEDIA_CMD_PAUSE: i32 = 1;
/// Switch between playing and paused.
pub const UDA_MEDIA_CMD_TOGGLE: i32 = 2;
/// Advance to the next track.
pub const UDA_MEDIA_CMD_NEXT: i32 = 3;
/// Return to the previous track.
pub const UDA_MEDIA_CMD_PREVIOUS: i32 = 4;
/// Stop playback and unload the track.
pub const UDA_MEDIA_CMD_STOP: i32 = 5;

// ---------------------------------------------------------------------------
// Session & power capability bitmask (uda_session_capabilities)
// ---------------------------------------------------------------------------

/// A session backend exists on this platform.
///
/// This bit says nothing by itself: it only means at least one of the six
/// actions below is reachable. Test the action's own bit before offering it.
pub const UDA_SESSION_CAP_MANAGEMENT: u32 = 1 << 16;
/// The session can be locked - the only action safe to automate.
pub const UDA_SESSION_CAP_LOCK: u32 = 1 << 17;
/// The calling user's session can be ended.
pub const UDA_SESSION_CAP_LOGOUT: u32 = 1 << 18;
/// The machine can be suspended to RAM.
pub const UDA_SESSION_CAP_SUSPEND: u32 = 1 << 19;
/// The machine can be hibernated to disk.
pub const UDA_SESSION_CAP_HIBERNATE: u32 = 1 << 20;
/// The machine can be rebooted.
pub const UDA_SESSION_CAP_REBOOT: u32 = 1 << 21;
/// The machine can be powered off.
pub const UDA_SESSION_CAP_SHUTDOWN: u32 = 1 << 22;

// ---------------------------------------------------------------------------
// Tray callbacks
// ---------------------------------------------------------------------------

/// Callback type for a plain text menu row, or NULL for a silent row.
///
/// `item_id` is the row's id, as returned by `uda_tray_menu_add_text()`;
/// `user_data` is the pointer registered alongside this callback.
pub type UdaTrayTextCallback = Option<extern "C" fn(u64, *mut c_void)>;

/// Callback type for a checkbox menu row, or NULL for a silent row.
///
/// `item_id` is the row's id, as returned by `uda_tray_menu_add_checkbox()`;
/// `checked` is the NEW state after the toggle (`0` or `1`): the row's own
/// stored value has already been updated, so this is what the shell renders
/// next; `user_data` is the pointer registered alongside this callback.
pub type UdaTrayCheckboxCallback = Option<extern "C" fn(u64, i32, *mut c_void)>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_match_the_documented_values() {
        assert_eq!(UDA_OK, 0);
        assert_eq!(UDA_ERR_INVALID_ARGUMENT, -1);
        assert_eq!(UDA_ERR_NOT_SUPPORTED, -2);
        assert_eq!(UDA_ERR_DETECTION_FAILED, -3);
        assert_eq!(UDA_ERR_IO, -4);
        assert_eq!(UDA_ERR_INTERNAL, -5);
        assert_eq!(UDA_ERR_PANIC, -6);
    }

    #[test]
    fn capability_bits_are_distinct_powers_of_two() {
        let bits = [
            UDA_SESSION_CAP_MANAGEMENT,
            UDA_SESSION_CAP_LOCK,
            UDA_SESSION_CAP_LOGOUT,
            UDA_SESSION_CAP_SUSPEND,
            UDA_SESSION_CAP_HIBERNATE,
            UDA_SESSION_CAP_REBOOT,
            UDA_SESSION_CAP_SHUTDOWN,
        ];
        for (index, bit) in bits.iter().enumerate() {
            assert_eq!(bit.count_ones(), 1, "capability bits must be powers of two");
            assert_eq!(
                *bit,
                1 << (16 + index),
                "bit positions are part of the C ABI"
            );
            for other in &bits[index + 1..] {
                assert_ne!(bit, other, "capability bits must be distinct");
            }
        }
    }
}
