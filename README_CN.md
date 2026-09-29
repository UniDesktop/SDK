<div align="center">

# <image src="./icons/UniDesktop_3D_transparent_mini.png" height="30"/>  UniDesktop API (UDA)

*Qt 缺失的另一半——用一套类型安全的 API，统一碎片化的 Linux 桌面环境与 Windows。*

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/UniDesktop/SDK/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/UniDesktop/SDK/actions/workflows/ci.yml)
![Platform: Linux](https://img.shields.io/badge/platform-Linux%20(GNOME%20%2F%20KDE%20%2F%20Wayland)-lightgrey.svg)
![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-lightgrey.svg)

[English](README.md) · **简体中文**

</div>

---

> [!IMPORTANT]
> **🚀 v0.2.0 发版筹备分支 (`develop`)**
>
> 本分支已收官 **Phase 2（交互式 Shell 与系统集成）**：系统托盘、媒体播控与会话电源生命周期均已在真实后端上完成验证。随即将以 **v0.2.0** 标签合并发布。如需使用经过长期考验的生产稳定版，请切换至 [`main`](https://github.com/UniDesktop/SDK/tree/main) 分支或查看 [v0.1.0 正式发布版](https://github.com/UniDesktop/SDK/releases)。

## 为什么需要 UDA？

如今开发跨平台桌面应用，同一个功能往往要写五遍：Windows 上用 `SystemParametersInfoW`，Linux 上则是 `gsettings`、`qdbus`、`swww`、`hyprpaper` 各写一套——失败模式各异、会话假设不同，也没有统一的类型系统。UDA 用一套能力驱动（capability-driven）的 Rust API 取代这种碎片化：探测运行环境、选择可用后端、优雅降级，而不是直接崩溃。

- **Portal 优先** —— 所有 Linux 功能都先尝试 XDG Desktop Portal，再回落到发行版特定方案。
- **级联回退** —— 严格的层级链：Portal → 原生桌面环境 IPC → CLI 工具 → 强类型 `UdaError::Unsupported`。
- **能力驱动** —— 功能主动上报 `SupportLevel::Full` / `Restricted` / `Unsupported`，让你的应用在出错前就能分流。
- **零重型运行时依赖** —— Linux 侧为纯 Rust `zbus`，Windows 侧为 `windows-rs`。不依赖 Qt、GTK 或任何打包工具箱。

---

## 🖥️ 桌面环境与平台支持矩阵

| 平台 / 桌面环境 | 主题 | 壁纸 | 通知 | 防休眠锁 | 系统托盘 | 媒体播控 | 会话电源 |
| --- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **Windows 10 / 11** | ✅ | ✅ | ✅ [^1] | ✅ | ✅ | ✅ | ✅ |
| **GNOME 42+** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **KDE Plasma 5 / 6** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **XFCE** | ✅ | ✅ | ✅ | ✅ | ⚠️ | ✅ | ✅ |
| **Hyprland** | ⚠️ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ | ✅ |
| **Sway** | ⚠️ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ | ✅ |
| **通用 X11** | ⚠️ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ | ✅ |

> **本版本已交付：** 深浅色外观检测并支持跟随系统 · 壁纸管理（`Crop` / `Fill` / `Fit` / `Stretch` 四种模式、多显示器指定、深浅色配对）· 原生通知（含图标与紧急度提示）· 防休眠常亮锁（`PreventDisplaySleep` / `PreventSystemIdle`）· **系统托盘**（文本项、复选框、分隔线、子菜单的跨平台菜单模型）· **媒体播控**（元数据读取与播放控制）· **会话与电源生命周期**（锁屏、注销、睡眠、休眠、重启、关机）。
>
> `✅` 已验证对接真实后端 · `⚠️` 尽力而为的回退方案——具体能力通过运行时的 `capabilities()` 上报。

[^1]: Windows 通知存在两项平台限制。在 **Microsoft Store 安装的运行时**（如商店版 Python、Node.js）中，toast 来源会显示为宿主应用的**包族名**（例如 `PythonSoftwareFoundation.Python.3.13_qbz5n2kfra8p0`），因为 Package Identity 会覆盖 AppUserModelID，UDA 无法改写该绑定。在**未打包的脚本环境**中，交互式*动作按钮*会静默降级为只读文本卡片，因为 toast 按钮需要由 MSIX 包注册的 COM 激活器。详见 [`docs/internals/notification_specs.md`](docs/internals/notification_specs.md) §3。

---

## 🚀 三种语言快速上手

UDA 提供三个入口：原生 Rust crate，以及稳定的 C-ABI（`include/uda.h`），可从 Python 与 Node.js 调用。

### Rust

```rust
use uda_core::appearance::AppearanceManager;
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};
use uda_platform_linux::appearance::LinuxAppearanceManager;
use uda_platform_linux::wallpaper::LinuxWallpaperManager;

fn main() -> Result<(), uda_core::error::UdaError> {
    let theme = LinuxAppearanceManager::new().detect_theme()?;
    println!("系统主题: {theme:?}");
    LinuxWallpaperManager::new().set_wallpaper(
        "/usr/share/backgrounds/gnome/adwaita-l.jpg",
        &WallpaperOptions {
            fill_mode: FillMode::Fit,
            ..Default::default()
        },
    )
}
```

按需引入 crate：

```toml
uda-core = { path = "crates/uda-core" }
uda-platform-linux = { path = "crates/uda-platform-linux" }   # 或 uda-platform-windows
```

运行内置诊断 CLI，查看本机各后端状态：

```bash
cargo run -p uda-cli
```

### Python

[`examples/python/uda.py`](examples/python/uda.py) 已提供开箱即用的 `ctypes` 封装。它通过 `cargo metadata` 定位动态库，因此即使 `target/` 目录被重定向也能正常工作，并支持 `UDA_LIBRARY` 环境变量。

```python
from uda import Uda, FillMode

with Uda() as uda:
    print(uda.detect_theme())                        # 'dark' | 'light' | 'unknown'
    print(uda.get_wallpaper())                       # 当前壁纸路径，未设置则为 None
    uda.set_wallpaper("/usr/share/backgrounds/gnome/adwaita-l.jpg", FillMode.FIT)
```

> 当当前会话没有任何可用后端时（无 Portal、无 GNOME/KDE，且 `PATH` 中既无 `feh` 也无 `nitrogen`），`set_wallpaper()` 会抛出 `UdaError`。请捕获该异常并读取上报的能力，而不是假定一定成功——这正是[为什么需要 UDA？](#为什么需要-uda)所述的能力驱动契约。

运行完整演示：

```bash
cargo build -p uda-ffi
python3 examples/python/demo.py
```

### Node.js

[`examples/nodejs/demo.js`](examples/nodejs/demo.js) 通过 [`koffi`](https://koffi.dev/) 调用同一套 C-ABI——无需 `node-gyp`，无需编译原生插件。

```bash
cd examples/nodejs && npm install
cargo build -p uda-ffi
node demo.js
```

```javascript
const koffi = require('koffi');

const lib = koffi.load(process.env.UDA_LIBRARY ?? 'uda_ffi.dll');
const setWallpaper = lib.func('uda_set_wallpaper', 'int32', ['const char *', 'int32']);
const detectTheme = lib.func('uda_detect_theme', 'int32', ['void *']);

// 填充模式：UDA_FILL_CROP=0、UDA_FILL_FILL=1、UDA_FILL_FIT=2、UDA_FILL_STRETCH=3
setWallpaper('C:\\Users\\me\\Pictures\\wall.png', 1);

const slot = koffi.alloc(koffi.types.int32, 1);
if (detectTheme(slot) === 0) {
  console.log('主题代码:', koffi.decode(slot, koffi.types.int32)); // 1 = 深色，2 = 浅色
}
```

> **出参写法：** `koffi.alloc(type, 1)` 返回一块可写的 BigInt 地址，再用 `koffi.decode(address, type)` 读回结果。UDA 返回的字符串必须用 `uda_free_string()` 释放——已验证的正确用法见 [`examples/nodejs/demo.js`](examples/nodejs/demo.js)。

---

## 🏗️ 架构与项目结构

UDA 是一个 Cargo 工作区，分为平台无关的契约层、每个操作系统一个实现 crate，以及面向非 Rust 消费者的 FFI 垫片。

```text
uda/
├── crates/
│   ├── uda-core/              # Trait、枚举、Capability 位标志、统一 UdaError
│   ├── uda-platform-linux/    # Portal + D-Bus + Wayland IPC + CLI 回退
│   ├── uda-platform-windows/  # Win32 / COM / WinRT（整体 #![cfg(windows)] 门控）
│   ├── uda-ffi/               # C-ABI 导出 → libuda_ffi.so / uda_ffi.dll
│   └── uda-cli/               # 本地诊断 CLI
├── examples/                  # Python（ctypes）与 Node.js（koffi）示例
├── include/                   # uda.h —— 稳定的 C 头文件
├── docs/internals/            # 各平台与功能的协议规格说明
└── scripts/                   # 无头环境验证脚本
```

| Crate | 职责 |
| --- | --- |
| [`uda-core`](crates/uda-core) | 定义公共契约：`AppearanceManager`、`WallpaperManager`、`NotificationManager`、`WakeLockManager`、`TrayManager`、`MediaManager`、`SessionManager`，以及 `Capability`、`SupportLevel`、`Theme`、`FillMode`、`WallpaperOptions`、`UdaError`。不含任何平台代码。 |
| [`uda-platform-linux`](crates/uda-platform-linux) | 基于 `zbus`、XDG Desktop Portal、GNOME `gsettings`、KDE `plasmashell`、Hyprland/Sway socket、X11 CLI 工具（`feh`、`nitrogen`）、MPRIS v2、SNI 与 logind 实现上述契约。 |
| [`uda-platform-windows`](crates/uda-platform-windows) | 基于注册表、`SystemParametersInfoW`、`SetThreadExecutionState`、WinRT toast、`Shell_NotifyIconW`、WinRT SMTC 与 Win32 会话/电源 API 实现上述契约。以 `#![cfg(windows)]` 门控，确保 Linux 宿主编译不受影响。 |
| [`uda-ffi`](crates/uda-ffi) | 将两套后端封装为若干 `#[no_mangle] extern "C"` 函数，边界处遏制 panic，并提供线程本地的 last-error 槽位。 |

---

## 🧩 v0.2.0 新增 —— 托盘、媒体与会话

### 系统托盘

跨平台菜单数据模型：文本项、复选框、分隔线、子菜单与禁用态。Linux 侧通过 SNI（`org.kde.StatusNotifierItem` + DBusMenu）导出托盘图标；Windows 侧使用 `Shell_NotifyIconW`，由独立工作线程与专用消息泵驱动，宿主的事件循环永不被劫持。

**Rust**

```rust
use uda_core::tray::{TrayIcon, TrayMenu, TrayIconSource};
use uda_platform_linux::tray::LinuxTrayManager;

let menu = TrayMenu::new()
    .text("设置", || log::info!("点击了设置"))
    .checkbox("开机自启", true, |on| log::info!("自启 = {on}"))
    .separator()
    .text("退出", || std::process::exit(0));

let icon = LinuxTrayManager::new()
    .create(TrayIcon::builder()
        .name("UDA Demo")
        .tooltip("UDA Tray")
        .icon(TrayIconSource::Path("icons/UniDesktop_3D_transparent_mini.png".into()))
        .menu(menu)
        .build())?;

icon.wait();          // 阻塞至图标被销毁
drop(icon);           // 或交给 Drop 自动从托盘注销
```

**Python**

```python
from uda import Uda

with Uda() as uda:
    menu = uda.create_tray_menu()
    menu.add_text("设置", lambda: print("设置"))
    menu.add_checkbox("开机自启", True, lambda on: print("自启", on))
    menu.add_separator()

    icon = uda.create_tray_icon("UDA Demo", tooltip="UDA Tray")
    icon.menu = menu
    icon.wait()
```

**Node.js**

```javascript
const { Uda } = require('./uda');

const uda = new Uda();
const menu = uda.createTrayMenu();
menu.addText('设置', () => console.log('设置'));
menu.addCheckbox('开机自启', true, (on) => console.log('自启', on));
menu.addSeparator();

const icon = uda.createTrayIcon('UDA Demo', { tooltip: 'UDA Tray' });
icon.menu = menu;
await icon.wait();
icon.destroy();
```

### 媒体播控

读取当前播放信息并控制播放器。Linux 侧走会话总线上的 MPRIS v2，Windows 侧使用 WinRT SMTC。

**Rust**

```rust
use uda_core::media::{MediaCommand, MediaManager};
use uda_platform_linux::media::LinuxMediaManager;

let manager = LinuxMediaManager::new().await?;
if let Some(track) = manager.active_metadata()? {
    println!("{} — {}", track.title, track.artists.join(", "));
}
manager.send_command(MediaCommand::PlayPause)?;
```

**Python**

```python
with Uda() as uda:
    track = uda.media.now_playing          # 无播放器时为 None
    if track:
        print(f"{track.title} — {', '.join(track.artists)}")
    print(uda.media.status)                # 'playing' | 'paused' | 'stopped'
    uda.media.send("play_pause")
```

**Node.js**

```javascript
const uda = new Uda();
const now = uda.media.nowPlaying;         // 无播放器时为 null
if (now) console.log(`${now.title} — ${now.artists.join(', ')}`);
console.log(uda.media.status);
uda.media.send('play_pause');
```

### 会话与电源生命周期

一次调用完成锁屏、注销、睡眠、休眠、重启与关机。Windows 侧在 `ExitWindowsEx` 之前会通过标准的 Token 提权流程获取 `SeShutdownPrivilege`，提权被拒时上报类型化错误而非 panic。

**Rust**

```rust
use uda_core::session::{SessionAction, SessionManager, perform};
use uda_platform_windows::session::WindowsSessionManager;

let manager = WindowsSessionManager::new();

// 先查询：锁屏是唯一被认为可安全自动化的动作。
for action in [SessionAction::Lock, SessionAction::Suspend] {
    if manager.capabilities()?.contains(action.capability()) {
        perform(&manager, action)?;
    }
}
```

**Python**

```python
with Uda() as uda:
    caps = uda.session.capabilities()          # {'lock': True, 'reboot': False, ...}
    if caps["lock"]:
        uda.session.lock()                     # 可安全自动化
    # uda.session.reboot()                    # 破坏性动作——需显式启用
```

**Node.js**

```javascript
const uda = new Uda();
console.log(uda.session.capabilities);        // { lock: true, reboot: false, ... }
if (uda.session.supports('lock')) uda.session.lock();
// uda.session.reboot();                     // 破坏性动作——需显式启用
```

> **安全约定：** 只有 `lock` 是非破坏性的。其余动作各自对应独立的能力位，UI 可以先查询再决定是否展示可能导致关机的菜单项。

---

## 🗺️ 路线图

**Phase 2 —— 交互式 Shell 与系统集成（v0.2.0）已收官。** 系统托盘、媒体播控与会话电源生命周期均已交付，并已在真实后端上完成验证。

**Phase 3（v0.3.0）—— Shell 扩展与窗口拓扑** 为当前目标：

| 功能 | 重点 |
| --- | --- |
| 全局快捷键 | 覆盖 Wayland Portal、X11 与 Win32 的组合键监听 |
| 高级剪贴板 | 多格式 MIME 读写与变更监听 |
| 音频端点路由 | 默认输出设备切换与主音量控制 |
| 显示器亮度 | ACPI 背光与外接显示器 DDC/CI |

后续阶段还将覆盖显示器拓扑与 HiDPI、任务栏角标与进度条、原生文件对话框、虚拟桌面、窗口美化（Mica / Acrylic / KWin 模糊）、输入模拟、屏幕捕获与用户空闲检测。

---

## 🤝 社区与赞助

**组织机构：** Universal Desktop Community

UDA 由 Universal Desktop Community 开发并维护，这是一个致力于降低跨平台桌面集成成本的开源社区。

- 💛 **赞助页面：** [https://afdian.com/a/srinternet](https://afdian.com/a/srinternet)
- 🌐 **官方网站：** [https://unidesktop.sr-studio.cn](https://unidesktop.sr-studio.cn)

---

<div align="center">

本项目采用 **Apache License 2.0**（[`LICENSE-APACHE`](LICENSE-APACHE)）或 **MIT License**（[`LICENSE-MIT`](LICENSE-MIT)）双许可，由你任选其一。

[English](README.md) · **简体中文**

</div>
