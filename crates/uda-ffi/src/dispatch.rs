//! Platform dispatch for the exported C functions.
//!
//! The C ABI is platform-neutral, so every export funnels into one of the
//! functions here and the `cfg` at the bottom of this file picks the backend:
//!
//! | Target | Appearance | Wallpaper | Wake lock |
//! |--------|------------|-----------|-----------|
//! | Linux  | `uda_platform_linux::appearance` | `uda_platform_linux::wallpaper` | `uda_platform_linux::wakelock` + Tier-3 CLI |
//! | Windows| `uda_platform_windows::appearance` | `uda_platform_windows::wallpaper` | `uda_platform_windows::wakelock` |
//! | Other  | `UnsupportedBackend` | `UnsupportedBackend` | CLI tier only |
//!
//! Wallpaper reads and writes are owned by the platform manager, including its
//! CLI fallback chain, timeout policy and reply decoding. Dispatch only maps the
//! result onto the C ABI's optional path contract.

// The trait imports are required: the platform managers implement these traits
// rather than exposing inherent methods, and calling them without the traits in
// scope is the most common way to make this dispatch layer fail to compile.
use uda_core::appearance::AppearanceManager;
use uda_core::capability::{RgbaColor, Theme};
use uda_core::error::UdaError;
use uda_core::tray::{TrayIcon, TrayIconConfig};
use uda_core::wakelock::WakeLockType;
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};

use crate::abi::{UDA_THEME_DARK, UDA_THEME_LIGHT, UDA_THEME_UNKNOWN};
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
/// Returns `0` (unknown), `1` (dark) or `2` (light). `Theme::Auto` and
/// `Theme::Unknown` are both reported as unknown: the C ABI has no "follow the
/// system" state, and reporting either as dark or light would be a guess.
pub(crate) fn detect_theme_code() -> Result<i32, Failure> {
    let theme = appearance().detect_theme()?;
    Ok(theme_code(theme))
}

/// Read the system accent colour as four `u8` channels.
///
/// Returns `None` when the platform does not expose one (most Linux desktops)
/// or when the value cannot be parsed. A C caller cannot tell "no accent" from
/// "failed to read" apart, which is why this is a plain option rather than an
/// error: the demo prints one message for both.
pub(crate) fn accent_color() -> Option<RgbaColor> {
    appearance().get_accent_color().ok()
}

/// Translate a [`Theme`] into the C ABI code.
fn theme_code(theme: Theme) -> i32 {
    match theme {
        Theme::Dark => UDA_THEME_DARK,
        Theme::Light => UDA_THEME_LIGHT,
        // Neither an "auto" preference nor an undeterminable one has a positive
        // answer in the C ABI, so both report `UDA_THEME_UNKNOWN`.
        Theme::Auto | Theme::Unknown => UDA_THEME_UNKNOWN,
        // `Theme` is `#[non_exhaustive]`: a future appearance maps to the
        // same "no positive answer" code instead of breaking this crate.
        _ => UDA_THEME_UNKNOWN,
    }
}

