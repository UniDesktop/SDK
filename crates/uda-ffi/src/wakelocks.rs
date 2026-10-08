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
//! The table is a `Mutex<HashMap>` guarded by a process-wide lock. Entries are
//! removed *before* their release closure runs so a slow D-Bus call can never
//! hold the lock that another thread needs.
//!
//! # Release strategy per tier
//!
//! - **Tier 1/2 (native IPC):** the guard owns a D-Bus `UnInhibit` cookie or a
//!   Win32 `SetThreadExecutionState` reset. Dropping it is the whole release.
//! - **Tier 3 (CLI fallback):** `systemd-inhibit --mode=block` holds the lock
//!   only for as long as its child process lives, so the entry owns the child
//!   and releasing means killing it. A child that already exited on its own is
//!   not an error: the lock was already gone. The tier is also refused
//!   outright when `org.freedesktop.login1` is not on the system bus:
//!   `systemd-inhibit` does not hold a lock by itself - systemd-logind does -
//!   so with no logind (a WSL host, for one) nothing would receive the
//!   inhibit, the screen would still sleep, and returning success would hand
//!   the caller a lock that is pure fiction.
//!
//! # Lazy reaping
//!
//! A CLI-tier lock lives no longer than its `sleep` child, and nothing observes
//! that child unless someone comes back for the handle. Every access to the
//! table (insertion, release, and the diagnostic count) therefore sweeps it
//! first: entries whose child already exited are reaped and removed, and an
//! entry at or past its `expires_at` deadline is retired outright - the
//! documented lifetime of the lock is over, so the child is killed if it still
//! runs and then reaped. Exited children thus never linger as zombies, and the
//! table stays honest about which locks are actually live.

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

/// Lock the table, recovering from a poisoned lock.
///
/// A panic while holding the table must not make every later call fail: the
/// remaining entries are still valid and removable, which is strictly better
/// for a host that is shutting down.
fn lock_registry() -> std::sync::MutexGuard<'static, HashMap<u64, LockEntry>> {
    match registry().lock() {
        Ok(table) => table,
        Err(poisoned) => {
            log::debug!("wake-lock registry lock was poisoned; recovering");
            poisoned.into_inner()
        }
    }
}

/// Source of handle values. Starts at 1 so `0` can mean "no lock" in C.
fn next_handle() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Allocate the next handle, rejecting the `0` the counter could wrap to.
///
/// The counter starts at 1, so the rejection only fires after 2^64 acquires;
/// it is kept because a handle of `0` would read as "no lock" in C.
fn next_valid_handle() -> Result<WakeLockHandle, Failure> {
    WakeLockHandle::from_raw(next_handle())
        .ok_or_else(|| Failure::InvalidArgument("handle counter produced 0".to_string()))
}

/// Acquire a wake lock of `lock_type` and return its handle.
///
/// Tries the platform's native IPC first (Tier 1/2) and falls back to the CLI
/// tier (Tier 3) when the session cannot provide it - for example a Linux box
/// whose session bus has no ScreenSaver service but whose system bus still has
/// systemd-logind. A box with neither tier refuses instead: the CLI tier
/// verifies its receiver before spawning, so a lock nobody would honour is
/// never reported as held.
pub(crate) fn acquire(lock_type: WakeLockType, reason: &str) -> Result<WakeLockHandle, Failure> {
    match acquire_native(lock_type, reason) {
        Ok(guard) => register(next_valid_handle()?, LockEntry::Native(guard)),
        Err(error) => {
            log::debug!("native wake lock unavailable ({error}); trying the CLI tier");
            acquire_cli(lock_type, reason)
        }
    }
}

/// Try to acquire the lock through the platform's native IPC.
///
/// `new()` and `acquire()` are async but the FFI surface is synchronous, so the
/// futures run through the shared blocking bridge `crate::notify::run_sync`,
/// which is safe to call both with and without an ambient tokio runtime.
fn acquire_native(lock_type: WakeLockType, reason: &str) -> Result<WakeLockGuard, UdaError> {
    // `acquire` is a trait method, so the trait must be in scope wherever a
    // future below is built.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    use uda_core::wakelock::WakeLockManager as _;

    // Each block hands the bridge one already-`'static` future; the acquired
    // guard is the block's value.
    #[cfg(target_os = "linux")]
    {
        let reason = reason.to_string();
        crate::notify::run_sync(async move {
            let manager = uda_platform_linux::wakelock::LinuxWakeLockManager::new().await?;
            manager.acquire(lock_type, &reason).await
        })?
    }

    #[cfg(target_os = "windows")]
    {
        let reason = reason.to_string();
        crate::notify::run_sync(async move {
            let manager = uda_platform_windows::wakelock::WindowsWakeLockManager::new();
            manager.acquire(lock_type, &reason).await
        })?
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = (lock_type, reason);
        Err(UdaError::NotSupported(
            "no wake-lock backend for this target".to_string(),
        ))
    }
}

