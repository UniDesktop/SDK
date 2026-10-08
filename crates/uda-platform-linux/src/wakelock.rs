use std::sync::{Arc, OnceLock};

use uda_core::capability::{Capability, SupportLevel};
use uda_core::wakelock::{WakeLockGuard, WakeLockManager, WakeLockType};
use zbus::Connection;

use crate::error::UdaError;
use crate::internal_dbus;

/// `org.freedesktop.ScreenSaver` on the session bus, where a wake lock lives.
const SCREEN_SAVER_SERVICE: &str = "org.freedesktop.ScreenSaver";
const SCREEN_SAVER_PATH: &str = "/org/freedesktop/ScreenSaver";
const SCREEN_SAVER_INTERFACE: &str = "org.freedesktop.ScreenSaver";

/// `org.freedesktop.login1` on the system bus: systemd-logind, the component
/// that receives - and therefore is the only one that can honour - a
/// `systemd-inhibit` lock.
const LOGIND_SERVICE: &str = "org.freedesktop.login1";

pub struct LinuxWakeLockManager {
    connection: Arc<Connection>,
}

impl LinuxWakeLockManager {
    pub async fn new() -> Result<Self, UdaError> {
        let connection =
            internal_dbus("connecting to the session bus", Connection::session()).await?;
        Ok(Self {
            connection: Arc::new(connection),
        })
    }

    /// The capability set this backend can honestly offer right now.
    ///
    /// `WAKE_LOCK` is claimed only when a receiver that will actually honour a
    /// lock is reachable: the native ScreenSaver service on the session bus,
    /// or systemd-logind on the system bus (which the CLI tier's
    /// `systemd-inhibit` locks are received by). The probe result is cached
    /// per process, exactly like the appearance backend's accent-colour probe;
    /// a receiver does not get installed mid-session, and the query may sit on
    /// hot paths.
    pub fn capabilities(&self) -> Capability {
        wake_lock_support(Self::reachability()).0
    }

    /// How well wake locks work right now, with the reason when degraded.
    ///
    /// `Full` when the native ScreenSaver protocol answers on the session bus;
    /// `Partial` with the reason when only logind is there (the acquire path
    /// then has to go through the CLI tier); `None` when neither is reachable,
    /// matching an empty [`Self::capabilities`].
    pub fn support_level(&self) -> SupportLevel {
        wake_lock_support(Self::reachability()).1
    }

    /// Which wake-lock receivers are reachable, probed once per process.
    ///
    /// A probe error caches as "absent": a receiver that cannot be reached
    /// cannot be claimed either, the same collapse
    /// `LinuxAppearanceManager::accent_color_key_available` applies.
    fn reachability() -> (bool, bool) {
        static REACHABLE: OnceLock<(bool, bool)> = OnceLock::new();
        *REACHABLE.get_or_init(|| {
            let screensaver = probe_or_absent(crate::sync::run_async(screensaver_present()));
            let logind = probe_or_absent(crate::sync::run_async(logind_present()));
            (screensaver, logind)
        })
    }
}

/// Is systemd-logind (`org.freedesktop.login1`) reachable on the system bus?
///
/// The CLI wake-lock tier spawns `systemd-inhibit`, whose lock only exists
/// because logind receives it: with no logind on the system bus nothing
/// answers the inhibit, the screen still sleeps, and reporting success would
/// hand the host a lock that is pure fiction (observed on WSL, which exports
/// no logind). Every step is bounded by `crate::DBUS_TIMEOUT` and mapped per
/// the crate's D-Bus error style; any error means "not proven present", and
/// callers must refuse the CLI tier rather than spawn a lock nobody honours.
pub async fn logind_present() -> Result<bool, UdaError> {
    let connection = internal_dbus("connecting to the system bus", Connection::system()).await?;
    bus_has_service(
        &connection,
        "building the system bus daemon proxy",
        LOGIND_SERVICE,
    )
    .await
}

/// Is the native ScreenSaver inhibit service reachable on the session bus?
///
/// Same contract as [`logind_present`], one tier up: this is the receiver the
/// manager's own `acquire` speaks to.
async fn screensaver_present() -> Result<bool, UdaError> {
    let connection = internal_dbus("connecting to the session bus", Connection::session()).await?;
    bus_has_service(
        &connection,
        "building the session bus daemon proxy",
        SCREEN_SAVER_SERVICE,
    )
    .await
}

