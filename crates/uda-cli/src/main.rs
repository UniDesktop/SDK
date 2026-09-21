use uda_core::appearance::AppearanceManager;
use uda_core::notification::{Notification, NotificationManager, Urgency};
use uda_core::wakelock::{WakeLockManager, WakeLockType};
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};
use uda_platform_linux::appearance::LinuxAppearanceManager;
use uda_platform_linux::detection::detect_environment;
use uda_platform_linux::notification::LinuxNotificationManager;
use uda_platform_linux::wakelock::LinuxWakeLockManager;
use uda_platform_linux::wallpaper::LinuxWallpaperManager;

fn main() {
    println!("=== UDA Runtime Diagnostic ===");

    // 1. 测试环境检测
    match detect_environment() {
        Ok(env) => println!("Detected Environment: {:#?}", env),
        Err(e) => println!("Failed to detect environment: {:?}", e),
    }

    // 2. 测试外观检测
    let manager = LinuxAppearanceManager::new();
    match manager.detect_theme() {
        Ok(theme) => println!("Detected Theme: {:?}", theme),
        Err(e) => println!("Failed to detect theme: {:?}", e),
    }

    // 3. 测试壁纸设置
    let wallpaper_manager = LinuxWallpaperManager::new();
    let options = WallpaperOptions {
        fill_mode: FillMode::Fill,
        monitor_index: None,
        dark_mode: false,
    };

    match wallpaper_manager.set_wallpaper("/home/user/Pictures/wallpaper.jpg", &options) {
        Ok(()) => println!("Wallpaper set successfully"),
        Err(e) => println!("Failed to set wallpaper: {:?}", e),
    }

    // 4. 测试通知发送（最小示例）
    let rt = tokio::runtime::Runtime::new().unwrap();
    let notification_manager =
        rt.block_on(async { LinuxNotificationManager::new().await.unwrap() });

    let notification = Notification {
        app_name: "UDA".to_string(),
        replaces_id: 0,
        app_icon: String::new(),
        summary: "Hello from UDA".to_string(),
        body: "This is a minimal notification example".to_string(),
        actions: Vec::new(),
        expire_timeout: 5000,
        urgency: Urgency::Normal,
    };

    match rt.block_on(async { notification_manager.send(&notification).await }) {
        Ok(id) => println!("Notification sent successfully, id={}", id),
        Err(e) => println!("Failed to send notification: {:?}", e),
    }

    // 5. 测试 WakeLock（持有 2 秒后自动释放）
    let wakelock_manager = rt.block_on(async { LinuxWakeLockManager::new().await.unwrap() });

    let guard = rt.block_on(async {
        wakelock_manager
            .acquire(WakeLockType::PreventDisplaySleep, "UDA demo wakelock")
            .await
    });

    match guard {
        Ok(guard) => {
            println!("WakeLock acquired, holding for 2 seconds...");
            std::thread::sleep(std::time::Duration::from_secs(2));
            println!("Releasing WakeLock early...");
            guard.release();
            println!("WakeLock released");
        }
        Err(e) => println!("Failed to acquire WakeLock: {:?}", e),
    }
}
