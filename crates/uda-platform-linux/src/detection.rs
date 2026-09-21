use std::env;

use uda_core::error::UdaError;

/// Detected desktop environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DesktopEnvironment {
    #[default]
    Unknown,
    Gnome,
    Kde,
    Xfce,
}

impl DesktopEnvironment {
    /// Infer the desktop environment from environment variables.
    pub fn infer(xdg_current_desktop: &str, desktop_session: &str) -> Self {
        let current = xdg_current_desktop.to_ascii_lowercase();
        let session = desktop_session.to_ascii_lowercase();

        if current.contains("gnome") || session.contains("gnome") {
            DesktopEnvironment::Gnome
        } else if current.contains("kde") || session.contains("kde") || session.contains("plasma") {
            DesktopEnvironment::Kde
        } else if current.contains("xfce") || session.contains("xfce") {
            DesktopEnvironment::Xfce
        } else {
            DesktopEnvironment::Unknown
        }
    }
}

/// Structured Linux desktop environment information.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct EnvironmentInfo {
    /// Detected desktop environment.
    pub desktop_environment: DesktopEnvironment,
    /// `XDG_CURRENT_DESKTOP` value when present.
    pub xdg_current_desktop: Option<String>,
    /// `XDG_SESSION_TYPE` value when present.
    pub xdg_session_type: Option<String>,
    /// `DESKTOP_SESSION` value when present.
    pub desktop_session: Option<String>,
    /// `WAYLAND_DISPLAY` value when present.
    pub wayland_display: Option<String>,
    /// `DISPLAY` value when present.
    pub display: Option<String>,
}

/// Detect the current Linux desktop environment and session information.
///
/// Detection priority for environment variables:
/// `XDG_CURRENT_DESKTOP`, `XDG_SESSION_TYPE`, `DESKTOP_SESSION`,
/// `WAYLAND_DISPLAY`, `DISPLAY`.
pub fn detect_environment() -> Result<EnvironmentInfo, UdaError> {
    let xdg_current_desktop = env::var("XDG_CURRENT_DESKTOP").ok();
    let xdg_session_type = env::var("XDG_SESSION_TYPE").ok();
    let desktop_session = env::var("DESKTOP_SESSION").ok();
    let wayland_display = env::var("WAYLAND_DISPLAY").ok();
    let display = env::var("DISPLAY").ok();

    let desktop_environment = DesktopEnvironment::infer(
        xdg_current_desktop.as_deref().unwrap_or(""),
        desktop_session.as_deref().unwrap_or(""),
    );

    Ok(EnvironmentInfo {
        desktop_environment,
        xdg_current_desktop,
        xdg_session_type,
        desktop_session,
        wayland_display,
        display,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    /// Environment variables are process-global, so tests that mutate them must
    /// not run concurrently. `AGENTS.md` Principle 1 forbids `unwrap()` on
    /// fallible operations; for a poisoned mutex we recover the guard instead of
    /// panicking, because a panic in one test must not cascade into others.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Guard that restores the captured environment snapshot on drop, ensuring
    /// one test never leaks its `XDG_*`/`DISPLAY` values into another test.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn acquire() -> Self {
            let lock = match ENV_LOCK.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            Self { _lock: lock }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for key in [
                "XDG_CURRENT_DESKTOP",
                "XDG_SESSION_TYPE",
                "DESKTOP_SESSION",
                "WAYLAND_DISPLAY",
                "DISPLAY",
            ] {
                env::remove_var(key);
            }
        }
    }

    #[test]
    fn env_gnome() {
        let _guard = EnvGuard::acquire();
        env::set_var("XDG_CURRENT_DESKTOP", "GNOME");
        env::set_var("XDG_SESSION_TYPE", "wayland");
        env::set_var("DESKTOP_SESSION", "gnome");
        env::remove_var("WAYLAND_DISPLAY");
        env::remove_var("DISPLAY");

        let info = match detect_environment() {
            Ok(info) => info,
            Err(e) => panic!("detect_environment failed: {e}"),
        };
        assert_eq!(info.desktop_environment, DesktopEnvironment::Gnome);
        assert_eq!(info.xdg_current_desktop.as_deref(), Some("GNOME"));
        assert_eq!(info.xdg_session_type.as_deref(), Some("wayland"));
        assert_eq!(info.desktop_session.as_deref(), Some("gnome"));
    }

    #[test]
    fn env_kde() {
        let _guard = EnvGuard::acquire();
        env::set_var("XDG_CURRENT_DESKTOP", "KDE");
        env::set_var("DESKTOP_SESSION", "plasma");
        env::remove_var("XDG_SESSION_TYPE");
        env::remove_var("WAYLAND_DISPLAY");
        env::remove_var("DISPLAY");

        let info = match detect_environment() {
            Ok(info) => info,
            Err(e) => panic!("detect_environment failed: {e}"),
        };
        assert_eq!(info.desktop_environment, DesktopEnvironment::Kde);
        assert_eq!(info.xdg_current_desktop.as_deref(), Some("KDE"));
        assert_eq!(info.desktop_session.as_deref(), Some("plasma"));
        assert!(info.xdg_session_type.is_none());
    }

    #[test]
    fn env_xorg() {
        let _guard = EnvGuard::acquire();
        env::set_var("DESKTOP_SESSION", "gnome-xorg");
        env::remove_var("XDG_CURRENT_DESKTOP");
        env::remove_var("XDG_SESSION_TYPE");
        env::remove_var("WAYLAND_DISPLAY");
        env::set_var("DISPLAY", ":0");

        let info = match detect_environment() {
            Ok(info) => info,
            Err(e) => panic!("detect_environment failed: {e}"),
        };
        assert_eq!(info.desktop_environment, DesktopEnvironment::Gnome);
        assert_eq!(info.desktop_session.as_deref(), Some("gnome-xorg"));
        assert_eq!(info.display.as_deref(), Some(":0"));
    }

    #[test]
    fn env_wayland() {
        let _guard = EnvGuard::acquire();
        env::set_var("XDG_SESSION_TYPE", "wayland");
        env::set_var("WAYLAND_DISPLAY", "wayland-0");
        env::remove_var("XDG_CURRENT_DESKTOP");
        env::remove_var("DESKTOP_SESSION");
        env::remove_var("DISPLAY");

        let info = match detect_environment() {
            Ok(info) => info,
            Err(e) => panic!("detect_environment failed: {e}"),
        };
        assert_eq!(info.xdg_session_type.as_deref(), Some("wayland"));
        assert_eq!(info.wayland_display.as_deref(), Some("wayland-0"));
    }

    #[test]
    fn env_unknown() {
        let _guard = EnvGuard::acquire();
        env::remove_var("XDG_CURRENT_DESKTOP");
        env::remove_var("XDG_SESSION_TYPE");
        env::remove_var("DESKTOP_SESSION");
        env::remove_var("WAYLAND_DISPLAY");
        env::remove_var("DISPLAY");

        let info = match detect_environment() {
            Ok(info) => info,
            Err(e) => panic!("detect_environment failed: {e}"),
        };
        assert_eq!(info.desktop_environment, DesktopEnvironment::Unknown);
        assert!(info.xdg_current_desktop.is_none());
        assert!(info.xdg_session_type.is_none());
        assert!(info.desktop_session.is_none());
        assert!(info.wayland_display.is_none());
        assert!(info.display.is_none());
    }
}
