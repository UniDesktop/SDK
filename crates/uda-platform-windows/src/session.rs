//! Windows session backend: Win32 session and power APIs.
//!
//! | Action   | API                                                              |
//! |----------|------------------------------------------------------------------|
//! | Lock     | `user32!LockWorkStation`                                          |
//! | Logout   | `ExitWindowsEx(EWX_LOGOFF, 0)`                                    |
//! | Suspend  | `powrprof!SetSuspendState(false, force, wakeup_events_disabled)`   |
//! | Hibernate| `powrprof!SetSuspendState(true, force, wakeup_events_disabled)`    |
//! | Reboot   | `ExitWindowsEx(EWX_REBOOT \| EWX_FORCEIFHUNG, 0)` after `SE_SHUTDOWN_NAME` |
//! | Shutdown | `ExitWindowsEx(EWX_POWEROFF \| EWX_FORCEIFHUNG, 0)` after `SE_SHUTDOWN_NAME` |
//!
//! # Why the privilege dance is required
//!
//! `ExitWindowsEx` with `EWX_REBOOT` or `EWX_POWEROFF` fails with
//! `ERROR_PRIVILEGE_NOT_HELD` (1314) unless the process token carries the
//! `SeShutdownPrivilege` privilege **and** it is *enabled*, so two steps are
//! needed and skipping either produces the same misleading failure:
//!
//! 1. `OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &token)`
//!    opens the token. `TOKEN_QUERY` is required because
//!    `AdjustTokenPrivileges` reports what it actually did through its
//!    `previousstate` out parameter.
//! 2. `AdjustTokenPrivileges(token, false, &privileges, ...)` enables the
//!    privilege. It returns a *success* status even when it granted nothing, so
//!    `GetLastError()` must be checked for `ERROR_NOT_ALL_ASSIGNED` - the only
//!    reliable way to tell "the account lacks the privilege" from "done".
//!
//! Logout and the two sleep states need no privilege, which is why only the two
//! power-off paths go through [`acquire_shutdown_privilege`].
//!
//! # Handle hygiene
//!
//! The token handle is owned by a guard whose `Drop` calls `CloseHandle`, so
//! every early return releases it. Following `AGENTS.md` Principle 1 there is no
//! `unwrap`/`expect`: every Win32 `BOOL`/`Result` is inspected and mapped into a
//! typed [`UdaError`] carrying the Win32 error code in its message.
//!
//! See `docs/internals/session_specs.md` for the full mapping.

use std::mem::size_of;

