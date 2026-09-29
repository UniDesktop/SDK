# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [v0.2.0] - Interactive Shell & System Integration

### ✨ Added

- ✨ **系统托盘模块 (`tray`)**：提供跨平台菜单数据模型（文本项、复选框、分隔线、子菜单、禁用态）。
  - Linux：基于 SNI（`org.kde.StatusNotifierItem` + `com.canonical.dbusmenu`）实现。
  - Windows：基于 `Shell_NotifyIconW` 与隐藏消息窗口 + 独立工作线程的消息泵实现，宿主事件循环永不被劫持。
  - 支持图标下采样与通道对齐，`TrayIcon` 实现 `Drop` 以在退出时自动从托盘注销。

- ✨ **媒体播控中心 (`media`)**：Linux 端基于 MPRIS v2，Windows 端基于 WinRT SMTC。
  - 读取当前播放曲目：标题、艺人、专辑、时长。
  - 提供播放 / 暂停 / 停止 / 上一首 / 下一歌控制，以及播放状态查询。

- ✨ **会话与电源生命周期 (`session`)**：一行代码实现跨平台
  - 锁屏（Lock）、注销（Logout）、睡眠（Suspend）、休眠（Hibernate）、重启（Reboot）、关机（Shutdown）。
  - Windows 端内置安全的 Token 提权机制（`SeShutdownPrivilege`），提权失败映射为类型化错误而非 panic。
  - 只读动作（Lock）与破坏性动作在能力位层面区分，调用方可先行查询再决定是否展示菜单项。

- ✨ **`Notification::app_icon` 在 Windows 端落地**：图标经 `file://` URI 规范化后写入 toast 模板的 `<image>` 节点；无图标时降级到无图模板。

### 🛠️ Changed

- 🛠️ **全线重构模块化单功能 Demo**：`examples/` 重组为 `01_appearance` 至 `07_session`，Rust / Python / Node.js 三语言完全对齐，每个示例只演示一项能力且默认无破坏性副作用。

### 🧪 Testing

- 🧪 **自动化测试扩充**：全工作区单元测试突破至 186+ 项，并保持全绿通过。
  - D-Bus 相关测试在 `dbus-run-session` + `python3-dbusmock` 夹具中运行。
  - Windows 目标通过 `x86_64-pc-windows-gnu` 交叉编译校验（`#![cfg(windows)]` 门控保证 Linux 宿主不受影响）。

---

## [v1-alpha1]

### Features
- feat: 新增 `uda-core` 核心类型与跨平台 Trait（AppearanceManager、WallpaperManager、NotificationManager、WakeLockManager）
- feat: 新增 `uda-platform-linux` Linux 平台实现，覆盖环境检测、外观感知、壁纸管理、系统通知、防休眠锁
- feat: 新增 `uda-cli` 诊断工具，展示环境检测、外观、壁纸、通知、WakeLock 调用示例
- feat: 新增 Capability 位标志体系（DETECT_THEME / SET_WALLPAPER / GET_WALLPAPER / READ_ACCENT_COLOR / FOLLOW_SYSTEM_THEME / SEND_NOTIFICATION / WAKE_LOCK）
