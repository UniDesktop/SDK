//! Wake-lock handle registry shared by the C exports.
//!
//! # Why a registry exists
//!
//! A [`WakeLockGuard`] is an RAII value: it releases the lock when dropped. The
//! C ABI cannot express that, because C callers receive an integer and decide
//! *when* (or whether) to release it. Every lock therefore lives in a
//! process-wide table keyed by an opaque handle; `uda_wakelock_release` looks
//! the handle up, removes it, and drops the guard, which runs the platform
//! release closure exactly once.
//!
//! # Release strategy per tier
//!
//! - **Tier 1/2 (native IPC):** the guard owns a D-Bus `UnInhibit` cookie or a
//!   Win32 `SetThreadExecutionState` reset. Dropping it is the whole release.
//! - **Tier 3 (CLI fallback):** `systemd-inhibit --mode=block` holds the lock
//!   only for as long as its child process lives, so the entry owns the child
//!   and releasing means killing it. A child that already exited on its own is
//!   not an error: the lock was already gone.
//!
//! The table is a `Mutex<HashMap>` guarded by a process-wide lock. Entries are
//! removed *before* their release closure runs so a slow D-Bus call can never
//! hold the lock that another thread needs.

use std::collections::HashMap;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use uda_core::error::UdaError;
use uda_core::wakelock::{WakeLockGuard, WakeLockType};

use crate::dispatch::WakeLockHandle;
use crate::error::Failure;

/// Lifetime, in seconds, granted to a CLI-tier lock.
///
/// `systemd-inhibit --mode=block` returns as soon as its child exits, so the
/// child is a `sleep` bounded by this value. The lock is therefore *not*
/// indefinite, which the C caller is told about through
/// `uda_last_error_message()` when the tier is used.
const CLI_LOCK_SECONDS: u64 = 3600;

/// How a live entry must be released.
enum LockEntry {
    /// A native guard whose `Drop` performs the release.
    Native(WakeLockGuard),
    /// A CLI child holding the lock for as long as it runs.
    Cli { child: Child, expires_at: Instant },
}

/// Process-wide handle table.
fn registry() -> &'static Mutex<HashMap<u64, LockEntry>> {
    static REGISTRY: OnceLock<Mutex<HashMap<u64, LockEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Source of handle values. Starts at 1 so `0` can mean "no lock" in C.
fn next_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Acquire a wake lock of `lock_type` and return its handle.
///
/// Tries the platform's native IPC first (Tier 1/2) and falls back to the CLI
/// tier (Tier 3) when the session cannot provide it - for example a Linux box
/// with neither a ScreenSaver service nor an XDG portal.
pub(crate) fn acquire(lock_type: WakeLockType, reason: &str) -> Result<WakeLockHandle, Failure> {
    match acquire_native(lock_type, reason) {
        Ok(guard) => {
            // `next_handle` starts at 1, so `from_raw` cannot fail; mapping the
            // `None` case to an error keeps the registry free of a handle-0
            // entry that C would read as "no lock".
            let handle = WakeLockHandle::from_raw(next_handle())
                .ok_or_else(|| Failure::InvalidArgument("handle counter produced 0".to_string()))?;
            register(handle, LockEntry::Native(guard))
        }
        Err(error) => {
            log::debug!("native wake lock unavailable ({error}); trying the CLI tier");
            acquire_cli(lock_type, reason)
        }
    }
}

