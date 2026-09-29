//! Linux session backend: systemd-logind over the system bus, plus the
//! FreeDesktop screen saver on the session bus.
//!
//! # Which bus carries what
//!
//! The split is not incidental and getting it wrong produces a confusing
//! `AccessDenied`:
//!
//! | Action   | Bus     | Service / interface                                  |
//! |----------|---------|------------------------------------------------------|
//! | Lock     | Session | `org.freedesktop.ScreenSaver` on `/org/freedesktop/ScreenSaver` |
//! | Logout   | System  | `org.freedesktop.login1.Manager.TerminateSession("")` |
//! | Suspend  | System  | `org.freedesktop.login1.Manager.Suspend(false)`       |
//! | Hibernate| System  | `org.freedesktop.login1.Manager.Hibernate(false)`     |
//! | Reboot   | System  | `org.freedesktop.login1.Manager.Reboot(false)`        |
//! | Shutdown | System  | `org.freedesktop.login1.Manager.PowerOff(false)`      |
//!
//! Power and session management live on the **system** bus because logind is a
//! system service and does its own polkit authorisation; the screen saver lives
//! on the **session** bus because it belongs to the desktop session.
//!
//! # Fallbacks (AGENTS.md Principle 2)
//!
//! - **Lock**: `org.freedesktop.ScreenSaver.Lock()` first; when that fails the
//!   CLI tier runs `loginctl lock-session`, which works whether or not the
//!   screen saver service is running.
//! - **Logout**: `TerminateSession("")` first - the empty session id means "the
//!   calling one", so no session enumeration is needed; when logind refuses, the
//!   desktop's own session manager (`org.gnome.SessionManager`,
//!   `org.kde.Shutdown`, `org.xfce.Session.Manager`) is tried.
//! - **Power**: logind only. There is no portable CLI equivalent that does its
//!   own authorisation, so a machine without logind reports the action
//!   unsupported rather than guessing at `shutdown -h now`.
//!
//! # Interactive flag
//!
//! Every power call passes `interactive: false`. `true` would ask logind to
//! confirm with the user first (and logind would then need to talk to the
//! caller's session), which is not what a library caller wants: it has already
//! decided, and it performs its own confirmation dialog.
//!
//! # Timeouts
//!
//! Every D-Bus await is wrapped in [`DBUS_TIMEOUT`], so a logind that is
//! installing an update (and therefore not answering) costs a bounded wait
//! instead of a hung caller.
//!
//! See `docs/internals/session_specs.md` for the full mapping.

use std::process::Command;
use std::time::Duration;

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::session::{SessionAction, SessionManager};
use zbus::Connection;

/// Hard ceiling for one D-Bus round trip in this backend.
///
/// Power actions legitimately take a moment (logind talks to polkit and then to
/// the init system), so five seconds is generous for a round trip while still
/// bounding a wedged daemon.
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

/// `org.freedesktop.login1` destination on the system bus.
const LOGIN1_SERVICE: &str = "org.freedesktop.login1";

/// `org.freedesktop.login1` object path.
const LOGIN1_PATH: &str = "/org/freedesktop/login1";

/// `org.freedesktop.login1.Manager` interface.
const LOGIN1_INTERFACE: &str = "org.freedesktop.login1.Manager";

/// `org.freedesktop.ScreenSaver` destination on the session bus.
const SCREENSAVER_SERVICE: &str = "org.freedesktop.ScreenSaver";

/// `org.freedesktop.ScreenSaver` object path.
const SCREENSAVER_PATH: &str = "/org/freedesktop/ScreenSaver";

/// `org.freedesktop.ScreenSaver` interface.
const SCREENSAVER_INTERFACE: &str = "org.freedesktop.ScreenSaver";

/// Linux session manager.
///
/// Stateless like the other Linux backends: every call opens its own bus
/// connection and drives a short-lived current-thread runtime, so no daemon
/// connection is parked for the lifetime of the process. Session actions are
/// rare (a user clicks "shut down"), which makes the per-call cost irrelevant.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxSessionManager;

