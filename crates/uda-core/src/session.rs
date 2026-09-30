//! Cross-platform session and power lifecycle interface.
//!
//! Six actions make up the surface: lock the session, log out, suspend,
//! hibernate, reboot, and shut down. They are grouped because both platforms
//! reach them through the same two subsystems (a session bus on Linux, the
//! Win32 power and shutdown APIs on Windows) and because callers almost always
//! want to ask "what can this machine do?" *before* they ask for a specific
//! action.
//!
//! # Safety posture
//!
//! Five of the six actions are destructive: they end the user's session or stop
//! the machine. The model therefore separates
//!
//! - [`SessionManager::capabilities`], which is a *static, side-effect-free*
//!   query a UI can call freely, from
//! - the six action methods, each of which performs an irreversible system
//!   operation the moment it returns `Ok`.
//!
//! A caller is expected to gate the destructive methods behind
//! [`SessionAction::is_destructive`] plus its own confirmation dialog. Only
//! [`SessionAction::Lock`] is safe to automate: it locks the session and leaves
//! every running program alone.
//!
//! No action is attempted on a platform that cannot deliver it: each method
//! returns [`UdaError::NotSupported`] when the corresponding capability bit is
//! absent, so "unsupported" is distinguishable from "tried and failed".
//!
//! See `docs/internals/session_specs.md` for the protocol-level mapping.

use crate::capability::Capability;
use crate::error::UdaError;

/// A session or power action to perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    /// Lock the session; the user's programs keep running.
    Lock,
    /// End the calling user's session.
    Logout,
    /// Suspend the machine to RAM.
    Suspend,
    /// Hibernate the machine to disk.
    Hibernate,
    /// Restart the machine.
    Reboot,
    /// Power the machine off.
    Shutdown,
}

impl SessionAction {
    /// The capability bit a backend must advertise to support this action.
    ///
    /// A caller can compare this against [`SessionManager::capabilities`] to
    /// decide whether to offer the action at all, without triggering it.
    pub const fn capability(self) -> Capability {
        match self {
            Self::Lock => Capability::LOCK,
            Self::Logout => Capability::LOGOUT,
            Self::Suspend => Capability::SUSPEND,
            Self::Hibernate => Capability::HIBERNATE,
            Self::Reboot => Capability::REBOOT,
            Self::Shutdown => Capability::SHUTDOWN,
        }
    }

    /// Whether performing this action can lose the user's unsaved work. Only
    /// [`SessionAction::Lock`] is safe to automate; the rest must be gated behind
    /// an explicit confirmation in the host application.
    pub const fn is_destructive(self) -> bool {
        !matches!(self, Self::Lock)
    }
}

/// Read and drive the machine's session and power state.
///
/// Each method leaves running programs alone where the platform allows it, and
/// the protocol-level mapping lives in `docs/internals/session_specs.md`.
pub trait SessionManager {
    /// Lock the session.
    fn lock(&self) -> Result<(), UdaError>;

    /// End the calling user's session.
    fn logout(&self) -> Result<(), UdaError>;

    /// Suspend the machine to RAM.
    fn suspend(&self) -> Result<(), UdaError>;

    /// Hibernate the machine to disk.
    fn hibernate(&self) -> Result<(), UdaError>;

    /// Restart the machine.
    fn reboot(&self) -> Result<(), UdaError>;

    /// Power the machine off.
    fn shutdown(&self) -> Result<(), UdaError>;

    /// The session actions this platform's backend can deliver.
    ///
    /// Computed once at construction and answering for the platform, not the
    /// moment: a machine that *could* sleep but has hibernation switched off
    /// still reports [`Capability::HIBERNATE`], because the code path exists and
    /// the failure would be a runtime `Err`, not an `Unsupported`.
    fn capabilities(&self) -> Capability;
}

