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
//!   `org.kde.Shutdown`, `org.xfce.SessionManager`) is tried.
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
//! # Capabilities are probed, not assumed
//!
//! [`LinuxSessionManager::capabilities`] asks the buses which receivers are
//! actually there before advertising anything: the full set only when
//! `org.freedesktop.login1` answers on the system bus, `LOCK` alone when only
//! `org.freedesktop.ScreenSaver` answers on the session bus, and nothing when
//! neither does (a WSL host exports neither). The probe runs once per process
//! and is cached, the same trade the wake-lock backend makes.
//!
//! Whatever the probe said, an action that is attempted and then not delivered
//! (logind refusing or timing out, `loginctl` exiting nonzero) reports
//! `UDA_ERR_NOT_SUPPORTED`, the one failure code `include/uda.h` promises C
//! hosts for an attempted session action; the diagnostic stays in the message.
//!
//! # Timeouts
//!
//! Every D-Bus await is wrapped in `DBUS_TIMEOUT`, so a logind that is
//! installing an update (and therefore not answering) costs a bounded wait
//! instead of a hung caller.
//!
//! See `docs/internals/session_specs.md` for the full mapping.

use std::process::Command;
use std::sync::OnceLock;

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::session::{SessionAction, SessionManager};
use zbus::Connection;