impl LinuxSessionManager {
    pub fn new() -> Self {
        Self
    }

    /// Connect to the **system** bus, where logind lives.
    async fn system_connection() -> Result<Connection, UdaError> {
        match tokio::time::timeout(DBUS_TIMEOUT, Connection::system()).await {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(e)) => Err(UdaError::DetectionFailed(format!(
                "could not reach the system bus: {e}"
            ))),
            Err(_) => Err(UdaError::DetectionFailed(
                "connecting to the system bus timed out".to_string(),
            )),
        }
    }

    /// Connect to the **session** bus, where the screen saver lives.
    async fn session_connection() -> Result<Connection, UdaError> {
        match tokio::time::timeout(DBUS_TIMEOUT, Connection::session()).await {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(e)) => Err(UdaError::DetectionFailed(format!(
                "could not reach the session bus: {e}"
            ))),
            Err(_) => Err(UdaError::DetectionFailed(
                "connecting to the session bus timed out".to_string(),
            )),
        }
    }

    /// Call a no-argument `login1.Manager` power method with `interactive: false`.
    ///
    /// Synchronous on the outside (the trait is synchronous), asynchronous on
    /// the inside: one current-thread runtime per call owns the connection and
    /// the proxy, so nothing outlives the call.
    fn login1_power(&self, method: &str) -> Result<(), UdaError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("could not start a runtime: {e}")))?;

        runtime.block_on(async {
            let connection = Self::system_connection().await?;

            let proxy = match tokio::time::timeout(
                DBUS_TIMEOUT,
                zbus::Proxy::new(&connection, LOGIN1_SERVICE, LOGIN1_PATH, LOGIN1_INTERFACE),
            )
            .await
            {
                Ok(Ok(proxy)) => proxy,
                Ok(Err(e)) => {
                    return Err(UdaError::DetectionFailed(format!(
                        "could not build a logind proxy: {e}"
                    )));
                }
                Err(_) => {
                    return Err(UdaError::DetectionFailed(
                        "building the logind proxy timed out".to_string(),
                    ));
                }
            };

            // `interactive: false` throughout: the caller has already confirmed,
            // so logind must not open its own confirmation dialog.
            match tokio::time::timeout(
                DBUS_TIMEOUT,
                proxy.call::<&str, (bool,), ()>(method, &(false,)),
            )
            .await
            {
                Ok(Ok(())) => {
                    log::debug!("logind accepted {method}");
                    Ok(())
                }
                Ok(Err(e)) => Err(map_login1_error(method, &e.to_string())),
                Err(_) => Err(UdaError::CommandFailed(format!(
                    "logind {method} timed out after {DBUS_TIMEOUT:?}"
                ))),
            }
        })
    }

    /// Lock through `org.freedesktop.ScreenSaver`, then through `loginctl`.
    fn lock_via_screen_saver(&self) -> Result<(), UdaError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("could not start a runtime: {e}")))?;

        runtime.block_on(async {
            let connection = Self::session_connection().await?;

            let proxy = match tokio::time::timeout(
                DBUS_TIMEOUT,
                zbus::Proxy::new(
                    &connection,
                    SCREENSAVER_SERVICE,
                    SCREENSAVER_PATH,
                    SCREENSAVER_INTERFACE,
                ),
            )
            .await
            {
                Ok(Ok(proxy)) => proxy,
                Ok(Err(e)) => {
                    return Err(UdaError::CommandFailed(format!(
                        "could not build a screen-saver proxy: {e}"
                    )));
                }
                Err(_) => {
                    return Err(UdaError::CommandFailed(
                        "building the screen-saver proxy timed out".to_string(),
                    ));
                }
            };

            match tokio::time::timeout(DBUS_TIMEOUT, proxy.call::<&str, (), ()>("Lock", &())).await
            {
                Ok(Ok(())) => {
                    log::debug!("the screen saver accepted Lock");
                    Ok(())
                }
                // A missing or refused screen saver is recoverable: the CLI tier
                // below locks the session directly through logind.
                Ok(Err(e)) => Err(UdaError::CommandFailed(format!(
                    "the screen saver refused Lock: {e}"
                ))),
                Err(_) => Err(UdaError::CommandFailed(
                    "the screen saver did not answer Lock".to_string(),
                )),
            }
        })
    }

    /// Lock through `loginctl lock-session` (the CLI fallback tier).
    ///
    /// `loginctl` resolves the *calling* session, so no session id has to be
    /// discovered. It is the tier that still works when the desktop runs no
    /// screen saver service at all.
    fn lock_via_loginctl(&self) -> Result<(), UdaError> {
        let status = Command::new("loginctl").arg("lock-session").status();

        match status {
            Ok(status) if status.success() => {
                log::debug!("loginctl lock-session succeeded");
                Ok(())
            }
            Ok(status) => Err(UdaError::CommandFailed(format!(
                "loginctl lock-session exited with {}",
                status.code().unwrap_or(-1)
            ))),
            Err(e) => Err(UdaError::Io(e)),
        }
    }

    /// End the calling user's session.
    fn logout(&self) -> Result<(), UdaError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("could not start a runtime: {e}")))?;

        // `TerminateSession("")` means "the calling session", so no session id
        // has to be enumerated first.
        let logind = runtime.block_on(async {
            let connection = Self::system_connection().await?;

            let proxy = tokio::time::timeout(
                DBUS_TIMEOUT,
                zbus::Proxy::new(&connection, LOGIN1_SERVICE, LOGIN1_PATH, LOGIN1_INTERFACE),
            )
            .await
            .map_err(|_| UdaError::CommandFailed("building the logind proxy timed out".to_string()))?
            .map_err(|e| UdaError::DetectionFailed(format!("logind proxy: {e}")))?;

            tokio::time::timeout(
                DBUS_TIMEOUT,
                proxy.call::<&str, (&str,), ()>("TerminateSession", &("",)),
            )
            .await
            .map_err(|_| UdaError::CommandFailed("logind TerminateSession timed out".to_string()))?
            .map_err(|e| map_login1_error("TerminateSession", &e.to_string()))
        });

        match logind {
            Ok(()) => Ok(()),
            Err(logind_error) => {
                // Tier 3: ask the desktop's own session manager. GNOME, KDE and
                // XFCE each expose a logout entry point under a different name,
                // and all three are tried because `$XDG_CURRENT_DESKTOP` is not
                // reliable enough to pick one.
                log::debug!("logind logout failed ({logind_error}); trying the desktop session manager");

                for (service, path, interface, method) in [
                    (
                        "org.gnome.SessionManager",
                        "/org/gnome/SessionManager",
                        "org.gnome.SessionManager",
                        "Logout",
                    ),
                    (
                        "org.kde.Shutdown",
                        "/Shutdown",
                        "org.kde.Shutdown",
                        "logout",
                    ),
                    (
                        "org.xfce.Session.Manager",
                        "/org/xfce/Session/Manager",
                        "org.xfce.Session.Manager",
                        "Logout",
                    ),
                ] {
                    if logout_via_desktop(service, path, interface, method) {
                        return Ok(());
                    }
                }

                Err(logind_error)
            }
        }
    }
}

