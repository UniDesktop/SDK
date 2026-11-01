<div align="center">

# <image src="./icons/UniDesktop_3D_transparent_mini.png" height="30"/>  UniDesktop API (UDA)

*用一套类型安全的 API，统一碎片化的 Linux 桌面环境与 Windows。*

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/UniDesktop/SDK/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/UniDesktop/SDK/actions/workflows/ci.yml)
[![Version](https://img.shields.io/badge/release-v0.2.1-brightgreen)](https://github.com/UniDesktop/SDK/releases)
![Platform: Linux](https://img.shields.io/badge/platform-Linux%20(GNOME%20%2F%20KDE%20%2F%20Wayland)-lightgrey.svg)
![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-lightgrey.svg)

[English](README.md) · **简体中文**

</div>

---

## 为什么需要 UDA？

今天开发跨平台桌面应用，同一个功能往往要写五遍：Windows 上 `SystemParametersInfoW`，Linux 上 `gsettings`、`qdbus`、`swww`、`hyprpaper` —— 各自的失败模式、会话假设，且没有共享的类型系统。UDA 用一套**能力驱动**的 Rust API 取代这种碎片化：探测环境、选择可用后端，**优雅降级而不是 panic**。

| 原则 | 含义 |
| --- | --- |
| **级联回退** | XDG Desktop Portal → 原生 DE IPC（`$XDG_CURRENT_DESKTOP`）→ CLI 工具（`swww`、`hyprpaper` …）→ 类型化 `UdaError::NotSupported`。全程不 panic。并非所有模块都走第一级：壁纸与防休眠直接从第二级开始，目前只有外观检测通过 `org.freedesktop.portal.Settings` 使用 Portal。 |
| **能力驱动** | 每个能力上报 `SupportLevel::Full` / `Partial(reason)` / `None`，让你的应用*在出错之前*分流。`Partial` 携带原因，降级可以被解释而不是被猜测。 |
| **零重依赖** | Linux 用纯 Rust `zbus`，Windows 用 `windows-rs`。不引入 Qt、GTK 或任何打包工具链。 |

## 🖥️ 桌面环境与平台支持矩阵

| 功能 | Windows 10/11 | GNOME 42+ | KDE Plasma 5/6 | XFCE | Hyprland | Sway | 通用 X11 |
|------|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| 主题检测 | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ |
| 强调色 | ✅ | ✅ | ⚠️ | ⚠️ | ❌ | ❌ | ❌ |
| 壁纸 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 通知 | ✅ [^1] | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ |
| 通知图标 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 通知按钮 | ⚠️ [^2] | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 防休眠锁 | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ |
| 系统托盘 | ✅ | ✅ | ✅ | ⚠️ | ✅ | ✅ | ✅ |
| 媒体播控 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 会话 —— 锁屏 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 会话 —— 注销 | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ⚠️ |
| 会话 —— 睡眠 / 休眠 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| 会话 —— 重启 / 关机 | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

`✅` 已对真实系统验证 · `⚠️` 尽力而为的回退或依赖额外组件 · `❌` 平台无此概念。精确能力始终由运行时的 `capabilities()` / `support_level()` 上报 —— 不要以本表作为假设依据。

[^1]: 在 **Microsoft Store 运行时**中，toast 来源显示为宿主的包系列名，因为 Package Identity 会覆盖 AppUserModelID。详见 [`docs/internals/notification_specs.md`](docs/internals/notification_specs.md) §3.1。
[^2]: 在**未打包的脚本宿主**中，交互式*操作按钮*降级为只读文本卡片，因为 toast 按钮需要 MSIX 包注册的 COM activator。详见同一文档 §3.2。

每一格所用的后端、⚠️ 的实际行为与各环境的上报结果：**[平台支持矩阵](https://unidesktop.github.io/reference/platform-support/)**。

## 🚀 三种语言快速上手

三个入口：原生 Rust crate，以及可从 Python 与 Node.js 调用的稳定 C-ABI（`include/uda.h`，由 cbindgen 从 Rust 源码生成）。

```rust
// Rust —— 直接用原生 Trait，无需 FFI 层
let manager = uda_platform_linux::LinuxWallpaperManager::new();
manager.set_wallpaper("~/Pictures/a.png", FillMode::Fill)?;
```

```python
# Python —— examples/python/uda.py（ctypes，零第三方依赖）
from uda import Uda
with Uda() as uda:
    print(uda.theme)                      # 'dark' | 'light' | 'unknown'
    uda.notify("标题", "正文内容")          # 系统通知
```

```javascript
// Node.js —— examples/nodejs/uda.js（koffi，零编译）
const { Uda } = require('./uda');
const uda = new Uda();
uda.notify('标题', '正文内容');
uda.dispose();
```

完整安装步骤、能力教程与全部 API 参考：**[unidesktop.github.io](https://unidesktop.github.io/)**。

## 🏗️ 架构与项目结构

```text
uda/
├── crates/
│   ├── uda-core/              # 公共 Trait、枚举、Capability 位标志、统一错误
│   ├── uda-platform-linux/    # Linux 后端：D-Bus (zbus)、Wayland IPC、X11
│   ├── uda-platform-windows/  # Windows 后端：Win32、COM、WinRT
│   ├── uda-ffi/               # C-ABI 导出层（Python ctypes / Node.js koffi）
│   └── uda-cli/               # 诊断 CLI，用于手动探测
├── docs/internals/            # 协议规格词典（速查手册）
├── examples/                  # 01_appearance … 07_session，三语言对齐
└── plans/                     # 各阶段实施计划
```

| 模块 | Linux 后端 | Windows 后端 |
| --- | --- | --- |
| 外观 | FreeDesktop portal `Settings` → GSettings → `kreadconfig` → `xfconf-query`；全部无结果时返回 `Theme::Unknown` | 注册表 `AppsUseLightTheme` / `SystemUsesLightTheme` |
| 壁纸 | GSettings → KDE `plasmashell` D-Bus → `hyprpaper` / `swww` → `feh` / `nitrogen`。不使用 Portal：`org.freedesktop.portal.Wallpaper` 存在但从未被调用 | `SystemParametersInfoW` |
| 通知 | `org.freedesktop.Notifications` | WinRT `ToastNotificationManager` |
| 常亮锁 | `org.freedesktop.ScreenSaver` 的 `Inhibit` | `SetThreadExecutionState` |
| 系统托盘 | `org.kde.StatusNotifierItem` + `com.canonical.dbusmenu` | `Shell_NotifyIconW` + 工作线程消息泵 |
| 媒体 | 会话总线上的 MPRIS v2 | WinRT SMTC |
| 会话与电源 | `systemd-logind` + `org.freedesktop.ScreenSaver` | Win32 电源与关机 API |

**C-ABI** 每次调用返回 `int32_t` 状态码（`0` 成功，`-1` 参数非法，`-2` 不支持，`-3` 检测失败，`-4` I/O 错误，`-5` 内部错误，`-6` 已捕获的 panic），且永不跨边界展开 —— 每个导出函数体都运行在 `catch_unwind` 内。

## 🗺️ 路线图

| 阶段 | 范围 | 状态 |
| --- | --- | --- |
| **1** | 核心类型、外观、壁纸、通知、常亮锁、C-ABI 与 SDK | **v0.1.0 已发布** |
| **2** | 系统托盘、媒体播控、会话与电源生命周期 | **v0.2.0 已发布** |
| **3** | 全局快捷键、高级剪贴板、音频输出路由、显示器亮度 | 进行中 |
| **4** | 虚拟桌面、外部窗口控制、窗口外观（Mica / Acrylic / KWin blur） | 计划中 |
| **5** | 输入模拟、取色器、屏幕捕获、空闲检测 | 计划中 |

分阶段细节见 [`plans/`](plans/)，工程规范见 [`AGENTS.md`](AGENTS.md)。

## 🤝 社区与赞助

欢迎在 [GitHub](https://github.com/UniDesktop/SDK) 提交 issue 与 pull request。本项目采用 [MIT](LICENSE-MIT) 与 [Apache-2.0](LICENSE-APACHE) 双许可。
