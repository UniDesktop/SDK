//! UDA 诊断 CLI：在开发者本机上手动验证 UDA 的桌面能力。
//!
//! 用法：
//!
//! ```text
//! cargo run -p uda-cli -- [--dry-run] [壁纸路径]
//! ```
//!
//! - `--dry-run`：只打印将执行的动作序列，不做任何有副作用的操作（不设置壁纸、
//!   不发通知、不持锁），可在无桌面会话或非 Linux 平台上安全运行。
//! - `壁纸路径`：可选，传给 `set_wallpaper` 的图片路径。不再硬编码用户目录，
//!   避免对不存在的路径产生误导性报错。
//!
//! 实际执行仅在 Linux 上可用：Linux 专属调用全部位于 `#[cfg(target_os = "linux")]`
//! 门禁之后（与 `uda-platform-windows` 的 `#![cfg(windows)]` 对称，见 AGENTS.md
//! Principle 3 的交叉编译卫生要求），其它平台得到友好提示而不是编译错误或 panic。

use std::env;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

const USAGE: &str = "用法: cargo run -p uda-cli -- [--dry-run] [壁纸路径]";

/// 轻量错误包装：`Debug` 直接输出消息本身，`main` 返回 `Result` 时打印
/// `Error: 未知选项 ...` 而不是 `Error: "未知选项 ..."`。
struct CliError(String);

impl fmt::Debug for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for CliError {}

/// 解析后的命令行参数。
struct Options {
    /// 只打印将执行的动作，不真正执行。
    dry_run: bool,
    /// 要设置成壁纸的图片路径；缺省时跳过壁纸设置。
    wallpaper: Option<PathBuf>,
}

impl Options {
    /// 解析命令行参数；未知选项按类型化错误拒绝，而不是静默忽略。
    fn parse<I: Iterator<Item = String>>(args: I) -> Result<Self, String> {
        let mut options = Self {
            dry_run: false,
            wallpaper: None,
        };
        for arg in args {
            match arg.as_str() {
                "--dry-run" => options.dry_run = true,
                "--help" | "-h" => {
                    println!("{USAGE}");
                    println!("  --dry-run        只打印将执行的动作，不设置壁纸/发通知/持锁");
                    println!("  [壁纸路径]       传给 set_wallpaper 的图片路径（可选）");
                    std::process::exit(0);
                }
                other if other.starts_with('-') => {
                    return Err(format!("未知选项 `{other}`；使用 --help 查看用法"));
                }
                other => {
                    if options.wallpaper.is_some() {
                        return Err(format!("多余的参数 `{other}`；壁纸路径只能指定一次"));
                    }
                    options.wallpaper = Some(PathBuf::from(other));
                }
            }
        }
        Ok(options)
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let options = Options::parse(env::args().skip(1)).map_err(CliError)?;

    if options.dry_run {
        print_plan(&options);
        return Ok(());
    }
    run(&options)
}

/// 打印将要执行的诊断动作序列（无副作用）。
fn print_plan(options: &Options) {
    println!("=== UDA Diagnostic Plan (dry-run) ===");
    println!("1. Detect desktop environment (detect_environment)");
    println!("2. Detect dark/light theme (detect_theme)");
    match &options.wallpaper {
        Some(path) => println!("3. Set wallpaper to {} (fill mode: Fill)", path.display()),
        None => {
            println!("3. Set wallpaper: skipped (pass a path as the first argument to exercise it)")
        }
    }
    println!("4. Send a test notification (summary: \"Hello from UDA\")");
    println!("5. Acquire a display-sleep wakelock, hold it 2 s, then release");
    if cfg!(not(target_os = "linux")) {
        println!();
        println!(
            "注意：当前平台（{}）不支持实际执行，以上仅为计划展示。",
            env::consts::OS
        );
    }
}

/// 执行真正的诊断序列。
#[cfg(target_os = "linux")]
fn run(options: &Options) -> Result<(), Box<dyn Error>> {
    run_linux(options)
}

/// 非 Linux 平台：给出友好提示而不是 panic（Linux 专属代码未参与编译）。
#[cfg(not(target_os = "linux"))]
fn run(options: &Options) -> Result<(), Box<dyn Error>> {
    if let Some(path) = &options.wallpaper {
        println!(
            "（本次未执行：命令行给出的壁纸路径 {:?} 只会在 Linux 目标上被设置。）",
            path.display()
        );
    }
    println!(
        "UDA 诊断 CLI 的实际执行目前仅支持 Linux（当前平台：{}）。",
        env::consts::OS
    );
    println!("Linux 专属检测（环境探测、GSettings 壁纸、D-Bus 通知与常亮锁）已按 cfg 门禁排除；");
    println!("使用 `cargo run -p uda-cli -- --dry-run` 可无副作用地查看将执行的动作序列。");
    Ok(())
}

/// Linux 专属诊断：环境检测、外观、壁纸、通知与常亮锁。
///
/// 各步骤独立报告成败——这是诊断工具，一个子系统失败不应掩盖其余子系统的
/// 结果；只有 tokio 运行时创建失败才提前返回（通知与常亮锁都依赖它）。
/// 全程不使用 `unwrap`/`expect`（AGENTS.md Principle 1）。
#[cfg(target_os = "linux")]
fn run_linux(options: &Options) -> Result<(), Box<dyn Error>> {
    use uda_core::appearance::AppearanceManager;
    use uda_core::notification::{Notification, NotificationManager, Urgency};
    use uda_core::wakelock::{WakeLockManager, WakeLockType};
    use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};
    use uda_platform_linux::appearance::LinuxAppearanceManager;
    use uda_platform_linux::detection::detect_environment;
    use uda_platform_linux::notification::LinuxNotificationManager;
    use uda_platform_linux::wakelock::LinuxWakeLockManager;
    use uda_platform_linux::wallpaper::LinuxWallpaperManager;

