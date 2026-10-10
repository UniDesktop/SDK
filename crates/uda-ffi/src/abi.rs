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

    /// Pull the identifier after a marker out of one source/header line.
    fn identifier_after(line: &str, marker: &str) -> Option<String> {
        let start = line.find(marker)? + marker.len();
        let rest = &line[start..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            None
        } else {
            Some(name)
        }
    }

    /// The exported C-ABI names this crate's sources declare, gathered by
    /// scanning `src/*.rs` the way the safety-classification gate does.
    fn exported_names_from_sources() -> (Vec<String>, Vec<String>, Vec<String>) {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut functions = Vec::new();
        let mut constants = Vec::new();
        let mut callbacks = Vec::new();
        let mut pending_no_mangle = false;

        for entry in std::fs::read_dir(&src).expect("the crate's src directory") {
            let path = entry.expect("src entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("readable Rust source");
            for line in source.lines() {
                if line.trim() == "#[no_mangle]" {
                    pending_no_mangle = true;
                    continue;
                }
                if pending_no_mangle {
                    pending_no_mangle = false;
                    if let Some(name) = identifier_after(line, " fn ") {
                        functions.push(name);
                    }
                }
                if let Some(name) = identifier_after(line, "pub const UDA_") {
                    constants.push(format!("UDA_{name}"));
                }
                // Only abi.rs's `pub type` aliases are C callback typedefs;
                // other files define Rust-side type aliases (error.rs's
                // `UdaStatus`, say) that never reach the header.
                let is_abi = path.file_name().and_then(|n| n.to_str()) == Some("abi.rs");
                if is_abi {
                    if let Some(name) = identifier_after(line, "pub type ") {
                        callbacks.push(name);
                    }
                }
            }
        }
        (functions, constants, callbacks)
    }

    #[test]
    fn the_generated_header_declares_exactly_the_exported_symbols() {
        // `gen-header.sh --check` pins the header's *content* against a fresh
        // cbindgen run, but only inside CI; a plain `cargo test` must catch a
        // Rust-side export whose header counterpart went missing (or a hand
        // edit that invented one) without any script running. Sets compare in
        // both directions: adding an export without regenerating fails here,
        // and so does deleting one the header still declares.
        let header_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../include/uda.h");
        let header = std::fs::read_to_string(&header_path).expect(
            "include/uda.h sits at the repository root; run the tests from a full checkout",
        );

        let mut header_defines = Vec::new();
        let mut header_callbacks = Vec::new();
        // Function declarations can wrap across lines, so the function scan
        // runs over the header with every comment stripped (doc mentions like
        // `uda_free_string()` must not count as declarations) instead of over
        // individual lines.
        let mut code = String::new();
        let mut in_block_comment = false;
        for line in header.lines() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("#define UDA_") {
                if let Some(name) = identifier_after(trimmed, "#define ") {
                    // The include guard is header plumbing, not an ABI item.
                    if name != "UDA_H_" {
                        header_defines.push(name);
                    }
                }
            }
            if trimmed.starts_with("typedef") {
                if let Some(name) = identifier_after(trimmed, "(*") {
                    header_callbacks.push(name);
                }
            }

            let mut rest = trimmed;
            if in_block_comment {
                match rest.find("*/") {
                    Some(end) => {
                        in_block_comment = false;
                        rest = &rest[end + 2..];
                    }
                    None => continue,
                }
            }
            loop {
                match rest.find("/*") {
                    Some(start) => {
                        code.push_str(&rest[..start]);
                        let after = &rest[start + 2..];
                        match after.find("*/") {
                            Some(end) => rest = &after[end + 2..],
                            None => {
                                in_block_comment = true;
                                break;
                            }
                        }
                    }
                    None => {
                        if let Some(start) = rest.find("//") {
                            code.push_str(&rest[..start]);
                        } else {
                            code.push_str(rest);
                        }
                        code.push('\n');
                        break;
                    }
                }
            }
        }

        // Every `uda_name(` outside a comment is a declaration; the header
        // contains no calls.
        let mut header_functions = Vec::new();
        let mut search_from = 0;
        while let Some(found) = code[search_from..].find("uda_") {
            let absolute = search_from + found;
            if let Some(name) = identifier_after(&code[absolute..], "uda_") {
                let after_identifier = code[absolute + 4 + name.len()..].trim_start();
                if after_identifier.starts_with('(') {
                    header_functions.push(format!("uda_{name}"));
                }
            }
            search_from = absolute + 4;
        }

        let (mut functions, mut constants, mut callbacks) = exported_names_from_sources();
        functions.sort();
        constants.sort();
        callbacks.sort();
        header_functions.sort();
        header_defines.sort();
        header_callbacks.sort();

        assert_eq!(
            functions, header_functions,
            "the header's function declarations and this crate's #[no_mangle] \
             exports disagree; re-run scripts/gen-header.sh"
        );
        assert_eq!(
            constants, header_defines,
            "the header's UDA_* defines and abi.rs's pub constants disagree; \
             re-run scripts/gen-header.sh"
        );
        assert_eq!(
            callbacks, header_callbacks,
            "the header's callback typedefs and abi.rs's pub types disagree; \
             re-run scripts/gen-header.sh"
        );
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