/// The `--what=` value the CLI tier passes for `lock_type`.
///
/// The flag is the only difference the CLI tier can express between the two
/// lock types.
fn cli_what_flag(lock_type: WakeLockType) -> &'static str {
    match lock_type {
        WakeLockType::PreventDisplaySleep => "idle",
        WakeLockType::PreventSystemIdle => "idle:sleep",
    }
}

/// Acquire the lock through the CLI tier (`systemd-inhibit`).
fn acquire_cli(lock_type: WakeLockType, reason: &str) -> Result<WakeLockHandle, Failure> {
    // The lock only exists because systemd-logind receives it; refuse rather
    // than spawn an inhibit nobody would honour (see the verifier).
    #[cfg(target_os = "linux")]
    verify_someone_receives_the_lock()?;

    let child = Command::new("systemd-inhibit")
        .arg("--who=UDA")
        .arg(format!("--why={reason}"))
        .arg(format!("--what={}", cli_what_flag(lock_type)))
        .arg("--mode=block")
        .arg("sleep")
        .arg(CLI_LOCK_SECONDS.to_string())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| {
            Failure::Uda(UdaError::NotSupported(format!(
                "no native wake lock and the systemd-inhibit CLI fallback is unavailable: {error}"
            )))
        })?;

    log::debug!(
        "acquired CLI wake lock {lock_type:?} ({reason}) via systemd-inhibit for {CLI_LOCK_SECONDS}s"
    );

    let entry = LockEntry::Cli {
        child,
        expires_at: Instant::now() + Duration::from_secs(CLI_LOCK_SECONDS),
    };
    register(next_valid_handle()?, entry)
}

/// Verify that somebody will actually receive the CLI tier's inhibit lock.
///
/// `systemd-inhibit --mode=block` does not hold a lock by itself: it asks
/// systemd-logind (`org.freedesktop.login1` on the system bus) to take one.
/// On a system without logind - the maintainer's WSL was the reported case -
/// no component receives it, the screen still sleeps, and a success return
/// from [`acquire_cli`] would be a lie the host cannot detect. The probe runs
/// through the FFI's shared blocking bridge [`crate::notify::run_sync`], which
/// is safe with and without an ambient tokio runtime; the platform side owns
/// the bus work and its `DBUS_TIMEOUT` bounds.
///
/// Both "probe says absent" and "probe failed" refuse: an unprovable receiver
/// is not a receiver, so failing closed is the only honest answer.
#[cfg(target_os = "linux")]
fn verify_someone_receives_the_lock() -> Result<(), Failure> {
    // `run_sync` yields `Result<Result<bool, UdaError>, UdaError>`: the outer
    // layer is the bridge's own failure, the inner one is the probe's verdict.
    // Both refuse, so every arm below closes the gate.
    match crate::notify::run_sync(uda_platform_linux::wakelock::logind_present()) {
        Ok(Ok(true)) => Ok(()),
        Ok(Ok(false)) => Err(Failure::Uda(UdaError::NotSupported(
            "no systemd-logind on the system bus: an inhibition lock would not be honoured"
                .to_string(),
        ))),
        Ok(Err(error)) | Err(error) => Err(Failure::Uda(error)),
    }
}

/// Kill and reap a CLI child so it never lingers as a zombie.
///
/// Both callers of this - an early release and an expiry sweep - must end with
/// the child reaped, so the kill and the wait are kept together rather than
/// repeated at each site. `cause` names why the child is being ended and
/// travels into the log: an expiry retirement is the one way a host's handle
/// dies without the host asking for it, so the kill that ends it is logged at
/// `info`, where the host (or the operator reading the log) can see it,
/// instead of in the `debug` stream the rest of the tier talks in.
fn kill_and_reap(handle: u64, child: &mut Child, cause: &str) {
    match child.try_wait() {
        // Still running: the lock is only released by ending the child.
        Ok(None) => {
            if let Err(error) = child.kill() {
                log::debug!("failed to kill CLI wake lock {handle}: {error}");
            } else {
                log::info!("CLI wake lock {handle} terminated ({cause})");
            }
        }
        Ok(Some(_)) => log::debug!("CLI wake lock {handle} had already exited ({cause})"),
        Err(error) => log::debug!("try_wait failed for CLI wake lock {handle}: {error}"),
    }

    if let Err(error) = child.wait() {
        log::debug!("failed to reap CLI wake lock {handle}: {error}");
    }
}

