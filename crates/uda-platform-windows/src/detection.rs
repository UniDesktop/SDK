//! Windows environment detection.
//!
//! # Backend selection
//!
//! Linux has `XDG_CURRENT_DESKTOP`, `DESKTOP_SESSION` and friends; Windows has no
//! equivalent because there is no pluggable desktop environment to detect. The
//! only meaningful facts are the OS version (which decides whether the modern
//! personalization registry values exist) and whether the process is running in
//! a Terminal Services / remote session (which restricts some shell APIs).
//!
//! `RtlGetVersion` is used instead of the deprecated `GetVersionExW` because the
//! latter reports a fake version unless the application carries a compatibility
//! manifest, which would make Phase 1 feature gating unreliable.

use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

use uda_core::error::UdaError;

/// Detected Windows release family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WindowsRelease {
    /// Windows 7 / Windows Server 2008 R2 (build < 9600).
    ///
    /// The `AppsUseLightTheme` personalization value does not exist, so
    /// dark/light detection degrades to
    /// [`UdaError::NotSupported`](uda_core::error::UdaError::NotSupported).
    #[default]
    Legacy,
    /// Windows 8 / Windows Server 2012 (build 9200..=9600).
    Eight,
    /// Windows 10 version 1507..=1607 (build 10240..=14393).
    Ten,
    /// Windows 10 version 1709 or newer, and every Windows 11 build (>= 16299).
    ///
    /// This is the family where `AppsUseLightTheme` is guaranteed to be present.
    Modern,
}

impl WindowsRelease {
    /// Classify a build number into a release family.
    pub fn from_build(build: u32) -> Self {
        match build {
            0..=9199 => WindowsRelease::Legacy,
            9200..=9600 => WindowsRelease::Eight,
            9601..=16298 => WindowsRelease::Ten,
            _ => WindowsRelease::Modern,
        }
    }

    /// Whether the modern personalization registry values are expected to exist.
    pub fn supports_dark_mode_registry(&self) -> bool {
        matches!(self, WindowsRelease::Modern)
    }
}

/// Structured Windows environment information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsEnvironmentInfo {
    /// Detected release family.
    pub release: WindowsRelease,
    /// Reported major version.
    pub major_version: u32,
    /// Reported minor version.
    pub minor_version: u32,
    /// Reported build number.
    pub build_number: u32,
    /// Service pack string, empty on modern builds.
    pub service_pack: String,
}

impl Default for WindowsEnvironmentInfo {
    fn default() -> Self {
        Self {
            release: WindowsRelease::Legacy,
            major_version: 0,
            minor_version: 0,
            build_number: 0,
            service_pack: String::new(),
        }
    }
}

/// Read the OS version through the undocumented-but-stable `RtlGetVersion`.
///
/// `OSVERSIONINFOW` is zero-initialised before the call and its
/// `dwOSVersionInfoSize` field must be set to the structure's byte length,
/// otherwise the kernel rejects the request.
fn query_os_version() -> Result<WindowsEnvironmentInfo, UdaError> {
    // SAFETY: `OSVERSIONINFOW` is a plain-old-data struct with no invalid bit
    // patterns, so an all-zero value is a valid instance. Every field is then
    // either overwritten by `RtlGetVersion` or left as a meaningful zero
    // (empty service-pack string), and `dwOSVersionInfoSize` is set explicitly
    // below before the call.
    let mut info: OSVERSIONINFOW = unsafe { std::mem::zeroed() };
    let expected_len = u32::try_from(std::mem::size_of::<OSVERSIONINFOW>())
        .map_err(|_| UdaError::Internal("OSVERSIONINFOW size overflow".to_string()))?;
    info.dwOSVersionInfoSize = expected_len;

    // SAFETY: `info` is a live stack structure whose `dwOSVersionInfoSize` was
    // set to its exact byte length above. `RtlGetVersion` writes only within the
    // declared size and returns `STATUS_SUCCESS` (0) on success.
    let status = unsafe { ntdll::RtlGetVersion(&mut info) };

    if status != 0 {
        return Err(UdaError::Internal(format!(
            "RtlGetVersion failed with NTSTATUS {status:#x}"
        )));
    }

    let build_number = info.dwBuildNumber;
    Ok(WindowsEnvironmentInfo {
        release: WindowsRelease::from_build(build_number),
        major_version: info.dwMajorVersion,
        minor_version: info.dwMinorVersion,
        build_number,
        service_pack: from_wide_null_terminated(&info.szCSDVersion),
    })
}

