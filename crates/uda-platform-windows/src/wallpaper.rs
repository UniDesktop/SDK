//! Windows wallpaper manager: static wallpaper via `SystemParametersInfoW`.
//!
//! # Backend selection
//!
//! Windows has no portal or desktop-shell abstraction; the single supported
//! mechanism is Win32 (see `docs/internals/wallpaper_specs.md`):
//!
//! | Concern                | API / location                                              |
//! |------------------------|-------------------------------------------------------------|
//! | Set wallpaper          | `SystemParametersInfoW(SPI_SETDESKWALLPAPER, ...)`           |
//! | Read wallpaper         | `SystemParametersInfoW(SPI_GETDESKWALLPAPER, ...)`           |
//! | Fit mode               | `HKCU\Control Panel\Desktop` -> `WallpaperStyle` / `TileWallpaper` (REG_SZ) |
//!
//! `SystemParametersInfoW` only *points* the desktop at a path; the stretch/tile
//! behaviour is read from the two `Control Panel\Desktop` registry values. Those
//! values must therefore be written **before** the wallpaper is applied, which is
//! why [`WindowsWallpaperManager::set_wallpaper`] does not have a fallback chain:
//! there is exactly one tier.
//!
//! Both values are `REG_SZ` null-terminated UTF-16 strings holding decimal digits
//! (never `REG_DWORD`): Explorer reads them with a string parser and silently
//! ignores any other type. The mapping is:
//!
//! | `FillMode`   | `WallpaperStyle` | `TileWallpaper` | Windows meaning |
//! |--------------|------------------|-----------------|-----------------|
//! | `Fill`       | `"10"`           | `"0"`           | Fill (crop to screen) |
//! | `Crop`       | `"10"`           | `"0"`           | Fill (no native crop mode) |
//! | `Fit`        | `"6"`            | `"0"`           | Fit (letterbox) |
//! | `Stretch`    | `"2"`            | `"0"`           | Stretch (distort) |
//!
//! # Multi-monitor
//!
//! `SPI_SETDESKWALLPAPER` applies the image to *all* monitors. Windows exposes no
//! per-monitor wallpaper in the public Win32 API (it requires the private
//! `IDesktopWallpaper` COM interface introduced in Windows 8), so
//! [`WallpaperOptions::monitor_index`] is accepted for cross-platform trait
//! parity but has no effect here.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_GETDESKWALLPAPER,
    SPI_SETDESKWALLPAPER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};

/// Sub-key holding the per-user desktop (wallpaper style) settings.
const CONTROL_PANEL_DESKTOP_SUBKEY: &str = "Control Panel\\Desktop";

/// Registry value controlling how the wallpaper is stretched.
///
/// Stored as a `REG_SZ` decimal string: `"0"` = tiled, `"1"` = centered,
/// `"2"` = stretched, `"6"` = fit, `"10"` = fill.
const WALLPAPER_STYLE: &str = "WallpaperStyle";

/// Registry value controlling tiling.
///
/// Stored as a `REG_SZ` decimal string: `"0"` = no tiling, `"1"` = tile the
/// image across the desktop.
const TILE_WALLPAPER: &str = "TileWallpaper";

/// Maximum length, in UTF-16 code units, of a wallpaper path accepted by
/// `SystemParametersInfoW`. The Win32 constant is `MAX_PATH` (260) code units.
const MAX_PATH_CODE_UNITS: usize = 260;

/// Windows wallpaper manager.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsWallpaperManager;

impl WindowsWallpaperManager {
    pub fn new() -> Self {
        Self
    }

    /// Map the cross-platform [`FillMode`] onto the Windows registry pair.
    ///
    /// Windows 7 and later understand four distinct styles natively, so every
    /// mode maps onto its own registry pair except `Crop`, which has no Windows
    /// equivalent and therefore reuses `Fill` (the closest behaviour: scale to
    /// cover the screen, cropping the overflow).
    ///
    /// The values are returned as string slices rather than numbers because the
    /// registry stores them as `REG_SZ` decimal strings; converting once here
    /// keeps the call site free of formatting code.
    fn wallpaper_style_values(fill_mode: FillMode) -> (&'static str, &'static str) {
        match fill_mode {
            // Fill and Crop both cover the screen without letterboxing; Windows
            // has no native crop, so both use style "10" (fill).
            FillMode::Fill => ("10", "0"),
            FillMode::Crop => ("10", "0"),
            // "6" is the native fit mode: scale to the largest rectangle that
            // fits inside the screen, letterboxing the remainder.
            FillMode::Fit => ("6", "0"),
            // "2" stretches the image to the screen, distorting the aspect ratio.
            FillMode::Stretch => ("2", "0"),
        }
    }