/// Sweep the table: reap CLI children that exited and retire locks past their
/// deadline.
///
/// Called on every access path (insertion, release, the diagnostic count) so
/// the table never advertises a lock whose backing process is already gone and
/// exited children are reaped instead of lingering as zombies. An entry at or
/// past `expires_at` is retired even while its child still runs: the deadline is
/// the documented lifetime of the lock, and letting it quietly continue would
/// turn the CLI tier's bounded-lock contract into a lie.
///
/// Retiring an expired entry holds the table lock while reaping, but the child
/// has been `SIGKILL`ed first, so the wait returns within milliseconds.
fn reap_retired(table: &mut HashMap<u64, LockEntry>) {
    let now = Instant::now();
    table.retain(|handle, entry| match entry {
        LockEntry::Native(_) => true,
        LockEntry::Cli { child, expires_at } => {
            if now >= *expires_at {
                // The lock is over whether or not the child noticed. The
                // retirement itself is announced here, before the reap, so the
                // one change a host can only observe as "my handle suddenly
                // stopped working" is traceable in the log to its cause.
                log::info!(
                    "CLI wake lock {handle} retired: its bounded {CLI_LOCK_SECONDS}s lifetime \
                     expired; a later release of the handle reports UDA_ERR_INVALID_ARGUMENT"
                );
                kill_and_reap(*handle, child, "retired at its expiry deadline");
                return false;
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    log::debug!("reaped exited CLI wake lock {handle} ({status})");
                    false
                }
                // Still running and not yet due: the lock is live.
                Ok(None) => true,
                Err(error) => {
                    // A failed probe says nothing about the lock itself, so the
                    // entry stays rather than dropping a live lock.
                    log::debug!("try_wait failed for CLI wake lock {handle}: {error}");
                    true
                }
            }
        }
    });
}

/// Insert an entry under `handle`, reusing the handle on registry failure.
fn register(handle: WakeLockHandle, entry: LockEntry) -> Result<WakeLockHandle, Failure> {
    let raw = handle.raw();
    let mut table = lock_registry();
    // Insertion is an access path, so stale entries are reaped here too: a host
    // that acquires repeatedly never accumulates zombie children.
    reap_retired(&mut table);
    table.insert(raw, entry);
    Ok(handle)
}

/// Release the lock behind `handle`.
///
/// The entry is removed first so the release path never holds the table lock
/// while releasing; the remaining entries are then swept, which makes every
/// release also reap CLI children that exited in the meantime. Releasing an
/// unknown or already-released handle is reported as
/// [`UDA_ERR_INVALID_ARGUMENT`](crate::error::UDA_ERR_INVALID_ARGUMENT): the
/// caller passed a handle this process does not own.
pub(crate) fn release(handle: WakeLockHandle) -> Result<(), Failure> {
    let raw = handle.raw();
    let entry = {
        let mut table = lock_registry();
        let entry = table.remove(&raw);
        reap_retired(&mut table);
        entry
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
        LockEntry::Cli { mut child, .. } => {
            kill_and_reap(raw, &mut child, "released by the host");
            log::debug!("released CLI wake lock {raw}");
        }
    }

    Ok(())
}

