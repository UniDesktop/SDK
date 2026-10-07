use std::process::Stdio;
use std::time::Duration;

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};

use crate::detection::{detect_environment, DesktopEnvironment, EnvironmentInfo};
use crate::COMMAND_TIMEOUT;

/// How long a freshly spawned `swww` daemon gets to create its IPC socket.
///
/// Two seconds is far more than the socket needs on a healthy machine while
/// keeping a daemon that never comes up from stalling the caller (P0-3).
const SWWW_STARTUP_BUDGET: Duration = Duration::from_secs(2);

/// How long to wait between two `swww query` probes while its daemon starts.
const SWWW_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Linux wallpaper manager.
///
/// # Backend selection
///
/// - **GNOME**: `gsettings` CLI (`org.gnome.desktop.background`).
/// - **KDE Plasma**: D-Bus `org.kde.plasmashell` -> `/PlasmaShell` -> `evaluateScript`.
/// - **Hyprland / Sway**: `hyprctl hyprpaper` (IPC front-end of the hyprpaper
///   daemon), then `swww`; both are Tier-3 CLI tools.
/// - **X11 fallback**: `feh` / `nitrogen` CLI tools.
///
/// Every external tool runs under [`COMMAND_TIMEOUT`], and a tool that had to be
/// killed is reported rather than treated as a soft miss.
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

    /// Decode a GNOME wallpaper value (`'file:///path'`) back into a filesystem
    /// path, undoing exactly the escaping [`Self::file_uri`] applies.
    ///
    /// A value that is not a `file://` URI (an unset or foreign key) yields
    /// `None` rather than a made-up path.
    fn strip_file_uri(value: &str) -> Option<String> {
        let value = value.trim().trim_matches('\'').trim_matches('"').trim();
        let path = value.strip_prefix("file://")?;
        Some(percent_decode(path))
    }

    /// Whether GNOME's `color-scheme` key says the dark preference is active.
    fn prefers_dark_from_color_scheme(value: &str) -> bool {
        let value = value.trim().trim_matches('\'').trim();
        value.eq_ignore_ascii_case("prefer-dark")
    }

    /// Start a tool with quiet standard streams and wait for it under
    /// [`COMMAND_TIMEOUT`].
    ///
    /// Shared by [`Self::run_command`], [`Self::command_output`] and
    /// [`Self::command_exists`] so the spawn flags and the kill-on-timeout rule
    /// exist exactly once. `kill_on_drop` is what makes abandonment safe: when
    /// the timeout drops the wait, the child is killed instead of being left
    /// behind. A tool that had to be killed is [`UdaError::CommandFailed`]; a
    /// tool that ran and failed is [`UdaError::DetectionFailed`], which the
    /// cascade reads as "this tier did not work, try the next one".
    async fn quiet_command<I, S>(
        program: &str,
        args: I,
        stdout: Stdio,
    ) -> Result<std::process::Output, UdaError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let child = tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| UdaError::DetectionFailed(format!("failed to execute {program}: {e}")))?;

        match tokio::time::timeout(COMMAND_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => Ok(output),
            Ok(Err(e)) => Err(UdaError::DetectionFailed(format!("{program}: {e}"))),
            Err(_) => Err(UdaError::CommandFailed(format!(
                "{program} timed out after {COMMAND_TIMEOUT:?}"
            ))),
        }
    }

    /// Run a program to completion with a hard timeout, discarding its output.
    async fn run_command<I, S>(program: &str, args: I) -> Result<(), UdaError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let output = Self::quiet_command(program, args, Stdio::null()).await?;
        if output.status.success() {
            return Ok(());
        }
        Err(UdaError::DetectionFailed(format!(
            "{program} exited with status {}",
            output.status.code().unwrap_or(-1)
        )))
    }

    /// Run a program with a hard timeout and capture its trimmed stdout.
    async fn command_output(program: &str, args: &[&str]) -> Result<Option<String>, UdaError> {
        let output = Self::quiet_command(program, args, Stdio::piped()).await?;
        if !output.status.success() {
            return Err(UdaError::DetectionFailed(format!(
                "{program} exited with status {}",
                output.status.code().unwrap_or(-1)
            )));
        }
        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            Ok(None)
        } else {
            Ok(Some(text))
        }
    }

    /// Whether a tool can be started at all.
    ///
    /// Existence is "the process ran", not "it exited cleanly": some of the
    /// probed tools do not implement `--version` and would otherwise look
    /// missing. A tool that never answers within [`COMMAND_TIMEOUT`] does not
    /// count as available.
    async fn command_exists(program: &str) -> bool {
        Self::quiet_command(program, ["--version"], Stdio::null())
            .await
            .is_ok()
    }

    /// Set the wallpaper on GNOME via the `gsettings` CLI.
    ///
    /// Both the light (`picture-uri`) and dark (`picture-uri-dark`) keys are
    /// written so the wallpaper survives an appearance switch without a gap.
    async fn set_wallpaper_gnome(path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
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
        )
        .await?;
        Self::run_command(
            "gsettings",
            ["set", "org.gnome.desktop.background", secondary_key, &uri],
        )
        .await?;

        Ok(())
    }

    /// Read the current wallpaper on GNOME through the `gsettings` CLI.
    ///
    /// The dark key is consulted first while the color scheme prefers dark,
    /// mirroring the write path and `docs/internals/wallpaper_specs.md`; the
    /// light key is the fallback for GNOME releases that only publish it.
    async fn get_wallpaper_gnome() -> Result<Option<String>, UdaError> {
        let scheme = Self::command_output(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "color-scheme"],
        )
        .await;
        // A key that cannot be read is the same as "not dark": the light key is
        // consulted first and the dark one still gets its turn as the fallback.
        let prefer_dark = scheme
            .ok()
            .flatten()
            .is_some_and(|value| Self::prefers_dark_from_color_scheme(&value));

        let (primary, secondary) = if prefer_dark {
            ("picture-uri-dark", "picture-uri")
        } else {
            ("picture-uri", "picture-uri-dark")
        };

        for key in [primary, secondary] {
            let value = match Self::command_output(
                "gsettings",
                &["get", "org.gnome.desktop.background", key],
            )
            .await
            {
                Ok(value) => value,
                // A missing key is not a broken backend; try the next one.
                Err(_) => continue,
            };
            if let Some(path) = value.as_deref().and_then(Self::strip_file_uri) {
                return Ok(Some(path));
            }
        }

        Ok(None)
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

    /// Set the wallpaper on KDE Plasma via `evaluateScript`.
    ///
    /// Every D-Bus round trip is bounded by [`DBUS_TIMEOUT`] so a wedged
    /// `plasmashell` costs a bounded wait instead of a hung caller (P2-39). A
    /// failure is reported as [`UdaError::DetectionFailed`], the variant that
    /// lets [`WallpaperManager::set_wallpaper`] fall through to the CLI chain.
    async fn set_wallpaper_kde(path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        let script = Self::kde_script(path, options);

        let connection = crate::detect_dbus("zbus session", zbus::Connection::session()).await?;

        let proxy = crate::detect_dbus(
            "zbus proxy",
            zbus::Proxy::new(
                &connection,
                "org.kde.plasmashell",
                "/PlasmaShell",
                "org.kde.PlasmaShell",
            ),
        )
        .await?;

        crate::detect_dbus(
            "zbus call",
            proxy.call::<_, _, ()>("evaluateScript", &script),
        )
        .await
    }

    /// Set the wallpaper through the Hyprland tool chain (Tier 3 CLI).
    ///
    /// Two independent tools are supported, tried in order:
    ///
    /// - **hyprpaper**, driven through `hyprctl hyprpaper` - hyprpaper itself
    ///   has no usable CLI; its control surface is the IPC front-end `hyprctl`
    ///   exposes (`preload` + `wallpaper "MON,/path"`).
    /// - **swww**, with the daemon started on demand.
    ///
    /// Unlike the GNOME/KDE tiers, a failure here is *reported*: the caller
    /// decides whether the X11 fallback makes sense (see [`WallpaperManager::
    /// set_wallpaper`]).
    async fn set_wallpaper_hyprland(
        path: &str,
        options: &WallpaperOptions,
    ) -> Result<(), UdaError> {
        if Self::command_exists("hyprctl").await {
            match Self::set_wallpaper_hyprpaper(path, options).await {
                Ok(()) => return Ok(()),
                Err(e) => log::debug!("hyprpaper via hyprctl failed: {e}"),
            }
        }

        if Self::command_exists("swww").await {
            return Self::set_wallpaper_swww(path, options).await;
        }

        Err(UdaError::NotSupported(
            "neither hyprpaper (via hyprctl) nor swww is available".to_string(),
        ))
    }

    /// `hyprctl hyprpaper preload <path>` argument vector.
    fn hyprpaper_preload_args(path: &str) -> Vec<String> {
        vec![
            "hyprpaper".to_string(),
            "preload".to_string(),
            path.to_string(),
        ]
    }

    /// `hyprctl hyprpaper wallpaper "<monitor>,<path>"` argument vector.
    ///
    /// The empty monitor name is hyprpaper's wildcard for every output.
    fn hyprpaper_wallpaper_args(monitor: &str, path: &str) -> Vec<String> {
        vec![
            "hyprpaper".to_string(),
            "wallpaper".to_string(),
            format!("{monitor},{path}"),
        ]
    }

    /// Wallpaper through hyprpaper, per `docs/internals/wallpaper_specs.md`:
    /// preload the image first, then point each monitor at it.
    async fn set_wallpaper_hyprpaper(
        path: &str,
        options: &WallpaperOptions,
    ) -> Result<(), UdaError> {
        Self::run_command("hyprctl", Self::hyprpaper_preload_args(path)).await?;

        for monitor in Self::hyprpaper_monitor_tokens(options).await? {
            Self::run_command("hyprctl", Self::hyprpaper_wallpaper_args(&monitor, path)).await?;
        }

        Ok(())
    }

    /// The monitor tokens the `hyprctl hyprpaper wallpaper` step needs.
    ///
    /// hyprpaper addresses one output per call as `"<monitor>,<path>"`. With a
    /// specific `monitor_index` the name is resolved through `hyprctl monitors`;
    /// otherwise one call per monitor is issued so multi-monitor setups all
    /// update (P0-3). Without a monitor list at all the wildcard form (an empty
    /// name) still covers every output.
    async fn hyprpaper_monitor_tokens(options: &WallpaperOptions) -> Result<Vec<String>, UdaError> {
        if let Some(index) = options.monitor_index {
            return Ok(vec![Self::resolve_monitor(index).await?]);
        }
        match Self::list_monitors().await {
            Ok(monitors) => Ok(monitors),
            Err(_) => Ok(vec![String::new()]),
        }
    }

    /// List the connected monitor names with `hyprctl monitors`.
    async fn list_monitors() -> Result<Vec<String>, UdaError> {
        let output = Self::command_output("hyprctl", &["monitors"]).await?;
        let names = parse_hyprctl_monitors(output.as_deref().unwrap_or(""));
        if names.is_empty() {
            return Err(UdaError::DetectionFailed(
                "hyprctl monitors listed no monitors (is Hyprland running?)".to_string(),
            ));
        }
        Ok(names)
    }

    /// Resolve a monitor index to an output name via `hyprctl monitors`.
    ///
    /// An index that cannot be resolved is a caller error and must not silently
    /// widen to "all outputs".
    async fn resolve_monitor(index: usize) -> Result<String, UdaError> {
        let monitors = Self::list_monitors().await?;
        monitors.into_iter().nth(index).ok_or_else(|| {
            UdaError::InvalidArgument(format!(
                "monitor index {index} does not exist on this machine"
            ))
        })
    }

    /// `swww img` argument vector; `-o` takes the plain output name.
    fn swww_img_args(path: &str, monitor: Option<&str>) -> Vec<String> {
        let mut args = vec!["img".to_string(), path.to_string()];
        if let Some(monitor) = monitor {
            args.push("-o".to_string());
            args.push(monitor.to_string());
        }
        args
    }

    /// Wallpaper through `swww`, starting its daemon when it is not running.
    async fn set_wallpaper_swww(path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        if !Self::swww_daemon_running().await {
            Self::start_swww_daemon().await?;
        }

        let monitor = match options.monitor_index {
            Some(index) => Some(Self::resolve_monitor(index).await?),
            None => None,
        };
        Self::run_command("swww", Self::swww_img_args(path, monitor.as_deref())).await
    }

    /// Start an swww daemon, waiting for it to answer its socket.
    ///
    /// `swww init` is the documented entry point; swww >= 0.10 split the daemon
    /// into its own `swww-daemon` binary, which is tried when `swww init` does
    /// not come up. Both are spawned detached: waiting on the daemon process
    /// itself would block the caller, so readiness is polled instead.
    async fn start_swww_daemon() -> Result<(), UdaError> {
        Self::spawn_swww_daemon("swww", ["init"])?;
        if Self::wait_for_swww_daemon().await {
            return Ok(());
        }

        Self::spawn_swww_daemon("swww-daemon", [])?;
        if Self::wait_for_swww_daemon().await {
            return Ok(());
        }

        Err(UdaError::CommandFailed(format!(
            "the swww daemon did not come up within {SWWW_STARTUP_BUDGET:?}"
        )))
    }

    /// Whether an swww daemon is already answering.
    async fn swww_daemon_running() -> bool {
        Self::run_command("swww", ["query"]).await.is_ok()
    }

    /// Start one swww daemon detached, without waiting for it to exit.
    fn spawn_swww_daemon<const N: usize>(program: &str, args: [&str; N]) -> Result<(), UdaError> {
        tokio::process::Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
            .map_err(|e| UdaError::DetectionFailed(format!("failed to start {program}: {e}")))
    }

    /// Give a freshly started swww daemon a short window to create its socket.
    async fn wait_for_swww_daemon() -> bool {
        let deadline = tokio::time::Instant::now() + SWWW_STARTUP_BUDGET;
        loop {
            if Self::swww_daemon_running().await {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(SWWW_POLL_INTERVAL).await;
        }
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

    async fn set_wallpaper_x11(path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        if Self::command_exists("feh").await {
            let args = Self::feh_args(path, options.fill_mode, options.monitor_index);
            return Self::run_command("feh", &args).await;
        }

        if Self::command_exists("nitrogen").await {
            let args = Self::nitrogen_args(path, options.monitor_index);
            return Self::run_command("nitrogen", &args).await;
        }

        Err(UdaError::NotSupported(
            "neither feh nor nitrogen is available".to_string(),
        ))
    }
}

/// Whether `XDG_CURRENT_DESKTOP` explicitly names Hyprland.
///
/// A pure helper so the "Hyprland failures are final" rule is unit-testable
/// without touching the process environment.
fn desktop_is_hyprland(xdg_current_desktop: Option<&str>) -> bool {
    xdg_current_desktop
        .map(|value| value.to_ascii_lowercase().contains("hyprland"))
        .unwrap_or(false)
}

/// Whether this session reads and writes GNOME wallpaper keys.
///
/// `XDG_CURRENT_DESKTOP` wins because it is the variable the desktop itself
/// publishes; when it is absent the broader environment inference (which also
/// consults `DESKTOP_SESSION`) decides.
fn gnome_wallpaper_keys(info: &EnvironmentInfo) -> bool {
    match info.xdg_current_desktop.as_deref() {
        Some(value) => value.to_ascii_lowercase().contains("gnome"),
        None => matches!(info.desktop_environment, DesktopEnvironment::Gnome),
    }
}

/// Parse the monitor names out of `hyprctl monitors` output.
///
/// Every monitor starts a line like `Monitor eDP-1 (ID 1):`; the remainder of
/// the output is indented detail lines that must not match.
fn parse_hyprctl_monitors(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let rest = line.trim().strip_prefix("Monitor ")?;
            let name = rest.split_whitespace().next()?;
            Some(name.to_string())
        })
        .collect()
}