/// Perform `action` on `manager`, refusing a call the platform cannot deliver.
///
/// Every caller shares this guard, so the "unsupported" answer is identical
/// whichever method was chosen. It lives in the core rather than in each backend
/// so the check is unit-testable without a D-Bus session or a Windows host.
///
/// `manager` is a `&dyn` so one dynamic dispatch per call is all it costs, which
/// is irrelevant next to the round trip it protects.
pub fn perform(manager: &dyn SessionManager, action: SessionAction) -> Result<(), UdaError> {
    let capabilities = manager.capabilities();

    if !capabilities.contains(action.capability()) {
        return Err(UdaError::NotSupported(format!(
            "this platform's session backend does not support {action:?}"
        )));
    }

    match action {
        SessionAction::Lock => manager.lock(),
        SessionAction::Logout => manager.logout(),
        SessionAction::Suspend => manager.suspend(),
        SessionAction::Hibernate => manager.hibernate(),
        SessionAction::Reboot => manager.reboot(),
        SessionAction::Shutdown => manager.shutdown(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A manager that records calls and reports a fixed capability set, so the
    /// suite never touches the machine's power state.
    struct RecordingManager {
        capabilities: Capability,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl RecordingManager {
        fn with(capabilities: Capability) -> Self {
            Self {
                capabilities,
                calls: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls
                .lock()
                .expect("the mock lock is never poisoned")
                .clone()
        }
    }

    impl SessionManager for RecordingManager {
        fn lock(&self) -> Result<(), UdaError> {
            self.calls.lock().unwrap().push("lock");
            Ok(())
        }

        fn logout(&self) -> Result<(), UdaError> {
            self.calls.lock().unwrap().push("logout");
            Ok(())
        }

        fn suspend(&self) -> Result<(), UdaError> {
            self.calls.lock().unwrap().push("suspend");
            Ok(())
        }

        fn hibernate(&self) -> Result<(), UdaError> {
            self.calls.lock().unwrap().push("hibernate");
            Ok(())
        }

        fn reboot(&self) -> Result<(), UdaError> {
            self.calls.lock().unwrap().push("reboot");
            Ok(())
        }

        fn shutdown(&self) -> Result<(), UdaError> {
            self.calls.lock().unwrap().push("shutdown");
            Ok(())
        }

        fn capabilities(&self) -> Capability {
            self.capabilities
        }
    }

    #[test]
    fn every_action_maps_to_a_distinct_capability_bit() {
        // Two actions sharing a bit would let one of them be offered on a
        // platform that cannot deliver it.
        let bits = [
            SessionAction::Lock.capability(),
            SessionAction::Logout.capability(),
            SessionAction::Suspend.capability(),
            SessionAction::Hibernate.capability(),
            SessionAction::Reboot.capability(),
            SessionAction::Shutdown.capability(),
        ];

        for (index, bit) in bits.iter().enumerate() {
            for other in bits.iter().skip(index + 1) {
                assert_ne!(bit, other, "actions {index} share a capability bit");
            }
        }
    }

    #[test]
    fn only_locking_is_non_destructive() {
        assert!(!SessionAction::Lock.is_destructive());

        for action in [
            SessionAction::Logout,
            SessionAction::Suspend,
            SessionAction::Hibernate,
            SessionAction::Reboot,
            SessionAction::Shutdown,
        ] {
            assert!(action.is_destructive(), "{action:?} must be destructive");
        }
    }

    #[test]
    fn a_supported_action_reaches_the_backend() {
        let manager = RecordingManager::with(Capability::LOCK);

        assert!(perform(&manager, SessionAction::Lock).is_ok());
        assert_eq!(manager.calls(), vec!["lock"]);
    }

    #[test]
    fn an_unsupported_action_is_refused_before_any_system_call() {
        // The guard is the safety net: without it a caller that skipped the
        // capability check would still be stopped here.
        let manager = RecordingManager::with(Capability::LOCK);

        let outcome = perform(&manager, SessionAction::Shutdown);

        match outcome {
            Err(UdaError::NotSupported(message)) => {
                assert!(message.contains("Shutdown"), "message: {message}");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
        // The decisive part: the mock recorded nothing, so no backend ran.
        assert!(manager.calls().is_empty());
    }

    #[test]
    fn a_capability_flag_for_one_action_does_not_authorise_another() {
        // A backend that can reboot but not shut down must not accept a
        // shutdown request just because "power actions" were available.
        let manager = RecordingManager::with(Capability::REBOOT);

        assert!(perform(&manager, SessionAction::Reboot).is_ok());
        assert!(perform(&manager, SessionAction::Shutdown).is_err());
        assert!(perform(&manager, SessionAction::Suspend).is_err());
        assert_eq!(manager.calls(), vec!["reboot"]);
    }

    #[test]
    fn the_full_flag_set_authorises_every_action() {
        let all = Capability::LOCK
            | Capability::LOGOUT
            | Capability::SUSPEND
            | Capability::HIBERNATE
            | Capability::REBOOT
            | Capability::SHUTDOWN;
        let manager = RecordingManager::with(all);

        for action in [
            SessionAction::Lock,
            SessionAction::Logout,
            SessionAction::Suspend,
            SessionAction::Hibernate,
            SessionAction::Reboot,
            SessionAction::Shutdown,
        ] {
            assert!(perform(&manager, action).is_ok(), "{action:?} was refused");
        }
    }

    #[test]
    fn session_management_is_an_umbrella_not_an_action_gate() {
        // `SESSION_MANAGEMENT` says a backend exists; it must not by itself
        // authorise any action, or a caller asking only "is there a backend?"
        // could accidentally power the machine off.
        let manager = RecordingManager::with(Capability::SESSION_MANAGEMENT);

        assert!(perform(&manager, SessionAction::Lock).is_err());
        assert!(manager.calls().is_empty());
    }

    #[test]
    fn a_backend_error_propagates_unchanged() {
        // A supported action that the platform then rejects must surface the
        // backend's own diagnosis, not a generic failure.
        struct FailingManager;

        impl SessionManager for FailingManager {
            fn lock(&self) -> Result<(), UdaError> {
                Err(UdaError::CommandFailed(
                    "the screen saver refused".to_string(),
                ))
            }

            fn logout(&self) -> Result<(), UdaError> {
                Ok(())
            }

            fn suspend(&self) -> Result<(), UdaError> {
                Ok(())
            }

            fn hibernate(&self) -> Result<(), UdaError> {
                Ok(())
            }

            fn reboot(&self) -> Result<(), UdaError> {
                Ok(())
            }

            fn shutdown(&self) -> Result<(), UdaError> {
                Ok(())
            }

            fn capabilities(&self) -> Capability {
                Capability::LOCK
            }
        }

        match perform(&FailingManager, SessionAction::Lock) {
            Err(UdaError::CommandFailed(message)) => {
                assert!(message.contains("screen saver"), "message: {message}");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }
}
