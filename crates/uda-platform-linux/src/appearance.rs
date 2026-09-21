use std::process::Command;

use uda_core::appearance::AppearanceManager;
use uda_core::capability::{Capability, RgbaColor, Theme};
use uda_core::error::UdaError;

/// Linux appearance manager.
///
/// # Fallback chain
///
/// 1. **XDG Desktop Portal** (`zbus` + `org.freedesktop.portal.Settings`):
///    - `Read("org.freedesktop.appearance", "color-scheme")`
/// 2. **GNOME** (`gsettings`):
///    - `org.gnome.desktop.interface color-scheme`
///    - `org.gnome.desktop.interface gtk-theme`
/// 3. **KDE Plasma** (`kreadconfig5`):
///    - `kdeglobals` -> `General` -> `ColorScheme`
/// 4. **XFCE** (`xfconf-query`):
///    - `xfce4-desktop` related theme settings
/// 5. **Default**: `Theme::Light`
///
/// Accent color detection is GNOME-only in this phase and may return
/// `UdaError::NotSupported` on other desktops or when parsing fails.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxAppearanceManager;

impl LinuxAppearanceManager {
    pub fn new() -> Self {
        Self
    }

    fn run_command<I, S>(program: &str, args: I) -> Result<Option<String>, UdaError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Command::new(program).args(args).output();
        match output {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if text.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(text))
                }
            }
            _ => Ok(None),
        }
    }

    async fn portal_color_scheme() -> Result<Option<u32>, UdaError> {
        let connection = zbus::Connection::session()
            .await
            .map_err(|e| UdaError::DetectionFailed(format!("zbus session: {e}")))?;

        let proxy = zbus::Proxy::new(
            &connection,
            "org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings",
        )
        .await
        .map_err(|e| UdaError::DetectionFailed(format!("zbus proxy: {e}")))?;

        let value: zvariant::OwnedValue = proxy
            .call("Read", &("org.freedesktop.appearance", "color-scheme"))
            .await
            .map_err(|e| UdaError::DetectionFailed(format!("zbus call: {e}")))?;

        match value.downcast_ref::<u32>() {
            Ok(num) => Ok(Some(num)),
            Err(_) => Ok(None),
        }
    }

    fn parse_theme_from_gsettings(value: &str) -> Option<Theme> {
        let v = value
            .trim()
            .trim_start_matches('\'')
            .trim_end_matches('\'')
            .trim();
        match v {
            "1" => Some(Theme::Dark),
            "2" => Some(Theme::Light),
            _ => {
                if v.eq_ignore_ascii_case("prefer-dark") {
                    Some(Theme::Dark)
                } else if v.eq_ignore_ascii_case("prefer-light") {
                    Some(Theme::Light)
                } else {
                    None
                }
            }
        }
    }

    fn parse_theme_from_gtk_theme(value: &str) -> Option<Theme> {
        let v = value
            .trim()
            .trim_start_matches('\'')
            .trim_end_matches('\'')
            .trim();
        if v.to_lowercase().ends_with("-dark") {
            Some(Theme::Dark)
        } else {
            Some(Theme::Light)
        }
    }

    fn detect_gnome_theme() -> Option<Theme> {
        let color_scheme = Self::run_command(
            "gsettings",
            ["get", "org.gnome.desktop.interface", "color-scheme"],
        );
        if let Ok(Some(value)) = color_scheme {
            if let Some(theme) = Self::parse_theme_from_gsettings(&value) {
                return Some(theme);
            }
        }

        let gtk_theme = Self::run_command(
            "gsettings",
            ["get", "org.gnome.desktop.interface", "gtk-theme"],
        );
        if let Ok(Some(value)) = gtk_theme {
            return Self::parse_theme_from_gtk_theme(&value);
        }

        None
    }

    fn detect_kde_theme() -> Option<Theme> {
        let output = Self::run_command(
            "kreadconfig5",
            [
                "--file",
                "kdeglobals",
                "--group",
                "General",
                "--key",
                "ColorScheme",
            ],
        );

        match output {
            Ok(Some(value)) if !value.trim().is_empty() => {
                let v = value.trim().to_lowercase();
                if v.contains("dark") {
                    Some(Theme::Dark)
                } else {
                    Some(Theme::Light)
                }
            }
            _ => None,
        }
    }

    fn detect_xfce_theme() -> Option<Theme> {
        let prop = "xfce4-desktop";
        let key = "/backdrop/screen0/mode";

        let output = Self::run_command("xfconf-query", ["-c", prop, "-p", key]);
        match output {
            Ok(Some(value)) => {
                let v = value.trim().to_lowercase();
                if v.contains("dark") {
                    Some(Theme::Dark)
                } else {
                    Some(Theme::Light)
                }
            }
            _ => None,
        }
    }

    fn detect_gnome_accent_color() -> Result<RgbaColor, UdaError> {
        let output = Self::run_command(
            "gsettings",
            ["get", "org.gnome.desktop.interface", "accent-color"],
        );

        match output {
            Ok(Some(value)) => {
                let v = value
                    .trim()
                    .trim_start_matches('\'')
                    .trim_end_matches('\'')
                    .trim();
                parse_rgba_string(v)
            }
            _ => Err(UdaError::NotSupported(
                "Accent color is not supported on this desktop".into(),
            )),
        }
    }
}