use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, LUID};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED, SE_SHUTDOWN_NAME,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Power::SetSuspendState;
use windows::Win32::System::Shutdown::{
    ExitWindowsEx, LockWorkStation, EWX_FORCEIFHUNG, EWX_LOGOFF, EWX_POWEROFF, EWX_REBOOT,
    EXIT_WINDOWS_FLAGS,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::session::{SessionAction, SessionManager};

/// The privilege name Windows requires for reboot and shutdown.
/// `SE_SHUTDOWN_NAME` is already a wide-string constant in the `windows` crate,
/// so this text form is only used in error messages.
const SE_SHUTDOWN_DISPLAY: &str = "SeShutdownPrivilege";

/// Win32 error code reported when a privilege cannot be granted.
const ERROR_NOT_ALL_ASSIGNED_CODE: u32 = 1300;

/// Windows session manager.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsSessionManager;

impl WindowsSessionManager {
    pub fn new() -> Self {
        Self
    }

    /// Lock the workstation.
    fn lock(&self) -> Result<(), UdaError> {
        // SAFETY: `LockWorkStation` takes no parameters and returns only a
        // status; it cannot touch caller memory.
        match unsafe { LockWorkStation() } {
            Ok(()) => {
                log::debug!("LockWorkStation succeeded");
                Ok(())
            }
            Err(e) => Err(UdaError::CommandFailed(format!(
                "LockWorkStation failed (Win32 {}): {e}",
                last_error_code()
            ))),
        }
    }

    /// Log the calling user out.
    fn logout(&self) -> Result<(), UdaError> {
        // The flags come from the same table the tests read, so a change to the
        // mapping cannot pass the tests while shipping something else.
        let flags = exit_flags_for(SessionAction::Logout)
            .ok_or_else(|| UdaError::Internal("logout lost its ExitWindowsEx flags".to_string()))?;

        // `EWX_LOGOFF` needs no privilege: a user may always end their own
        // session. `EWX_FORCEIFHUNG` is deliberately *not* used, because forcing
        // a hung application out would discard whatever it had not saved.
        exit_windows(flags, "logout")
    }

    /// Suspend the machine to RAM.
    fn suspend(&self) -> Result<(), UdaError> {
        sleep_state(false)
    }

    /// Hibernate the machine to disk.
    fn hibernate(&self) -> Result<(), UdaError> {
        sleep_state(true)
    }

    /// Reboot the machine.
    fn reboot(&self) -> Result<(), UdaError> {
        self.exit_power_off_path(SessionAction::Reboot)
    }

    /// Power the machine off.
    fn shutdown(&self) -> Result<(), UdaError> {
        self.exit_power_off_path(SessionAction::Shutdown)
    }

    /// Shared tail of reboot and shutdown: acquire the privilege, then exit.
    ///
    /// Both need `SeShutdownPrivilege`, and both fail the same way without it,
    /// so they share one path rather than two near-copies. The privilege guard
    /// must stay alive across `exit_windows`: dropping it closes the token
    /// *and* disables the privilege, which would turn a working call into
    /// `ERROR_PRIVILEGE_NOT_HELD`.
    fn exit_power_off_path(&self, action: SessionAction) -> Result<(), UdaError> {
        debug_assert!(
            requires_shutdown_privilege(action),
            "{action:?} does not take the privilege path"
        );

        let flags = exit_flags_for(action).ok_or_else(|| {
            UdaError::Internal(format!("{action:?} lost its ExitWindowsEx flags"))
        })?;

        let _privilege = acquire_shutdown_privilege()?;
        exit_windows(flags, action_label(action))
    }
}

/// Call `ExitWindowsEx` and map its outcome onto a typed [`UdaError`].
///
/// `action` is only used to build the diagnostic message.
fn exit_windows(flags: EXIT_WINDOWS_FLAGS, action: &str) -> Result<(), UdaError> {
    // SAFETY: both parameters are plain values; `ExitWindowsEx` returns whether
    // the request was accepted, and returns before the machine goes down, so no
    // caller memory is touched afterwards.
    match unsafe { ExitWindowsEx(flags, Default::default()) } {
        Ok(()) => {
            log::debug!("ExitWindowsEx accepted the {action} request");
            Ok(())
        }
        Err(e) => {
            let code = last_error_code();

            // 1314 is `ERROR_PRIVILEGE_NOT_HELD`. It is reported as
            // `NotSupported` because a retry would need the privilege first -
            // a generic failure would invite a pointless retry.
            if is_privilege_not_held(code) {
                return Err(UdaError::NotSupported(format!(
                    "ExitWindowsEx({action}) failed: the process is missing \
                     {SE_SHUTDOWN_DISPLAY} (ERROR_PRIVILEGE_NOT_HELD)"
                )));
            }

            Err(UdaError::CommandFailed(format!(
                "ExitWindowsEx({action}) failed (Win32 {code}): {e}"
            )))
        }
    }
}

/// Suspend or hibernate through `powrprof!SetSuspendState`.
///
/// `hibernate == true` selects hibernation; `false` selects sleep. The machine is
/// *not* forced (`bForce = false`) and wake-up events stay enabled, so a program
/// that has requested wake timers keeps working - matching what the Linux
/// backend does by leaving logind's `interactive` flag off.
fn sleep_state(hibernate: bool) -> Result<(), UdaError> {
    let label = if hibernate { "hibernate" } else { "suspend" };

    // SAFETY: three `BOOLEAN` parameters, no pointers, and the function returns
    // before the machine changes power state.
    let accepted = unsafe { SetSuspendState(hibernate, false, false) };

    if accepted.as_bool() {
        log::debug!("SetSuspendState accepted the {label} request");
        return Ok(());
    }

    // `SetSuspendState` returns a `BOOLEAN` rather than an `HRESULT`, so the
    // reason has to come from `GetLastError`. Hibernation that is switched off
    // is the common one and is reported as unsupported.
    let code = last_error_code();

    if code == 1 || code == 2 {
        // ERROR_INVALID_FUNCTION / ERROR_FILE_NOT_FOUND: the hibernation file
        // does not exist, i.e. hibernation is disabled in the firmware/OS.
        return Err(UdaError::NotSupported(format!(
            "SetSuspendState({label}) is not available (Win32 {code}): \
             hibernation is probably disabled"
        )));
    }

    Err(UdaError::CommandFailed(format!(
        "SetSuspendState({label}) failed (Win32 {code})"
    )))
}

/// RAII owner for an access-token handle.
///
/// The guard is the only reason a failure between `OpenProcessToken` and the end
/// of [`acquire_shutdown_privilege`] cannot leak a kernel handle: the token is
/// closed as the guard goes out of scope, on every path.
struct TokenGuard(HANDLE);

impl Drop for TokenGuard {
    fn drop(&mut self) {
        // SAFETY: the handle came from `OpenProcessToken` and is closed exactly
        // once, here. A failure is not actionable and must not mask the real
        // error of the operation, so it is only logged.
        if let Err(e) = unsafe { CloseHandle(self.0) } {
            log::debug!("CloseHandle failed for the shutdown token: {e}");
        }
    }
}

/// Enable `SeShutdownPrivilege` for the current process.
///
/// Returns the token guard so the caller's token stays enabled for the duration
/// of the `ExitWindowsEx` call that follows; dropping it closes the handle.
///
/// Both reasons for a failure are distinguished, because they need different
/// user-facing advice:
///
/// - the token cannot be opened or the privilege cannot be found -> a genuine
///   environment problem, reported as [`UdaError::Internal`];
/// - the privilege exists but the account does not hold it
///   (`ERROR_NOT_ALL_ASSIGNED`) -> reported as [`UdaError::NotSupported`], so a
///   host can say "run as administrator" instead of retrying.
fn acquire_shutdown_privilege() -> Result<TokenGuard, UdaError> {
    let mut token = HANDLE::default();

    // SAFETY: `GetCurrentProcess` returns a pseudo-handle valid for the lifetime
    // of this process, `desiredaccess` combines two documented access masks, and
    // `token` points at writable stack memory sized for a `HANDLE`.
    let opened = unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    };

    if let Err(e) = opened {
        return Err(UdaError::Internal(format!(
            "OpenProcessToken failed (Win32 {}): {e}",
            last_error_code()
        )));
    }

    let guard = TokenGuard(token);

    let mut luid = LUID::default();

    // SAFETY: a null system name means "the local system", `SE_SHUTDOWN_NAME`
    // is a static wide string, and `luid` points at writable stack memory.
    let looked_up = unsafe { LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid) };
    if let Err(e) = looked_up {
        return Err(UdaError::Internal(format!(
            "LookupPrivilegeValueW({SE_SHUTDOWN_DISPLAY}) failed (Win32 {}): {e}",
            last_error_code()
        )));
    }

    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [windows::Win32::Security::LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }; 1],
    };

    // SAFETY: the token handle is valid, `disableallprivileges` is `false` (the
    // documented way to adjust rather than clear), and `newstate` points at a
    // correctly sized `TOKEN_PRIVILEGES`. `previousstate`/`returnlength` are
    // `None`, which the API documents as "the caller does not need the old
    // state".
    let adjusted = unsafe {
        AdjustTokenPrivileges(
            token,
            false,
            Some(&privileges),
            size_of::<TOKEN_PRIVILEGES>() as u32,
            None,
            None,
        )
    };

    // `AdjustTokenPrivileges` returns success even when it granted nothing, so
    // the thread's last error is the only way to detect a missing privilege.
    if let Err(e) = adjusted {
        return Err(UdaError::Internal(format!(
            "AdjustTokenPrivileges({SE_SHUTDOWN_DISPLAY}) failed: {e}"
        )));
    }

    if last_error_code() == NOT_ALL_ASSIGNED {
        return Err(UdaError::NotSupported(format!(
            "this account does not hold {SE_SHUTDOWN_DISPLAY}; \
             reboot and shutdown need it (ERROR_NOT_ALL_ASSIGNED)"
        )));
    }

    log::debug!("{SE_SHUTDOWN_DISPLAY} is enabled for this process");
    Ok(guard)
}

