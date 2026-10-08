use std::process::Stdio;
use std::sync::OnceLock;

use tokio::io::AsyncReadExt;

use uda_core::appearance::AppearanceManager;
use uda_core::capability::{Capability, RgbaColor, Theme};
use uda_core::error::UdaError;

use crate::COMMAND_TIMEOUT;

/// Linux appearance manager.
///
/// # Fallback chain
///
/// 1. **XDG Desktop Portal** (`zbus` + `org.freedesktop.portal.Settings`):
///    - `Read("org.freedesktop.appearance", "color-scheme")`
/// 2. **GNOME** (`gsettings`):
///    - `org.gnome.desktop.interface color-scheme`
///    - `org.gnome.desktop.interface gtk-theme`, trusted for `Light` only when
///      `dconf` shows the key was explicitly set (see
///      `parse_theme_from_gtk_theme`)
/// 3. **KDE Plasma** (`kreadconfig5`):
///    - `kdeglobals` -> `General` -> `ColorScheme`
/// 4. **XFCE** (`xfconf-query`):
///    - `xfce4-desktop` related theme settings
/// 5. **Default**: `Theme::Unknown`
///
/// The fallback is `Unknown` rather than `Light`: on a tiling window manager no
/// desktop component publishes a colour scheme, so the honest answer is "this
/// could not be determined". Returning `Light` would tell a host application
/// something the backend does not actually know.
///
/// Accent color detection is GNOME-only in this phase and may return
/// `UdaError::NotSupported` on other desktops or when parsing fails. GNOME 42
/// through 46 store an `rgb()`/`rgba()` string in the key; GNOME 47 and newer
/// store one of the fixed enum names, which are mapped onto the default HIG
/// palette (`accent_color_from_name`).
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxAppearanceManager;

/// One finished tool invocation: whether it exited cleanly and what it wrote to
/// stdout.
struct ToolRun {
    success: bool,
    stdout: Vec<u8>,
}

impl LinuxAppearanceManager {
    pub fn new() -> Self {
        Self
    }

    /// Run `program` under `COMMAND_TIMEOUT` and collect what it printed.
    ///
    /// Every failure mode - the tool missing, no stdout pipe, unreadable output,
    /// a timeout - collapses to `None`, and a run that is abandoned is killed so
    /// no helper lingers. Callers decide whether a failed run means "absent
    /// source" ([`Self::run_command`]) or "the probe cannot answer"
    /// ([`Self::probe_command`]).
    async fn run_tool(program: &str, args: &[&str]) -> Option<ToolRun> {
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .ok()?;

        let mut stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => {
                let _ = child.start_kill();
                return None;
            }
        };

        let mut bytes = Vec::new();
        let read = stdout.read_to_end(&mut bytes);
        let wait = child.wait();

