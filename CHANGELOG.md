# Changelog

本文件记录本项目的全部值得注意的变更。
This file records every notable change to this project.

格式基于 [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)，且本项目遵循 [语义化版本](https://semver.org/spec/v2.0.0.html)。
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> 完整文档见 [unidesktop.github.io](https://unidesktop.github.io/)。
> Full documentation lives at [unidesktop.github.io](https://unidesktop.github.io/).

## [Unreleased] - Cross-Review Hardening · 未发布

> 对应 PR：交叉审查清单（issue #4，50 项）中 v0.2.1 未覆盖的 48 项全部在本节落地。
> Corresponds to the cross-review checklist (issue #4): the 48 findings not already
> shipped in v0.2.1 all land in this section.

### ✨ Added

- ✨ **`uda_tray_capabilities` C 导出**：托盘能力位首次对 C / Python / Node 宿主可见，新增 8 个 `UDA_TRAY_CAP_*` 常量（append-only，位与核心 `Capability` 一一对应），宿主可以按 AGENTS.md 的要求"先查能力再降级"。
- ✨ **A `uda_tray_capabilities` C export**: tray capability bits become visible to C / Python / Node hosts for the first time, with eight new `UDA_TRAY_CAP_*` constants (append-only, mapping 1:1 onto the core `Capability` bits), so hosts can follow "query capabilities, then degrade".

### 🛠️ Changed

- 🛠️ **`UdaError` 新增 `InvalidArgument` 变体并映射 `UDA_ERR_INVALID_ARGUMENT`（`-1`）**：空标签、非法图标等调用方输入错误不再伪装成"平台不支持"（`-2`），`include/uda.h` 与绑定文档同步。
- 🛠️ **`UdaError` gains an `InvalidArgument` variant mapped to `UDA_ERR_INVALID_ARGUMENT` (`-1`)**: caller mistakes such as an empty label or an invalid icon no longer masquerade as "platform not supported" (`-2`); `include/uda.h` and the binding docs say the same.
- 🛠️ **`Theme`、`SupportLevel`、`UdaError` 标记 `#[non_exhaustive]`**：这三个枚举在补丁版本中就可能新增变体（`Unknown`、`Partial(reason)`、`InvalidArgument` 均是先例）；下游的穷尽 `match` 需要通配臂，新增变体不再构成编译期破坏。crate 内所有跨包 `match` 已补通配兜底。
- 🛠️ **`Theme`, `SupportLevel` and `UdaError` are now `#[non_exhaustive]`**: these enums gain variants in patch releases (`Unknown`, `Partial(reason)` and `InvalidArgument` are all precedents); downstream exhaustive `match`es need a wildcard arm, so future additions stop being compile-time breaks. Every cross-crate `match` in this workspace carries the wildcard now.
- 🛠️ **关于 FFI 导出的 `unsafe extern "C"`（兼容性说明）**：导出函数的 Rust 定义侧自 v0.2.1 起为 `unsafe extern "C"`，本节延续该状态（新增的 `uda_tray_capabilities` 同样如此）。对 C 调用方与声明 `extern "C" { ... }` 的 Rust 宿主（2021 及更早 edition）没有影响；edition 2024 的 Rust 宿主需用 `unsafe extern` 块声明。
- 🛠️ **On the `unsafe extern "C"` FFI exports (compatibility note)**: the Rust definition side has been `unsafe extern "C"` since v0.2.1 and this release keeps that state (the new `uda_tray_capabilities` included). C callers and Rust hosts declaring `extern "C" { ... }` blocks (edition 2021 and earlier) are unaffected; edition-2024 Rust hosts must declare with `unsafe extern` blocks.
- 🛠️ **同步 API 桥的构建顺序**：`uda-platform-linux` 的 `run_async` 与 `uda-ffi` 的 `run_sync` 现在都是"先判定运行时上下文、后构建"，环境运行时分支不再把runtime 搬去工作线程。
- 🛠️ **Sync-bridge build order**: `run_async` (`uda-platform-linux`) and `run_sync` (`uda-ffi`) both branch on the ambient runtime *before* building anything, so the nested branch no longer ships a runtime across threads.

### 🧰 Fixed

- 🧰 **Windows 托盘七项**：`hide()`→`show()` 往返失效（`dwStateMask=0`）；GDI 位图全路径不释放（含 `CreateIconIndirect` 前后、mask 失败早退）；模态菜单泵重入窗口过程造成同一 `Worker` 两个 `&mut`（别名 UB，菜单流程拆为两段并改为 per-worker 重入守卫）；`NIM_MODIFY` 失败泄漏新 HICON；更新失败被记为已应用后永不重试；`AppendMenuW` 结果一律丢弃（子菜单失败泄漏 HMENU）；explorer 重启后图标永久消失（改隐藏顶层窗口 + `TaskbarCreated` 重加）。
- 🧰 **Seven Windows tray fixes**: `hide()`→`show()` never restoring (`dwStateMask=0`); GDI bitmaps leaked on every path (around `CreateIconIndirect` and the mask-failure early exit); the modal menu pump re-entering the window proc with a second `&mut Worker` (aliasing UB — the menu flow is now two-phase with a per-worker reentrancy guard); a leaked HICON when `NIM_MODIFY` fails; updates recorded as applied without being so and never retried; `AppendMenuW` results discarded wholesale (leaking the submenu HMENU); and the icon gone forever after an explorer restart (now a hidden top-level window that re-registers on `TaskbarCreated`).
- 🧰 **Windows 托盘窗口不再幽灵化**：工作窗口改 `WS_POPUP` + `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`，不再出现在 Alt+Tab、任务管理器与 `EnumWindows` 中；`WM_NCDESTROY` 现在清 `GWLP_USERDATA` 并 `PostQuitMessage`，外部销毁路径不再悬空解引用。
- 🧰 **The Windows worker window stops being a ghost**: it is now `WS_POPUP` with `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`, invisible to Alt+Tab, Task Manager and `EnumWindows`; `WM_NCDESTROY` clears `GWLP_USERDATA` and posts a quit message, so an externally destroyed window can no longer leave a dangling pointer behind.
- 🧰 **Windows AUMID 两项**：提前返回泄漏 `CoTaskMem`；`availability()` 用空 app_name 抢注进程身份导致通知归属分叉。
- 🧰 **Two Windows AUMID fixes**: the early-return path leaked `CoTaskMem`, and `availability()` claimed the process identity with an empty app_name, splitting notification ownership.
- 🧰 **Windows 通知补齐**：`NIF_SHOWTIP`（v4 下标准 tooltip 被抑制）、`NIN_SELECT`/`NIN_KEYSELECT` 键盘激活、`WM_CONTEXTMENU` 锚点语义、`SetTimer` / `SetForegroundWindow` 结果检查。
- 🧰 **Windows notifications completed**: `NIF_SHOWTIP` (v4 suppressed the standard tooltip), `NIN_SELECT`/`NIN_KEYSELECT` keyboard activation, `WM_CONTEXTMENU` anchor semantics, and `SetTimer` / `SetForegroundWindow` result checks.
- 🧰 **Linux 托盘六项**：dbusmenu 信号从不发射 + `Arc::as_ptr` 变更检测失效（现按内容指纹发射真实 `LayoutUpdated`，并补规范要求的 `NewToolTip`）；`GetLayout` 忽略 `parent_id` 且曲解 `recursionDepth`；dbusmenu id 按遍历位置重排（现持久稳定映射，退役 id 不复用）；worker 初始化失败仍宣称全套能力（现就绪握手，失败返回错误）；未注册双击回调时第二次点击被吞（现回退 `on_click`）；200ms 每 tick 无条件重转码图标。
- 🧰 **Six Linux tray fixes**: dbusmenu signals never emitted plus a blind `Arc::as_ptr` change detector (now a content fingerprint with a real `LayoutUpdated`, plus the spec-mandated `NewToolTip`); `GetLayout` ignoring `parent_id` and misreading `recursionDepth`; dbusmenu ids re-shuffled per snapshot (now a persistent stable map whose retired ids are never reused); a failed worker init still advertising full capabilities (now a readiness handshake that fails `create()`); the second click swallowed when no double-click handler exists (now falls back to `on_click`); and an unconditional per-tick icon transcode.
- 🧰 **Linux 平台四项**：Hyprland 壁纸链路不可用（hyprpaper 改走 `hyprctl`、swww 守护进程探测/拉起、全程超时、失败上报）；同步 API 内联 `block_on` 在 tokio 上下文 panic（现统一 `try_current` 分流桥）；logout Tier-3 回退服务名/参数签名错误；GNOME 47+ 强调色枚举解析恒失败（映射 HIG 调色板）。
- 🧰 **Four Linux platform fixes**: an unusable Hyprland wallpaper chain (hyprpaper now driven via `hyprctl`, swww daemon probed/spawned, everything under timeouts, failures reported); sync APIs panicking under an ambient tokio runtime (now one `try_current`-based bridge); logout Tier-3 fallback with wrong service names and argument signatures; and GNOME 47+ accent-colour enums never parsing (now mapped onto the HIG palette).
- 🧰 **FFI 契约四项**：`uda_tray_menu_destroy` 后点击触发已释放回调（先从所有存活图标解挂再删记录，`set_menu` 单锁线性化）；`set_action` 失败仍返回 `UDA_OK`；复选框蹦床 `Arc` 自引用环（改 `Weak`）；菜单行 id 用 `entries().last()` 反推（现直接使用 `push` 返回值）。
- 🧰 **Four FFI contract fixes**: clicks reaching freed callbacks after `uda_tray_menu_destroy` (the menu is detached from every live icon before the record goes, with `set_menu` linearised under one lock); `set_action` failures still reporting `UDA_OK`; the checkbox trampoline's `Arc` self-reference cycle (now `Weak`); and menu row ids reverse-engineered from `entries().last()` (now taken straight from `push`).
- 🧰 **杂项**：空壁纸路径未被拒绝；空字符串字段返回非 NULL 空串；Tier-3 壁纸探测不区分桌面环境；dbusmenu 根节点 `GetProperty(0)` 恒 `InvalidArgs` 且缺标准属性；点击回调 panic 后 `on_click` 永久失效；wakelock `expires_at` 死状态无回收；D-Bus 调用无超时上限（5 个模块）；`GET_WALLPAPER` 能力位与实现错位；`uda-cli` 裸 unwrap / 硬编码路径 / 破坏性副作用；Node `new Uda(path)` 参数被丢弃、`_readWallpaper` 双调用、行添加失败泄漏 koffi 槽位；Python 兜底诊断写死状态码 0、非 NULL 空串跳过释放。
- 🧰 **Assorted**: empty wallpaper paths not rejected; empty string fields returning non-NULL pointers; the Tier-3 wallpaper probe ignoring the desktop environment; `GetProperty(0)` always `InvalidArgs` with standard root properties missing; `on_click` gone for good after a panicking handler; the dead `expires_at` with no reaper; missing D-Bus timeouts across five modules; the `GET_WALLPAPER` capability/implementation mismatch; `uda-cli`'s bare unwraps, hardcoded path and destructive side effects; Node dropping the `new Uda(path)` argument, double-reading the wallpaper and leaking koffi slots on failed row adds; Python reporting status code 0 in fallback diagnostics and skipping the release of non-NULL empty strings.

---

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
- 📦 **示例代码无需调整（仅就 v0.2.1 自身而言）**：v0.2.1 为修复性补丁，未新增或改动任何 API；后续版本对 API 的变更见各自条目。
- 📦 **No example changes needed (scoped to v0.2.1 itself)**: v0.2.1 is a patch release with no API additions or changes; API changes made by later releases are recorded under their own entries.

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