    /// Write `WallpaperStyle` and `TileWallpaper` under `HKCU\Control Panel\Desktop`.
    ///
    /// Both values are always written (rather than only the changed one) so the
    /// two keys can never disagree, which is what produces the classic Windows
    /// "wallpaper is stretched and tiled at once" artefact.
    fn write_desktop_style(fill_mode: FillMode) -> Result<(), UdaError> {
        let (style, tile) = Self::wallpaper_style_values(fill_mode);
        let wide_subkey = to_wide(CONTROL_PANEL_DESKTOP_SUBKEY);
        let wide_style = to_wide(WALLPAPER_STYLE);
        let wide_tile = to_wide(TILE_WALLPAPER);

        // SAFETY: `wide_subkey` is a null-terminated buffer that outlives the
        // call, `key` is a valid out-parameter, and the handle is closed exactly
        // once by `close_key` below regardless of the outcome.
        let key = unsafe {
            let mut key: HKEY = HKEY::default();
            let status = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide_subkey.as_ptr()),
                0,
                PCWSTR(std::ptr::null()),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            );
            match status {
                code if code == ERROR_SUCCESS => key,
                other => {
                    return Err(UdaError::Internal(format!(
                        "RegCreateKeyExW(HKCU\\{CONTROL_PANEL_DESKTOP_SUBKEY}) failed with Win32 error {}",
                        other.0
                    )));
                }
            }
        };

        let write_result = Self::write_string(key, &wide_style, style)
            .and_then(|()| Self::write_string(key, &wide_tile, tile));
        close_key(key);
        write_result
    }

    /// Write a single `REG_SZ` string value into an open registry key.
    ///
    /// Explorer parses `WallpaperStyle` and `TileWallpaper` with a string reader,
    /// so the values must be null-terminated UTF-16 with the `REG_SZ` type. The
    /// byte length passed to the API therefore includes the null terminator,
    /// which `to_wide` always appends.
    fn write_string(key: HKEY, value_name: &[u16], value: &str) -> Result<(), UdaError> {
        let wide_value = to_wide(value);
        let byte_len = wide_value.len() * std::mem::size_of::<u16>();

        // SAFETY: `wide_value` is a null-terminated UTF-16 buffer that outlives
        // the call, and `byte_len` is its exact size including the terminator, so
        // the registry API cannot read past its end. `key` was opened with
        // `KEY_SET_VALUE` above and is still open.
        let status = unsafe {
            RegSetValueExW(
                key,
                PCWSTR(value_name.as_ptr()),
                0,
                REG_SZ,
                Some(std::slice::from_raw_parts(
                    wide_value.as_ptr().cast::<u8>(),
                    byte_len,
                )),
            )
        };

        match status {
            code if code == ERROR_SUCCESS => Ok(()),
            other => Err(UdaError::Internal(format!(
                "RegSetValueExW failed with Win32 error {}",
                other.0
            ))),
        }
    }

    /// Read the current wallpaper path via `SPI_GETDESKWALLPAPER`.
    ///
    /// Returns `Ok(None)` when no wallpaper is configured.
    fn read_wallpaper_path() -> Result<Option<String>, UdaError> {
        let mut buffer = vec![0u16; MAX_PATH_CODE_UNITS];

        // SAFETY: `buffer` holds `MAX_PATH_CODE_UNITS` UTF-16 code units, which is
        // exactly the buffer length declared to `SystemParametersInfoW`; the API
        // writes at most that many code units and always null-terminates within
        // the buffer on success.
        let result = unsafe {
            SystemParametersInfoW(
                SPI_GETDESKWALLPAPER,
                buffer.len() as u32,
                Some(buffer.as_mut_ptr().cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };

        match result {
            Ok(()) => Ok(Some(from_wide(&buffer))),
            Err(e) => {
                // `GetLastError` is only meaningful here because
                // `SystemParametersInfoW` returns a `HRESULT` failure rather than
                // a `BOOL`; the message keeps the Win32 code for diagnostics.
                log::debug!("SPI_GETDESKWALLPAPER failed: {e}");
                Ok(None)
            }
        }
    }

    /// Apply `path` as the desktop wallpaper for every monitor.
    fn apply_wallpaper(path: &str) -> Result<(), UdaError> {
        if path.is_empty() {
            return Err(UdaError::NotSupported(
                "wallpaper path must not be empty".to_string(),
            ));
        }

        let wide_path = to_wide(path);
        if wide_path.len() > MAX_PATH_CODE_UNITS {
            return Err(UdaError::NotSupported(format!(
                "wallpaper path exceeds the Windows MAX_PATH limit ({MAX_PATH_CODE_UNITS} UTF-16 code units)"
            )));
        }

        // The buffer must stay alive across the FFI call, hence the local.
        let mut wide_path = wide_path;

        // SAFETY: `wide_path` is a null-terminated buffer of `len` code units
        // that outlives the call, and `SPI_SETDESKWALLPAPER` treats `pvparam` as
        // an in/out pointer to that buffer. The action and flags are the
        // documented combination for persisting the change and notifying shells.
        let result = unsafe {
            SystemParametersInfoW(
                SPI_SETDESKWALLPAPER,
                0,
                Some(wide_path.as_mut_ptr().cast()),
                SPIF_UPDATEINIFILE | SPIF_SENDCHANGE,
            )
        };

        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(UdaError::Internal(format!(
                "SPI_SETDESKWALLPAPER failed for '{path}': {e} (last error {})",
                last_error()
            ))),
        }
    }
}