/// Whether `service` is currently owned on `connection`.
///
/// `DBusProxy::list_names` resolves to the fdo error type rather than
/// `zbus::Error`, so that one call cannot go through
/// [`crate::internal_dbus`]; it gets its own `DBUS_TIMEOUT` and the same
/// mapping instead.
async fn bus_has_service(
    connection: &Connection,
    step: &str,
    service: &str,
) -> Result<bool, UdaError> {
    let proxy = internal_dbus(step, zbus::fdo::DBusProxy::new(connection)).await?;
    let names = match tokio::time::timeout(crate::DBUS_TIMEOUT, proxy.list_names()).await {
        Ok(Ok(names)) => names,
        Ok(Err(e)) => return Err(UdaError::Internal(format!("listing bus names failed: {e}"))),
        Err(_) => {
            return Err(UdaError::Internal(format!(
                "listing bus names timed out after {:?}",
                crate::DBUS_TIMEOUT
            )))
        }
    };
    Ok(bus_names_contain(
        names.iter().map(|name| name.as_str()),
        service,
    ))
}

/// Collapse a bridged probe into a plain bool.
///
/// The sync bridge wraps the probe's own `Result`, so there are two failure
/// layers: the bridge failing to run the future at all, and the probe failing
/// on the bus. For capability honesty both mean the same thing - "not proven
/// present" - so both collapse to `false`, the same answer
/// `LinuxAppearanceManager::accent_color_key_available` gives its probe.
fn probe_or_absent(probe: Result<Result<bool, UdaError>, UdaError>) -> bool {
    match probe {
        Ok(Ok(reachable)) => reachable,
        Ok(Err(error)) | Err(error) => {
            log::debug!("wake-lock reachability probe did not answer: {error}");
            false
        }
    }
}

/// Turn probe answers into the honest capability bit and support level.
///
/// Pure so the whole matrix is testable without a bus. Both receivers
/// reachable - or the native one alone - is `Full`: the session-bus protocol
/// is the designed path. Only logind is `Partial`, because acquisition then
/// rides the CLI tier, whose locks are bounded rather than indefinite. Neither
/// claims nothing at all.
fn wake_lock_support(reachability: (bool, bool)) -> (Capability, SupportLevel) {
    let (screensaver, logind) = reachability;
    if screensaver {
        return (Capability::WAKE_LOCK, SupportLevel::Full);
    }
    if logind {
        return (
            Capability::WAKE_LOCK,
            SupportLevel::Partial(
                "only the systemd-inhibit CLI tier is reachable: systemd-logind honours the \
                 lock, but each lock is bounded to the CLI tier's lifetime rather than indefinite"
                    .to_string(),
            ),
        );
    }
    (Capability::empty(), SupportLevel::None)
}

/// Whether the well-known `service` name appears in `names`.
///
/// Pure so the matching rule is testable without a bus. Only an exact match
/// counts, and the comparison is case-sensitive, because D-Bus well-known
/// names are: `org.freedesktop.Login1` is a different name from logind's, and
/// a unique connection name (`:1.42`) never matches a well-known service.
fn bus_names_contain<'a, I>(names: I, service: &str) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    names.into_iter().any(|name| name == service)
}