/// Decode the two hex digits starting at `start`, if both are valid.
fn hex_pair(bytes: &[u8], start: usize) -> Option<u8> {
    let pair = bytes.get(start..start + 2)?;
    if !pair.iter().all(u8::is_ascii_hexdigit) {
        return None;
    }
    u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()
}

/// Decode the `%XX` escapes [`LinuxWallpaperManager::file_uri`] produces.
///
/// Malformed escapes are kept literally: a filename containing a stray `%` must
/// survive the round trip instead of being truncated or panicking.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        // `%` is kept with its digits when the escape is malformed.
        let escaped = if bytes[index] == b'%' {
            hex_pair(bytes, index + 1)
        } else {
            None
        };
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                index += 3;
            }
            None => {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
    }

    match String::from_utf8(decoded) {
        Ok(text) => text,
        Err(error) => String::from_utf8_lossy(error.as_bytes()).into_owned(),
    }
}

impl WallpaperManager for LinuxWallpaperManager {
    fn set_wallpaper(&self, path: &str, options: &WallpaperOptions) -> Result<(), UdaError> {
        // The bridge requires an owned, 'static future, and a wallpaper set is
        // a rare operation: copying the arguments is the cheapest way there.
        let path = path.to_string();
        let options = options.clone();

        crate::sync::run_async(async move {
            let info = detect_environment().unwrap_or_default();

            // GNOME and KDE each own a native tier. A native tier that ran and
            // failed is not final: `DetectionFailed` means "this backend did
            // not work", so the cascade falls through to the CLI chain. Any
            // other error variant is a caller error and final.
            let native = match info.desktop_environment {
                DesktopEnvironment::Gnome => Some((
                    "gsettings",
                    Self::set_wallpaper_gnome(&path, &options).await,
                )),
                DesktopEnvironment::Kde => Some((
                    "plasmashell",
                    Self::set_wallpaper_kde(&path, &options).await,
                )),
                DesktopEnvironment::Xfce | DesktopEnvironment::Unknown => None,
            };
            if let Some((backend, result)) = native {
                match result {
                    Ok(()) => return Ok(()),
                    Err(UdaError::DetectionFailed(_)) => {
                        log::debug!("{backend} could not set the wallpaper; trying the CLI chain")
                    }
                    Err(e) => return Err(e),
                }
            }

            match Self::set_wallpaper_hyprland(&path, &options).await {
                Ok(()) => return Ok(()),
                // An explicitly Hyprland session has no business in the X11
                // tier: feh/nitrogen cannot address a Wayland compositor, so
                // swallowing the error would report success while nothing
                // happened (P0-3).
                Err(e) if desktop_is_hyprland(info.xdg_current_desktop.as_deref()) => {
                    return Err(e)
                }
                Err(e) => {
                    log::debug!("the Hyprland tool chain failed ({e}); trying the X11 fallback");
                }
            }

            Self::set_wallpaper_x11(&path, &options).await
        })?
    }

