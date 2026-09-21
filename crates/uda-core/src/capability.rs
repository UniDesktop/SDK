use bitflags::bitflags;

/// Capability flags describing what a platform backend can do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CapabilityMatrix;

impl CapabilityMatrix {
    pub const DETECT_THEME: u32 = 1 << 0;
    pub const SET_WALLPAPER: u32 = 1 << 1;
    pub const GET_WALLPAPER: u32 = 1 << 2;
    pub const READ_ACCENT_COLOR: u32 = 1 << 3;
    pub const FOLLOW_SYSTEM_THEME: u32 = 1 << 4;
    pub const SEND_NOTIFICATION: u32 = 1 << 5;
    pub const WAKE_LOCK: u32 = 1 << 6;
}

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Capability: u32 {
        const DETECT_THEME = CapabilityMatrix::DETECT_THEME;
        const SET_WALLPAPER = CapabilityMatrix::SET_WALLPAPER;
        const GET_WALLPAPER = CapabilityMatrix::GET_WALLPAPER;
        const READ_ACCENT_COLOR = CapabilityMatrix::READ_ACCENT_COLOR;
        const FOLLOW_SYSTEM_THEME = CapabilityMatrix::FOLLOW_SYSTEM_THEME;
        const SEND_NOTIFICATION = CapabilityMatrix::SEND_NOTIFICATION;
        const WAKE_LOCK = CapabilityMatrix::WAKE_LOCK;
    }
}

/// Support level for a specific capability on the current platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportLevel {
    /// Capability is unavailable.
    None,
    /// Capability is partially available (e.g., limited monitors, modes, or formats).
    Partial,
    /// Capability is fully supported.
    Full,
}

/// System desktop theme / appearance mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Theme {
    /// Light appearance.
    Light,
    /// Dark appearance.
    Dark,
    /// Follow system automatic switching.
    Auto,
}

/// RGBA color with per-channel 0..=255 values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RgbaColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}