use crate::wakelock::{logind_present, screensaver_present};
use crate::DBUS_TIMEOUT;
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
    ///
    /// A bus that cannot be reached folds into [`not_delivered`] like every
    /// other failure in this module.
    async fn system_connection() -> Result<Connection, UdaError> {
        match tokio::time::timeout(DBUS_TIMEOUT, Connection::system()).await {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(e)) => Err(not_delivered(format!(
                "could not reach the system bus: {e}"
            ))),
            Err(_) => Err(not_delivered(
                "connecting to the system bus timed out".to_string(),
            )),
        }
    }

    /// Connect to the **session** bus, where the screen saver lives.
    ///
    /// Same contract as [`Self::system_connection`].
    async fn session_connection() -> Result<Connection, UdaError> {
        match tokio::time::timeout(DBUS_TIMEOUT, Connection::session()).await {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(e)) => Err(not_delivered(format!(
                "could not reach the session bus: {e}"
            ))),
            Err(_) => Err(not_delivered(
                "connecting to the session bus timed out".to_string(),
            )),
        }
    }

    /// Call a no-argument `login1.Manager` power method with `interactive: false`.
    ///
    /// Synchronous on the outside (the trait is synchronous), asynchronous on
    /// the inside: the future is driven by `crate::sync::run_async`, which is
    /// safe to call from inside a tokio runtime as well (P1-15).
    fn login1_power(&self, method: &str) -> Result<(), UdaError> {
        let method = method.to_string();
        // The future takes ownership of `method`; `action` keeps the name for
        // the bridge-failure message below.
        let action = method.clone();

        crate::sync::run_async(async move {
            let connection = Self::system_connection().await?;

            let proxy = match tokio::time::timeout(
                DBUS_TIMEOUT,
                zbus::Proxy::new(&connection, LOGIN1_SERVICE, LOGIN1_PATH, LOGIN1_INTERFACE),
            )
            .await
            {
                Ok(Ok(proxy)) => proxy,
                Ok(Err(e)) => {
                    return Err(not_delivered(format!(
                        "could not build a logind proxy: {e}"
                    )));
                }
                Err(_) => {
                    return Err(not_delivered(
                        "building the logind proxy timed out".to_string(),
                    ));
                }
            };

            // `interactive: false` throughout: the caller has already confirmed,
            // so logind must not open its own confirmation dialog.
            match tokio::time::timeout(
                DBUS_TIMEOUT,
                proxy.call::<&str, (bool,), ()>(method.as_str(), &(false,)),
            )
            .await
            {
                Ok(Ok(())) => {
                    log::debug!("logind accepted {method}");
                    Ok(())
                }
                Ok(Err(e)) => Err(map_login1_error(&method, &e.to_string())),
                Err(_) => Err(not_delivered(format!(
                    "logind {method} timed out after {DBUS_TIMEOUT:?}"
                ))),
            }
        })
        // A bridge failure would otherwise leak as -5; fold it into the
        // contract error like every other outcome.
        .map_err(|e| {
            not_delivered(format!(
                "the logind {action} call could not be attempted: {e}"
            ))
        })?
    }

    /// Lock through `org.freedesktop.ScreenSaver`, then through `loginctl`.
    fn lock_via_screen_saver(&self) -> Result<(), UdaError> {
        crate::sync::run_async(async {
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
                    return Err(not_delivered(format!(
                        "could not build a screen-saver proxy: {e}"
                    )));
                }
                Err(_) => {
                    return Err(not_delivered(
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
                Ok(Err(e)) => Err(not_delivered(format!("the screen saver refused Lock: {e}"))),
                Err(_) => Err(not_delivered(
                    "the screen saver did not answer Lock".to_string(),
                )),
            }
        })
        .map_err(|e| not_delivered(format!("the screen-saver lock could not be attempted: {e}")))?
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
            Ok(status) => Err(map_loginctl_exit(status.code())),
            Err(e) => Err(map_loginctl_spawn(e)),
        }
    }

    /// End the calling user's session.
    fn logout(&self) -> Result<(), UdaError> {
        // `TerminateSession("")` means "the calling session", so no session id
        // has to be enumerated first.
        let logind = crate::sync::run_async(async {
            let connection = Self::system_connection().await?;

            let proxy = tokio::time::timeout(
                DBUS_TIMEOUT,
                zbus::Proxy::new(&connection, LOGIN1_SERVICE, LOGIN1_PATH, LOGIN1_INTERFACE),
            )
            .await
            .map_err(|_| not_delivered("building the logind proxy timed out".to_string()))?
            .map_err(|e| not_delivered(format!("logind proxy: {e}")))?;

            tokio::time::timeout(
                DBUS_TIMEOUT,
                proxy.call::<&str, (&str,), ()>("TerminateSession", &("",)),
            )
            .await
            .map_err(|_| not_delivered("logind TerminateSession timed out".to_string()))?
            .map_err(|e| map_login1_error("TerminateSession", &e.to_string()))
        })
        .map_err(|e| not_delivered(format!("the logind logout could not be attempted: {e}")))?;

        match logind {
            Ok(()) => Ok(()),
            Err(logind_error) => {
                // Tier 3: ask the desktop's own session manager. GNOME, KDE and
                // XFCE each expose a logout entry point under a different name
                // *and* a different wire signature, so the candidates carry
                // their arguments (P1-19); all three are tried because
                // `$XDG_CURRENT_DESKTOP` is not reliable enough to pick one.
                log::debug!(
                    "logind logout failed ({logind_error}); trying the desktop session manager"
                );

                for candidate in LOGOUT_CANDIDATES {
                    if logout_via_desktop(*candidate) {
                        return Ok(());
                    }
                }

                Err(logind_error)
            }
        }
    }
}

/// One desktop session manager's logout entry point, with the exact wire
/// signature of its method (P1-19: a single shared argument list cannot satisfy
/// all three interfaces - KDE takes none and XFCE takes two booleans, so a
/// uniform `(0u32,)` body always came back as `InvalidArgs`).
///
/// `Copy`, and every field is `'static`, so a candidate can travel into the
/// future [`logout_via_desktop`] hands to `crate::sync::run_async`.
#[derive(Clone, Copy)]
struct LogoutCandidate {
    service: &'static str,
    path: &'static str,
    interface: &'static str,
    method: &'static str,
    args: LogoutArgs,
}