/// The current thread's last Win32 error code.
///
/// The `windows` crate surfaces most failures as an `HRESULT`/`Err`, but
/// `SetSuspendState` (a `BOOLEAN`-returning function) and
/// `AdjustTokenPrivileges` (which succeeds even when it grants nothing) report
/// their reason only through `GetLastError`, so both need this helper.
fn last_error_code() -> u32 {
    // SAFETY: `GetLastError` is a pure read of thread-local state.
    unsafe { GetLastError() }.0
}

impl SessionManager for WindowsSessionManager {
    fn lock(&self) -> Result<(), UdaError> {
        WindowsSessionManager::lock(self)
    }

    fn logout(&self) -> Result<(), UdaError> {
        WindowsSessionManager::logout(self)
    }

    fn suspend(&self) -> Result<(), UdaError> {
        WindowsSessionManager::suspend(self)
    }

    fn hibernate(&self) -> Result<(), UdaError> {
        WindowsSessionManager::hibernate(self)
    }

    fn reboot(&self) -> Result<(), UdaError> {
        WindowsSessionManager::reboot(self)
    }

    fn shutdown(&self) -> Result<(), UdaError> {
        WindowsSessionManager::shutdown(self)
    }

    fn capabilities(&self) -> Capability {
        // Every action maps to a documented Win32 API present on Windows 10/11.
        // Whether the *account* holds `SeShutdownPrivilege` is a runtime
        // question answered by the methods above (`ERROR_NOT_ALL_ASSIGNED`),
        // exactly as a polkit refusal is on Linux - not a capability question.
        Capability::SESSION_MANAGEMENT
            | Capability::LOCK
            | Capability::LOGOUT
            | Capability::SUSPEND
            | Capability::HIBERNATE
            | Capability::REBOOT
            | Capability::SHUTDOWN
    }
}

