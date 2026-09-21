use std::process::Command;

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};

use crate::detection::{detect_environment, DesktopEnvironment};

/// Linux wallpaper manager.
///
/// # Backend selection
///
/// - **GNOME**: `gsettings` CLI (`org.gnome.desktop.background`).
/// - **KDE Plasma**: D-Bus `org.kde.plasmashell` -> `/PlasmaShell` -> `evaluateScript`.
/// - **Hyprland / Sway**: CLI tools (`hyprpaper` / `swww`) via `std::process::Command`.
/// - **X11 fallback**: `feh` / `nitrogen` CLI tools.
///
/// All external command invocations are performed via `std::process::Command`.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxWallpaperManager;

impl LinuxWallpaperManager {
    pub fn new() -> Self {
        Self
    }

    fn file_uri(path: &str) -> String {
        let path = path.trim();
        if path.starts_with("file://") {
            return path.to_string();
        }
        let encoded = path
            .replace('%', "%25")
            .replace(' ', "%20")
            .replace('#', "%23")
            .replace('?', "%3F");
        format!("file://{}", encoded)
    }

    fn run_command<I, S>(program: &str, args: I) -> Result<(), UdaError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let status = Command::new(program).args(args).status();
        match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(UdaError::DetectionFailed(format!(
                "Command failed with status: {}",
                status.code().unwrap_or(-1)
            ))),
            Err(e) => Err(UdaError::DetectionFailed(format!(
                "Failed to execute command: {}",
                e
            ))),
        }
    }

    fn command_exists(program: &str) -> bool {
        Command::new(program)
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
    }

    /// Set the wallpaper on GNOME via the `gsettings` CLI.
    ///
    /// Both the light (`picture-uri`) and dark (`picture-uri-dark`) keys are
    /// written so the wallpaper survives an appearance switch without a gap.
    fn set_wallpaper_gnome(path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        let uri = Self::file_uri(path);

        let primary_key = if options.dark_mode {
            "picture-uri-dark"
        } else {
            "picture-uri"
        };
        let secondary_key = if options.dark_mode {
            "picture-uri"
        } else {
            "picture-uri-dark"
        };

        Self::run_command(
            "gsettings",
            ["set", "org.gnome.desktop.background", primary_key, &uri],
        )?;
        Self::run_command(
            "gsettings",
            ["set", "org.gnome.desktop.background", secondary_key, &uri],
        )?;

        Ok(())
    }

    /// Build the KDE Plasma `evaluateScript` JavaScript for the given options.
    fn kde_script(path: &str, options: &WallpaperOptions) -> String {
        let uri = Self::file_uri(path);
        format!(
            r#"
            let allDesktops = desktops();
            for (let i = 0; i < allDesktops.length; i++) {{
                let d = allDesktops[i];
                d.wallpaperPlugin = "org.kde.image";
                d.currentConfigGroup = Array("Wallpaper", "org.kde.image", "General");
                d.writeConfig("Image", "{}");
                d.writeConfig("FillMode", {}); // 0=scaled, 1=wallpaper, 2=crop, 3=stretch
            }}
            "#,
            uri,
            Self::kde_fill_mode(options.fill_mode)
        )
    }

    fn kde_fill_mode(fill_mode: FillMode) -> u8 {
        match fill_mode {
            FillMode::Crop => 2,
            FillMode::Fill => 0,
            FillMode::Fit => 1,
            FillMode::Stretch => 3,
        }
    }

    /// Build the `hyprpaper` CLI arguments for the given options.
    fn hyprpaper_args(path: &str, monitor_index: Option<usize>) -> Vec<String> {
        match monitor_index {
            Some(monitor) => vec![
                "wallpaper".to_string(),
                format!("HDMI-{}", monitor),
                path.to_string(),
            ],
            None => vec!["wallpaper".to_string(), ",".to_string(), path.to_string()],
        }
    }

    /// Build the `swww` CLI image arguments for the given options.
    fn swww_img_args(path: &str, monitor_index: Option<usize>) -> Vec<String> {
        let mut img_args = vec!["img".to_string(), path.to_string()];
        if let Some(monitor) = monitor_index {
            img_args.push("-o".to_string());
            img_args.push(format!("output:HDMI-{}", monitor));
        }
        img_args
    }

    /// Build the `feh` CLI arguments for the given options.
    fn feh_args(path: &str, fill_mode: FillMode, monitor_index: Option<usize>) -> Vec<String> {
        let arg = match fill_mode {
            FillMode::Crop => "--bg-center",
            FillMode::Fill => "--bg-fill",
            FillMode::Fit => "--bg-scale",
            FillMode::Stretch => "--bg-fill",
        };
        let mut args = vec![arg.to_string(), path.to_string()];
        if let Some(monitor) = monitor_index {
            args.push("--screen".to_string());
            args.push(monitor.to_string());
        }
        args
    }

    /// Build the `nitrogen` CLI arguments for the given options.
    fn nitrogen_args(path: &str, monitor_index: Option<usize>) -> Vec<String> {
        let head = match monitor_index {
            Some(monitor) => monitor.to_string(),
            None => "0".to_string(),
        };
        vec![
            "--set-zoom-fill".to_string(),
            "--head".to_string(),
            head,
            path.to_string(),
        ]
    }

    async fn set_wallpaper_kde(path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        let script = Self::kde_script(path, options);

        let connection = zbus::Connection::session()
            .await
            .map_err(|e| UdaError::DetectionFailed(format!("zbus session: {e}")))?;

        let proxy = zbus::Proxy::new(
            &connection,
            "org.kde.plasmashell",
            "/PlasmaShell",
            "org.kde.PlasmaShell",
        )
        .await
        .map_err(|e| UdaError::DetectionFailed(format!("zbus proxy: {e}")))?;

        proxy
            .call::<_, _, ()>("evaluateScript", &script)
            .await
            .map_err(|e| UdaError::DetectionFailed(format!("zbus call: {e}")))?;

        Ok(())
    }

    async fn set_wallpaper_hyprland(
        path: &str,
        options: &WallpaperOptions,
    ) -> Result<(), UdaError> {
        if Self::command_exists("hyprpaper") {
            let args = Self::hyprpaper_args(path, options.monitor_index);
            return Self::run_command("hyprpaper", &args);
        }

        if Self::command_exists("swww") {
            let mut init_args = vec!["init".to_string(), "--no-daemon".to_string()];
            if let Some(monitor) = options.monitor_index {
                init_args.push("-o".to_string());
                init_args.push(format!("output:HDMI-{}", monitor));
            }
            Self::run_command("swww", &init_args)?;

            let img_args = Self::swww_img_args(path, options.monitor_index);
            return Self::run_command("swww", &img_args);
        }

        Err(UdaError::NotSupported(
            "Neither hyprpaper nor swww is available".to_string(),
        ))
    }

    async fn set_wallpaper_x11(path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        if Self::command_exists("feh") {
            let args = Self::feh_args(path, options.fill_mode, options.monitor_index);
            return Self::run_command("feh", &args);
        }

        if Self::command_exists("nitrogen") {
            let args = Self::nitrogen_args(path, options.monitor_index);
            return Self::run_command("nitrogen", &args);
        }

        Err(UdaError::NotSupported(
            "Neither feh nor nitrogen is available".to_string(),
        ))
    }
}