/// The typed argument lists the three logout entry points accept.
#[derive(Clone, Copy)]
enum LogoutArgs {
    /// GNOME `org.gnome.SessionManager.Logout(u mode)`: mode 0 asks for a
    /// normal, user-confirmed logout.
    Gnome(u32),
    /// KDE `org.kde.Shutdown.logout()` takes no arguments at all.
    Kde,
    /// XFCE `org.xfce.Session.Manager.Logout(bb)`.
    Xfce(bool, bool),
}

/// The Tier-3 logout candidates, in try order.
const LOGOUT_CANDIDATES: &[LogoutCandidate] = &[
    LogoutCandidate {
        service: "org.gnome.SessionManager",
        path: "/org/gnome/SessionManager",
        interface: "org.gnome.SessionManager",
        method: "Logout",
        args: LogoutArgs::Gnome(0),
    },
    LogoutCandidate {
        service: "org.kde.Shutdown",
        path: "/Shutdown",
        interface: "org.kde.Shutdown",
        method: "logout",
        args: LogoutArgs::Kde,
    },
    LogoutCandidate {
        service: "org.xfce.SessionManager",
        path: "/org/xfce/SessionManager",
        interface: "org.xfce.Session.Manager",
        method: "Logout",
        // Upstream signature is `Logout(allow_save: b, arbitrary: b)`.
        // `allow_save = true` keeps the session-save option the desktop's own
        // logout dialog offers a user; `arbitrary = false` requests a normal
        // logout instead of forcing one past the session manager's checks.
        args: LogoutArgs::Xfce(true, false),
    },
];

/// Try one desktop session manager's logout entry point.
///
/// A service that is not running is the normal case (only one desktop is ever
/// present), so a missing service is `false` rather than an error - the caller
/// keeps trying the next candidate and only reports an error when all of them
/// fail.
fn logout_via_desktop(candidate: LogoutCandidate) -> bool {
    crate::sync::run_async(async move {
        let LogoutCandidate {
            service,
            path,
            interface,
            method,
            args,
        } = candidate;

        let connection = match tokio::time::timeout(DBUS_TIMEOUT, Connection::session()).await {
            Ok(Ok(connection)) => connection,
            Ok(Err(e)) => {
                log::debug!("no session bus for {service}: {e}");
                return false;
            }
            Err(_) => {
                log::debug!("connecting to the session bus timed out for {service}");
                return false;
            }
        };

        let Ok(Ok(proxy)) = tokio::time::timeout(
            DBUS_TIMEOUT,
            zbus::Proxy::new(&connection, service, path, interface),
        )
        .await
        else {
            return false;
        };

        // Each interface disagrees about the Logout signature, so the argument
        // tuple comes from the candidate instead of being shared.
        let call = match args {
            LogoutArgs::Gnome(mode) => {
                tokio::time::timeout(
                    DBUS_TIMEOUT,
                    proxy.call::<&str, (u32,), ()>(method, &(mode,)),
                )
                .await
            }
            LogoutArgs::Kde => {
                tokio::time::timeout(DBUS_TIMEOUT, proxy.call::<&str, (), ()>(method, &())).await
            }
            LogoutArgs::Xfce(allow_save, arbitrary) => {
                tokio::time::timeout(
                    DBUS_TIMEOUT,
                    proxy.call::<&str, (bool, bool), ()>(method, &(allow_save, arbitrary)),
                )
                .await
            }
        };

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
    })
    .unwrap_or(false)
}