impl AppearanceManager for LinuxAppearanceManager {
    fn detect_theme(&self) -> Result<Theme, UdaError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("tokio runtime: {e}")))?;

        if let Ok(Some(value)) = rt.block_on(Self::portal_color_scheme()) {
            match value {
                1 => return Ok(Theme::Dark),
                2 => return Ok(Theme::Light),
                _ => {}
            }
        }

        if let Some(theme) = Self::detect_gnome_theme() {
            return Ok(theme);
        }

        if let Some(theme) = Self::detect_kde_theme() {
            return Ok(theme);
        }

        if let Some(theme) = Self::detect_xfce_theme() {
            return Ok(theme);
        }

        Ok(Theme::Light)
    }

    fn get_accent_color(&self) -> Result<RgbaColor, UdaError> {
        Self::detect_gnome_accent_color()
    }

    fn capabilities(&self) -> Result<Capability, UdaError> {
        let mut caps = Capability::DETECT_THEME;

        if command_exists("gsettings") {
            caps |= Capability::READ_ACCENT_COLOR;
        }

        Ok(caps)
    }
}

fn command_exists(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn parse_rgba_string(value: &str) -> Result<RgbaColor, UdaError> {
    let value = value.trim().to_lowercase();
    let Some(value) = value
        .strip_prefix("rgba(")
        .or_else(|| value.strip_prefix("rgb("))
    else {
        return Err(UdaError::NotSupported(format!(
            "Unsupported accent color format: {value}"
        )));
    };

    let Some(value) = value.strip_suffix(')') else {
        return Err(UdaError::NotSupported(format!(
            "Unsupported accent color format: {value}"
        )));
    };

    let mut parts = value.split(',');
    let r = parts
        .next()
        .and_then(|part| part.trim().parse::<u8>().ok())
        .ok_or_else(|| UdaError::NotSupported(format!("Invalid accent color: {value}")))?;
    let g = parts
        .next()
        .and_then(|part| part.trim().parse::<u8>().ok())
        .ok_or_else(|| UdaError::NotSupported(format!("Invalid accent color: {value}")))?;
    let b = parts
        .next()
        .and_then(|part| part.trim().parse::<u8>().ok())
        .ok_or_else(|| UdaError::NotSupported(format!("Invalid accent color: {value}")))?;

    let a = parts
        .next()
        .and_then(|part| part.trim().parse::<f32>().ok())
        .map(|value| (value * 255.0).round() as u8)
        .unwrap_or(255);

    Ok(RgbaColor { r, g, b, a })
}