        // Reading and waiting run concurrently, so a chatty tool cannot fill the
        // pipe and deadlock while it is waited on.
        match tokio::time::timeout(COMMAND_TIMEOUT, async { tokio::join!(read, wait) }).await {
            Ok((Ok(_), Ok(status))) => Some(ToolRun {
                success: status.success(),
                stdout: bytes,
            }),
            _ => {
                // A timeout leaves the tool running and a broken read or wait
                // leaves its fate unknown; either way this run is over.
                let _ = child.start_kill();
                None
            }
        }
    }

    /// Run a tool and return its trimmed stdout.
    ///
    /// Theme detection is a cascade of optional sources, so one source that is
    /// missing, failed or wedged must not fail the whole detection: it reads as
    /// absent.
    async fn run_command(program: &str, args: &[&str]) -> Option<String> {
        let run = Self::run_tool(program, args).await?;
        if !run.success {
            return None;
        }
        let text = String::from_utf8_lossy(&run.stdout).trim().to_string();
        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }

    /// Whether a probe command runs, exits cleanly and prints anything.
    ///
    /// Used for capability probing, where "the schema key exists" is the
    /// question; unlike the theme readers, the answer must not be swallowed
    /// silently. Any output counts, even whitespace: the probe asks whether the
    /// key answered at all, not whether its value parses.
    async fn probe_command(program: &str, args: &[&str]) -> bool {
        match Self::run_tool(program, args).await {
            Some(run) => run.success && !run.stdout.is_empty(),
            None => false,
        }
    }

    /// Read the XDG settings portal's `color-scheme` key.
    ///
    /// Every round trip is bounded by [`crate::DBUS_TIMEOUT`]; the connection and
    /// the proxy live inside this call, mirroring the other backends. A failure
    /// is [`UdaError::DetectionFailed`], which the theme cascade reads as "the
    /// portal has no answer" and moves on from.
    async fn portal_color_scheme() -> Result<Option<u32>, UdaError> {
        let connection = crate::detect_dbus("zbus session", zbus::Connection::session()).await?;

        let proxy = crate::detect_dbus(
            "zbus proxy",
            zbus::Proxy::new(
                &connection,
                "org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings",
            ),
        )
        .await?;

        let value: zvariant::OwnedValue = crate::detect_dbus(
            "zbus call",
            proxy.call("Read", &("org.freedesktop.appearance", "color-scheme")),
        )
        .await?;

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

    /// The bare theme name inside a `gsettings get` value.
    ///
    /// gsettings wraps string values in single quotes and callers may hand the
    /// value through with surrounding whitespace; both are stripped. Pure so
    /// the quote rules are testable without gsettings.
    fn gtk_theme_name(value: &str) -> &str {
        value
            .trim()
            .trim_start_matches('\'')
            .trim_end_matches('\'')
            .trim()
    }

    /// Whether a raw `gtk-theme` value names a dark theme.
    ///
    /// GNOME marks dark themes with a `-dark` suffix in any casing
    /// (`Adwaita-dark`, `Yaru-DARK`). Pure so the matching rule is testable
    /// without gsettings.
    fn gtk_theme_name_is_dark(value: &str) -> bool {
        Self::gtk_theme_name(value)
            .to_lowercase()
            .ends_with("-dark")
    }

    /// Decide the theme from the `gtk-theme` key alone.
    ///
    /// A `-dark` suffix answers [`Theme::Dark`] on its own. Any other name is
    /// only [`Theme::Light`] when `key_explicitly_set` proves the key was
    /// actually written: `gsettings get` reports the schema default just the
    /// same as a stored value, so in an environment without the settings
    /// portal there is no way to tell "the user chose a light theme" from
    /// "nobody ever chose anything" without that proof. A never-configured
    /// key therefore yields `None`, which lets the cascade end in the honest
    /// [`Theme::Unknown`] instead of a guessed `Light` - the case that
    /// motivated this rule is a WSL host whose GTK wrapper exports the schema
    /// with its untouched `'Adwaita'` default.
    fn parse_theme_from_gtk_theme(value: &str, key_explicitly_set: bool) -> Option<Theme> {
        if Self::gtk_theme_name_is_dark(value) {
            return Some(Theme::Dark);
        }
        let name = Self::gtk_theme_name(value);
        if key_explicitly_set && !name.is_empty() {
            return Some(Theme::Light);
        }
        None
    }

    /// Whether the `gtk-theme` key holds an explicitly stored value.
    ///
    /// `dconf read` prints exactly what is stored in the user's dconf
    /// database, so a non-empty answer is proof the key was written by the
    /// user (or by a tool acting for them), while empty output means it was
    /// never set. A missing `dconf` binary reads the same way through the
    /// tool runner: without the database there is no proof of an explicit
    /// choice either.
    async fn gtk_theme_key_explicitly_set() -> bool {
        Self::run_command("dconf", &["read", "/org/gnome/desktop/interface/gtk-theme"])
            .await
            .is_some()
    }

    async fn detect_gnome_theme() -> Option<Theme> {
        let color_scheme = Self::run_command(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "color-scheme"],
        )
        .await;
        if let Some(theme) = color_scheme.and_then(|value| Self::parse_theme_from_gsettings(&value))
        {
            return Some(theme);
        }

        let gtk_theme = Self::run_command(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "gtk-theme"],
        )
        .await?;
        if Self::gtk_theme_name_is_dark(&gtk_theme) {
            return Some(Theme::Dark);
        }
        // Not a dark name: only an explicitly chosen light theme may be
        // reported as `Light`. The dconf lookup is what keeps a
        // never-configured schema default (WSL's untouched `'Adwaita'`) from
        // being read as an answer.
        let key_explicitly_set = Self::gtk_theme_key_explicitly_set().await;
        Self::parse_theme_from_gtk_theme(&gtk_theme, key_explicitly_set)
    }

    async fn detect_kde_theme() -> Option<Theme> {
        let output = Self::run_command(
            "kreadconfig5",
            &[
                "--file",
                "kdeglobals",
                "--group",
                "General",
                "--key",
                "ColorScheme",
            ],
        )
        .await;

        let value = output?;
        if value.is_empty() {
            return None;
        }
        if value.to_lowercase().contains("dark") {
            Some(Theme::Dark)
        } else {
            Some(Theme::Light)
        }
    }

    async fn detect_xfce_theme() -> Option<Theme> {
        let prop = "xfce4-desktop";
        let key = "/backdrop/screen0/mode";

        let value = Self::run_command("xfconf-query", &["-c", prop, "-p", key]).await?;
        if value.to_lowercase().contains("dark") {
            Some(Theme::Dark)
        } else {
            Some(Theme::Light)
        }
    }

    async fn detect_gnome_accent_color() -> Result<RgbaColor, UdaError> {
        let value = Self::run_command(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "accent-color"],
        )
        .await
        .ok_or_else(|| {
            UdaError::NotSupported("Accent color is not supported on this desktop".to_string())
        })?;

        let v = value
            .trim()
            .trim_start_matches('\'')
            .trim_end_matches('\'')
            .trim();

        // GNOME 47+ publishes one of the fixed enum names; older GNOME stored
        // a raw rgb()/rgba() string. Both spellings are accepted here.
        match accent_color_from_name(v) {
            Some(color) => Ok(color),
            None => parse_rgba_string(v),
        }
    }

    /// Whether GNOME publishes the `accent-color` key at all.
    ///
    /// GNOME 42..46 store `rgb()` strings and GNOME 47+ store the enum names;
    /// anything older has no key, so a bare "gsettings exists" check used to
    /// advertise a read that could never succeed (P1-21). The probe result is
    /// cached: the key cannot appear mid-process, and `capabilities()` may be
    /// called from hot paths.
    fn accent_color_key_available() -> bool {
        static AVAILABLE: OnceLock<bool> = OnceLock::new();
        *AVAILABLE.get_or_init(|| {
            crate::sync::run_async(Self::probe_command(
                "gsettings",
                &["get", "org.gnome.desktop.interface", "accent-color"],
            ))
            .unwrap_or(false)
        })
    }
}