/// The exit flags a given action maps onto.
///
/// Both the running code and the tests read this table, so a change to the
/// mapping cannot pass the tests while shipping something else - and the
/// distinction between `EWX_REBOOT` and `EWX_POWEROFF` is unit-testable without
/// touching the machine's power state.
pub(crate) fn exit_flags_for(action: SessionAction) -> Option<EXIT_WINDOWS_FLAGS> {
    match action {
        SessionAction::Lock | SessionAction::Suspend | SessionAction::Hibernate => None,
        SessionAction::Logout => Some(EWX_LOGOFF),
        SessionAction::Reboot => Some(EWX_REBOOT | EWX_FORCEIFHUNG),
        SessionAction::Shutdown => Some(EWX_POWEROFF | EWX_FORCEIFHUNG),
    }
}

/// The label used in diagnostics for `action`.
///
/// Kept beside [`exit_flags_for`] so the flag table and the wording cannot drift
/// apart from each other.
fn action_label(action: SessionAction) -> &'static str {
    match action {
        SessionAction::Lock => "lock",
        SessionAction::Logout => "logout",
        SessionAction::Suspend => "suspend",
        SessionAction::Hibernate => "hibernate",
        SessionAction::Reboot => "reboot",
        SessionAction::Shutdown => "shutdown",
    }
}

