use std::sync::Arc;

use uda_core::wakelock::{WakeLockGuard, WakeLockManager, WakeLockType};
use zbus::Connection;

use crate::error::UdaError;
use crate::internal_dbus;

/// `org.freedesktop.ScreenSaver` on the session bus, where a wake lock lives.
const SCREEN_SAVER_SERVICE: &str = "org.freedesktop.ScreenSaver";
const SCREEN_SAVER_PATH: &str = "/org/freedesktop/ScreenSaver";
const SCREEN_SAVER_INTERFACE: &str = "org.freedesktop.ScreenSaver";

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
}
