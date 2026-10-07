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
use uda_core::capability::{RgbaColor, Theme};
use uda_core::error::UdaError;
use uda_core::tray::{TrayIcon, TrayIconConfig};
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
        Theme::Dark => 1,
        Theme::Light => 2,
        // Neither an "auto" preference nor an undeterminable one has a positive
        // answer in the C ABI, so both report `UDA_THEME_UNKNOWN`.
        Theme::Auto | Theme::Unknown => 0,
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
    if let Some(path) = wallpaper().get_wallpaper().ok().flatten() {
        return Ok(Some(path));
    }

    // Tier 3: the platform trait may not implement the read (Linux currently
    // returns `NotSupported`), so probe the standard CLI tools before giving up.
    Ok(cli_wallpaper_path())
}

/// Ask the standard desktop CLIs for the configured wallpaper path.
///
/// The only CLI probed is `gsettings`, which speaks for GNOME-family sessions
/// alone. On KDE, Hyprland or a bare window manager the GNOME schema may still
/// exist (it is commonly installed as a dependency), but nothing applies that
/// value, so reporting it would answer a different desktop's question. The
/// probe is therefore gated on `XDG_CURRENT_DESKTOP`.
///
/// `None` is the answer whenever the probe cannot produce a path - a missing
/// tool, a non-zero exit, empty output, or a non-GNOME session.
fn cli_wallpaper_path() -> Option<String> {
    if !session_is_gnome_family() {
        log::debug!("gsettings wallpaper probe skipped: not a GNOME-family session");
        return None;
    }

    let raw = run_capture(
        "gsettings",
        &["get", "org.gnome.desktop.background", "picture-uri"],
    )?;

    // `gsettings` prints a GVariant string literal, quotes included.
    let unquoted = raw.trim().trim_matches(|c| c == '\'' || c == '"');
    if unquoted.is_empty() {
        return None;
    }
    Some(strip_file_scheme(unquoted))
}

/// Whether `XDG_CURRENT_DESKTOP` names a GNOME-family session.
///
/// The variable is a colon-separated list such as `ubuntu:GNOME`; entries are
/// matched case-insensitively because the spec leaves the case to the session.
/// An unset or unreadable variable means "not GNOME": the probe is a GNOME-only
/// affordance, so absence of evidence defaults to refusing it.
fn session_is_gnome_family() -> bool {
    std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|value| is_gnome_family_desktop(&value))
}

/// [`session_is_gnome_family`] with the environment value injected, so tests
/// never touch process-global state.
fn is_gnome_family_desktop(value: &str) -> bool {
    value.split(':').any(|entry| {
        let entry = entry.trim();
        entry.eq_ignore_ascii_case("GNOME") || entry.eq_ignore_ascii_case("Unity")
    })
}

/// Drop a leading `file://` scheme so callers receive a plain filesystem path.
fn strip_file_scheme(value: &str) -> String {
    value
        .strip_prefix("file://")
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_owned())
}

/// Run a command and return its trimmed stdout when it succeeded and printed.
///
/// A spawn failure, a non-zero exit and empty output all mean "no answer": the
/// probe is best-effort, so the reason is logged for diagnostics and the caller
/// sees `None` instead of a typed error it would only discard.
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let output = match Command::new(program).args(args).output() {
        Ok(output) => output,
        Err(error) => {
            log::debug!("{program} probe failed: {error}");
            return None;
        }
    };

    if !output.status.success() {
        log::debug!("{program} probe exited unsuccessfully: {}", output.status);
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        None
    } else {
        Some(text)
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
        assert_eq!(theme_code(Theme::Dark), 1);
        assert_eq!(theme_code(Theme::Light), 2);
        // Neither "auto" nor "undeterminable" has a positive C representation.
        assert_eq!(theme_code(Theme::Auto), 0);
        assert_eq!(theme_code(Theme::Unknown), 0);
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
    fn the_gsettings_probe_only_trusts_gnome_family_sessions() {
        // Exactly the sessions the GNOME schema actually governs: plain GNOME,
        // the Unity derivative, and colon-separated composite names, all
        // case-insensitively.
        assert!(is_gnome_family_desktop("GNOME"));
        assert!(is_gnome_family_desktop("gnome"));
        assert!(is_gnome_family_desktop("Unity"));
        assert!(is_gnome_family_desktop("ubuntu:GNOME"));
        assert!(is_gnome_family_desktop("KDE:GNOME"));
        assert!(is_gnome_family_desktop(" ubuntu : GNOME "));

        // Everything else must refuse, including entries that merely contain
        // the name as a substring: a per-entry match is what keeps KDE's or a
        // tiling WM's stray GNOME schema from being reported as the wallpaper.
        assert!(!is_gnome_family_desktop("KDE"));
        assert!(!is_gnome_family_desktop("Hyprland"));
        assert!(!is_gnome_family_desktop("GNOME-Flashback"));
        assert!(!is_gnome_family_desktop("unity-session"));
        assert!(!is_gnome_family_desktop(""));
        assert!(!is_gnome_family_desktop(":"));
    }

    #[test]
    fn releasing_an_unknown_handle_is_an_error_not_a_panic() {
        let bogus = WakeLockHandle::from_raw(0xDEAD_BEEF).expect("synthetic handle");
        let result = release_wakelock(bogus);
        assert!(result.is_err(), "an unknown handle must be rejected");
    }
}