/// Translate a logind D-Bus error into a typed [`UdaError`].
///
/// logind reports "not authorised" as `org.freedesktop.DBus.Error.AccessDenied`,
/// `NotAuthorized`, or polkit's `InteractiveAuthorizationRequired` when the rule
/// wants a dialog the caller cannot show. "This machine cannot do it" arrives as
/// `org.freedesktop.systemd1.NoSuchOperation` or a plain `Failed`. The case
/// distinction lives in the message so a caller can show "ask your
/// administrator" instead of a generic failure, which matters because power
/// actions are exactly where users hit polkit.
///
/// Every outcome - polkit refusal, "this machine cannot", or a plain failure -
/// maps to [`UdaError::NotSupported`] via [`not_delivered`], so one status
/// comparison in a host catches all of them; only the message distinguishes
/// the cases.
///
/// The error is taken as already-rendered text so the classification is a pure
/// function over a string: unit-testable, and free of any assumption about how
/// the D-Bus library formats a method error.
fn map_login1_error(method: &str, text: &str) -> UdaError {
    if error_is_authorization(text) {
        return not_delivered(format!(
            "logind refused {method}: the caller is not authorised (polkit)"
        ));
    }

    if text.contains("NoSuchOperation") || text.contains("NotSupported") {
        return not_delivered(format!("logind cannot {method} on this machine: {text}"));
    }

    not_delivered(format!("logind {method} failed: {text}"))
}

/// Translate a failed `loginctl lock-session` run into the contract error.
///
/// A nonzero exit is an attempted action the CLI refused (the WSL case): the
/// host must see `UDA_ERR_NOT_SUPPORTED`, with the exit code kept in the
/// message for diagnosis.
fn map_loginctl_exit(code: Option<i32>) -> UdaError {
    not_delivered(format!(
        "loginctl lock-session exited with {}",
        code.unwrap_or(-1)
    ))
}

/// Translate a `loginctl` spawn failure into the contract error.
///
/// A missing `loginctl` means the CLI tier cannot deliver at all, so it folds
/// into the same contract error as a refused run; the OS error stays in the
/// message.
fn map_loginctl_spawn(error: std::io::Error) -> UdaError {
    not_delivered(format!("could not run loginctl: {error}"))
}

/// The single error an attempted session action may report when it cannot
/// deliver.
///
/// `include/uda.h` promises C hosts that an attempted `uda_session_*` action
/// which is not delivered fails with `UDA_ERR_NOT_SUPPORTED` and nothing else,
/// so one status comparison is enough to degrade. Every failure path in this
/// module funnels through this constructor, directly or through
/// [`map_login1_error`] and the `loginctl` mappers, which makes that promise
/// hold by construction; the diagnostic always stays in the message.
fn not_delivered(detail: impl std::fmt::Display) -> UdaError {
    UdaError::NotSupported(detail.to_string())
}

/// Which session receivers are reachable, probed once per process.
///
/// Returns `(logind, screen_saver)`, cached in a process-wide [`OnceLock`]:
/// receivers are not installed mid-session, while `capabilities()` sits on the
/// hot path (the core `perform` guard consults it before *every* action), so
/// one bounded probe pair per process beats a bus round trip per query. A
/// receiver that appears later is only noticed after a restart, which errs in
/// the honest direction: something unadvertised stays unavailable, never the
/// reverse. A probe failure caches as "absent"; see [`probe_reachable`].
fn session_reachability() -> (bool, bool) {
    static REACHABLE: OnceLock<(bool, bool)> = OnceLock::new();
    *REACHABLE.get_or_init(|| {
        let logind = probe_reachable(crate::sync::run_async(logind_present()), "logind");
        let screen_saver = probe_reachable(
            crate::sync::run_async(screensaver_present()),
            "screen saver",
        );
        (logind, screen_saver)
    })
}

/// Collapse a bridged probe into a plain bool.
///
/// The sync bridge wraps the probe's own `Result`, so there are two failure
/// layers: the bridge failing to run the future at all, and the probe failing
/// on the bus. For capability honesty both mean the same thing - "not proven
/// present" - so both collapse to `false`, matching the wake-lock backend's
/// fold.
fn probe_reachable(probe: Result<Result<bool, UdaError>, UdaError>, receiver: &str) -> bool {
    match probe {
        Ok(Ok(reachable)) => reachable,
        Ok(Err(error)) | Err(error) => {
            log::debug!("the {receiver} reachability probe did not answer: {error}");
            false
        }
    }
}

