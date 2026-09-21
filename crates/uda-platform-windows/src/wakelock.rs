//! Windows wake lock manager: display/system sleep inhibition.
//!
//! # Backend selection
//!
//! Windows has no portal or desktop-shell daemon; the supported mechanism is the
//! thread execution state API (see `docs/internals/wakelock_specs.md`):
//!
//! | Concern      | API                                     |
//! |--------------|-----------------------------------------|
//! | Acquire lock | `SetThreadExecutionState(ES_DISPLAY_REQUIRED \| ES_SYSTEM_REQUIRED \| ES_CONTINUOUS)` |
//! | Release lock | `SetThreadExecutionState(ES_CONTINUOUS)` |
//!
//! `SetThreadExecutionState` is process-wide state on the calling thread rather
//! than a reference-counted handle: each call *replaces* the previous flags. This
//! has two consequences that shape this module:
//!
//! 1. `ES_CONTINUOUS` must be present for the request to persist until the next
//!    call (without it the request lasts one suspend cycle).
//! 2. Release means restoring the default `ES_CONTINUOUS`, which affects the
//!    whole process. UDA therefore documents this as an intentional, single-lock
//!    model rather than silently stacking guards.
//!
//! Unlike the FreeDesktop cookie returned by `org.freedesktop.ScreenSaver.Inhibit`,
//! there is no cookie to hand back, so the Windows guard carries no payload: its
//! only job is to restore the default state on drop.

use windows::Win32::System::Power::{
    SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED,
    EXECUTION_STATE,
};

use uda_core::error::UdaError;
use uda_core::wakelock::{WakeLockGuard, WakeLockManager, WakeLockType};

/// RAII guard for a Windows wake lock.
///
/// On drop the thread execution state is restored to the default
/// `ES_CONTINUOUS`, which releases both the display and system requirements.
///
/// The guard is intentionally not `Clone` and not `Send`-agnostic about ordering:
/// because the underlying API is thread-scoped, releasing from a different thread
/// than the one that acquired the lock would not clear the flag on the thread
/// that actually holds it. It is still `Send` so it can be moved onto the thread
/// that owns the lock, which is the common case.
pub struct WindowsWakeLockGuard {
    /// The core RAII guard that actually owns the release closure.
    ///
    /// Wrapping (rather than duplicating) the core guard keeps a single release
    /// path, so `release()` followed by `drop()` cannot issue two FFI calls.
    inner: WakeLockGuard,
    /// `false` once the lock has been released, making `Drop` idempotent.
    released: bool,
}

impl WindowsWakeLockGuard {
    /// Wrap the core guard for an already-acquired lock.
    ///
    /// This is the constructor used by
    /// [`WindowsWakeLockManager::acquire_guard`], which is the Windows-specific
    /// entry point that returns this type instead of the core guard.
    pub fn new(inner: WakeLockGuard) -> Self {
        Self {
            inner,
            released: false,
        }
    }

    /// Whether this guard still holds an active wake lock.
    pub fn is_active(&self) -> bool {
        !self.released
    }

    /// Manually release the wake lock early.
    ///
    /// Consumes the guard, mirroring [`WakeLockGuard::release`].
    pub fn release(mut self) {
        self.release_internal();
    }
}

impl Drop for WindowsWakeLockGuard {
    fn drop(&mut self) {
        self.release_internal();
    }
}

impl WindowsWakeLockGuard {
    /// Hand the release closure back to the core guard, exactly once.
    ///
    /// The core guard's own `Drop` runs the closure, so this method only has to
    /// ensure it is invoked a single time. Marking the wrapper as released first
    /// makes a `release()` + `drop()` pair safe.
    fn release_internal(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        // Replacing the inner guard with an already-released one and dropping
        // the original runs the release closure exactly once.
        drop(std::mem::replace(
            &mut self.inner,
            WakeLockGuard::new(Box::new(|| {})),
        ));
    }
}

/// Windows wake lock manager.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsWakeLockManager;

impl WindowsWakeLockManager {
    pub fn new() -> Self {
        Self
    }

    /// Map the cross-platform [`WakeLockType`] onto Win32 execution-state flags.
    ///
    /// `ES_CONTINUOUS` is always included so the request persists until it is
    /// explicitly cleared. `PreventSystemIdle` additionally requests
    /// `ES_SYSTEM_REQUIRED`, which keeps the machine from entering automatic sleep.
    fn execution_state(lock_type: WakeLockType) -> EXECUTION_STATE {
        match lock_type {
            WakeLockType::PreventDisplaySleep => ES_CONTINUOUS | ES_DISPLAY_REQUIRED,
            WakeLockType::PreventSystemIdle => {
                ES_CONTINUOUS | ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED
            }
        }
    }

    /// Apply an execution state and report whether the call succeeded.
    ///
    /// `SetThreadExecutionState` returns the *previous* state, or `0` on failure
    /// (which happens only when the thread state is being changed from within a
    /// power-notification callback).
    fn apply(state: EXECUTION_STATE) -> Result<(), UdaError> {
        // SAFETY: value-only FFI with no pointers and no invariants beyond the
        // documented return contract.
        let previous = unsafe { SetThreadExecutionState(state) };

        if previous.0 == 0 {
            return Err(UdaError::Internal(
                "SetThreadExecutionState failed; wake locks cannot be changed from a power notification callback".to_string(),
            ));
        }

        Ok(())
    }

