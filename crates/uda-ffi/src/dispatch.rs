//! Platform dispatch for the exported C functions.
//!
//! The C ABI is platform-neutral, so every export funnels into one of the
//! functions here and the `cfg` at the bottom of this file picks the backend:
//!
//! | Target | Appearance | Wallpaper | Wake lock |
//! |--------|------------|-----------|-----------|
//! | Linux  | `uda_platform_linux::appearance` | `uda_platform_linux::wallpaper` | `uda_platform_linux::wakelock` + Tier-3 CLI |
//! | Windows| `uda_platform_windows::appearance` | `uda_platform_windows::wallpaper` | `uda_platform_windows::wakelock` |
//!
//! Per `AGENTS.md` Principle 2 (Cascading Fallback Engine), each Linux feature
//! keeps its full tier chain. The `uda-platform-linux` crate implements Tiers
//! 1–2 for appearance and wallpaper; this module adds the **Tier 3** CLI probes
//! for the two capabilities whose platform crates deliberately punt, so the FFI
//! surface stays useful on a bare window manager:
//!
//! - `get_wallpaper` -> `gsettings get org.gnome.desktop.background picture-uri`
//! - `wakelock_acquire` -> `systemd-inhibit --what=... sleep <seconds>`

use std::process::Command;

// The trait imports are required: the platform managers implement these traits
// rather than exposing inherent methods, and calling them without the traits in
// scope is the most common way to make this dispatch layer fail to compile.
use uda_core::appearance::AppearanceManager;
use uda_core::capability::Theme;
use uda_core::error::UdaError;
use uda_core::wakelock::WakeLockType;
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};

use crate::error::Failure;

/// Opaque wake-lock handle handed to the caller.
///
/// A newtype rather than a bare `u64` so a handle can never be confused with an
/// unrelated integer at a call site, and so `0` (the C ABI's "no lock" value)
/// is unrepresentable in the Rust layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WakeLockHandle(u64);

impl WakeLockHandle {
    /// Wrap a raw numeric handle coming from the C side.
    ///
    /// Returns `None` for `0`, which the C ABI defines as "no lock"; treating it
    /// as a real handle would make a zeroed out-parameter look like success.
    pub(crate) fn from_raw(raw: u64) -> Option<Self> {
        if raw == 0 {
            None
        } else {
            Some(Self(raw))
        }
    }

    /// The numeric value to hand back to C.
    pub(crate) fn raw(self) -> u64 {
        self.0
    }
}

/// Detect the system colour scheme and map it onto the C ABI's integer codes.
///
/// Returns `0` (unknown), `1` (dark) or `2` (light). `Theme::Auto` is reported
/// as unknown: the C ABI has no "follow the system" state, and reporting it as
/// either dark or light would be a guess.
pub(crate) fn detect_theme_code() -> Result<i32, Failure> {
    let theme = appearance().detect_theme()?;
    Ok(theme_code(theme))
}

/// Translate a [`Theme`] into the C ABI code.
fn theme_code(theme: Theme) -> i32 {
    match theme {
        Theme::Dark => 1,
        Theme::Light => 2,
        Theme::Auto => 0,
    }
}

/// Set the desktop wallpaper with the requested fill mode.
pub(crate) fn set_wallpaper(path: &str, fill_mode: FillMode) -> Result<(), Failure> {
    let options = WallpaperOptions {
        fill_mode,
        // The C ABI exposes no monitor selector, so `None` (all monitors) is the
        // documented cross-platform behaviour.
        monitor_index: None,
        // The C ABI exposes no dark/light pairing either.
        dark_mode: false,
    };
    wallpaper().set_wallpaper(path, &options)?;
    Ok(())
}

/// Read the current wallpaper path, if the backend can report one.
///
/// Returns `Ok(None)` when no wallpaper is configured *or* the platform cannot
/// answer the question at all - both are legitimate "no path" answers for a C
/// caller, which distinguishes them only through `uda_last_error_message()`.
pub(crate) fn get_wallpaper_path() -> Result<Option<String>, Failure> {
    if let Some(path) = wallpaper().get_wallpaper().ok().flatten() {
        return Ok(Some(path));
    }

    // Tier 3: the platform trait may not implement the read (Linux currently
    // returns `NotSupported`), so probe the standard CLI tools before giving up.
    match cli_wallpaper_path() {
        Ok(path) => Ok(path),
        Err(error) => {
            log::debug!("wallpaper CLI fallback failed: {error}");
            Ok(None)
        }
    }
}