    println!("=== UDA Runtime Diagnostic ===");

    // 1. 测试环境检测
    match detect_environment() {
        Ok(env) => println!("Detected Environment: {env:#?}"),
        Err(e) => println!("Failed to detect environment: {e:?}"),
    }

    // 2. 测试外观检测
    match LinuxAppearanceManager::new().detect_theme() {
        Ok(theme) => println!("Detected Theme: {theme:?}"),
        Err(e) => println!("Failed to detect theme: {e:?}"),
    }

    // 3. 测试壁纸设置（路径来自命令行参数，不再硬编码用户目录）
    match options
        .wallpaper
        .as_deref()
        .and_then(std::path::Path::to_str)
    {
        Some(path) => {
            let wallpaper_options = WallpaperOptions {
                fill_mode: FillMode::Fill,
                monitor_index: None,
                dark_mode: false,
            };
            match LinuxWallpaperManager::new().set_wallpaper(path, &wallpaper_options) {
                Ok(()) => println!("Wallpaper set successfully: {path}"),
                Err(e) => println!("Failed to set wallpaper: {e:?}"),
            }
        }
        None => println!("Wallpaper: skipped (no UTF-8 path given on the command line)"),
    }

    // 4/5. 通知与常亮锁需要异步运行时；创建失败按类型化错误提前返回。
    let runtime = tokio::runtime::Runtime::new()?;

    // 4. 测试通知发送（最小示例）
    match runtime.block_on(LinuxNotificationManager::new()) {
        Ok(manager) => {
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
            match runtime.block_on(manager.send(&notification)) {
                Ok(id) => println!("Notification sent successfully, id={id}"),
                Err(e) => println!("Failed to send notification: {e:?}"),
            }
        }
        Err(e) => println!("Failed to create notification manager: {e:?}"),
    }

    // 5. 测试 WakeLock（持有 2 秒后释放）
    match runtime.block_on(LinuxWakeLockManager::new()) {
        Ok(manager) => {
            // 先构造 future 再 block_on，让 acquire 的调用参数保持在可读的
            // 单行内。
            let acquire =
                manager.acquire(WakeLockType::PreventDisplaySleep, "UDA diagnostic wakelock");
            match runtime.block_on(acquire) {
                Ok(guard) => {
                    println!("WakeLock acquired, holding for 2 seconds...");
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    println!("Releasing WakeLock early...");
                    guard.release();
                    println!("WakeLock released");
                }
                Err(e) => println!("Failed to acquire WakeLock: {e:?}"),
            }
        }
        Err(e) => println!("Failed to create wakelock manager: {e:?}"),
    }

    Ok(())
}