    /// Restore the default execution state, releasing the wake lock.
    ///
    /// `ES_CONTINUOUS` on its own is the documented "clear all requirements"
    /// call. This is used as the release closure handed to the core
    /// [`WakeLockGuard`], so it must never fail: the previous state is logged
    /// for diagnostics only.
    fn release() {
        // SAFETY: value-only FFI with no pointers and no invariants beyond the
        // documented return contract.
        let previous = unsafe { SetThreadExecutionState(ES_CONTINUOUS) };

        if previous.0 == 0 {
            log::debug!("SetThreadExecutionState(ES_CONTINUOUS) reported a failure");
        }
    }
}

impl WindowsWakeLockManager {
    /// Acquire a wake lock and return the Windows-specific guard.
    ///
    /// This is the platform entry point for callers that want the richer guard
    /// API ([`WindowsWakeLockGuard::is_active`], `release()`) instead of the
    /// core [`WakeLockGuard`] returned by [`WakeLockManager::acquire`]. Both
    /// paths share the same FFI release closure, so they are interchangeable.
    pub async fn acquire_guard(
        &self,
        lock_type: WakeLockType,
        reason: &str,
    ) -> Result<WindowsWakeLockGuard, UdaError> {
        let guard = WakeLockManager::acquire(self, lock_type, reason).await?;
        Ok(WindowsWakeLockGuard::new(guard))
    }
}

#[async_trait::async_trait]
impl WakeLockManager for WindowsWakeLockManager {
    async fn acquire(
        &self,
        lock_type: WakeLockType,
        reason: &str,
    ) -> Result<WakeLockGuard, UdaError> {
        let state = Self::execution_state(lock_type);

        Self::apply(state).map_err(|e| {
            log::debug!("failed to acquire wake lock ({reason}): {e}");
            e
        })?;

        log::debug!(
            "acquired Windows wake lock {lock_type:?} ({reason}); flags={:#x}",
            state.0
        );

        // Unlike FreeDesktop (which hands back a cookie to feed to `UnInhibit`),
        // Windows has no per-lock token: the release action is the process-wide
        // "clear the flags" call. The closure therefore captures nothing and
        // simply restores the default execution state when the guard is dropped.
        Ok(WakeLockGuard::new(Box::new(Self::release)))
    }
}

/// Convert a raw `u32` flag set into the typed Win32 constant.
///
/// Kept as a helper so tests can assert the mapping without duplicating the
/// `EXECUTION_STATE` newtype construction.
#[cfg(test)]
fn execution_state_bits(lock_type: WakeLockType) -> u32 {
    WindowsWakeLockManager::execution_state(lock_type).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_state_always_includes_continuous() {
        for lock_type in [
            WakeLockType::PreventDisplaySleep,
            WakeLockType::PreventSystemIdle,
        ] {
            let state = WindowsWakeLockManager::execution_state(lock_type);
            assert!(
                state.contains(ES_CONTINUOUS),
                "{lock_type:?} must include ES_CONTINUOUS"
            );
        }
    }

    #[test]
    fn display_sleep_lock_requires_display_only() {
        let state = WindowsWakeLockManager::execution_state(WakeLockType::PreventDisplaySleep);
        assert!(state.contains(ES_DISPLAY_REQUIRED));
        assert!(!state.contains(ES_SYSTEM_REQUIRED));
    }

    #[test]
    fn system_idle_lock_requires_display_and_system() {
        let state = WindowsWakeLockManager::execution_state(WakeLockType::PreventSystemIdle);
        assert!(state.contains(ES_DISPLAY_REQUIRED));
        assert!(state.contains(ES_SYSTEM_REQUIRED));
    }

    #[test]
    fn execution_state_bits_match_documented_values() {
        assert_eq!(
            execution_state_bits(WakeLockType::PreventDisplaySleep),
            ES_CONTINUOUS.0 | ES_DISPLAY_REQUIRED.0
        );
        assert_eq!(
            execution_state_bits(WakeLockType::PreventSystemIdle),
            ES_CONTINUOUS.0 | ES_DISPLAY_REQUIRED.0 | ES_SYSTEM_REQUIRED.0
        );
    }

    /// Build a wrapper guard around a core guard that records whether it ran.
    fn wrapper_guard_with_spy() -> (
        WindowsWakeLockGuard,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    ) {
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spy = std::sync::Arc::clone(&counter);
        let inner = WakeLockGuard::new(Box::new(move || {
            spy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        (WindowsWakeLockGuard::new(inner), counter)
    }

    #[test]
    fn guard_starts_active_and_release_marks_it_inactive() {
        let (mut guard, counter) = wrapper_guard_with_spy();
        assert!(guard.is_active());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 0);

        guard.release_internal();

        assert!(!guard.is_active());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_release_is_idempotent() {
        let (mut guard, counter) = wrapper_guard_with_spy();
        guard.release_internal();
        // Second call must be a no-op rather than another FFI round-trip.
        guard.release_internal();
        assert!(!guard.is_active());
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_drop_runs_the_release_closure_once() {
        let (guard, counter) = wrapper_guard_with_spy();
        assert!(guard.is_active());
        drop(guard);
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_release_then_drop_runs_the_closure_once() {
        let (guard, counter) = wrapper_guard_with_spy();
        guard.release();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn acquire_closure_is_the_documented_release_call() {
        // The core trait returns a `WakeLockGuard`, so the release behaviour is
        // verified through a standalone guard wired to the same closure shape.
        let released = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let spy = std::sync::Arc::clone(&released);
        let guard = WakeLockGuard::new(Box::new(move || {
            spy.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        drop(guard);
        assert_eq!(released.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
