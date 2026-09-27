//! Windows appearance manager: dark/light detection via the Win32 registry.
//!
//! # Backend selection
//!
//! Windows 10/11 expose the user's app/theme preference under
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize`
//! (see `docs/internals/appearance_specs.md`):
//!
//! | Value                | Type   | Meaning                       |
//! |----------------------|--------|-------------------------------|
//! | `AppsUseLightTheme`  | DWORD  | `0` = Dark, `1` = Light       |
//! | `SystemUsesLightTheme` | DWORD | `0` = Dark, `1` = Light     |
//!
//! `AppsUseLightTheme` governs Win32/UWP app chrome and is the value UDA reads
//! first; `SystemUsesLightTheme` (added in Windows 1809) governs the taskbar and
//! is consulted only as a graceful fallback when the primary value is absent.
//!
//! The accent colour lives under `HKCU\Software\Microsoft\Windows\DWM` as the
//! `AccentColor` DWORD. Windows stores it as a 32-bit integer whose bytes are
//! ordered **A, B, G, R** from most to least significant, so reading it as a
//! `u32` and slicing the bytes is the documented way to recover the channels.
//! Writing them back in the other order swaps red and blue, which is invisible
//! in a unit test that uses evenly spaced channels — hence the dedicated
//! `accent_dword_is_split_into_rgba` regression test below.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_SUCCESS, HANDLE, WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    REG_VALUE_TYPE,
};

use uda_core::appearance::AppearanceManager;
use uda_core::capability::{Capability, RgbaColor, Theme};
use uda_core::error::UdaError;

/// Sub-key holding the Windows 10/11 personalization (theme) values.
const PERSONALIZE_SUBKEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";

/// DWORD value describing the app (not system) light/dark preference.
const APPS_USE_LIGHT_THEME: &str = "AppsUseLightTheme";

/// DWORD value describing the system (taskbar) light/dark preference.
const SYSTEM_USES_LIGHT_THEME: &str = "SystemUsesLightTheme";

/// Sub-key holding the Desktop Window Manager values, including the accent colour.
const DWM_SUBKEY: &str = "Software\\Microsoft\\Windows\\DWM";

/// DWORD value holding the accent colour, packed as A, B, G, R.
const ACCENT_COLOR: &str = "AccentColor";

/// Windows appearance manager.
///
/// This type is a zero-sized marker: all state lives in the registry, so the
/// manager is freely cloneable and usable from multiple threads.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsAppearanceManager;

impl WindowsAppearanceManager {
    pub fn new() -> Self {
        Self
    }

    /// Read a DWORD value from `HKCU\<subkey>`.
    ///
    /// Returns `Ok(None)` when the key or the value does not exist, which is a
    /// normal condition on older Windows builds rather than a hard failure.
    fn read_registry_dword(subkey: &str, value_name: &str) -> Result<Option<u32>, UdaError> {
        let wide_subkey = to_wide(subkey);
        let wide_value = to_wide(value_name);

        // SAFETY: `wide_subkey` and `wide_value` are null-terminated `Vec<u16>`
        // buffers that outlive the call. `key` is a valid out-parameter, and on
        // the error paths below we close it exactly once. The handle is only
        // dereferenced by the registry API, never handed to user code.
        let key = unsafe {
            let mut key: HKEY = HKEY::default();
            let status = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide_subkey.as_ptr()),
                0,
                KEY_READ,
                &mut key,
            );
            match status {
                code if code == ERROR_SUCCESS => key,
                ERROR_FILE_NOT_FOUND => return Ok(None),
                other => {
                    log::debug!(
                        "RegOpenKeyExW(HKLM\\{subkey}) failed with Win32 error {}",
                        other.0
                    );
                    return Ok(None);
                }
            }
        };

        // The key handle must always be closed, even when the query fails.
        let query_result = Self::query_dword(key, &wide_value);
        close_key(key);
        query_result
    }

    /// Query a DWORD value from an already-open registry key.
    ///
    /// `lpdata` is a two-stage pattern required by `RegQueryValueExW`: first call
    /// with a null buffer to learn the buffer size, then call again with a
    /// correctly sized buffer. Both the size and the reported value type are
    /// validated before the value is trusted.
    fn query_dword(key: HKEY, value_name: &[u16]) -> Result<Option<u32>, UdaError> {
        // Stage 1: learn the required buffer size and the stored value type.
        let mut value_type: REG_VALUE_TYPE = REG_VALUE_TYPE::default();
        let mut byte_len: u32 = 0;

        // SAFETY: `key` is a handle opened above and not yet closed. A null data
        // pointer with a non-null `lpcbdata` is the documented "size query"
        // invocation, so the registry API only writes to `byte_len`.
        let status = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(value_name.as_ptr()),
                None,
                Some(&mut value_type),
                None,
                Some(&mut byte_len),
            )
        };

        let status = match status {
            code if code == ERROR_SUCCESS => code,
            ERROR_FILE_NOT_FOUND | ERROR_INVALID_PARAMETER => return Ok(None),
            other => {
                log::debug!(
                    "RegQueryValueExW size query failed with Win32 error {}",
                    other.0
                );
                return Ok(None);
            }
        };
        debug_assert_eq!(status, ERROR_SUCCESS);

        // A DWORD is exactly 4 bytes; anything else means the value was retyped
        // by policy or a third-party tool, so refuse to reinterpret the bytes.
        if value_type != REG_DWORD || byte_len != u32::try_from(size_of::<u32>()).unwrap_or(4) {
            log::debug!(
                "registry value is not a DWORD (type={:?}, len={byte_len}); ignoring",
                value_type.0
            );
            return Ok(None);
        }

        // Stage 2: read the actual value into an exactly-sized buffer.
        let mut data: u32 = 0;
        let mut read_len: u32 = u32::try_from(size_of::<u32>()).unwrap_or(4);

        // SAFETY: `data` is a live `u32` on this stack frame and `read_len`
        // matches its byte length, so the registry API cannot write out of
        // bounds. `key` is still open and valid. The pointer is taken with
        // `&raw mut` so that no reference aliases `data` while the registry API
        // writes to it through the raw pointer.
        let data_ptr: *mut u8 = std::ptr::addr_of_mut!(data).cast::<u8>();
        let status = unsafe {
            RegQueryValueExW(
                key,
                PCWSTR(value_name.as_ptr()),
                None,
                Some(&mut value_type),
                Some(data_ptr),
                Some(&mut read_len),
            )
        };

        match status {
            code if code == ERROR_SUCCESS => Ok(Some(data)),
            ERROR_FILE_NOT_FOUND | ERROR_INVALID_PARAMETER => Ok(None),
            other => {
                log::debug!(
                    "RegQueryValueExW value read failed with Win32 error {}",
                    other.0
                );
                Ok(None)
            }
        }
    }

    /// Translate a raw registry DWORD into a [`Theme`].
    ///
    /// `0` means Dark and `1` means Light. Any other value is treated as
    /// "unknown" and falls through to the caller's default.
    fn theme_from_light_flag(flag: u32) -> Option<Theme> {
        match flag {
            0 => Some(Theme::Dark),
            1 => Some(Theme::Light),
            other => {
                log::debug!("unexpected AppsUseLightTheme value {other}; ignoring");
                None
            }
        }
    }

    /// Split the `AccentColor` DWORD into its R, G, B, A channels.
    ///
    /// Windows packs the value as **A, B, G, R** from the most to the least
    /// significant byte, so the byte at index 0 is alpha and index 3 is red.
    /// Reading `u32::to_le_bytes` and slicing in that order is the documented
    /// recovery; any other pairing swaps red and blue, which no caller can
    /// detect because both are valid channel values.
    fn accent_from_dword(packed: u32) -> RgbaColor {
        // Index 0 is the least significant byte on a little-endian target, which
        // is where Windows puts the red channel.
        let [r, g, b, a] = packed.to_le_bytes();
        RgbaColor { r, g, b, a }
    }
}

/// Close a registry key handle, ignoring the result.
///
/// A failure to close is not actionable for the caller (the OS reclaims the
/// handle at process exit) and must never mask the real error of the operation.
fn close_key(key: HKEY) {
    // SAFETY: `key` is a handle obtained from `RegOpenKeyExW` above and is closed
    // exactly once here. `RegCloseKey` does not dereference the handle after a
    // successful close, and the value is `Copy`, so this is safe even if the
    // surrounding operation already returned an error.
    let status = unsafe { RegCloseKey(key) };
    if status != ERROR_SUCCESS {
        log::debug!("RegCloseKey failed with Win32 error {}", status.0);
    }
}

/// Convert a UTF-8 Rust string into a null-terminated UTF-16 buffer.
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

impl AppearanceManager for WindowsAppearanceManager {
    fn detect_theme(&self) -> Result<Theme, UdaError> {
        if let Some(flag) = Self::read_registry_dword(PERSONALIZE_SUBKEY, APPS_USE_LIGHT_THEME)? {
            if let Some(theme) = Self::theme_from_light_flag(flag) {
                return Ok(theme);
            }
        }

        // Fallback: Windows 1809+ also tracks a system-level preference that
        // covers the taskbar. Using it keeps detection working when only the
        // system value has been written.
        if let Some(flag) = Self::read_registry_dword(PERSONALIZE_SUBKEY, SYSTEM_USES_LIGHT_THEME)?
        {
            if let Some(theme) = Self::theme_from_light_flag(flag) {
                return Ok(theme);
            }
        }

        // Neither value is present (e.g. Windows 7/8 or a stripped image).
        Err(UdaError::NotSupported(
            "Windows theme preference registry values are not available".to_string(),
        ))
    }

    fn get_accent_color(&self) -> Result<RgbaColor, UdaError> {
        // A missing value is a normal condition on builds that predate the DWM
        // accent colour (or on images with DWM disabled), so it degrades to
        // `NotSupported` rather than an internal error. `read_registry_dword`
        // already maps every Win32 failure onto `Ok(None)`.
        match Self::read_registry_dword(DWM_SUBKEY, ACCENT_COLOR)? {
            Some(packed) => Ok(Self::accent_from_dword(packed)),
            None => Err(UdaError::NotSupported(
                "Windows accent colour is not available in the registry".to_string(),
            )),
        }
    }

    fn capabilities(&self) -> Result<Capability, UdaError> {
        // Theme detection is a plain registry read that works on every supported
        // Windows build, and the accent colour is read from the same registry
        // through the same helper. Neither needs a speculative probe at startup:
        // both are reported unconditionally and degrade to `NotSupported` per
        // call when the value is absent.
        Ok(Capability::DETECT_THEME | Capability::READ_ACCENT_COLOR)
    }
}

/// Win32 `HANDLE` re-export guard: keeps the `windows` feature list honest by
/// documenting that this module only needs `Win32_Foundation` (for the error and
/// handle types) and `Win32_System_Registry`.
#[allow(dead_code)]
type _RegistryDependencies = (HKEY, WIN32_ERROR, HANDLE);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_wide_is_null_terminated() {
        let wide = to_wide("AppsUseLightTheme");
        assert_eq!(wide.last().copied(), Some(0));
        assert!(!wide[..wide.len() - 1].contains(&0));
    }

    #[test]
    fn to_wide_handles_empty_string() {
        let wide = to_wide("");
        assert_eq!(wide, vec![0]);
    }

    #[test]
    fn theme_from_light_flag_maps_zero_to_dark() {
        assert_eq!(
            WindowsAppearanceManager::theme_from_light_flag(0),
            Some(Theme::Dark)
        );
    }

    #[test]
    fn theme_from_light_flag_maps_one_to_light() {
        assert_eq!(
            WindowsAppearanceManager::theme_from_light_flag(1),
            Some(Theme::Light)
        );
    }

    #[test]
    fn theme_from_light_flag_rejects_unknown_values() {
        assert_eq!(WindowsAppearanceManager::theme_from_light_flag(2), None);
        assert_eq!(
            WindowsAppearanceManager::theme_from_light_flag(u32::MAX),
            None
        );
    }

    #[test]
    fn capabilities_reports_theme_detection_and_accent_colour() {
        let manager = WindowsAppearanceManager::new();
        let caps = match manager.capabilities() {
            Ok(caps) => caps,
            Err(e) => panic!("capabilities failed: {e}"),
        };
        assert!(caps.contains(Capability::DETECT_THEME));
        // Reporting the flag is what lets a host decide to *call*
        // `get_accent_color`; the per-call `NotSupported` still covers a host
        // whose registry lacks the value.
        assert!(caps.contains(Capability::READ_ACCENT_COLOR));
        assert_eq!(caps, Capability::DETECT_THEME | Capability::READ_ACCENT_COLOR);
    }

    #[test]
    fn personalization_subkey_matches_specification() {
        assert_eq!(
            PERSONALIZE_SUBKEY,
            "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
        );
    }

    #[test]
    fn dwm_subkey_matches_specification() {
        assert_eq!(DWM_SUBKEY, "Software\\Microsoft\\Windows\\DWM");
        assert_eq!(ACCENT_COLOR, "AccentColor");
    }

    #[test]
    fn accent_dword_is_split_into_rgba() {
        // Windows packs the DWORD as A, B, G, R from the most to the least
        // significant byte. A value whose four bytes are all distinct proves the
        // channels are not transposed: `0xDDCCBBAA` has red `0xAA`, green
        // `0xBB`, blue `0xCC`, alpha `0xDD`.
        let color = WindowsAppearanceManager::accent_from_dword(0xDDCC_BBAA);
        assert_eq!(color.r, 0xAA);
        assert_eq!(color.g, 0xBB);
        assert_eq!(color.b, 0xCC);
        assert_eq!(color.a, 0xDD);
    }

    #[test]
    fn accent_channels_survive_a_realistic_opaque_colour() {
        // A fully opaque mid-blue, the shape Windows actually writes for the
        // default accent: alpha `0xFF`, blue `0x00`, green `0x78`, red `0xD7`.
        let color = WindowsAppearanceManager::accent_from_dword(0xFF00_78D7);
        assert_eq!(color, RgbaColor { r: 0xD7, g: 0x78, b: 0x00, a: 0xFF });
    }

    #[test]
    fn accent_red_and_blue_are_never_swapped() {
        // Pure red must stay red. Byte-swapping the two channels yields blue,
        // which is exactly the defect this test exists to catch.
        let color = WindowsAppearanceManager::accent_from_dword(0xFF00_00FF);
        assert_eq!(color.r, 0xFF);
        assert_eq!(color.b, 0x00);
    }

    #[test]
    fn pcwstr_from_buffer_is_non_null() {
        // The registry helpers build `PCWSTR` values directly from the buffer
        // pointer; this asserts the invariant that the buffer is never empty and
        // therefore never yields a null pointer.
        let buffer = to_wide("AppsUseLightTheme");
        let ptr = PCWSTR(buffer.as_ptr());
        assert!(!ptr.0.is_null());
    }
}
