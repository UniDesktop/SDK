use crate::error::UdaError;

/// Type of wake lock to acquire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeLockType {
    /// Prevent the display/screen from sleeping.
    PreventDisplaySleep,
    /// Prevent the system from idling/suspending.
    PreventSystemIdle,
}

impl WakeLockType {
    /// Map to FreeDesktop ScreenSaver inhibit flags (org.freedesktop.ScreenSaver).
    pub fn to_screen_saver_flags(&self) -> u32 {
        match self {
            WakeLockType::PreventDisplaySleep => 8, // InhibitIdle
            WakeLockType::PreventSystemIdle => 12,  // InhibitIdle | InhibitSuspend
        }
    }
}

/// RAII guard for a wake lock.
///
/// When this guard is dropped, the underlying inhibit is released automatically.
pub struct WakeLockGuard {
    release_fn: Option<Box<dyn FnOnce() + Send>>,
}

impl WakeLockGuard {
    pub fn new(release_fn: Box<dyn FnOnce() + Send>) -> Self {
        Self {
            release_fn: Some(release_fn),
        }
    }

    /// Manually release the wake lock early.
    pub fn release(mut self) {
        if let Some(release_fn) = self.release_fn.take() {
            release_fn();
        }
    }
}

impl Drop for WakeLockGuard {
    fn drop(&mut self) {
        if let Some(release_fn) = self.release_fn.take() {
            release_fn();
        }
    }
}

/// Cross-platform wake lock management interface.
#[async_trait::async_trait]
pub trait WakeLockManager {
    /// Acquire a wake lock of the given type.
    ///
    /// Returns a [`WakeLockGuard`] that will automatically release the lock when dropped.
    async fn acquire(
        &self,
        lock_type: WakeLockType,
        reason: &str,
    ) -> Result<WakeLockGuard, UdaError>;
}