/// Number of live locks. Exposed for tests and diagnostics.
///
/// Sweeps the table first, so the count reflects entries whose backing process
/// is still alive rather than entries that merely were never released.
#[cfg(test)]
fn live_count() -> usize {
    let mut table = lock_registry();
    reap_retired(&mut table);
    table.len()
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

    /// Acquire a lock through whatever tier this host offers.
    ///
    /// Returns `None` when no tier exists at all - the C ABI reports that as
    /// `UDA_ERR_NOT_SUPPORTED`, and the exported-function tests early-return
    /// the same way. The registry mechanics are additionally covered by the
    /// synthetic-entry tests below, which depend on no tier whatsoever.
    fn acquire_any_tier(reason: &str) -> Option<WakeLockHandle> {
        acquire(WakeLockType::PreventDisplaySleep, reason).ok()
    }

    /// Whether `handle` is still in the registry, sweeping stale entries first
    /// exactly like a real access path would.
    fn registry_contains(handle: u64) -> bool {
        let mut table = lock_registry();
        reap_retired(&mut table);
        table.contains_key(&handle)
    }

    /// Spawn a `sleep` child standing in for a CLI-tier lock holder.
    ///
    /// The reaper can only be exercised against a real process, so these tests
    /// use the plainest one available.
    #[cfg(unix)]
    fn spawn_sleeper(seconds: &str) -> Child {
        Command::new("sleep")
            .arg(seconds)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("`sleep` exists on the unix hosts this crate is tested on")
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
        // On a host with a ScreenSaver service this exercises the native tier;
        // on one without but with systemd-logind it exercises the CLI tier -
        // whose acquire now refuses when logind is absent, the WSL shape. A
        // host with neither tier legitimately refuses (the `None` early
        // return), and the synthetic-entry tests below still cover the
        // registry mechanics there.
        let Some(handle) = acquire_any_tier("uda-ffi test") else {
            return;
        };
        let after_acquire = live_count();
        assert!(after_acquire >= 1, "acquiring a lock must add an entry");

        release(handle).expect("releasing a live lock succeeds");
        // This test holds the serialising lock, so nothing else can add or
        // remove entries between the two counts.
        assert_eq!(
            live_count(),
            after_acquire - 1,
            "the entry must be removed on release"
        );
    }

    #[test]
    fn double_release_is_reported_not_silently_ignored() {
        let _guard = registry_guard();
        let Some(handle) = acquire_any_tier("uda-ffi double release") else {
            return;
        };
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
        let mut handles = Vec::new();
        for index in 0..4 {
            let Some(handle) = acquire_any_tier(&format!("uda-ffi batch {index}")) else {
                // No tier on this host; nothing to exercise here.
                return;
            };
            handles.push(handle);
        }
        let with_all_four = live_count();
        assert!(with_all_four >= 4, "all four locks are live");

        for handle in handles {
            release(handle).expect("each lock releases");
        }
        assert_eq!(live_count(), with_all_four - 4, "every lock was removed");
    }

    #[test]
    fn the_reaper_never_touches_native_entries() {
        let _guard = registry_guard();

        let handle = WakeLockHandle::from_raw(next_handle()).expect("non-zero handle");
        register(
            handle,
            LockEntry::Native(WakeLockGuard::new(Box::new(|| {}))),
        )
        .expect("the entry registers");

        // Sweeping is what every access path does, and a native entry must
        // survive it: its lifetime is caller-controlled rather than bound to a
        // child process.
        assert!(
            registry_contains(handle.raw()),
            "a native entry must survive a reaping sweep"
        );

        release(handle).expect("the synthetic entry releases like a real one");
    }

    #[cfg(unix)]
    #[test]
    fn the_reaper_removes_a_cli_entry_whose_child_already_exited() {
        let _guard = registry_guard();

        let handle = WakeLockHandle::from_raw(next_handle()).expect("non-zero handle");
        register(
            handle,
            LockEntry::Cli {
                child: spawn_sleeper("0"),
                expires_at: Instant::now() + Duration::from_secs(CLI_LOCK_SECONDS),
            },
        )
        .expect("the entry registers");

        // `sleep 0` exits almost immediately; poll the registry so the test
        // does not race the child's exit. Every probe below is a real access
        // path, so it is the sweep - not the polling - that reaps the entry.
        let mut reaped = false;
        for _ in 0..500 {
            if !registry_contains(handle.raw()) {
                reaped = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            reaped,
            "an entry whose child exited must be reaped and removed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_reaper_retires_a_cli_entry_past_its_expiry() {
        let _guard = registry_guard();

        // A child that would keep running far past the test, paired with a
        // deadline that is already due: the lock's documented lifetime is over,
        // so the entry must be retired (child killed and reaped) even though
        // the caller never released the handle.
        let child = spawn_sleeper("30");
        let handle = WakeLockHandle::from_raw(next_handle()).expect("non-zero handle");
        register(
            handle,
            LockEntry::Cli {
                child,
                expires_at: Instant::now(),
            },
        )
        .expect("the entry registers");

        assert!(
            !registry_contains(handle.raw()),
            "an entry past expires_at must be retired by the reaper"
        );
    }

    #[test]
    fn cli_lock_arguments_cover_the_requested_flags() {
        // The `--what` flag must widen for system-idle locks; this is the only
        // difference the CLI tier can express.
        assert_eq!(cli_what_flag(WakeLockType::PreventDisplaySleep), "idle");
        assert_eq!(cli_what_flag(WakeLockType::PreventSystemIdle), "idle:sleep");
    }
}