/// Map a GNOME 47+ `accent-color` enum name onto the default HIG palette.
///
/// GNOME 47 replaced the `rgb()`/`rgba()` strings previously stored in
/// `org.gnome.desktop.interface accent-color` with a fixed set of names. The
/// values below are the default (non-custom) accent colours from the GNOME HIG
/// / libadwaita palette; a theme that overrides them cannot be detected from
/// the key alone, so these are documented approximations, not measurements.
fn accent_color_from_name(name: &str) -> Option<RgbaColor> {
    // gsettings wraps values in single quotes; tolerate them here so callers
    // can pass the raw key output.
    let name = name.trim().trim_matches('\'').trim();
    let (r, g, b) = match name.to_ascii_lowercase().as_str() {
        "blue" => (0x35, 0x84, 0xe4),
        "teal" => (0x21, 0x90, 0xa4),
        "green" => (0x3a, 0x94, 0x4a),
        "yellow" => (0xc8, 0x88, 0x00),
        "orange" => (0xed, 0x5b, 0x00),
        "red" => (0xe6, 0x2d, 0x42),
        "pink" => (0xd5, 0x61, 0x99),
        "purple" => (0x91, 0x41, 0xac),
        "slate" => (0x6f, 0x83, 0x96),
        _ => return None,
    };
    Some(RgbaColor { r, g, b, a: 255 })
}

impl AppearanceManager for LinuxAppearanceManager {
    fn detect_theme(&self) -> Result<Theme, UdaError> {
        crate::sync::run_async(async {
            if let Ok(Some(value)) = Self::portal_color_scheme().await {
                match value {
                    1 => return Ok(Theme::Dark),
                    2 => return Ok(Theme::Light),
                    _ => {}
                }
            }

            if let Some(theme) = Self::detect_gnome_theme().await {
                return Ok(theme);
            }

            if let Some(theme) = Self::detect_kde_theme().await {
                return Ok(theme);
            }

            if let Some(theme) = Self::detect_xfce_theme().await {
                return Ok(theme);
            }

            Ok(Theme::Unknown)
        })?
    }

    fn get_accent_color(&self) -> Result<RgbaColor, UdaError> {
        crate::sync::run_async(Self::detect_gnome_accent_color())?
    }