#[async_trait::async_trait]
impl WakeLockManager for LinuxWakeLockManager {
    async fn acquire(
        &self,
        lock_type: WakeLockType,
        reason: &str,
    ) -> Result<WakeLockGuard, UdaError> {
        let proxy = internal_dbus(
            "building the ScreenSaver proxy",
            zbus::Proxy::new(
                &self.connection,
                SCREEN_SAVER_SERVICE,
                SCREEN_SAVER_PATH,
                SCREEN_SAVER_INTERFACE,
            ),
        )
        .await?;

        let flags = lock_type.to_screen_saver_flags();
        let cookie: u32 = internal_dbus(
            "the Inhibit call",
            proxy.call("Inhibit", &(env!("CARGO_PKG_NAME"), reason, flags)),
        )
        .await?;

        let connection = Arc::clone(&self.connection);

        Ok(WakeLockGuard::new(Box::new(move || {
            let connection = Arc::clone(&connection);
            std::thread::spawn(move || {
                let runtime = match tokio::runtime::Runtime::new() {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        log::warn!("could not start a runtime to release the wake lock: {e}");
                        return;
                    }
                };
                runtime.block_on(async move {
                    // Releasing is best effort: the guard has already been
                    // consumed, so a failure can only be logged.
                    let proxy = match internal_dbus(
                        "building the ScreenSaver proxy to release the wake lock",
                        zbus::Proxy::new(
                            &connection,
                            SCREEN_SAVER_SERVICE,
                            SCREEN_SAVER_PATH,
                            SCREEN_SAVER_INTERFACE,
                        ),
                    )
                    .await
                    {
                        Ok(proxy) => proxy,
                        Err(e) => {
                            log::warn!("{e}");
                            return;
                        }
                    };

                    let release = internal_dbus(
                        "releasing the wake lock",
                        proxy.call::<&str, (u32,), ()>("UnInhibit", &(cookie,)),
                    )
                    .await;
                    if let Err(e) = release {
                        log::warn!("{e}");
                    }
                });
            })
            .join()
            .ok();
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn guard_runs_release_on_drop() {
        let released = Arc::new(AtomicBool::new(false));
        let released_clone = Arc::clone(&released);
        let guard = WakeLockGuard::new(Box::new(move || {
            released_clone.store(true, Ordering::SeqCst);
        }));
        drop(guard);
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn guard_manual_release() {
        let released = Arc::new(AtomicBool::new(false));
        let released_clone = Arc::clone(&released);
        let guard = WakeLockGuard::new(Box::new(move || {
            released_clone.store(true, Ordering::SeqCst);
        }));
        guard.release();
        assert!(released.load(Ordering::SeqCst));
    }

    #[test]
    fn wake_lock_type_flags() {
        assert_eq!(WakeLockType::PreventDisplaySleep.to_screen_saver_flags(), 8);
        assert_eq!(WakeLockType::PreventSystemIdle.to_screen_saver_flags(), 12);
    }

    #[test]
    fn bus_names_match_only_the_exact_well_known_service() {
        let bus = ["org.freedesktop.DBus", "org.freedesktop.login1", ":1.42"];
        assert!(bus_names_contain(bus, LOGIND_SERVICE));
        assert!(bus_names_contain(
            ["org.freedesktop.ScreenSaver"],
            SCREEN_SAVER_SERVICE
        ));

        // D-Bus names are case-sensitive: logind is lowercase, and a different
        // case is a different peer.
        assert!(!bus_names_contain(
            ["org.freedesktop.Login1"],
            LOGIND_SERVICE
        ));
        // A name that merely contains or extends the service is not the
        // service.
        assert!(!bus_names_contain(
            ["org.freedesktop.login1.daemon"],
            LOGIND_SERVICE
        ));
        assert!(!bus_names_contain(
            ["xxorg.freedesktop.login1"],
            LOGIND_SERVICE
        ));
        // Unique connection names never count: without the well-known name a
        // caller cannot address the service anyway.
        assert!(!bus_names_contain([":1.42"], LOGIND_SERVICE));
        assert!(!bus_names_contain([] as [&str; 0], LOGIND_SERVICE));
    }

    #[test]
    fn the_capability_matrix_claims_wake_lock_whenever_a_receiver_answers() {
        // Native tier reachable: Full, with or without the CLI tier behind it.
        let (caps, level) = wake_lock_support((true, false));
        assert_eq!(caps, Capability::WAKE_LOCK);
        assert_eq!(level, SupportLevel::Full);

        let (caps, level) = wake_lock_support((true, true));
        assert_eq!(caps, Capability::WAKE_LOCK);
        assert_eq!(level, SupportLevel::Full);
    }

    #[test]
    fn logind_only_degrades_to_partial_with_a_reason() {
        let (caps, level) = wake_lock_support((false, true));
        assert_eq!(caps, Capability::WAKE_LOCK);
        let reason = match &level {
            SupportLevel::Partial(reason) => reason.as_str(),
            _ => panic!("logind-only reachability must degrade to Partial, got {level:?}"),
        };
        assert!(
            !reason.is_empty(),
            "a degraded answer must carry its reason (AGENTS.md Principle 1)"
        );
        assert_eq!(level.reason(), Some(reason));
    }

    #[test]
    fn no_receiver_claims_nothing() {
        let (caps, level) = wake_lock_support((false, false));
        assert!(caps.is_empty(), "no receiver, no claimed capability");
        assert_eq!(level, SupportLevel::None);
    }
}