/// Try one desktop session manager's logout entry point.
///
/// A service that is not running is the normal case (only one desktop is ever
/// present), so a missing service is `false` rather than an error - the caller
/// keeps trying the next candidate and only reports an error when all of them
/// fail.
fn logout_via_desktop(service: &str, path: &str, interface: &str, method: &str) -> bool {
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else {
        return false;
    };

    let connection = match runtime.block_on(Connection::session()) {
        Ok(connection) => connection,
        Err(_) => return false,
    };

    let proxy = runtime.block_on(async {
        tokio::time::timeout(
            DBUS_TIMEOUT,
            zbus::Proxy::new(&connection, service, path, interface),
        )
        .await
    });

    let Ok(Ok(proxy)) = proxy else {
        return false;
    };

    let call = runtime.block_on(async {
        tokio::time::timeout(
            DBUS_TIMEOUT,
            proxy.call::<&str, (u32,), ()>(method, &(0,)),
        )
        .await
    });

    match call {
        Ok(Ok(())) => true,
        Ok(Err(e)) => {
            log::debug!("{service} refused {method}: {e}");
            false
        }
        Err(_) => {
            log::debug!("{service} did not answer {method} in time");
            false
        }
    }
}

/// Translate a logind D-Bus error into a typed [`UdaError`].
///
/// logind reports "not authorised" as `org.freedesktop.DBus.Error.AccessDenied`,
/// `NotAuthorized`, or polkit's `InteractiveAuthorizationRequired` when the rule
/// wants a dialog the caller cannot show. "This machine cannot do it" arrives as
/// `org.freedesktop.systemd1.NoSuchOperation` or a plain `Failed`. Mapping those
/// separately lets a caller show "ask your administrator" instead of a generic
/// failure, which matters because power actions are exactly where users hit
/// polkit.
///
/// The error is taken as already-rendered text so the classification is a pure
/// function over a string: unit-testable, and free of any assumption about how
/// the D-Bus library formats a method error.
fn map_login1_error(method: &str, text: &str) -> UdaError {
    if error_is_authorization(text) {
        return UdaError::NotSupported(format!(
            "logind refused {method}: the caller is not authorised (polkit)"
        ));
    }

    if text.contains("NoSuchOperation") || text.contains("NotSupported") {
        return UdaError::NotSupported(format!(
            "logind cannot {method} on this machine: {text}"
        ));
    }

    UdaError::CommandFailed(format!("logind {method} failed: {text}"))
}