    fn capabilities(&self) -> Result<Capability, UdaError> {
        let mut caps = Capability::DETECT_THEME;

        // `READ_ACCENT_COLOR` is only honest when GNOME actually publishes the
        // key; advertising it on a bare "gsettings exists" made every read
        // fail with NotSupported (P1-21).
        if Self::accent_color_key_available() {
            caps |= Capability::READ_ACCENT_COLOR;
        }

        Ok(caps)
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accent_enum_names_map_onto_the_default_hig_palette() {
        // Every name GNOME 47 can store in `accent-color` must resolve; a miss
        // here is what made the read fail unconditionally before (P1-21).
        for name in [
            "blue", "teal", "green", "yellow", "orange", "red", "pink", "purple", "slate",
        ] {
            assert!(
                accent_color_from_name(name).is_some(),
                "{name} must map onto the default palette"
            );
        }
    }

    #[test]
    fn accent_enum_values_are_opaque_and_stable() {
        // Documented approximations from the GNOME HIG / libadwaita palette.
        assert_eq!(
            accent_color_from_name("blue"),
            Some(RgbaColor {
                r: 0x35,
                g: 0x84,
                b: 0xe4,
                a: 255
            })
        );
        assert_eq!(
            accent_color_from_name("slate"),
            Some(RgbaColor {
                r: 0x6f,
                g: 0x83,
                b: 0x96,
                a: 255
            })
        );
    }

    #[test]
    fn accent_enum_names_are_matched_case_insensitively() {
        assert_eq!(
            accent_color_from_name("Blue"),
            accent_color_from_name("blue")
        );
        assert_eq!(
            accent_color_from_name("'teal'"),
            accent_color_from_name("teal")
        );
    }

    #[test]
    fn unknown_accent_names_do_not_map() {
        // Non-enum values fall through to the rgb()/rgba() parser instead.
        assert!(accent_color_from_name("rgb(1, 2, 3)").is_none());
        assert!(accent_color_from_name("").is_none());
        assert!(accent_color_from_name("magenta").is_none());
    }

    #[test]
    fn rgb_and_rgba_strings_still_parse() {
        assert_eq!(
            parse_rgba_string("rgb(53, 132, 228)").ok(),
            Some(RgbaColor {
                r: 53,
                g: 132,
                b: 228,
                a: 255
            })
        );
        assert_eq!(
            parse_rgba_string("rgba(53, 132, 228, 0.5)").ok(),
            Some(RgbaColor {
                r: 53,
                g: 132,
                b: 228,
                a: 128
            })
        );
    }

    #[test]
    fn an_unrecognised_accent_format_is_rejected_not_guessed() {
        assert!(parse_rgba_string("#3584e4").is_err());
        assert!(parse_rgba_string("").is_err());
        assert!(parse_rgba_string("rgb(300, 0, 0)").is_err());
    }

    #[test]
    fn quoted_gsettings_theme_values_parse() {
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gsettings("'prefer-dark'"),
            Some(Theme::Dark)
        );
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gsettings("prefer-light"),
            Some(Theme::Light)
        );
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gsettings("'default'"),
            None
        );
    }

    #[test]
    fn a_gtk_theme_dark_suffix_answers_dark_on_its_own() {
        // The suffix is a positive signal, so it decides without provenance and
        // in any casing or quoting.
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("'Adwaita-dark'", false),
            Some(Theme::Dark)
        );
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("Yaru-DARK", false),
            Some(Theme::Dark)
        );
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme(
                "' Flat-Remix-GTK-Blue-Dark '",
                true
            ),
            Some(Theme::Dark)
        );
    }

    #[test]
    fn a_gtk_theme_light_answer_requires_proof_the_key_was_set() {
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("'Adwaita'", true),
            Some(Theme::Light)
        );
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("Ambiance", true),
            Some(Theme::Light)
        );
    }

    #[test]
    fn a_never_set_gtk_theme_key_is_unknown_not_light() {
        // The maintainer's WSL report: the schema default 'Adwaita' leaked
        // through `gsettings get` and was reported as `Light` although nobody
        // had ever chosen anything. Without dconf proof there is no answer.
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("'Adwaita'", false),
            None
        );
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("Adwaita", false),
            None
        );
    }

    #[test]
    fn a_missing_dconf_reads_as_unproven_light() {
        // `dconf read` missing and empty output both collapse to `false` by the
        // tool runner's contract (a failed run is `None`, so `is_some()` is
        // `false`); this pins that "cannot prove" and "unset" get the same
        // treatment: no `Light` answer.
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("'Ambiance'", false),
            None
        );
    }

    #[test]
    fn an_empty_gtk_theme_name_is_not_an_answer_even_if_set() {
        // `''` survives the tool runner (two quote characters are non-empty
        // stdout), but a nameless theme says nothing about light or dark.
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("''", true),
            None
        );
        assert_eq!(
            LinuxAppearanceManager::parse_theme_from_gtk_theme("", false),
            None
        );
    }

    #[test]
    fn gtk_theme_name_strips_quotes_and_whitespace() {
        assert_eq!(
            LinuxAppearanceManager::gtk_theme_name("' Adwaita '"),
            "Adwaita"
        );
        assert_eq!(LinuxAppearanceManager::gtk_theme_name("Yaru"), "Yaru");
        assert_eq!(LinuxAppearanceManager::gtk_theme_name("''"), "");
    }
}