impl WallpaperManager for LinuxWallpaperManager {
    fn set_wallpaper(&self, path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("tokio runtime: {e}")))?;

        let env = detect_environment().unwrap_or_default();
        let desktop = env.desktop_environment;

        if matches!(desktop, DesktopEnvironment::Gnome) {
            match Self::set_wallpaper_gnome(path, options) {
                Ok(()) => return Ok(()),
                Err(UdaError::DetectionFailed(_)) => {
                    // gsettings unavailable or failed; fall through to the CLI chain.
                }
                Err(e) => return Err(e),
            }
        }

        if matches!(desktop, DesktopEnvironment::Kde) {
            if let Err(e) = rt.block_on(Self::set_wallpaper_kde(path, options)) {
                if let UdaError::DetectionFailed(_) = e {
                    // Fallback to CLI chain
                } else {
                    return Err(e);
                }
            } else {
                return Ok(());
            }
        }

        if rt
            .block_on(Self::set_wallpaper_hyprland(path, options))
            .is_ok()
        {
            return Ok(());
        }

        rt.block_on(Self::set_wallpaper_x11(path, options))
    }

    fn get_wallpaper(&self) -> Result<Option<String>, UdaError> {
        Err(UdaError::NotSupported(
            "get_wallpaper is not supported in this phase".to_string(),
        ))
    }

    fn capabilities(&self) -> Result<Capability, UdaError> {
        let mut caps = Capability::SET_WALLPAPER;

        if Self::command_exists("gsettings") {
            caps |= Capability::GET_WALLPAPER;
        }

        Ok(caps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_with_spaces() {
        assert_eq!(
            LinuxWallpaperManager::file_uri("/path/with spaces/image.jpg"),
            "file:///path/with%20spaces/image.jpg"
        );
    }

    #[test]
    fn file_uri_encodes_special_characters() {
        assert_eq!(
            LinuxWallpaperManager::file_uri("/path/a%b#c?d.jpg"),
            "file:///path/a%25b%23c%3Fd.jpg"
        );
    }

    #[test]
    fn file_uri_preserves_existing_uri() {
        assert_eq!(
            LinuxWallpaperManager::file_uri("file:///path/中文图片.jpg"),
            "file:///path/中文图片.jpg"
        );
    }

    #[test]
    fn kde_script_fill_mode_mapping() {
        let options = WallpaperOptions {
            fill_mode: FillMode::Crop,
            monitor_index: None,
            dark_mode: false,
        };

        let script = LinuxWallpaperManager::kde_script("/tmp/wall.jpg", &options);
        assert!(script.contains(r#"writeConfig("Image", "file:///tmp/wall.jpg")"#));
        assert!(script.contains("writeConfig(\"FillMode\", 2)"));
    }

    #[test]
    fn kde_script_fit_maps_to_wallpaper_fill_mode() {
        let options = WallpaperOptions {
            fill_mode: FillMode::Fit,
            monitor_index: None,
            dark_mode: false,
        };

        let script = LinuxWallpaperManager::kde_script("/tmp/wall.jpg", &options);
        assert!(script.contains("writeConfig(\"FillMode\", 1)"));
    }

    #[test]
    fn hyprland_command_with_monitor() {
        let args = LinuxWallpaperManager::hyprpaper_args("/tmp/wall.jpg", Some(1));
        assert_eq!(
            args,
            vec![
                "wallpaper".to_string(),
                "HDMI-1".to_string(),
                "/tmp/wall.jpg".to_string(),
            ]
        );

        let args_all = LinuxWallpaperManager::hyprpaper_args("/tmp/wall.jpg", None);
        assert_eq!(
            args_all,
            vec![
                "wallpaper".to_string(),
                ",".to_string(),
                "/tmp/wall.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn swww_args_include_output_when_monitor_set() {
        let args = LinuxWallpaperManager::swww_img_args("/tmp/wall.jpg", Some(2));
        assert_eq!(
            args,
            vec![
                "img".to_string(),
                "/tmp/wall.jpg".to_string(),
                "-o".to_string(),
                "output:HDMI-2".to_string(),
            ]
        );
    }

    #[test]
    fn x11_fallback_chain_priority() {
        // feh is preferred over nitrogen; verify the exact argument vectors.
        let feh = LinuxWallpaperManager::feh_args("/tmp/wall.jpg", FillMode::Fill, None);
        assert_eq!(
            feh,
            vec!["--bg-fill".to_string(), "/tmp/wall.jpg".to_string()]
        );

        let nitrogen = LinuxWallpaperManager::nitrogen_args("/tmp/wall.jpg", Some(2));
        assert_eq!(
            nitrogen,
            vec![
                "--set-zoom-fill".to_string(),
                "--head".to_string(),
                "2".to_string(),
                "/tmp/wall.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn nitrogen_defaults_to_head_zero() {
        let args = LinuxWallpaperManager::nitrogen_args("/tmp/wall.jpg", None);
        assert_eq!(
            args,
            vec![
                "--set-zoom-fill".to_string(),
                "--head".to_string(),
                "0".to_string(),
                "/tmp/wall.jpg".to_string(),
            ]
        );
    }
}
