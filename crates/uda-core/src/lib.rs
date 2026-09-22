pub mod appearance;
pub mod capability;
pub mod error;
pub mod notification;
pub mod tray;
pub mod wakelock;
pub mod wallpaper;

/// Fallback identifier used when a backend discovers no application name.
///
/// Both the SNI bus name (`org.kde.StatusNotifierItem-<name>-<n>`) and the
/// Windows `NOTIFYICONDATAW` tooltip text embed this value, so every platform
/// backend must treat an empty name as a hard error and substitute this constant
/// rather than emitting an empty identifier.
pub const DEFAULT_APP_NAME: &str = "UDA";