    fn get_wallpaper(&self) -> Result<Option<String>, UdaError> {
        let info = detect_environment().unwrap_or_default();
        if !gnome_wallpaper_keys(&info) {
            return Err(UdaError::NotSupported(
                "reading the wallpaper is only implemented for GNOME".to_string(),
            ));
        }

        crate::sync::run_async(Self::get_wallpaper_gnome())?
    }

    fn capabilities(&self) -> Result<Capability, UdaError> {
        let mut caps = Capability::SET_WALLPAPER;

        // `GET_WALLPAPER` is only honest on GNOME, where `get_wallpaper` is
        // implemented; elsewhere the read would be a silent lie (P2-40).
        if gnome_wallpaper_keys(&detect_environment().unwrap_or_default())
            && crate::sync::run_async(Self::command_exists("gsettings")).unwrap_or(false)
        {
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
    fn hyprpaper_args_follow_the_hyprctl_grammar() {
        // hyprpaper has no CLI of its own: requests go through `hyprctl
        // hyprpaper`, preload first, then one `wallpaper` per monitor.
        assert_eq!(
            LinuxWallpaperManager::hyprpaper_preload_args("/tmp/wall.jpg"),
            vec![
                "hyprpaper".to_string(),
                "preload".to_string(),
                "/tmp/wall.jpg".to_string(),
            ]
        );

        assert_eq!(
            LinuxWallpaperManager::hyprpaper_wallpaper_args("eDP-1", "/tmp/wall.jpg"),
            vec![
                "hyprpaper".to_string(),
                "wallpaper".to_string(),
                "eDP-1,/tmp/wall.jpg".to_string(),
            ]
        );

        // The empty monitor name is hyprpaper's "every output" wildcard.
        assert_eq!(
            LinuxWallpaperManager::hyprpaper_wallpaper_args("", "/tmp/wall.jpg"),
            vec![
                "hyprpaper".to_string(),
                "wallpaper".to_string(),
                ",/tmp/wall.jpg".to_string(),
            ]
        );
    }

    #[test]
    fn hyprctl_monitor_lines_are_parsed() {
        let output = "Monitor eDP-1 (ID 1):\
                     \n\tmode: 1920x1080@60\
                     \n\nMonitor HDMI-A-1 (ID 2):\
                     \n\tmode: 3840x2160@60";

        assert_eq!(
            parse_hyprctl_monitors(output),
            vec!["eDP-1".to_string(), "HDMI-A-1".to_string()]
        );
        assert!(parse_hyprctl_monitors("").is_empty());
        assert!(parse_hyprctl_monitors("\tsome detail line\nmore").is_empty());
    }

    #[test]
    fn swww_img_args_use_the_output_flag_with_a_plain_name() {
        // `-o` takes the output name itself, not an "output:" prefix.
        assert_eq!(
            LinuxWallpaperManager::swww_img_args("/tmp/wall.jpg", Some("eDP-1")),
            vec![
                "img".to_string(),
                "/tmp/wall.jpg".to_string(),
                "-o".to_string(),
                "eDP-1".to_string(),
            ]
        );

        assert_eq!(
            LinuxWallpaperManager::swww_img_args("/tmp/wall.jpg", None),
            vec!["img".to_string(), "/tmp/wall.jpg".to_string()]
        );
    }

    #[test]
    fn hyprland_is_detected_from_the_xdg_variable() {
        assert!(desktop_is_hyprland(Some("Hyprland")));
        assert!(desktop_is_hyprland(Some("hyprland:wayland")));
        assert!(!desktop_is_hyprland(Some("GNOME")));
        assert!(!desktop_is_hyprland(Some("KDE")));
        assert!(!desktop_is_hyprland(None));
    }

    #[test]
    fn a_gnome_session_is_recognised_for_the_read_path() {
        let mut info = EnvironmentInfo::default();
        assert!(!gnome_wallpaper_keys(&info));

        info.xdg_current_desktop = Some("ubuntu:GNOME".to_string());
        assert!(gnome_wallpaper_keys(&info));

        info.xdg_current_desktop = None;
        info.desktop_environment = DesktopEnvironment::Gnome;
        assert!(gnome_wallpaper_keys(&info));
    }

    #[test]
    fn gsettings_values_are_decoded_back_to_paths() {
        assert_eq!(
            LinuxWallpaperManager::strip_file_uri("'file:///tmp/wall.jpg'"),
            Some("/tmp/wall.jpg".to_string())
        );
        // The escaping `file_uri` applies must round-trip.
        assert_eq!(
            LinuxWallpaperManager::strip_file_uri("'file:///path/with%20spaces/a%23b.jpg'"),
            Some("/path/with spaces/a#b.jpg".to_string())
        );
        // Not a file URI (unset key, foreign scheme): no made-up path.
        assert_eq!(LinuxWallpaperManager::strip_file_uri("''"), None);
        assert_eq!(LinuxWallpaperManager::strip_file_uri("'https://x/y'"), None);
    }

    #[test]
    fn a_stray_percent_survives_decoding() {
        assert_eq!(percent_decode("100%file"), "100%file");
        assert_eq!(percent_decode("%2"), "%2");
        assert_eq!(percent_decode("a%zzb"), "a%zzb");
    }

    #[test]
    fn the_dark_key_wins_while_the_scheme_prefers_dark() {
        assert!(LinuxWallpaperManager::prefers_dark_from_color_scheme(
            "'prefer-dark'"
        ));
        assert!(!LinuxWallpaperManager::prefers_dark_from_color_scheme(
            "'prefer-light'"
        ));
        assert!(!LinuxWallpaperManager::prefers_dark_from_color_scheme(
            "'default'"
        ));
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

    #[test]
    fn capabilities_never_promise_the_read_path_outside_gnome() {
        // On a non-GNOME (here: headless) machine `GET_WALLPAPER` must stay
        // dark even when a GNOME schema happens to be installed (P2-40).
        let caps = LinuxWallpaperManager::new().capabilities().ok();
        if let Some(caps) = caps {
            if !gnome_wallpaper_keys(&detect_environment().unwrap_or_default()) {
                assert!(!caps.contains(Capability::GET_WALLPAPER));
            }
        }
    }
}