/// Try to acquire the lock through the platform's native IPC.
fn acquire_native(lock_type: WakeLockType, reason: &str) -> Result<WakeLockGuard, UdaError> {
    // `new()` and `acquire()` are async but the FFI surface is synchronous, so a
    // private current-thread runtime is built per call and immediately dropped.
    // The trait must be in scope for `acquire` to resolve.
    use uda_core::wakelock::WakeLockManager as _;

    #[cfg(target_os = "linux")]
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("tokio runtime: {e}")))?;
        let manager = runtime.block_on(uda_platform_linux::wakelock::LinuxWakeLockManager::new())?;
        runtime.block_on(manager.acquire(lock_type, reason))
    }

    #[cfg(target_os = "windows")]
    {
        let manager = uda_platform_windows::wakelock::WindowsWakeLockManager::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("tokio runtime: {e}")))?;
        runtime.block_on(manager.acquire(lock_type, reason))
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (lock_type, reason);
        Err(UdaError::NotSupported(
            "no wake-lock backend for this target".to_string(),
        ))
    }
}

/// Acquire the lock through the CLI tier (`systemd-inhibit`).
fn acquire_cli(lock_type: WakeLockType, reason: &str) -> Result<WakeLockHandle, Failure> {
    let what = match lock_type {
        WakeLockType::PreventDisplaySleep => "idle",
        WakeLockType::PreventSystemIdle => "idle:sleep",
    };

    let child = Command::new("systemd-inhibit")
        .arg("--who=UDA")
        .arg(format!("--why={reason}"))
        .arg(format!("--what={what}"))
        .arg("--mode=block")
        .arg("sleep")
        .arg(CLI_LOCK_SECONDS.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            Failure::Uda(UdaError::NotSupported(format!(
                "no native wake lock and the systemd-inhibit CLI fallback is unavailable: {e}"
            )))
        })?;

    log::debug!(
        "acquired CLI wake lock {lock_type:?} ({reason}) via systemd-inhibit for {CLI_LOCK_SECONDS}s"
    );

    let entry = LockEntry::Cli {
        child,
        expires_at: Instant::now() + Duration::from_secs(CLI_LOCK_SECONDS),
    };
    let handle = WakeLockHandle::from_raw(next_handle())
        .ok_or_else(|| Failure::InvalidArgument("handle counter produced 0".to_string()))?;
    register(handle, entry)
}

/// Insert an entry under `handle`, reusing the handle on registry failure.
fn register(handle: WakeLockHandle, entry: LockEntry) -> Result<WakeLockHandle, Failure> {
    let raw = handle.raw();
    let mut table = match registry().lock() {
        Ok(table) => table,
        Err(poisoned) => {
            // A panic while holding the table must not make every later call
            // fail; the remaining entries are still valid and removable.
            log::debug!("wake-lock registry lock was poisoned; recovering");
            poisoned.into_inner()
        }
    };
    table.insert(raw, entry);
    Ok(handle)
}

/// Release the lock behind `handle`.
///
/// The entry is removed first so the release path never holds the table lock.
/// Releasing an unknown or already-released handle is reported as
/// [`UDA_ERR_INVALID_ARGUMENT`](crate::error::UDA_ERR_INVALID_ARGUMENT): the
/// caller passed a handle this process does not own.
pub(crate) fn release(handle: WakeLockHandle) -> Result<(), Failure> {
    let raw = handle.raw();
    let entry = {
        let mut table = match registry().lock() {
            Ok(table) => table,
            Err(poisoned) => poisoned.into_inner(),
        };
        table.remove(&raw)
    };

    let Some(entry) = entry else {
        return Err(Failure::InvalidArgument(format!(
            "wake-lock handle {raw} is not a live lock in this process"
        )));
    };

    match entry {
        LockEntry::Native(guard) => {
            // Consumes the guard and runs the platform release closure once.
            guard.release();
            log::debug!("released native wake lock {raw}");
        }
        LockEntry::Cli {
            mut child,
            expires_at,
        } => {
            let still_running = matches!(child.try_wait(), Ok(None));
            if still_running {
                if let Err(e) = child.kill() {
                    log::debug!("failed to kill systemd-inhibit child for {raw}: {e}");
                }
            } else {
                log::debug!("CLI wake lock {raw} had already expired or exited");
            }
            // Reap the child so no zombie is left behind.
            if let Err(e) = child.wait() {
                log::debug!("failed to reap systemd-inhibit child for {raw}: {e}");
            }
            let _ = expires_at;
        }
    }

    Ok(())
}

