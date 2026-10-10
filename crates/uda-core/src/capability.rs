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
    /// Backend can host at least one system tray (StatusNotifierItem / NotifyIcon).
    pub const SYSTEM_TRAY: u32 = 1 << 7;
    /// Tray icon can be shown, hidden, and swapped at runtime.
    pub const TRAY_ICON: u32 = 1 << 8;
    /// Tray exposes hover text.
    pub const TRAY_TOOLTIP: u32 = 1 << 9;
    /// Tray reports a single primary click.
    pub const TRAY_CLICK: u32 = 1 << 10;
    /// Tray reports a native double click (never true on Linux SNI).
    pub const TRAY_DOUBLE_CLICK: u32 = 1 << 11;
    /// Tray exposes a context menu.
    pub const TRAY_CONTEXT_MENU: u32 = 1 << 12;
    /// Menu rows can render a checkbox state.
    pub const TRAY_CHECKBOX: u32 = 1 << 13;
    /// Menu rows can be added, removed, or relabelled at runtime.
    pub const TRAY_DYNAMIC_MENU: u32 = 1 << 14;
    /// A media playback backend exists (MPRIS v2 on Linux, SMTC on Windows).
    pub const MEDIA_CONTROL: u32 = 1 << 15;
    /// A session backend exists: at least one of the actions below can be issued.
    pub const SESSION_MANAGEMENT: u32 = 1 << 16;
    /// The session can be locked (the only safe-to-automate action).
    pub const LOCK: u32 = 1 << 17;
    /// The calling user's session can be logged out.
    pub const LOGOUT: u32 = 1 << 18;
    /// The machine can be suspended to RAM.
    pub const SUSPEND: u32 = 1 << 19;
    /// The machine can be hibernated to disk.
    pub const HIBERNATE: u32 = 1 << 20;
    /// The machine can be rebooted (needs privilege on Windows).
    pub const REBOOT: u32 = 1 << 21;
    /// The machine can be powered off (needs privilege on Windows).
    pub const SHUTDOWN: u32 = 1 << 22;
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
        const SYSTEM_TRAY = CapabilityMatrix::SYSTEM_TRAY;
        const TRAY_ICON = CapabilityMatrix::TRAY_ICON;
        const TRAY_TOOLTIP = CapabilityMatrix::TRAY_TOOLTIP;
        const TRAY_CLICK = CapabilityMatrix::TRAY_CLICK;
        const TRAY_DOUBLE_CLICK = CapabilityMatrix::TRAY_DOUBLE_CLICK;
        const TRAY_CONTEXT_MENU = CapabilityMatrix::TRAY_CONTEXT_MENU;
        const TRAY_CHECKBOX = CapabilityMatrix::TRAY_CHECKBOX;
        const TRAY_DYNAMIC_MENU = CapabilityMatrix::TRAY_DYNAMIC_MENU;
        const MEDIA_CONTROL = CapabilityMatrix::MEDIA_CONTROL;
        const SESSION_MANAGEMENT = CapabilityMatrix::SESSION_MANAGEMENT;
        const LOCK = CapabilityMatrix::LOCK;
        const LOGOUT = CapabilityMatrix::LOGOUT;
        const SUSPEND = CapabilityMatrix::SUSPEND;
        const HIBERNATE = CapabilityMatrix::HIBERNATE;
        const REBOOT = CapabilityMatrix::REBOOT;
        const SHUTDOWN = CapabilityMatrix::SHUTDOWN;
    }
}

/// Support level for a specific capability on the current platform.
///
/// `Partial` carries the reason it exists: a host that needs to explain a
/// degraded behaviour to the user must not have to guess why.
///
/// Marked `#[non_exhaustive]`: new levels may be added in minor releases, so
/// downstream matches need a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SupportLevel {
    /// Capability is unavailable.
    None,
    /// Capability is available in a degraded form, with the reason attached.
    Partial(String),
    /// Capability is fully supported.
    Full,
}

impl SupportLevel {
    /// The reason a capability is degraded, when it is.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Partial(reason) => Some(reason.as_str()),
            _ => None,
        }
    }
}

/// System desktop theme / appearance mode.
///
/// Marked `#[non_exhaustive]`: new appearances may be added in minor releases,
/// so downstream matches need a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Theme {
    /// Light appearance.
    Light,
    /// Dark appearance.
    Dark,
    /// Follow system automatic switching.
    Auto,
    /// No desktop publishes a colour scheme, so the preference is unknown.
    ///
    /// This is deliberately distinct from [`Theme::Auto`]: `Auto` means "the
    /// system switches the theme itself", while `Unknown` means the question
    /// could not be answered at all. Guessing `Light` would make a UI pretend
    /// it knew the answer on a tiling window manager.
    Unknown,
}

/// RGBA color with per-channel 0..=255 values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RgbaColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_carries_a_reason_that_can_be_read_back() {
        let level = SupportLevel::Partial("double click is synthesised from two clicks".into());
        assert_eq!(
            level.reason(),
            Some("double click is synthesised from two clicks")
        );
    }

    #[test]
    fn full_and_none_have_no_reason() {
        assert_eq!(SupportLevel::Full.reason(), None);
        assert_eq!(SupportLevel::None.reason(), None);
    }

    #[test]
    fn partial_reasons_participate_in_equality() {
        // Two degraded answers are only equal when their reasons match, so a
        // host cannot accidentally treat a different degradation as the same one.
        assert_eq!(
            SupportLevel::Partial("a".into()),
            SupportLevel::Partial("a".into())
        );
        assert_ne!(
            SupportLevel::Partial("a".into()),
            SupportLevel::Partial("b".into())
        );
        assert_ne!(SupportLevel::Partial("a".into()), SupportLevel::Full);
    }
}