/// Convert a null-terminated UTF-16 buffer into a Rust `String`.
fn from_wide(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// Convert a UTF-8 Rust string into a null-terminated UTF-16 buffer.
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Close a registry key handle, ignoring the result.
fn close_key(key: HKEY) {
    // SAFETY: `key` comes from `RegCreateKeyExW` and is closed exactly once here.
    let status = unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS {
        log::debug!("RegCloseKey failed with Win32 error {}", status.0);
    }
}

/// Read the calling thread's last Win32 error code.
///
/// Used purely for diagnostics; a value of `0` simply means "no error recorded".
fn last_error() -> u32 {
    // SAFETY: `GetLastError` is a thread-local read with no pointer arguments
    // and cannot fail.
    unsafe { GetLastError().0 }
}

impl WallpaperManager for WindowsWallpaperManager {
    fn set_wallpaper(&self, path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        if options.monitor_index.is_some() {
            // Accepted for trait parity, but Win32 applies the wallpaper to every
            // monitor. Log rather than fail so the caller's intent is visible.
            log::debug!(
                "Windows applies the wallpaper to all monitors; monitor_index={:?} is ignored",
                options.monitor_index
            );
        }

        if options.dark_mode {
            // Windows has no separate dark-mode wallpaper slot in Win32; the
            // desktop image is shared by both appearances.
            log::debug!("Windows has no per-appearance wallpaper slot; dark_mode is ignored");
        }

        // Order matters: the style values must be in place before the wallpaper
        // is applied, otherwise the desktop renders with the previous style.
        Self::write_desktop_style(options.fill_mode)?;
        Self::apply_wallpaper(path)
    }

    fn get_wallpaper(&self) -> Result<Option<String>, UdaError> {
        Self::read_wallpaper_path()
    }

    fn capabilities(&self) -> Result<Capability, UdaError> {
        // Both directions are plain Win32 calls available on every supported
        // build, so they are reported without a speculative probe.
        Ok(Capability::SET_WALLPAPER | Capability::GET_WALLPAPER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallpaper_style_fill_maps_to_fill_ten() {
        assert_eq!(
            WindowsWallpaperManager::wallpaper_style_values(FillMode::Fill),
            ("10", "0")
        );
    }

    #[test]
    fn wallpaper_style_fit_maps_to_fit_six() {
        // Windows 7+ supports "fit" natively; it must not be mapped to tiling.
        assert_eq!(
            WindowsWallpaperManager::wallpaper_style_values(FillMode::Fit),
            ("6", "0")
        );
    }

    #[test]
    fn wallpaper_style_stretch_maps_to_stretch_two() {
        assert_eq!(
            WindowsWallpaperManager::wallpaper_style_values(FillMode::Stretch),
            ("2", "0")
        );
    }

    #[test]
    fn wallpaper_style_crop_reuses_fill() {
        // Windows has no native crop mode, so it falls back to "fill".
        assert_eq!(
            WindowsWallpaperManager::wallpaper_style_values(FillMode::Crop),
            ("10", "0")
        );
    }

    #[test]
    fn wallpaper_style_tile_wallpaper_is_always_zero() {
        // No supported FillMode ever enables tiling, so TileWallpaper must be
        // the string "0" (never "1" and never a DWORD) for every mode.
        for fill_mode in [
            FillMode::Crop,
            FillMode::Fill,
            FillMode::Fit,
            FillMode::Stretch,
        ] {
            let (style, tile) = WindowsWallpaperManager::wallpaper_style_values(fill_mode);
            assert_eq!(
                tile, "0",
                "TileWallpaper must be the string \"0\" for {fill_mode:?}, got \"{tile}\" (style \"{style}\")"
            );
        }
    }

    #[test]
    fn wallpaper_style_values_are_decimal_strings() {
        // Explorer parses these values as strings, so every emitted value must
        // be pure ASCII digits with no sign, whitespace or empty payload.
        for fill_mode in [
            FillMode::Crop,
            FillMode::Fill,
            FillMode::Fit,
            FillMode::Stretch,
        ] {
            let (style, tile) = WindowsWallpaperManager::wallpaper_style_values(fill_mode);
            for value in [style, tile] {
                assert!(
                    !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                    "value for {fill_mode:?} is not a plain decimal string: \"{value}\""
                );
            }
        }
    }

    #[test]
    fn wallpaper_style_values_are_known_windows_modes() {
        // Only the four styles Windows 7+ understands natively are allowed.
        const KNOWN_STYLES: [&str; 4] = ["10", "6", "2", "0"];
        for fill_mode in [
            FillMode::Crop,
            FillMode::Fill,
            FillMode::Fit,
            FillMode::Stretch,
        ] {
            let (style, _tile) = WindowsWallpaperManager::wallpaper_style_values(fill_mode);
            assert!(
                KNOWN_STYLES.contains(&style),
                "unknown WallpaperStyle for {fill_mode:?}: \"{style}\""
            );
        }
    }

    #[test]
    fn write_string_encodes_reg_sz_with_null_terminator() {
        // The byte length handed to RegSetValueExW must cover the UTF-16 code
        // units *including* the trailing null terminator.
        let wide = to_wide("10");
        assert_eq!(wide, ['1' as u16, '0' as u16, 0]);
        assert_eq!(
            wide.len() * std::mem::size_of::<u16>(),
            3 * std::mem::size_of::<u16>()
        );
    }

    #[test]
    fn registry_string_values_round_trip_through_utf16() {
        // Regression guard: the values written to the registry must survive a
        // UTF-16 round trip unchanged, i.e. the API receives exactly "10"/"0".
        for (mode, expected) in [
            (FillMode::Fill, "10"),
            (FillMode::Crop, "10"),
            (FillMode::Fit, "6"),
            (FillMode::Stretch, "2"),
        ] {
            let (style, _) = WindowsWallpaperManager::wallpaper_style_values(mode);
            assert_eq!(from_wide(&to_wide(style)), expected);
        }
    }

    #[test]
    fn registry_value_type_is_reg_sz() {
        // Explorer reads WallpaperStyle/TileWallpaper as REG_SZ strings; a
        // REG_DWORD write makes the shell ignore the fill mode entirely.
        assert_eq!(REG_SZ.0, 1u32);
    }

    #[test]
    fn to_wide_is_null_terminated() {
        let wide = to_wide("C:\\wall.jpg");
        assert_eq!(wide.last().copied(), Some(0));
    }

    #[test]
    fn from_wide_stops_at_null_terminator() {
        let buffer = [u16::from(b'a'), u16::from(b'b'), 0, u16::from(b'x')];
        assert_eq!(from_wide(&buffer), "ab");
    }

    #[test]
    fn from_wide_handles_unterminated_buffer() {
        let buffer = [u16::from(b'a'), u16::from(b'b')];
        assert_eq!(from_wide(&buffer), "ab");
    }

    #[test]
    fn from_wide_handles_empty_buffer() {
        assert_eq!(from_wide(&[0]), "");
        assert_eq!(from_wide(&[]), "");
    }

    #[test]
    fn empty_path_is_rejected() {
        let manager = WindowsWallpaperManager::new();
        let options = WallpaperOptions::default();
        match manager.set_wallpaper("", &options) {
            Err(UdaError::NotSupported(_)) => {}
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn over_long_path_is_rejected() {
        let manager = WindowsWallpaperManager::new();
        let options = WallpaperOptions::default();
        let long_path = "C:\\".to_string() + &"a".repeat(MAX_PATH_CODE_UNITS);
        match manager.set_wallpaper(&long_path, &options) {
            Err(UdaError::NotSupported(_)) => {}
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn capabilities_report_set_and_get() {
        let manager = WindowsWallpaperManager::new();
        let caps = match manager.capabilities() {
            Ok(caps) => caps,
            Err(e) => panic!("capabilities failed: {e}"),
        };
        assert_eq!(caps, Capability::SET_WALLPAPER | Capability::GET_WALLPAPER);
    }

    #[test]
    fn control_panel_desktop_subkey_matches_specification() {
        assert_eq!(CONTROL_PANEL_DESKTOP_SUBKEY, "Control Panel\\Desktop");
    }

    #[test]
    fn value_names_match_specification() {
        assert_eq!(WALLPAPER_STYLE, "WallpaperStyle");
        assert_eq!(TILE_WALLPAPER, "TileWallpaper");
    }

    #[test]
    fn max_path_limit_matches_win32() {
        assert_eq!(MAX_PATH_CODE_UNITS, 260);
    }
}
