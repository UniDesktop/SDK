# Changelog

本文件记录本项目的全部值得注意的变更。
This file records every notable change to this project.

格式基于 [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)，且本项目遵循 [语义化版本](https://semver.org/spec/v2.0.0.html)。
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> 完整文档见 [unidesktop.github.io](https://unidesktop.github.io/)。
> Full documentation lives at [unidesktop.github.io](https://unidesktop.github.io/).

## [v0.2.1] - Honest Theme Reporting & Reasoned Degradation · 2026-10-06

### ✨ Added

- ✨ **`Theme::Unknown` 变体**：无法判定系统深浅色时有了一等答案，与 `Theme::Auto`（系统自行切换）区分开。
- ✨ **A `Theme::Unknown` variant**: an undeterminable colour scheme now has a first-class answer, distinct from `Theme::Auto` (the system switches itself).
  - C-ABI 侧映射到既有的 `UDA_THEME_UNKNOWN`（`0`），`include/uda.h` 与各语言绑定无需改动即可暴露该值。
  - The C ABI maps it onto the existing `UDA_THEME_UNKNOWN` (`0`), so `include/uda.h` and the language bindings expose it unchanged.
  - Python / Node.js 侧 `'unknown'` 由此从几乎不可达变为常见返回值。
  - `'unknown'` in the Python / Node.js SDKs goes from nearly unreachable to a routine answer.

- ✨ **`SupportLevel::Partial` 携带原因字符串**：降级不再只说"受限"，而是同时给出为什么。
- ✨ **`SupportLevel::Partial` now carries a reason**: a degradation no longer says only "limited" but also why.
  - 新增 `SupportLevel::reason()` 返回 `Option<&str>`，宿主可直接把原因展示给用户。
  - A new `SupportLevel::reason()` returns `Option<&str>`, so a host can show the reason to the user verbatim.
  - 原因参与相等性比较：两种不同降级不会被误判为同一种。
  - The reason participates in equality, so two different degradations cannot be mistaken for the same one.

### 🛠️ Changed

- 🛠️ **Linux 主题检测兜底由 `Theme::Light` 改为 `Theme::Unknown`**：在没有任何桌面组件发布配色方案的环境（Hyprland / Sway / 通用 X11）中，后端不再伪装知道答案。
- 🛠️ **The Linux theme-detection fallback is now `Theme::Unknown` instead of `Theme::Light`**: on a desktop that publishes no colour scheme at all (Hyprland / Sway / generic X11), the backend stops pretending it knows the answer.
  - 原先返回 `Light` 会让宿主 UI 在平铺窗口管理器上显示一个从未被确认的状态。
  - Returning `Light` made a host UI state something that was never confirmed on a tiling window manager.

### 🧪 Testing

- 🧪 **新增 `SupportLevel` 契约测试**：锁定 `Partial` 原因可读回、`Full` / `None` 无原因、以及不同原因互不相等三条不变量。
- 🧪 **Added `SupportLevel` contract tests**: pinning the reason round-trip, the absence of a reason on `Full` / `None`, and the inequality of two different reasons.

### 📦 Metadata

- 📦 **补齐 crate 版本号**：五个 crate（`uda-core`、`uda-platform-linux`、`uda-platform-windows`、`uda-ffi`、`uda-cli`）与 `Cargo.lock` 由停留在 `0.1.0` 更新为 `0.2.1`；v0.2.0 发布时遗漏了这一步。
- 📦 **Crate versions brought in line**: the five crates and `Cargo.lock` move from the stale `0.1.0` to `0.2.1` — the step missed when v0.2.0 shipped.
- 📦 **示例包版本号同步跟进**：`examples/nodejs` 的 `package.json` 与 `package-lock.json` 更新为 `0.2.1`。
- 📦 **Example packages follow the release**: `examples/nodejs` `package.json` and `package-lock.json` move to 0.2.1.
- 📦 **示例代码无需调整**：本次为修复性补丁，未新增或改动任何 API，现有示例已覆盖 SDK 全部功能。
- 📦 **No example changes needed**: this is a patch release with no API additions or changes, and the existing examples already cover every capability.

---

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

[Unreleased]: https://github.com/UniDesktop/SDK/compare/v0.2.1...HEAD
[v0.2.1]: https://github.com/UniDesktop/SDK/releases/tag/v0.2.1
[v0.2.0]: https://github.com/UniDesktop/SDK/releases/tag/v0.2.0
[v1-alpha1]: https://github.com/UniDesktop/SDK/releases/tag/v1-alpha1
