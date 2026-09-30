# Changelog

本文件记录本项目的全部值得注意的变更。
This file records every notable change to this project.

格式基于 [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)，且本项目遵循 [语义化版本](https://semver.org/spec/v2.0.0.html)。
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> 完整文档见 [unidesktop.github.io](https://unidesktop.github.io/)。
> Full documentation lives at [unidesktop.github.io](https://unidesktop.github.io/).

## [v0.2.0] - Interactive Shell & System Integration · 2026-09-30

### ✨ Added

- ✨ **系统托盘模块 (`tray`)**：跨平台菜单数据模型，支持文本项、复选框、分隔线、子菜单与禁用态。
- ✨ **System tray module (`tray`)**: a cross-platform menu model with text items, checkboxes, separators, submenus and disabled states.
  - Linux：基于 SNI（`org.kde.StatusNotifierItem` + `com.canonical.dbusmenu`）实现。
  - Linux: built on SNI (`org.kde.StatusNotifierItem` + `com.canonical.dbusmenu`).
  - Windows：基于 `Shell_NotifyIconW` 与隐藏消息窗口 + 独立工作线程的消息泵实现，宿主事件循环永不被劫持。
  - Windows: `Shell_NotifyIconW` over a hidden message window plus a worker-thread message pump, so the host's event loop is never hijacked.
  - 支持图标下采样与通道对齐，`TrayIcon` 实现 `Drop` 以在退出时自动从托盘注销。
  - Icons are downsampled and channel-aligned, and `TrayIcon` implements `Drop` to unregister itself from the tray on shutdown.

- ✨ **媒体播控中心 (`media`)**：Linux 端基于 MPRIS v2，Windows 端基于 WinRT SMTC。
- ✨ **Media playback control (`media`)**: MPRIS v2 on Linux, WinRT SMTC on Windows.
  - 读取当前播放曲目的标题、艺人、专辑与时长。
  - Reads the active track's title, artists, album and duration.
  - 提供播放 / 暂停 / 停止 / 上一首 / 下一首控制，以及播放状态查询。
  - Provides play / pause / stop / previous / next commands plus a playback-status query.

- ✨ **会话与电源生命周期 (`session`)**：一行代码实现跨平台六个动作。
- ✨ **Session & power lifecycle (`session`)**: six cross-platform actions behind one line each.
  - 锁屏（Lock）、注销（Logout）、睡眠（Suspend）、休眠（Hibernate）、重启（Reboot）、关机（Shutdown）。
  - Lock, logout, suspend, hibernate, reboot and shutdown.
  - Windows 端内置安全的 Token 提权机制（`SeShutdownPrivilege`），提权失败映射为类型化错误而非 panic。
  - Windows performs a safe token-privilege dance (`SeShutdownPrivilege`); a refusal maps to a typed error rather than a panic.
  - 只读动作（Lock）与破坏性动作在能力位层面区分，调用方可先行查询再决定是否展示菜单项。
  - Read-only (Lock) and destructive actions are separated by a capability bit, so a caller can query before drawing a menu entry.

- ✨ **`Notification::app_icon` 在 Windows 端落地**：图标经 `file://` URI 规范化后写入 toast 模板的 `<image>` 节点；无图标时降级到无图模板。
- ✨ **`Notification::app_icon` now renders on Windows**: the icon is normalised into a `file://` URI and written to the toast template's `<image>` node; with no icon the backend degrades to the text-only template.

### 🛠️ Changed

- 🛠️ **全线重构模块化单功能 Demo**：`examples/` 重组为 `01_appearance` 至 `07_session`，Rust / Python / Node.js 三语言完全对齐，每个示例只演示一项能力且默认无破坏性副作用。
- 🛠️ **Rebuilt the demos as one capability each**: `examples/` is reorganised into `01_appearance` … `07_session`, aligned across Rust / Python / Node.js, with no destructive side effects by default.
- 🛠️ **全库注释清理**：删除背景陈述与逐行解释，保留架构决策、协议要点与 `# Safety` 契约。
- 🛠️ **Codebase-wide comment cleanup**: background statements and line-by-line narration removed; architecture decisions, protocol notes and `# Safety` contracts kept.

### 🧪 Testing

- 🧪 **自动化测试扩充**：全工作区单元测试 224 项，全部通过。
- 🧪 **Automated tests expanded**: 224 workspace unit tests, all passing.
  - D-Bus 相关测试在 `dbus-run-session` + `python3-dbusmock` 夹具中运行。
  - D-Bus tests run inside `dbus-run-session` with `python3-dbusmock` fixtures.
  - Windows 目标通过 `x86_64-pc-windows-gnu` 交叉编译校验（`#![cfg(windows)]` 门控保证 Linux 宿主不受影响）。
  - The Windows target is verified by `x86_64-pc-windows-gnu` cross-compilation, with `#![cfg(windows)]` gating keeping the Linux host unaffected.

---

## [v1-alpha1]

### Features

- feat: 新增 `uda-core` 核心类型与跨平台 Trait（AppearanceManager、WallpaperManager、NotificationManager、WakeLockManager）
- feat: added the `uda-core` types and cross-platform traits (AppearanceManager, WallpaperManager, NotificationManager, WakeLockManager)
- feat: 新增 `uda-platform-linux` Linux 平台实现，覆盖环境检测、外观感知、壁纸管理、系统通知、防休眠锁
- feat: added the `uda-platform-linux` backend covering detection, appearance, wallpaper, notifications and wake locks
- feat: 新增 `uda-cli` 诊断工具，展示环境检测、外观、壁纸、通知、WakeLock 调用示例
- feat: added the `uda-cli` diagnostic tool demonstrating detection, appearance, wallpaper, notification and WakeLock calls
- feat: 新增 Capability 位标志体系（DETECT_THEME / SET_WALLPAPER / GET_WALLPAPER / READ_ACCENT_COLOR / FOLLOW_SYSTEM_THEME / SEND_NOTIFICATION / WAKE_LOCK）
- feat: added the capability bit-flag set (DETECT_THEME / SET_WALLPAPER / GET_WALLPAPER / READ_ACCENT_COLOR / FOLLOW_SYSTEM_THEME / SEND_NOTIFICATION / WAKE_LOCK)

[Unreleased]: https://github.com/UniDesktop/SDK/compare/v0.2.0...HEAD
[v0.2.0]: https://github.com/UniDesktop/SDK/releases/tag/v0.2.0
[v1-alpha1]: https://github.com/UniDesktop/SDK/releases/tag/v1-alpha1
