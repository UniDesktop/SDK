use crate::error::UdaError;
use std::sync::Arc;
use uda_core::wakelock::{WakeLockGuard, WakeLockManager, WakeLockType};
use zbus::Connection;

pub struct LinuxWakeLockManager {
    connection: Arc<Connection>,
}

impl LinuxWakeLockManager {
    pub async fn new() -> Result<Self, UdaError> {
        let connection = Connection::session()
            .await
            .map_err(|e| UdaError::Internal(format!("Failed to connect to session bus: {e}")))?;
        Ok(Self {
            connection: Arc::new(connection),
        })
    }
}

#[async_trait::async_trait]
impl WakeLockManager for LinuxWakeLockManager {
    async fn acquire(
        &self,
        lock_type: WakeLockType,
        reason: &str,
    ) -> Result<WakeLockGuard, UdaError> {
        let proxy = zbus::Proxy::new(
            &self.connection,
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        )
        .await
        .map_err(|e| UdaError::Internal(format!("Failed to create ScreenSaver proxy: {e}")))?;

        let flags = lock_type.to_screen_saver_flags();
        let cookie: u32 = proxy
            .call("Inhibit", &(env!("CARGO_PKG_NAME"), reason, flags))
            .await
            .map_err(|e| UdaError::Internal(format!("Inhibit call failed: {e}")))?;

        let connection = Arc::clone(&self.connection);
        let cookie_for_release = cookie;

        Ok(WakeLockGuard::new(Box::new(move || {
            let connection = Arc::clone(&connection);
            let cookie = cookie_for_release;
            std::thread::spawn(move || {
                let rt = match tokio::runtime::Runtime::new() {
                    Ok(rt) => rt,
                    Err(_) => return,
                };
                rt.block_on(async move {
                    if let Ok(proxy) = zbus::Proxy::new(
                        &connection,
                        "org.freedesktop.ScreenSaver",
                        "/org/freedesktop/ScreenSaver",
                        "org.freedesktop.ScreenSaver",
                    )
                    .await
                    {
                        let _response: Result<(), _> = proxy.call("UnInhibit", &cookie).await;
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
}