/// Set the desktop wallpaper with the requested fill mode.
pub(crate) fn set_wallpaper(path: &str, fill_mode: FillMode) -> Result<(), Failure> {
    // Reject an empty path before any backend is consulted: a C caller passing
    // `""` is a contract violation, and letting it through would write a bare
    // `file://` URI into GNOME's settings - a silently broken wallpaper. Doing
    // it here keeps the answer identical on every target.
    if path.trim().is_empty() {
        return Err(Failure::Uda(UdaError::InvalidArgument(
            "wallpaper path must not be empty".to_string(),
        )));
    }

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
/// answer the question at all. Both collapse into the same `UDA_OK` + NULL
/// answer on the wire (see `uda_get_wallpaper` in `include/uda.h`): the caller
/// must check the pointer rather than the status, and no last-error message is
/// written on this path - success carries no diagnosis, so there is nothing to
/// distinguish the two cases with.
pub(crate) fn get_wallpaper_path() -> Result<Option<String>, Failure> {
    Ok(wallpaper().get_wallpaper().ok().flatten())
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

/// Register a tray icon through the platform backend.
///
/// The manager is a zero-sized unit struct on both platforms, so constructing
/// one per call is free; `create` is what opens the D-Bus connection (Linux) or
/// spawns the worker thread (Windows), exactly once per icon.
pub(crate) fn create_tray(config: TrayIconConfig) -> Result<TrayIcon, Failure> {
    // `create` is a trait method, so the trait must be in scope wherever a
    // platform backend is compiled in; targets without one never call it, and
    // keeping the import unconditional would warn there.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    use uda_core::tray::TrayManager as _;

    #[cfg(target_os = "linux")]
    {
        Ok(uda_platform_linux::tray::LinuxTrayManager::new().create(config)?)
    }

    #[cfg(target_os = "windows")]
    {
        Ok(uda_platform_windows::tray::WindowsTrayManager::new().create(config)?)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = config;
        Err(Failure::Uda(UdaError::NotSupported(
            "no tray backend for this target".to_string(),
        )))
    }
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

/// The placeholder backend for targets with no platform crate at all - anything
/// that is neither Linux nor Windows.
///
/// It exists so the dispatch helpers compile unchanged there and so every
/// question gets the same typed answer, [`UdaError::NotSupported`], which the C
/// ABI maps to `UDA_ERR_NOT_SUPPORTED`.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
struct UnsupportedBackend;

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl UnsupportedBackend {
    /// cbindgen:ignore
    const REASON: &'static str = "no backend for this target";

    fn not_supported() -> UdaError {
        UdaError::NotSupported(Self::REASON.to_string())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl AppearanceManager for UnsupportedBackend {
    fn detect_theme(&self) -> Result<Theme, UdaError> {
        Err(Self::not_supported())
    }

    fn get_accent_color(&self) -> Result<RgbaColor, UdaError> {
        Err(Self::not_supported())
    }

    fn capabilities(&self) -> Result<uda_core::capability::Capability, UdaError> {
        // No backend means no capability; a host that degrades on empty gets
        // the honest answer instead of a guessed feature set.
        Ok(uda_core::capability::Capability::empty())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
impl WallpaperManager for UnsupportedBackend {
    fn set_wallpaper(&self, _path: &str, _options: &WallpaperOptions) -> Result<(), UdaError> {
        Err(Self::not_supported())
    }

    fn get_wallpaper(&self) -> Result<Option<String>, UdaError> {
        Err(Self::not_supported())
    }

    fn capabilities(&self) -> Result<uda_core::capability::Capability, UdaError> {
        Ok(uda_core::capability::Capability::empty())
    }
}

/// Return a platform appearance manager.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn appearance() -> UnsupportedBackend {
    UnsupportedBackend
}

/// Return a platform wallpaper manager.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn wallpaper() -> UnsupportedBackend {
    UnsupportedBackend
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_codes_match_the_c_header() {
        // Verifies the mapping uses the ABI constants; the numeric values of
        // those constants are pinned as literals in `abi.rs`, so renumbering one
        // cannot pass by comparing a constant to itself.
        assert_eq!(theme_code(Theme::Dark), UDA_THEME_DARK);
        assert_eq!(theme_code(Theme::Light), UDA_THEME_LIGHT);
        // Neither "auto" nor "undeterminable" has a positive C representation.
        assert_eq!(theme_code(Theme::Auto), UDA_THEME_UNKNOWN);
        assert_eq!(theme_code(Theme::Unknown), UDA_THEME_UNKNOWN);
    }

    #[test]
    fn handle_zero_is_not_a_valid_lock() {
        assert_eq!(WakeLockHandle::from_raw(0), None);
        let handle = WakeLockHandle::from_raw(7).expect("non-zero is a handle");
        assert_eq!(handle.raw(), 7);
        assert_eq!(WakeLockHandle::from_raw(handle.raw()), Some(handle));
    }

    #[test]
    fn detect_theme_reports_a_documented_code() {
        match detect_theme_code() {
            // Any of the three codes is acceptable; what matters is that a
            // success neither panics nor returns something outside the
            // documented set.
            Ok(code) => assert!((0..=2).contains(&code), "unexpected theme code {code}"),
            // A target without an appearance backend legitimately refuses the
            // question instead of guessing.
            Err(Failure::Uda(UdaError::NotSupported(_))) => {}
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn set_wallpaper_rejects_an_empty_path() {
        // The guard must fire before any backend is consulted, so an empty
        // path is an argument error on every platform - not the platform's
        // NotSupported answer that used to make this test pass vacuously on
        // targets without a wallpaper backend.
        for path in ["", " ", "\t\n"] {
            match set_wallpaper(path, FillMode::Fill) {
                Err(Failure::Uda(UdaError::InvalidArgument(message))) => {
                    assert!(message.contains("empty"), "message was: {message}");
                }
                other => panic!("expected InvalidArgument for {path:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn releasing_an_unknown_handle_is_an_error_not_a_panic() {
        let bogus = WakeLockHandle::from_raw(0xDEAD_BEEF).expect("synthetic handle");
        let result = release_wakelock(bogus);
        assert!(result.is_err(), "an unknown handle must be rejected");
    }
}
