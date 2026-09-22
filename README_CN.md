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

> [!WARNING]
> **🚧 正在积极开发的前沿分支 (`develop`)**
> 
> 您当前浏览的是正在筹备 **v0.2.0** 的**前沿开发分支**。此分支代码正处于高频实验性开发期（系统托盘、DBusMenu、Win32 消息循环），API 可能会随时发生不兼容变动。如需使用经过全面测试的生产稳定版，请切换至 [`main`](https://github.com/UniDesktop/SDK/tree/main) 稳定分支或查看 [v0.1.0 正式发布版](https://github.com/UniDesktop/SDK/releases)。

## 为什么需要 UDA？

如今开发跨平台桌面应用，同一个功能往往要写五遍：Windows 上用 `SystemParametersInfoW`，Linux 上则是 `gsettings`、`qdbus`、`swww`、`hyprpaper` 各写一套——失败模式各异、会话假设不同，也没有统一的类型系统。UDA 用一套能力驱动（capability-driven）的 Rust API 取代这种碎片化：探测运行环境、选择可用后端、优雅降级，而不是直接崩溃。

- **Portal 优先** —— 所有 Linux 功能都先尝试 XDG Desktop Portal，再回落到发行版特定方案。
- **级联回退** —— 严格的层级链：Portal → 原生桌面环境 IPC → CLI 工具 → 强类型 `UdaError::Unsupported`。
- **能力驱动** —— 功能主动上报 `SupportLevel::Full` / `Restricted` / `Unsupported`，让你的应用在出错前就能分流。
- **零重型运行时依赖** —— Linux 侧为纯 Rust `zbus`，Windows 侧为 `windows-rs`。不依赖 Qt、GTK 或任何打包工具箱。

---

## 🖥️ 桌面环境与平台支持矩阵

| 平台 / 桌面环境 | 主题检测 | 壁纸 | 通知 | 防休眠锁 |
| --- | :---: | :---: | :---: | :---: |
| **Windows 10 / 11** | ✅ | ✅ | ✅ | ✅ |
| **GNOME 42+** | ✅ | ✅ | ✅ | ✅ |
| **KDE Plasma 5 / 6** | ✅ | ✅ | ✅ | ✅ |
| **XFCE** | ✅ | ✅ | ✅ | ✅ |
| **Hyprland** | ⚠️ | ✅ | ⚠️ | ⚠️ |
| **Sway** | ⚠️ | ✅ | ⚠️ | ⚠️ |
| **通用 X11** | ⚠️ | ✅ | ⚠️ | ⚠️ |

> **本版本已交付：** 深浅色外观检测并支持跟随系统 · 壁纸管理（`Crop` / `Fill` / `Fit` / `Stretch` 四种模式、多显示器指定、深浅色配对）· 原生通知（动作按钮与紧急度提示）· 防休眠常亮锁（`WakeLockType::PreventDisplaySleep` / `PreventSystemIdle`）。
>
> `✅` 已验证对接真实后端 · `⚠️` 尽力而为的回退方案——具体能力通过运行时的 `capabilities()` 上报。

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
| [`uda-core`](crates/uda-core) | 定义公共契约：`AppearanceManager`、`WallpaperManager`、`NotificationManager`、`WakeLockManager`，以及 `Capability`、`SupportLevel`、`Theme`、`FillMode`、`WallpaperOptions`、`UdaError`。不含任何平台代码。 |
| [`uda-platform-linux`](crates/uda-platform-linux) | 基于 `zbus`、XDG Desktop Portal、GNOME `gsettings`、KDE `plasmashell`、Hyprland/Sway socket 与 X11 CLI 工具（`feh`、`nitrogen`）实现上述契约。 |
| [`uda-platform-windows`](crates/uda-platform-windows) | 基于注册表、`SystemParametersInfoW`、`SetThreadExecutionState` 与 WinRT toast 实现上述契约。以 `#![cfg(windows)]` 门控，确保 Linux 宿主编译不受影响。 |
| [`uda-ffi`](crates/uda-ffi) | 将两套后端封装为 8 个 `#[no_mangle] extern "C"` 函数，边界处遏制 panic，并提供线程本地的 last-error 槽位。 |

---

## 🗺️ v0.2.0 路线图

以下功能**尚在规划中，尚未实现**。

| 功能 | 状态 |
| --- | --- |
| 系统托盘（Linux `org.kde.StatusNotifierItem` + Windows `Shell_NotifyIconW`） | 规划中 |
| MPRIS v2 媒体控制与元数据监听（Windows 侧为 SMTC） | 规划中 |
| 全局快捷键注册（Portal / X11 / Win32） | 规划中 |
| 基于 `libei` 的 Wayland 输入模拟 | 规划中 |

后续阶段还将覆盖剪贴板、音频路由、屏幕亮度、会话生命周期、虚拟桌面与屏幕捕获。

---

## 🤝 社区与赞助

**组织机构：** United Desktop Association

UDA 由 United Desktop Association 开发并维护，这是一个致力于降低跨平台桌面集成成本的开源社区。

- 💛 **赞助页面：** [https://afdian.com/a/srinternet](https://afdian.com/a/srinternet)
- 🌐 **官方网站：** [https://unidesktop.sr-studio.cn](https://unidesktop.sr-studio.cn)

---

<div align="center">

本项目采用 **Apache License 2.0**（[`LICENSE-APACHE`](LICENSE-APACHE)）或 **MIT License**（[`LICENSE-MIT`](LICENSE-MIT)）双许可，由你任选其一。

[English](README.md) · **简体中文**

</div>
