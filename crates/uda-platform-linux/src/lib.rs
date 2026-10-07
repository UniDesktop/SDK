use std::time::Duration;

use uda_core::error::UdaError;

pub mod appearance;
pub mod detection;
pub mod error;
pub mod media;
pub mod notification;
pub mod session;
pub(crate) mod sync;
pub mod tray;
pub mod wakelock;
pub mod wallpaper;

/// Hard ceiling for one D-Bus round trip, shared by every backend.
///
/// A wedged daemon (a `plasmashell` mid-update, a screen saver that stopped
/// answering) must cost a bounded wait instead of pinning the caller forever
/// (P2-39). Five seconds is far above real latency - including logind's polkit
/// round trip - and far below a host's patience.
pub(crate) const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Hard ceiling for one external CLI tool invocation.
///
/// Ten seconds covers a cold `gsettings`/`hyprctl` start with room to spare,
/// while a tool that never answers is killed instead of pinning the caller
/// forever (P0-3: no helper may block indefinitely).
pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// Run one D-Bus round trip under [`DBUS_TIMEOUT`], reporting every failure as
/// [`UdaError::Internal`].
///
/// Both "the call was refused" and "the peer never answered" mean the same
/// thing when the step is not part of a fallback cascade: the feature is broken
/// right now. Backends whose failures do select the next tier (session,
/// wallpaper, appearance) keep their own mapping because the *variant* drives
/// that choice; `step` names the operation for the message.
pub(crate) async fn internal_dbus<T>(
    step: &str,
    future: impl std::future::Future<Output = zbus::Result<T>>,
) -> Result<T, UdaError> {
    match tokio::time::timeout(DBUS_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(UdaError::Internal(format!("{step} failed: {e}"))),
        Err(_) => Err(UdaError::Internal(format!(
            "{step} timed out after {DBUS_TIMEOUT:?}"
        ))),
    }
}

/// Run one D-Bus round trip under [`DBUS_TIMEOUT`], reporting every failure as
/// [`UdaError::DetectionFailed`] so the caller's cascade falls through to the
/// next tier. `step` names the operation for the message.
pub(crate) async fn detect_dbus<T>(
    step: &str,
    future: impl std::future::Future<Output = zbus::Result<T>>,
) -> Result<T, UdaError> {
    match tokio::time::timeout(DBUS_TIMEOUT, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(UdaError::DetectionFailed(format!("{step}: {e}"))),
        Err(_) => Err(UdaError::DetectionFailed(format!(
            "{step} timed out after {DBUS_TIMEOUT:?}"
        ))),
    }
}