/// Detect the current Windows environment.
pub fn detect_environment() -> Result<WindowsEnvironmentInfo, UdaError> {
    query_os_version()
}

/// Convert a fixed-size UTF-16 array into a `String`, stopping at the first NUL.
fn from_wide_null_terminated(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// Minimal `ntdll` bindings.
///
/// The `windows` crate deliberately does not expose `RtlGetVersion` (it is not a
/// supported API), so the single function we need is declared locally. Linking
/// against `ntdll` is safe: every Windows process already has it loaded.
mod ntdll {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    #[link(name = "ntdll")]
    extern "system" {
        /// Returns the real OS version, unaffected by compatibility manifests.
        ///
        /// Returns `STATUS_SUCCESS` (0) on success.
        pub fn RtlGetVersion(lpVersionInformation: *mut OSVERSIONINFOW) -> i32;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_numbers_map_to_release_families() {
        assert_eq!(WindowsRelease::from_build(7601), WindowsRelease::Legacy);
        assert_eq!(WindowsRelease::from_build(9200), WindowsRelease::Eight);
        assert_eq!(WindowsRelease::from_build(9600), WindowsRelease::Eight);
        assert_eq!(WindowsRelease::from_build(10240), WindowsRelease::Ten);
        assert_eq!(WindowsRelease::from_build(14393), WindowsRelease::Ten);
        assert_eq!(WindowsRelease::from_build(16299), WindowsRelease::Modern);
        assert_eq!(WindowsRelease::from_build(22000), WindowsRelease::Modern);
        assert_eq!(WindowsRelease::from_build(26100), WindowsRelease::Modern);
    }

    #[test]
    fn legacy_and_eight_do_not_expose_dark_mode_registry() {
        assert!(!WindowsRelease::Legacy.supports_dark_mode_registry());
        assert!(!WindowsRelease::Eight.supports_dark_mode_registry());
        assert!(!WindowsRelease::Ten.supports_dark_mode_registry());
    }

    #[test]
    fn modern_release_exposes_dark_mode_registry() {
        assert!(WindowsRelease::Modern.supports_dark_mode_registry());
    }

    #[test]
    fn default_release_is_legacy() {
        // A conservative default: assume the oldest supported OS until proven
        // otherwise, so features are never advertised before they exist.
        assert_eq!(
            WindowsEnvironmentInfo::default().release,
            WindowsRelease::Legacy
        );
    }

    #[test]
    fn zero_build_is_legacy() {
        assert_eq!(WindowsRelease::from_build(0), WindowsRelease::Legacy);
    }

    #[test]
    fn from_wide_stops_at_null_terminator() {
        let buffer = [u16::from(b'S'), u16::from(b'P'), 0, u16::from(b'x')];
        assert_eq!(from_wide_null_terminated(&buffer), "SP");
    }

    #[test]
    fn from_wide_handles_empty_buffer() {
        assert_eq!(from_wide_null_terminated(&[0]), "");
        assert_eq!(from_wide_null_terminated(&[]), "");
    }

    #[test]
    fn detect_environment_reports_a_sane_release() {
        // This runs on the host Windows machine during `cargo test`, so the
        // values must be plausible rather than hard-coded.
        let info = match detect_environment() {
            Ok(info) => info,
            Err(e) => panic!("detect_environment failed: {e}"),
        };
        assert!(info.major_version >= 6);
        assert!(info.build_number >= 7600);
        assert!(!info.service_pack.contains(char::from(0)));
    }
}