/// Whether an action needs the `SeShutdownPrivilege` privilege.
///
/// Locking, logging out, sleeping and hibernating do not; rebooting and powering
/// off do.
pub(crate) fn requires_shutdown_privilege(action: SessionAction) -> bool {
    matches!(action, SessionAction::Reboot | SessionAction::Shutdown)
}

/// Whether a Win32 error code means "the privilege is missing".
///
/// Kept next to [`acquire_shutdown_privilege`] so the two power-off paths and
/// this predicate cannot drift apart.
pub(crate) fn is_privilege_not_held(code: u32) -> bool {
    code == 1314
}

/// The `AdjustTokenPrivileges` code that means "the account lacks it".
pub(crate) const NOT_ALL_ASSIGNED: u32 = ERROR_NOT_ALL_ASSIGNED_CODE;

#[cfg(test)]
mod tests {
    use super::*;
    use uda_core::session::SessionAction;
    use windows::Win32::System::Shutdown::{EWX_FORCEIFHUNG, EWX_LOGOFF, EWX_POWEROFF, EWX_REBOOT};

    #[test]
    fn lock_and_sleep_states_have_no_exit_flags() {
        // They do not go through `ExitWindowsEx` at all, so mapping them onto
        // exit flags would send a caller down a code path that cannot work.
        assert_eq!(exit_flags_for(SessionAction::Lock), None);
        assert_eq!(exit_flags_for(SessionAction::Suspend), None);
        assert_eq!(exit_flags_for(SessionAction::Hibernate), None);
    }

    #[test]
    fn logout_uses_the_logoff_flag_and_nothing_else() {
        assert_eq!(exit_flags_for(SessionAction::Logout), Some(EWX_LOGOFF));
        // `EWX_LOGOFF` needs no privilege, so no force flag is added either.
        assert_ne!(
            exit_flags_for(SessionAction::Logout),
            Some(EWX_LOGOFF | EWX_FORCEIFHUNG)
        );
    }

    #[test]
    fn reboot_and_shutdown_use_distinct_flags_with_force_if_hung() {
        assert_eq!(
            exit_flags_for(SessionAction::Reboot),
            Some(EWX_REBOOT | EWX_FORCEIFHUNG)
        );
        assert_eq!(
            exit_flags_for(SessionAction::Shutdown),
            Some(EWX_POWEROFF | EWX_FORCEIFHUNG)
        );
    }

    #[test]
    fn only_the_two_power_off_actions_need_the_privilege() {
        assert!(!requires_shutdown_privilege(SessionAction::Lock));
        assert!(!requires_shutdown_privilege(SessionAction::Logout));
        assert!(!requires_shutdown_privilege(SessionAction::Suspend));
        assert!(!requires_shutdown_privilege(SessionAction::Hibernate));
        assert!(requires_shutdown_privilege(SessionAction::Reboot));
        assert!(requires_shutdown_privilege(SessionAction::Shutdown));
    }

    #[test]
    fn the_privilege_not_held_code_is_recognised() {
        // 1314 is what `ExitWindowsEx` returns when the token lacks
        // `SeShutdownPrivilege`; confusing it with a generic failure would make
        // a host retry a call that can never succeed.
        assert!(is_privilege_not_held(1314));
        assert!(!is_privilege_not_held(5));
        assert!(!is_privilege_not_held(0));
    }

    #[test]
    fn the_not_all_assigned_constant_matches_windows() {
        // `ERROR_NOT_ALL_ASSIGNED` is 1300 in winerror.h; the privilege helper
        // compares against this literal, so it must stay correct.
        assert_eq!(NOT_ALL_ASSIGNED, 1300);
    }

    #[test]
    fn the_capability_set_covers_every_action() {
        let manager = WindowsSessionManager::new();
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
    fn the_token_guard_closes_the_handle_on_drop() {
        // A guard built over an invalid handle must not panic in `Drop`: the
        // point of the guard is that *every* path closes the token, including
        // the paths that already returned an error.
        let guard = TokenGuard(HANDLE::default());
        drop(guard);
    }
}