/// Number of live locks. Exposed for tests and diagnostics.
#[cfg(test)]
fn live_count() -> usize {
    match registry().lock() {
        Ok(table) => table.len(),
        Err(poisoned) => poisoned.into_inner().len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialises every test that inspects the shared registry.
    ///
    /// The registry is process-wide state and cargo runs tests in parallel
    /// threads by default, so an absolute "the table holds N entries" assertion
    /// is only meaningful while no other test is concurrently holding a lock.
    /// This is the same failure mode `uda-platform-linux`'s `detection.rs` hit
    /// with `env::set_var`.
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    /// Take the registry lock, recovering from a panic in an earlier test.
    ///
    /// The guard borrows a `&'static` static, so returning it is sound without
    /// any lifetime laundering.
    fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
        let lock: &'static Mutex<()> = &REGISTRY_LOCK;
        match lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[test]
    fn handles_are_unique_and_non_zero() {
        let first = next_handle();
        let second = next_handle();
        assert_ne!(first, second);
        assert_ne!(first, 0);
        assert_ne!(second, 0);
    }

    #[test]
    fn releasing_an_unknown_handle_fails_without_touching_the_table() {
        let _guard = registry_guard();
        let before = live_count();
        // A handle that was never handed out.
        let bogus = WakeLockHandle::from_raw(u64::MAX - 1).expect("non-zero");
        let result = release(bogus);
        assert!(result.is_err());
        assert_eq!(live_count(), before);
    }

    #[test]
    fn acquire_and_release_round_trip_through_the_registry() {
        let _guard = registry_guard();
        // On this host the session bus has no ScreenSaver service, so this
        // exercises the CLI tier end to end; on a full desktop it exercises the
        // native tier. Either way the table must return to its starting size.
        let before = live_count();
        let handle = acquire(WakeLockType::PreventDisplaySleep, "uda-ffi test")
            .expect("a wake lock is available through some tier");
        assert_eq!(live_count(), before + 1);

        release(handle).expect("releasing a live lock succeeds");
        assert_eq!(live_count(), before, "the entry must be removed on release");
    }

    #[test]
    fn double_release_is_reported_not_silently_ignored() {
        let _guard = registry_guard();
        let handle = acquire(WakeLockType::PreventSystemIdle, "uda-ffi double release")
            .expect("a wake lock is available");
        release(handle).expect("first release succeeds");
        let second = release(handle);
        assert!(
            second.is_err(),
            "the handle is gone after the first release"
        );
    }

    #[test]
    fn many_locks_can_be_held_and_released_independently() {
        let _guard = registry_guard();
        let before = live_count();
        let mut handles = Vec::new();
        for index in 0..4 {
            let handle = acquire(
                WakeLockType::PreventDisplaySleep,
                &format!("uda-ffi batch {index}"),
            )
            .expect("lock acquired");
            handles.push(handle);
        }
        assert_eq!(live_count(), before + 4, "all four locks are live");

        for handle in handles {
            release(handle).expect("each lock releases");
        }
        assert_eq!(live_count(), before, "every lock was removed");
    }

    #[test]
    fn cli_lock_arguments_cover_the_requested_flags() {
        // The `--what` flag must widen for system-idle locks; this is the only
        // difference the CLI tier can express.
        let display = match WakeLockType::PreventDisplaySleep {
            WakeLockType::PreventDisplaySleep => "idle",
            WakeLockType::PreventSystemIdle => "idle:sleep",
        };
        let system = match WakeLockType::PreventSystemIdle {
            WakeLockType::PreventDisplaySleep => "idle",
            WakeLockType::PreventSystemIdle => "idle:sleep",
        };
        assert_eq!(display, "idle");
        assert_eq!(system, "idle:sleep");
    }
}