/// Turn probe answers into the honest capability set.
///
/// Pure so the whole matrix is testable without a bus. With logind reachable,
/// the full set is honest: logind receives every power and session-management
/// action, and its presence is also what the `loginctl` CLI tier needs, so
/// `LOCK` is covered twice. With only the screen saver answering, `LOCK` alone
/// is claimable: no receiver exists for the power actions, and the desktop
/// logout managers are tried opportunistically rather than probed, so `LOGOUT`
/// stays unadvertised. With neither reachable - the WSL case - nothing is
/// claimed.
fn session_capability_set(logind: bool, screen_saver: bool) -> Capability {
    if logind {
        return Capability::SESSION_MANAGEMENT
            | Capability::LOCK
            | Capability::LOGOUT
            | Capability::SUSPEND
            | Capability::HIBERNATE
            | Capability::REBOOT
            | Capability::SHUTDOWN;
    }
    if screen_saver {
        return Capability::LOCK;
    }
    Capability::empty()
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
        // Probed, not hardcoded: a WSL host exports neither receiver. Whether
        // logind *authorises* the caller remains a runtime question for the
        // methods above, not a capability question.
        let (logind, screen_saver) = session_reachability();
        session_capability_set(logind, screen_saver)
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
    fn the_xfce_candidate_uses_the_upstream_service_name_and_path() {
        // The Tier-3 fallback used to dial `org.xfce.Session.Manager` /
        // `/org/xfce/Session/Manager`, which no service publishes (P1-19).
        let xfce = LOGOUT_CANDIDATES
            .iter()
            .find(|candidate| candidate.service.starts_with("org.xfce"))
            .expect("the XFCE candidate is part of the table");

        assert_eq!(xfce.service, "org.xfce.SessionManager");
        assert_eq!(xfce.path, "/org/xfce/SessionManager");
        assert_eq!(xfce.interface, "org.xfce.Session.Manager");
        assert_eq!(xfce.method, "Logout");
    }

    #[test]
    fn every_logout_candidate_carries_its_own_wire_signature() {
        // GNOME `Logout(u)`, KDE `logout()`, XFCE `Logout(bb)`: a uniform
        // `(0u32,)` body made every non-GNOME candidate fail with InvalidArgs.
        let [gnome, kde, xfce] = LOGOUT_CANDIDATES else {
            panic!("the candidate table has exactly three entries");
        };

        assert_eq!(gnome.service, "org.gnome.SessionManager");
        assert!(matches!(gnome.args, LogoutArgs::Gnome(0)));

        assert_eq!(kde.service, "org.kde.Shutdown");
        assert_eq!(kde.method, "logout");
        assert!(matches!(kde.args, LogoutArgs::Kde));

        assert!(matches!(xfce.args, LogoutArgs::Xfce(true, false)));
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
        assert!(!error_is_authorization(
            "org.freedesktop.DBus.Error.ServiceUnknown"
        ));
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
    fn a_generic_failure_is_still_an_undelivered_attempt() {
        // Whatever logind answered, a host that checks
        // `status == UDA_ERR_NOT_SUPPORTED` must catch it, so even a plain
        // failure folds into NotSupported; the reason stays in the message.
        let error = map_login1_error("Suspend", "device busy");

        match error {
            UdaError::NotSupported(message) => {
                assert!(message.contains("Suspend"), "message: {message}");
                assert!(message.contains("device busy"), "message: {message}");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn every_session_attempt_failure_is_not_supported() {
        // The C-ABI contract (include/uda.h, `uda_session_*`): an action that
        // was attempted but not delivered answers UDA_ERR_NOT_SUPPORTED and
        // nothing else, so one comparison degrades a host. Every mapping this
        // module can produce for an attempt is enumerated here; a new failure
        // path must route through one of them (or `not_delivered` itself).
        let attempt_failures = [
            not_delivered("anything at all"),
            map_login1_error("Suspend", "device busy"),
            map_login1_error(
                "PowerOff",
                "org.freedesktop.DBus.Error.AccessDenied: not authorised",
            ),
            map_login1_error("Hibernate", "org.freedesktop.systemd1.NoSuchOperation"),
            map_loginctl_exit(Some(1)),
            map_loginctl_exit(None),
            map_loginctl_spawn(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            )),
        ];

        for error in attempt_failures {
            assert!(
                matches!(error, UdaError::NotSupported(_)),
                "an attempted session action must report NotSupported, got {error:?}"
            );
        }
    }

    #[test]
    fn the_loginctl_exit_message_keeps_the_diagnostic() {
        // The WSL reproduction: `loginctl lock-session` exiting 1 must surface
        // as UDA_ERR_NOT_SUPPORTED (-2), which include/uda.h promises, not as
        // a generic internal error (-5).
        let error = map_loginctl_exit(Some(1));

        match error {
            UdaError::NotSupported(message) => {
                assert!(
                    message.contains("loginctl lock-session exited with 1"),
                    "message: {message}"
                );
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn the_loginctl_spawn_failure_keeps_the_os_error() {
        let error = map_loginctl_spawn(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no such file",
        ));

        match error {
            UdaError::NotSupported(message) => {
                assert!(message.contains("loginctl"), "message: {message}");
                assert!(message.contains("no such file"), "message: {message}");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn logind_reachability_advertises_the_full_set() {
        // Logind receives every power and session-management action, and its
        // presence is what the loginctl CLI tier needs, so both quadrants with
        // logind reachable claim all seven bits.
        for (logind, screen_saver) in [(true, true), (true, false)] {
            let capabilities = session_capability_set(logind, screen_saver);

            assert!(
                capabilities.contains(Capability::SESSION_MANAGEMENT),
                "logind reachable (screen saver: {screen_saver}) must claim \
                 SESSION_MANAGEMENT"
            );
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
                    "{action:?} is not advertised behind logind (screen saver: \
                     {screen_saver})"
                );
            }
        }
    }

    #[test]
    fn a_screen_saver_without_logind_advertises_lock_alone() {
        // Without logind the power actions have no receiver at all and the
        // loginctl CLI tier is gone with it; the desktop logout managers are
        // tried opportunistically, never probed. Only the screen-saver lock is
        // honestly claimable.
        let capabilities = session_capability_set(false, true);

        assert_eq!(capabilities, Capability::LOCK);
        for action in [
            SessionAction::Logout,
            SessionAction::Suspend,
            SessionAction::Hibernate,
            SessionAction::Reboot,
            SessionAction::Shutdown,
        ] {
            assert!(
                !capabilities.contains(action.capability()),
                "{action:?} must not be advertised without logind"
            );
        }
    }

    #[test]
    fn without_any_receiver_nothing_is_advertised() {
        // The WSL case: no logind, no screen saver, so the answer is the empty
        // set rather than a full house nothing can deliver.
        assert!(session_capability_set(false, false).is_empty());
    }

    #[test]
    fn a_failed_probe_counts_as_an_absent_receiver() {
        // Conservative direction: any failure - bridge or bus - collapses to
        // "not proven present", never to a claim.
        assert!(probe_reachable(Ok(Ok(true)), "logind"));
        assert!(!probe_reachable(Ok(Ok(false)), "logind"));
        assert!(!probe_reachable(
            Ok(Err(UdaError::DetectionFailed("no bus".to_string()))),
            "logind"
        ));
        assert!(!probe_reachable(
            Err(UdaError::Internal("no runtime".to_string())),
            "logind"
        ));
    }

    #[test]
    fn the_probed_service_names_are_the_exact_well_known_ones() {
        // The probe and the Lock proxy both address these services by name,
        // with exact, case-sensitive matching (pinned by the wake-lock
        // backend's `bus_names_contain` tests), so the constants must be
        // spelled exactly as the services publish themselves.
        assert_eq!(LOGIN1_SERVICE, "org.freedesktop.login1");
        assert_eq!(SCREENSAVER_SERVICE, "org.freedesktop.ScreenSaver");
    }
}