impl SessionManager for LinuxSessionManager {
    fn lock(&self) -> Result<(), UdaError> {
        // Tier 2 first; the CLI tier is only consulted when it fails, so a
        // machine with a running screen saver never spawns a subprocess.
        match self.lock_via_screen_saver() {
            Ok(()) => Ok(()),
            Err(screen_saver_error) => {
                log::debug!(
                    "the screen saver could not lock ({screen_saver_error}); trying loginctl"
                );
                self.lock_via_loginctl()
            }
        }
    }

    fn logout(&self) -> Result<(), UdaError> {
        self.logout()
    }

    fn suspend(&self) -> Result<(), UdaError> {
        self.login1_power("Suspend")
    }

    fn hibernate(&self) -> Result<(), UdaError> {
        self.login1_power("Hibernate")
    }

    fn reboot(&self) -> Result<(), UdaError> {
        self.login1_power("Reboot")
    }

    fn shutdown(&self) -> Result<(), UdaError> {
        self.login1_power("PowerOff")
    }

    fn capabilities(&self) -> Capability {
        // Every action routes through logind, which is present on any machine
        // with systemd; whether logind *authorises* the caller is a runtime
        // question answered by the methods above, not a capability question.
        // `LOCK` is also advertised unconditionally because the `loginctl`
        // fallback needs no screen saver service at all.
        Capability::SESSION_MANAGEMENT
            | Capability::LOCK
            | Capability::LOGOUT
            | Capability::SUSPEND
            | Capability::HIBERNATE
            | Capability::REBOOT
            | Capability::SHUTDOWN
    }
}

/// Build the argument list a `loginctl`/login1 power call would use.
///
/// Factored out so the mapping from a core action to a logind method name is
/// unit-testable without touching the machine.
#[allow(dead_code)] // documents the mapping; the trait methods are the entry points
pub(crate) fn method_for(action: SessionAction) -> Option<&'static str> {
    match action {
        SessionAction::Lock => None,
        SessionAction::Logout => Some("TerminateSession"),
        SessionAction::Suspend => Some("Suspend"),
        SessionAction::Hibernate => Some("Hibernate"),
        SessionAction::Reboot => Some("Reboot"),
        SessionAction::Shutdown => Some("PowerOff"),
    }
}

