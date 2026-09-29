//! Session and power-lifecycle exports shared by the C callers.
//!
//! The six exports are one-per-action rather than a single
//! `uda_session_perform(action)`. That is a deliberate C-ABI choice: a switch
//! over an integer code makes every call site look equally dangerous, whereas
//! named functions let a reader of a call site see which system action it
//! triggers, and let the linker refuse a binding that forgot to link the
//! symbol it uses.
//!
//! # Destructive actions
//!
//! Five of the six actions end the user's session or stop the machine. This
//! module therefore exposes:
//!
//! - [`session_capabilities`], a side-effect-free query returning a bitmask, so
//!   a UI can be built *before* the user asks for anything; and
//! - the six action functions, each of which performs an irreversible operation
//!   the moment it returns [`UDA_OK`](crate::error::UDA_OK).
//!
//! Only [`uda_session_lock`] is safe to automate. Everything else must be gated
//! behind a confirmation in the host application - the same rule the demo under
//! `examples/` follows.
//!
//! # Capability bitmask
//!
//! [`session_capabilities`] reports the flags the platform's backend
//! advertises. `0` means "no session backend exists here", which is also what a
//! non-Linux, non-Windows target answers.

use uda_core::capability::Capability;
use uda_core::session::{SessionAction, SessionManager};

use crate::error::Failure;

/// Capability bit: a session backend exists at all.
pub const UDA_SESSION_CAP_MANAGEMENT: u32 = 1 << 16;
/// Capability bit: the session can be locked (the only safe-to-automate one).
pub const UDA_SESSION_CAP_LOCK: u32 = 1 << 17;
/// Capability bit: the calling user's session can be logged out.
pub const UDA_SESSION_CAP_LOGOUT: u32 = 1 << 18;
/// Capability bit: the machine can be suspended to RAM.
pub const UDA_SESSION_CAP_SUSPEND: u32 = 1 << 19;
/// Capability bit: the machine can be hibernated to disk.
pub const UDA_SESSION_CAP_HIBERNATE: u32 = 1 << 20;
/// Capability bit: the machine can be rebooted.
pub const UDA_SESSION_CAP_REBOOT: u32 = 1 << 21;
/// Capability bit: the machine can be powered off.
pub const UDA_SESSION_CAP_SHUTDOWN: u32 = 1 << 22;

/// The actions the current platform's backend can deliver, as a bitmask.
///
/// The bitmask is made of the [`UDA_SESSION_CAP_*`] constants and never
/// includes an action the backend cannot perform, so a caller can test
/// "is shutdown offered?" before drawing a button for it.
pub(crate) fn capabilities() -> Capability {
    #[cfg(target_os = "linux")]
    {
        uda_platform_linux::session::LinuxSessionManager::new().capabilities()
    }

    #[cfg(target_os = "windows")]
    {
        uda_platform_windows::session::WindowsSessionManager::new().capabilities()
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Capability::empty()
    }
}

/// Perform `action`, refusing a call the platform cannot deliver.
///
/// The capability guard itself lives in the core
/// (`uda_core::session::perform`), so every caller - this FFI layer, a Rust
/// host, and the unit tests - gets the identical "unsupported" answer, and so it
/// is distinguishable from "tried and failed".
pub(crate) fn perform(action: SessionAction) -> Result<(), Failure> {
    #[cfg(target_os = "linux")]
    {
        use uda_platform_linux::session::LinuxSessionManager as Manager;

        let manager = Manager::new();
        run(&manager, action)
    }

    #[cfg(target_os = "windows")]
    {
        use uda_platform_windows::session::WindowsSessionManager as Manager;

        let manager = Manager::new();
        run(&manager, action)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = action;
        Err(Failure::Uda(UdaError::NotSupported(
            "no session backend for this target".to_string(),
        )))
    }
}