/// Ask the standard desktop CLIs for the configured wallpaper path.
fn cli_wallpaper_path() -> Result<Option<String>, UdaError> {
    let raw = match run_capture(
        "gsettings",
        &["get", "org.gnome.desktop.background", "picture-uri"],
    ) {
        Ok(value) => value,
        Err(error) => {
            log::debug!("gsettings wallpaper query failed: {error}");
            return Ok(None);
        }
    };

    let Some(raw) = raw else {
        return Ok(None);
    };

    // `gsettings` prints a GVariant string literal, quotes included.
    let unquoted = raw.trim().trim_matches(|c| c == '\'' || c == '"');
    if unquoted.is_empty() {
        return Ok(None);
    }
    Ok(Some(strip_file_scheme(unquoted)))
}

/// Drop a leading `file://` scheme so callers receive a plain filesystem path.
fn strip_file_scheme(value: &str) -> String {
    value
        .strip_prefix("file://")
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_owned())
}

/// Run a command and return its trimmed stdout when it succeeded and printed.
fn run_capture(program: &str, args: &[&str]) -> Result<Option<String>, UdaError> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| UdaError::CommandFailed(format!("spawn {program}: {e}")))?;

    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

/// Acquire a wake lock, returning a handle that [`release_wakelock`] accepts.
pub(crate) fn acquire_wakelock(
    lock_type: WakeLockType,
    reason: &str,
) -> Result<WakeLockHandle, Failure> {
    crate::wakelocks::acquire(lock_type, reason)
}

/// Release a wake lock previously obtained from [`acquire_wakelock`].
pub(crate) fn release_wakelock(handle: WakeLockHandle) -> Result<(), Failure> {
    crate::wakelocks::release(handle)
}

/// Return a platform appearance manager.
#[cfg(target_os = "linux")]
fn appearance() -> uda_platform_linux::appearance::LinuxAppearanceManager {
    uda_platform_linux::appearance::LinuxAppearanceManager::new()
}

/// Return a platform wallpaper manager.
#[cfg(target_os = "linux")]
fn wallpaper() -> uda_platform_linux::wallpaper::LinuxWallpaperManager {
    uda_platform_linux::wallpaper::LinuxWallpaperManager::new()
}

/// Return a platform appearance manager.
#[cfg(target_os = "windows")]
fn appearance() -> uda_platform_windows::appearance::WindowsAppearanceManager {
    uda_platform_windows::appearance::WindowsAppearanceManager::new()
}

/// Return a platform wallpaper manager.
#[cfg(target_os = "windows")]
fn wallpaper() -> uda_platform_windows::wallpaper::WindowsWallpaperManager {
    uda_platform_windows::wallpaper::WindowsWallpaperManager::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_codes_match_the_c_header() {
        assert_eq!(theme_code(Theme::Dark), 1);
        assert_eq!(theme_code(Theme::Light), 2);
        // "Auto" has no C representation, so it is reported as unknown.
        assert_eq!(theme_code(Theme::Auto), 0);
    }

    #[test]
    fn handle_zero_is_not_a_valid_lock() {
        assert_eq!(WakeLockHandle::from_raw(0), None);
        let handle = WakeLockHandle::from_raw(7).expect("non-zero is a handle");
        assert_eq!(handle.raw(), 7);
        assert_eq!(WakeLockHandle::from_raw(handle.raw()), Some(handle));
    }

    #[test]
    fn file_scheme_is_stripped_from_paths() {
        assert_eq!(
            strip_file_scheme("file:///home/user/wall.jpg"),
            "/home/user/wall.jpg"
        );
        assert_eq!(
            strip_file_scheme("/home/user/wall.jpg"),
            "/home/user/wall.jpg"
        );
    }

    #[test]
    fn detect_theme_reports_a_documented_code() {
        // Any of the three codes is acceptable; what matters is that the call
        // neither panics nor returns something outside the documented set.
        let code = detect_theme_code().expect("theme detection is best-effort");
        assert!((0..=2).contains(&code), "unexpected theme code {code}");
    }

    #[test]
    fn set_wallpaper_rejects_an_empty_path() {
        let result = set_wallpaper("", FillMode::Fill);
        assert!(result.is_err(), "an empty path must be rejected");
    }

    #[test]
    fn releasing_an_unknown_handle_is_an_error_not_a_panic() {
        let bogus = WakeLockHandle::from_raw(0xDEAD_BEEF).expect("synthetic handle");
        let result = release_wakelock(bogus);
        assert!(result.is_err(), "an unknown handle must be rejected");
    }
}
