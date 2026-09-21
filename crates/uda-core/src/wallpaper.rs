use crate::capability::Capability;
use crate::error::UdaError;

/// Fill mode for wallpaper rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillMode {
    /// Crop to fill while preserving aspect ratio.
    Crop,
    /// Stretch to fill the area, ignoring aspect ratio.
    Fill,
    /// Fit inside the area while preserving aspect ratio, leaving empty bars if needed.
    Fit,
    /// Stretch to fill the area (same as Fill in many shells).
    Stretch,
}

/// Options for setting a wallpaper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WallpaperOptions {
    /// Fill mode.
    pub fill_mode: FillMode,
    /// Monitor index for multi-monitor setups. `None` means all monitors.
    pub monitor_index: Option<usize>,
    /// Whether this wallpaper is for dark mode appearance.
    pub dark_mode: bool,
}

impl Default for WallpaperOptions {
    fn default() -> Self {
        Self {
            fill_mode: FillMode::Fill,
            monitor_index: None,
            dark_mode: false,
        }
    }
}

/// Cross-platform wallpaper management interface.
pub trait WallpaperManager {
    /// Set the system wallpaper to the image at `path`.
    fn set_wallpaper(&self, path: &str, options: &WallpaperOptions) -> Result<(), UdaError>;

    /// Get the current system wallpaper path.
    fn get_wallpaper(&self) -> Result<Option<String>, UdaError>;

    /// Return the set of capabilities supported by the current backend.
    fn capabilities(&self) -> Result<Capability, UdaError>;
}