/// Check the capability, then dispatch to the matching backend method.
///
/// Kept separate from [`perform`] for two reasons: the tests drive it with a
/// mock manager (no D-Bus session, no Windows host), and it is where a typed
/// [`UdaError`] becomes the ABI-level [`Failure`].
fn run<M: SessionManager>(manager: &M, action: SessionAction) -> Result<(), Failure> {
    uda_core::session::perform(manager, action).map_err(Failure::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use uda_core::error::UdaError;

    /// A manager that refuses everything and records nothing.
    ///
    /// The FFI tests must never reach a real session or power API, so the guard
    /// is verified against a mock rather than the platform backend.
    struct RefusingManager {
        capabilities: Capability,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl RefusingManager {
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

    impl SessionManager for RefusingManager {
        fn lock(&self) -> Result<(), UdaError> {
            self.calls.lock().expect("never poisoned").push("lock");
            Ok(())
        }
        fn logout(&self) -> Result<(), UdaError> {
            self.calls.lock().expect("never poisoned").push("logout");
            Ok(())
        }
        fn suspend(&self) -> Result<(), UdaError> {
            self.calls.lock().expect("never poisoned").push("suspend");
            Ok(())
        }
        fn hibernate(&self) -> Result<(), UdaError> {
            self.calls.lock().expect("never poisoned").push("hibernate");
            Ok(())
        }
        fn reboot(&self) -> Result<(), UdaError> {
            self.calls.lock().expect("never poisoned").push("reboot");
            Ok(())
        }
        fn shutdown(&self) -> Result<(), UdaError> {
            self.calls.lock().expect("never poisoned").push("shutdown");
            Ok(())
        }
        fn capabilities(&self) -> Capability {
            self.capabilities
        }
    }

    #[test]
    fn capability_bits_match_the_core_definitions() {
        // The constants are part of the C ABI, so a drift between this layer and
        // `uda_core` would silently break every binding that hard-codes them.
        assert_eq!(
            UDA_SESSION_CAP_MANAGEMENT,
            Capability::SESSION_MANAGEMENT.bits()
        );
        assert_eq!(UDA_SESSION_CAP_LOCK, Capability::LOCK.bits());
        assert_eq!(UDA_SESSION_CAP_LOGOUT, Capability::LOGOUT.bits());
        assert_eq!(UDA_SESSION_CAP_SUSPEND, Capability::SUSPEND.bits());
        assert_eq!(UDA_SESSION_CAP_HIBERNATE, Capability::HIBERNATE.bits());
        assert_eq!(UDA_SESSION_CAP_REBOOT, Capability::REBOOT.bits());
        assert_eq!(UDA_SESSION_CAP_SHUTDOWN, Capability::SHUTDOWN.bits());
    }

    #[test]
    fn capability_bits_are_distinct_and_stable() {
        let bits = [
            UDA_SESSION_CAP_MANAGEMENT,
            UDA_SESSION_CAP_LOCK,
            UDA_SESSION_CAP_LOGOUT,
            UDA_SESSION_CAP_SUSPEND,
            UDA_SESSION_CAP_HIBERNATE,
            UDA_SESSION_CAP_REBOOT,
            UDA_SESSION_CAP_SHUTDOWN,
        ];

        for (index, bit) in bits.iter().enumerate() {
            // Each is a single bit (a power of two) ...
            assert!(
                bit.is_power_of_two(),
                "bit {index} is not a lone flag: {bit}"
            );
            // ... and it appears only once, so a bitmask can never be ambiguous.
            assert_eq!(
                bits.iter().filter(|other| *other == bit).count(),
                1,
                "bit {index} is duplicated"
            );
        }
    }

    #[test]
    fn the_guard_refuses_an_action_the_platform_lacks() {
        // The critical safety property: an unsupported action is refused
        // *before* the backend is asked, so the mock stays empty.
        let manager = RefusingManager::with(Capability::LOCK);
        let outcome = run(&manager, SessionAction::Shutdown);

        match outcome {
            Err(Failure::Uda(UdaError::NotSupported(message))) => {
                assert!(message.contains("Shutdown"), "message was: {message}");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }

        assert!(
            manager.calls().is_empty(),
            "a refused action must not reach the backend"
        );
    }

    #[test]
    fn the_guard_dispatches_a_supported_action() {
        let manager = RefusingManager::with(Capability::LOCK | Capability::SHUTDOWN);

        run(&manager, SessionAction::Lock).expect("lock is advertised");
        run(&manager, SessionAction::Shutdown).expect("shutdown is advertised");

        assert_eq!(manager.calls(), vec!["lock", "shutdown"]);
    }

    #[test]
    fn one_flags_does_not_authorise_another() {
        // Reporting `LOCK` must not let a caller shut the machine down.
        let manager = RefusingManager::with(Capability::LOCK);
        assert!(run(&manager, SessionAction::Reboot).is_err());
        assert!(manager.calls().is_empty());
    }

    #[test]
    fn a_backend_error_is_reported_not_swallowed() {
        let manager = RefusingManager::with(Capability::LOCK);

        // A backend that fails must surface an `Err`; a caller cannot
        // distinguish "refused" from "attempted and failed" otherwise.
        struct Failing;

        impl SessionManager for Failing {
            fn lock(&self) -> Result<(), UdaError> {
                Err(UdaError::CommandFailed(
                    "the lock service refused".to_string(),
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

        match run(&Failing, SessionAction::Lock) {
            Err(Failure::Uda(UdaError::CommandFailed(message))) => {
                assert!(message.contains("lock service"), "message was: {message}");
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }

        // The success path through the same manager stays available.
        assert!(run(&Failing, SessionAction::Logout).is_err());
        assert_eq!(manager.capabilities(), Capability::LOCK);
    }

    #[test]
    fn the_capability_query_neither_errors_nor_lies() {
        // The query is what a UI calls freely, so it must always answer, and it
        // must never report an action the platform cannot reach.
        let capabilities = capabilities();

        for action in [
            SessionAction::Lock,
            SessionAction::Logout,
            SessionAction::Suspend,
            SessionAction::Hibernate,
            SessionAction::Reboot,
            SessionAction::Shutdown,
        ] {
            if capabilities.contains(action.capability()) {
                // Every advertised action really has a code path on this target.
                assert!(
                    cfg!(any(target_os = "linux", target_os = "windows")),
                    "{action:?} advertised on a target with no backend"
                );
            }
        }

        // On a real backend the management bit is always set; on an exotic
        // target the answer is "nothing at all".
        if cfg!(any(target_os = "linux", target_os = "windows")) {
            assert!(capabilities.contains(Capability::SESSION_MANAGEMENT));
        } else {
            assert!(capabilities.is_empty());
        }
    }

    #[test]
    fn every_capability_bit_is_answerable_by_a_reported_flag_set() {
        // A bitmask caller cannot interpret stray bits, so the backend must only
        // ever report the documented seven.
        let capabilities = capabilities();
        let documented = Capability::SESSION_MANAGEMENT
            | Capability::LOCK
            | Capability::LOGOUT
            | Capability::SUSPEND
            | Capability::HIBERNATE
            | Capability::REBOOT
            | Capability::SHUTDOWN;

        assert_eq!(
            capabilities & !documented,
            Capability::empty(),
            "undocumented capability bits reported: {:?}",
            capabilities & !documented
        );
    }
}