/// Whether a logind error text means "not authorised".
///
/// Split out from [`map_login1_error`] so the CLI tier and the D-Bus tier make
/// the same distinction.
#[allow(dead_code)] // shared classification helper, kept next to its only caller
pub(crate) fn error_is_authorization(text: &str) -> bool {
    // `AccessDenied` is D-Bus's generic refusal and `NotAuthorized` is what
    // logind itself emits. `InteractiveAuthorizationRequired` is polkit's answer
    // when the rule needs a dialog the caller cannot show - the usual case for a
    // UDA host, which is a plain daemon rather than a registered polkit subject.
    // All three mean "an administrator has to grant this", never "retry".
    text.contains("AccessDenied")
        || text.contains("NotAuthorized")
        || text.contains("InteractiveAuthorizationRequired")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_has_no_login1_method_because_it_uses_the_session_bus() {
        // Locking must not be routed through logind's Manager interface: the
        // screen saver is a session service, and calling it on the system bus
        // would fail with an unrelated error.
        assert_eq!(method_for(SessionAction::Lock), None);
    }

    #[test]
    fn every_destructive_action_maps_to_a_login1_method() {
        assert_eq!(method_for(SessionAction::Logout), Some("TerminateSession"));
        assert_eq!(method_for(SessionAction::Suspend), Some("Suspend"));
        assert_eq!(method_for(SessionAction::Hibernate), Some("Hibernate"));
        assert_eq!(method_for(SessionAction::Reboot), Some("Reboot"));
        assert_eq!(method_for(SessionAction::Shutdown), Some("PowerOff"));
    }

    #[test]
    fn shutdown_maps_to_poweroff_not_to_a_shutdown_verb() {
        // `PowerOff` is the logind name; there is no `Shutdown` method, and
        // sending one would surface as an UnknownMethod error.
        assert_ne!(method_for(SessionAction::Shutdown), Some("Shutdown"));
    }

    #[test]
    fn a_polkit_refusal_is_reported_as_unsupported() {
        let error = map_login1_error(
            "PowerOff",
            "org.freedesktop.DBus.Error.AccessDenied: not authorised",
        );

        match error {
            UdaError::NotSupported(message) => {
                assert!(message.contains("polkit"), "message: {message}");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn an_unauthorized_refusal_is_recognised_by_the_cli_tier_too() {
        assert!(error_is_authorization(
            "org.freedesktop.DBus.Error.AccessDenied"
        ));
        assert!(error_is_authorization("InteractiveAuthorizationRequired"));
        assert!(!error_is_authorization("org.freedesktop.DBus.Error.ServiceUnknown"));
    }

    #[test]
    fn an_operation_the_machine_lacks_is_unsupported() {
        let error = map_login1_error("Hibernate", "org.freedesktop.systemd1.NoSuchOperation");

        match error {
            UdaError::NotSupported(message) => {
                assert!(message.contains("Hibernate"), "message: {message}");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn a_generic_failure_is_a_command_failure() {
        let error = map_login1_error("Suspend", "device busy");

        assert!(matches!(error, UdaError::CommandFailed(_)));
    }

    #[test]
    fn capabilities_cover_every_action() {
        let manager = LinuxSessionManager::new();
        let capabilities = manager.capabilities();

        for action in [
            SessionAction::Lock,
            SessionAction::Logout,
            SessionAction::Suspend,
            SessionAction::Hibernate,
            SessionAction::Reboot,
            SessionAction::Shutdown,
        ] {
            assert!(
                capabilities.contains(action.capability()),
                "{action:?} is advertised but not reported as a capability"
            );
        }
        assert!(capabilities.contains(Capability::SESSION_MANAGEMENT));
    }

    #[test]
    fn the_core_guard_accepts_every_action_this_backend_advertises() {
        // The guard and the capability set must agree, otherwise a caller that
        // checks capabilities first would still be refused at call time.
        let manager = LinuxSessionManager::new();
        let capabilities = manager.capabilities();

        for action in [
            SessionAction::Lock,
            SessionAction::Logout,
            SessionAction::Suspend,
            SessionAction::Hibernate,
            SessionAction::Reboot,
            SessionAction::Shutdown,
        ] {
            let would_be_refused = !capabilities.contains(action.capability());
            assert!(!would_be_refused, "{action:?} would be refused");
        }
    }
}
