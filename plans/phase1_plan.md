# Phase 1 实施计划：UDA MVP Core Foundation

> **进度索引**
> - §1–§3：Phase 1 Linux 侧实施计划（历史记录，已完成）
> - §3 Step 9–11：Windows 平台对齐实施记录（2026-09-21，已完成）
> - §3 Step 12：Code Review 缺陷修复记录（2026-09-21，已完成）
> - §3 Step 13：C-ABI 导出层与多语言调用示例（2026-09-21，已完成）
> - §7：当前进度总览与测试基线

## 1. 总体目标
在 `uda-core` 与 `uda-platform-linux` 中建立 Phase 1 基础类型、Trait 与 Linux 初版实现，确保全工作区可编译、零警告，且符合协议字典与 AGENTS.md 约束。

**扩展目标（Windows 接管阶段新增）：** 在 `uda-platform-windows` 中完成与 Linux 平台完全对齐的核心模块实现，确保 Linux 宿主机上的跨平台编译卫生（`#![cfg(windows)]` 全局守卫）不被破坏。

## 2. 关键决策（已完成）

### 2.1 SupportLevel 命名
**决策：** 采用用户显式指定的 `None` / `Partial` / `Full`。
**理由：** 用户步骤 2 明确要求严格对齐其任务描述中的协议字典变体；`None` 在此处语义为“不支持”，虽与 `Option` 同名但在当前枚举上下文可接受。

### 2.2 深浅色检测回退链
**决策：** 首版实现按 **Portal 优先 + CLI 回退链** 执行：
1. **XDG Desktop Portal**（`zbus` + `org.freedesktop.portal.Settings.Read("org.freedesktop.appearance", "color-scheme")`）
2. **GNOME**（`gsettings`）
3. **KDE Plasma**（`kreadconfig5`）
4. **XFCE**（`xfconf-query`）
5. **默认**：`Theme::Light`
**理由：** 代码审查要求必须遵循 AGENTS.md 的 Portal-first 架构；当 Portal 不可用或调用失败时，再降级到 CLI 回退链。

### 2.3 Capability 类型
**决策：** 使用 `bitflags` 定义 `Capability`，支持多能力组合查询。
**理由：** `capabilities()` 需返回多项能力，位标志是最自然的表达。

### 2.4 错误处理
**决策：** 在 `UdaError` 中新增 `NotSupported`、`DetectionFailed(String)`、`CommandFailed(String)`、`Io(std::io::Error)`、`Internal(String)`，并使用 `thiserror` 派生 `std::error::Error` 与 `Debug`。
**理由：** 与用户步骤 1 完全对齐；`Io` 用 `#[from]` 自动转换；Portal 路径增加 `DetectionFailed` 承载 zbus 错误。

### 2.5 跨平台隔离
**决策：** `uda-core` 仅使用标准库与 `thiserror`/`serde`；`uda-platform-linux` 依赖 `uda-core`，并在需要时使用 `std::process::Command` 调用 CLI 工具（避免 Phase 1 引入 D-Bus 复杂性）。
**理由：** 符合 Zero-Bloat 哲学，降低 Phase 1 依赖。

## 3. 分步实施计划

### Step 1：确认约束与架构边界
- [x] 阅读 AGENTS.md、appearance_specs.md、wallpaper_specs.md
- [x] 阅读现有 Cargo.toml 与 lib.rs
- [x] 明确命名冲突与回退链决策（见 2.1、2.2）

### Step 2：实现 `uda-core` 核心类型
**文件：**
- `crates/uda-core/src/error.rs`
- `crates/uda-core/src/capability.rs`
- `crates/uda-core/src/appearance.rs`
- `crates/uda-core/src/wallpaper.rs`
- `crates/uda-core/src/notification.rs`
- `crates/uda-core/src/lib.rs`

**内容：**
- `UdaError` 枚举（`NotSupported`、`DetectionFailed(String)`、`CommandFailed(String)`、`Io(#[from] std::io::Error)`、`Internal(String)`）
- `SupportLevel` 枚举（`None`、`Partial`、`Full`）
- `Capability` bitflags（`DetectTheme`、`SetWallpaper`、`GetWallpaper`、`ReadAccentColor`、`FollowSystemTheme`、`SendNotification`、`WakeLock`）
- `Theme` 枚举（`Light`、`Dark`、`Auto`）
- `RgbaColor` 结构体（`r, g, b, a: u8`）
- `FillMode` 枚举（`Crop`、`Fill`、`Fit`、`Stretch`）
- `WallpaperOptions` 结构体（`fill_mode`、`monitor_index`、`dark_mode`）
- `AppearanceManager` trait（`detect_theme`、`get_accent_color`、`capabilities`）
- `WallpaperManager` trait（`set_wallpaper`、`get_wallpaper`、`capabilities`）
- `Notification` 结构体（`app_name`、`replaces_id`、`app_icon`、`summary`、`body`、`actions`、`expire_timeout`）
- `Urgency` 枚举（`Low`、`Normal`、`Critical`）
- `NotificationManager` trait（`send`、`capabilities`）
- `WakeLockType` 枚举（`PreventDisplaySleep`、`PreventSystemIdle`）
- `WakeLockGuard` 结构体（RAII，`Drop` 时自动释放）
- `WakeLockManager` trait（`acquire`、`capabilities`）

**验证：**
```bash
cargo check -p uda-core
```

### Step 3：实现 Linux 环境检测
**文件：**
- `crates/uda-platform-linux/src/detection.rs`
- `crates/uda-platform-linux/src/lib.rs`

**内容：**
- `DesktopEnvironment` 枚举（`Unknown`、`Gnome`、`Kde`、`Xfce`），并实现 `Default`
- `EnvironmentInfo` 结构体（`desktop_environment`、`xdg_current_desktop`、`xdg_session_type`、`desktop_session`、`wayland_display`、`display`）
- `detect_environment() -> Result<EnvironmentInfo, UdaError>`：按优先级读取环境变量，并通过 `DesktopEnvironment::infer()` 推断桌面环境
- 单元测试覆盖 GNOME/KDE/Xorg/Wayland/Unknown 场景

**验证：**
```bash
cargo check -p uda-platform-linux
```

### Step 4：实现 Linux 深浅色外观感知（Portal 优先）
**文件：**
- `crates/uda-platform-linux/src/appearance.rs`
- `crates/uda-platform-linux/src/lib.rs`

**内容：**
- `LinuxAppearanceManager` 结构体
- 实现 `AppearanceManager` trait：
  - `detect_theme()`：Portal-first + CLI 回退链
    - Portal: `zbus::Connection::session()` + `zbus::Proxy::new(...)` + `proxy.call("Read", &("org.freedesktop.appearance", "color-scheme"))`
    - GNOME: `gsettings get org.gnome.desktop.interface color-scheme`（返回 0/1/2，或尝试 `gtk-theme` 推导 `-dark` 后缀）
    - KDE: `kreadconfig5 --file kdeglobals --group General --key ColorScheme` 或解析 `~/.config/kdeglobals`
    - XFCE: `xfconf-query -c xfce4-desktop -p /backdrop/screen0/mode` 或解析 `xfce4-desktop.xml`（实际检测 XFCE 较复杂，Phase 1 可先尝试 `xfconf-query` 获取主题相关键，失败则降级）
  - `get_accent_color()`：GNOME `gsettings get org.gnome.desktop.interface accent-color`（返回 `'rgb(26, 116, 184)'`），解析失败返回 `UdaError::NotSupported`
  - `capabilities()`：返回 `Capability::all()` 或受限集合
- 错误处理：zbus 错误映射为 `UdaError::DetectionFailed`

**验证：**
```bash
cargo check -p uda-platform-linux
```

### Step 5：实现 Linux 壁纸管理器（Wallpaper Module）
**文件：**
- `crates/uda-core/src/wallpaper.rs`
- `crates/uda-platform-linux/src/wallpaper.rs`
- `crates/uda-platform-linux/src/lib.rs`
- `crates/uda-cli/src/main.rs`

**内容：**
- `uda-core` 层扩展：
  - `FillMode` 枚举（`Crop`、`Fill`、`Fit`、`Stretch`）
  - `WallpaperOptions` 结构体（`fill_mode`、`monitor_index`、`dark_mode`）
  - `WallpaperManager` Trait 更新为 `set_wallpaper(path: &str, options: &WallpaperOptions) -> Result<(), UdaError>` 与 `get_wallpaper() -> Result<Option<String>, UdaError>`
- `uda-platform-linux` 层实现 `LinuxWallpaperManager`：
  - GNOME：`gsettings` CLI 写 `org.gnome.desktop.background` 的 `picture-uri` / `picture-uri-dark`，路径经 `file://` URI 转换（含 percent-encoding）
  - KDE：`zbus` 调 `org.kde.plasmashell.evaluateScript`，按 `FillMode` 映射脚本参数
  - Hyprland/Sway：检测并调用 `hyprpaper` 或 `swww`，支持 `monitor_index`
  - X11：`feh` → `nitrogen` 回退，按 `FillMode` 映射参数
- 单元测试覆盖：`file_uri` percent-encoding（空格 `%20`、`%`/`#`/`?`）/中文/已有 `file://` 前缀、KDE FillMode 映射、Hyprland 多屏、swww output 参数、X11（feh/nitrogen）回退优先级与 `--head` 拆参等纯逻辑场景
- CLI 示例：`crates/uda-cli/src/main.rs` 补充壁纸设置示例

**验证：**
```bash
cargo check --workspace
./scripts/test-linux-mock.sh
```

### Step 6：实现 Linux 通知管理器（Notification Module）
**文件：**
- `crates/uda-core/src/notification.rs`
- `crates/uda-platform-linux/src/notification.rs`
- `crates/uda-platform-linux/src/lib.rs`
- `crates/uda-cli/src/main.rs`

**内容：**
- `uda-core` 层：
  - `Notification` 结构体（`app_name`、`replaces_id`、`app_icon`、`summary`、`body`、`actions`、`expire_timeout`、`urgency`）
  - `Urgency` 枚举（`Low`、`Normal`、`Critical`）
  - `NotificationManager` trait（`send`、`capabilities`），使用 `async-trait`
- `uda-platform-linux` 层实现 `LinuxNotificationManager`：
  - 存储 `Arc<zbus::Connection>` 复用 D-Bus 会话
  - `send()` 调用 `org.freedesktop.Notifications.Notify`
  - `actions` 展平为 `Vec<String>`（FreeDesktop 要求）
  - `urgency` 以 hint `("urgency", Value::from(u8))` 传入；`Low`（0）为服务器默认值，省略 hint
  - `capabilities()` 仅返回 `SEND_NOTIFICATION`（`READ_ACCENT_COLOR` 属于 Appearance 能力，不在通知模块暴露）
- 单元测试覆盖：`actions` 展平逻辑、空 actions、Notification default 值
- CLI 示例：`crates/uda-cli/src/main.rs` 补充通知发送示例

**验证：**
```bash
cargo check --workspace
./scripts/test-linux-mock.sh
```

### Step 7：实现 Linux 防休眠常亮锁（WakeLock Module）
**文件：**
- `crates/uda-core/src/wakelock.rs`
- `crates/uda-platform-linux/src/wakelock.rs`
- `crates/uda-platform-linux/src/lib.rs`
- `crates/uda-cli/src/main.rs`

**内容：**
- `uda-core` 层：
  - `WakeLockType` 枚举（`PreventDisplaySleep`、`PreventSystemIdle`）
  - `WakeLockGuard` 结构体（RAII，`Drop` 时自动释放）
  - `WakeLockManager` trait（`acquire`、`capabilities`），使用 `async-trait`
- `uda-platform-linux` 层实现 `LinuxWakeLockManager`：
  - 存储 `Arc<zbus::Connection>` 复用 D-Bus 会话
  - `acquire()` 调用 `org.freedesktop.ScreenSaver.Inhibit`，保存返回的 `cookie: u32`
  - `WakeLockGuard` 在 `Drop` 时调用 `UnInhibit(cookie)` 释放锁
  - `capabilities()` 返回 `Capability::WAKE_LOCK`
- 单元测试覆盖：Guard drop 释放、手动释放、`WakeLockType` 参数映射
- CLI 示例：`crates/uda-cli/src/main.rs` 补充 WakeLock 持有 2 秒示例

**验证：**
```bash
cargo check --workspace
./scripts/test-linux-mock.sh
```

### Step 8：测试脚本（按 AGENTS.md）
```bash
./scripts/test-linux-mock.sh
```

**验收标准：**
1. **Compilation Check:** `cargo check --workspace` 通过
2. **D-Bus Mock Tests:** `dbus-run-session -- cargo test -p uda-platform-linux -- --nocapture` 通过，且测试数量 >= 5（当前 23 项全部通过）
3. **Cross-Compilation Hygiene:** Linux 代码不破坏 Windows 编译

---

## 3A. Windows 平台对齐实施记录（2026-09-21）

### Step 9：上下文恢复与现状核查

**动作：**
- [x] 完整阅读 `AGENTS.md`、`plans/phase1_plan.md`、`docs/internals/` 全部三份协议字典
- [x] 核对 `uda-core` 契约层：5 变体 `UdaError`、`Capability` bitflags、`SupportLevel`、`Theme`、`RgbaColor`、`FillMode`、`WallpaperOptions`、`WakeLockType`、`Notification`/`Urgency`，4 个 Trait
- [x] 核对 `uda-platform-linux` 五大模块（Detection / Appearance / Wallpaper / Notification / WakeLock）源码完整

**发现（重要）：** Linux 基线测试实际为 **21 通过 + 2 失败**，与既有记录"23 项全部通过"不符。

**根因分析：** `crates/uda-platform-linux/src/detection.rs` 的测试通过 `std::env::set_var` 修改进程级环境变量，而 `cargo test` 默认多线程并行执行，导致测试间互相污染：`env_gnome` 设置的 `DESKTOP_SESSION=gnome` 被 `env_kde` 读取。`--test-threads=1` 下 5 项全部通过，证明**被测生产逻辑无缺陷**，仅测试隔离性不足。

**修复（最小侵入，未改动生产逻辑）：**
- 引入 `static ENV_LOCK: Mutex<()>` 串行化修改环境变量的测试
- 新增 `EnvGuard` RAII 类型，`Drop` 时清理 `XDG_CURRENT_DESKTOP` 等 5 个变量，防止跨测试泄漏
- 按 AGENTS.md Principle 1，将测试内 `.unwrap()` 改为 match + `panic!`
- 锁中毒时通过 `into_inner()` 恢复 guard，避免单测试 panic 级联到其他测试

**验证：** `./scripts/test-linux-mock.sh` → **23 passed / 0 failed**

### Step 10：Windows 五大模块实现

**前置调研：** 从本地 vendored `windows-0.58.0` 源码逐一核对 FFI 精确签名与常量值，避免编译偏差。关键发现：
- `HKEY` 与 `RegCreateKeyExW` 位于 `Win32_Security` feature 之后（`RegOpenKeyExW`/`RegSetValueExW` 则不需要）
- `XmlNodeList` 有 `Item(index)` 但**无** `GetAt` 的泛型版本可直接用
- `CreateToastNotification` 是 `ToastNotification` 的关联函数（WinRT 激活工厂），非 `ToastNotificationManager` 方法
- `SelectNodes` 接收 `&HSTRING`，不接受 `&str`

#### 10.1 外观感知 `crates/uda-platform-windows/src/appearance.rs`
- 实现 `WindowsAppearanceManager`（实现 `AppearanceManager` Trait）
- `RegOpenKeyExW` + `RegQueryValueExW` **两段式缓冲查询**读取 `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize` 的 `AppsUseLightTheme`（0=Dark，1=Light）
- 严格校验：值类型必须为 `REG_DWORD` 且字节长度恰为 4，否则拒绝重解释字节
- `SystemUsesLightTheme`（Win10 1809+）作为降级路径
- 键不存在（`ERROR_FILE_NOT_FOUND`）视为正常条件返回 `Ok(None)`，非错误
- `get_accent_color()` 返回 `NotSupported`（`AccentColor` 布局跨版本不稳定，Phase 1 不猜测）
- 注册表句柄保证只关闭一次，查询失败也必然关闭

#### 10.2 壁纸管理 `crates/uda-platform-windows/src/wallpaper.rs`
- 实现 `WindowsWallpaperManager`（实现 `WallpaperManager` Trait）
- **顺序关键：先写 `WallpaperStyle`/`TileWallpaper` 再调用 `SystemParametersInfoW`**，否则桌面沿用旧样式
- **值类型为 `REG_SZ`**：Explorer 用字符串解析器读取这两个值，写成 `REG_DWORD` 会被完全忽略 → 已废弃 `write_dword`，统一走 `write_string` 封装（`RegSetValueExW` + `REG_SZ`，字节长度含 UTF-16 null 终止符）
- `FillMode` → `(WallpaperStyle, TileWallpaper)` 精确字符串映射（Step 12 Code Review 修正）：

  | `FillMode` | `WallpaperStyle` | `TileWallpaper` | Windows 语义 |
  |------------|------------------|-----------------|--------------|
  | `Fill`     | `"10"`           | `"0"`           | Fill（裁切填满屏幕） |
  | `Crop`     | `"10"`           | `"0"`           | 无原生 Crop，复用 Fill |
  | `Fit`      | `"6"`            | `"0"`           | Fit（等比适配留黑边） |
  | `Stretch`  | `"2"`            | `"0"`           | Stretch（变形铺满） |

  > 历史缺陷：初版将 `Fit` 映射为 `(0, 1)` 平铺，属错误降级——Windows 7+ 原生支持 Fit，禁止再降级为平铺。
- 测试断言精确字符串值与 `REG_SZ` 类型常量，保证 `TileWallpaper` 恒为 `"0"`、`WallpaperStyle` 仅限 `"10"/"6"/"2"/"0"` 四种已知模式
- `SystemParametersInfoW(SPI_SETDESKWALLPAPER, 0, path, SPIF_UPDATEINIFILE | SPIF_SENDCHANGE)`
- `SPI_GETDESKWALLPAPER` 读回路径，缓冲 260 码元（Win32 `MAX_PATH`）
- 路径长度超限与空路径前置校验为 `NotSupported`
- `monitor_index`/`dark_mode` 保留 Trait 参数但记 `log::debug!`（Win32 公开 API 无 per-monitor 壁纸，需私有 `IDesktopWallpaper` COM；亦无独立暗色壁纸槽位）

#### 10.3 常亮锁 `crates/uda-platform-windows/src/wakelock.rs`
- 实现 `WindowsWakeLockManager` 与 Windows 专属 `WindowsWakeLockGuard`
- 获取：`SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED [| ES_SYSTEM_REQUIRED])`
- 释放：`SetThreadExecutionState(ES_CONTINUOUS)`（文档化的"清除全部请求"）
- **关键认知：Win32 无 cookie**（不同于 FreeDesktop `Inhibit` 返回 cookie），且执行状态是线程作用域、非引用计数。释放闭包因此不捕获任何状态，直接交给 core `WakeLockGuard`
- `WindowsWakeLockGuard` 包装 core guard 而非复制释放逻辑，保证 `release()` + `drop()` 只触发一次 FFI 调用
- 提供 `acquire_guard()` 返回带 `is_active()`/`release()` 的 Windows 专属 guard，避免 guard 成为死代码
- 返回值为 0 视为失败（仅在电源通知回调内改状态时发生）→ `Internal`

#### 10.4 环境检测 `crates/uda-platform-windows/src/detection.rs`
- `RtlGetVersion` 取代已弃用的 `GetVersionExW`（后者在无兼容性清单时虚报版本，会使特性门控不可靠）
- `OSVERSIONINFOW` 先零初始化再显式设置 `dwOSVersionInfoSize`
- `WindowsRelease` 家族分级（Legacy / Eight / Ten / Modern）+ `supports_dark_mode_registry()`
- 本地声明最小 `ntdll` 绑定（`windows` crate 刻意不导出该非支持 API）
- 默认 `Legacy`：保守假设最旧系统，特性永不提前被宣告可用

#### 10.5 通知 `crates/uda-platform-windows/src/notification.rs`
- WinRT `ToastNotificationManager::GetTemplateContent(ToastText02)` → XPath 定位 `text[1]`/`text[2]` → `SetInnerText`（自动 XML 转义，引号与换行不会破坏文档）→ `CreateToastNotification` + `Show`
- **错误映射设计（测试驱动修正）：** 初版将 `CreateToastNotifier` 失败一律映射为 `Internal`，测试暴露这在真实 Windows 上错误——非打包进程无 AppUserModelID 时 WinRT 返回 `0x80070490`（`ELEMENT_NOT_FOUND`）。按 AGENTS.md Principle 1，已将该可预期条件专门映射为 `NotSupported`，使调用方可感知并回退
- 常量写为 `0x8007_0490_u32 as i32`，避免手工十进制转换错误（首次实现即因此出错）
- `availability()` 暴露真实可用性：有身份但被禁用 → `DisabledForApplication`；无身份 → `NotSupported`
- 已知限制均记 `log::debug!` 而非静默丢弃：`actions`（toast 按钮需打包应用注册激活处理器）、`replaces_id`（WinRT 不返回通知 ID）、`expire_timeout` 负值、`app_icon`、`urgency` Critical

### Step 12：Code Review 缺陷修复（壁纸协议合规）

**背景：** Step 10–11 提交后 Code Review 驳回，指出 Windows 壁纸实现存在 2 处致命协议缺陷，阻塞合入。

**缺陷 1 — 注册表值类型错误**
- 问题：`WallpaperStyle` / `TileWallpaper` 被写成 `REG_DWORD`，但 Explorer 以字符串解析器读取这两个值，导致填充样式完全失效
- 修复：废弃 `write_dword`，实现 `write_string(key, value_name, value: &str)`：经 `to_wide` 转换为含 null 终止符的 UTF-16 缓冲，`RegSetValueExW` 以 `REG_SZ` 写入，字节长度取 `wide.len() * size_of::<u16>()`
- 全仓库检索确认 `write_dword` 已无调用点，写入路径统一（`appearance.rs` 中的 `REG_DWORD` 为读取路径校验 `AppsUseLightTheme`，与该缺陷无关，保持不变）

**缺陷 2 — FillMode 映射错误**
- 问题：`FillMode::Fit` 被映射为平铺 `(0, 1)`，属错误降级；Windows 7+ 原生支持 Fit 与 Fill
- 修复：`wallpaper_style_values` 返回值改为 `(&'static str, &'static str)`，严格按字符串映射（见 §10.2 表格）

**测试修正**
- 移除断言平铺的错误用例 `wallpaper_style_fit_maps_to_tiled` 与旧的 `(2, 0)` 数值断言
- 新增：`wallpaper_style_fill_maps_to_fill_ten`、`wallpaper_style_fit_maps_to_fit_six`、`wallpaper_style_stretch_maps_to_stretch_two`、`wallpaper_style_crop_reuses_fill`、`wallpaper_style_tile_wallpaper_is_always_zero`、`wallpaper_style_values_are_decimal_strings`、`wallpaper_style_values_are_known_windows_modes`、`write_string_encodes_reg_sz_with_null_terminator`、`registry_string_values_round_trip_through_utf16`、`registry_value_type_is_reg_sz`、`value_names_match_specification`
- 修复过程中暴露 1 个编译警告（测试中未使用的 `tile` 绑定），已改为 `_tile`

**验收结果：** `cargo test -p uda-platform-windows --target x86_64-pc-windows-gnu` → **51 passed / 0 failed**；`./scripts/test-linux-mock.sh` → **23 passed / 0 failed**；`cargo check --workspace --all-targets` 零警告。

### Step 13：C-ABI 导出层与多语言调用示例（Phase 1 收官）

**目标：** 把 crate 内的 Rust API 以稳定 C-ABI 导出为 `libuda_ffi.so` / `uda_ffi.dll`，并提供 Python 与 Node.js 的开箱即用示例。

**导出设计（`crates/uda-ffi/src/`）：**
- `lib.rs`：8 个 `#[no_mangle] pub extern "C"` 导出，全部走 [`catch_boundary`] 包裹
  - `uda_detect_theme(*mut c_int)` → 写入 0/1/2
  - `uda_set_wallpaper(*const c_char, c_int)`
  - `uda_get_wallpaper(*mut *mut c_char)` → 库分配字符串，调用方用 `uda_free_string` 释放
  - `uda_free_string(*mut c_char)`（NULL 安全）
  - `uda_wakelock_acquire(c_int, *const c_char, *mut u64)`
  - `uda_wakelock_release(u64)` → 句柄单次有效，重复释放返回 `-1`
  - `uda_last_error_message()`（thread-local 诊断，超出任务清单但为可诊断性必需）
  - `uda_status_message(c_int)`（静态状态码描述）
- `error.rs`：`UdaStatus` 常量 + `Failure`（区分"调用方违约"与"平台错误"）+ `UdaError` → 状态码映射 + 静态消息表
- `util.rs`：`owned_string_from`（空指针检查 + UTF-8 校验 + 只读到 null 终止符）、`c_string_from`/`free_c_string`（`CString::into_raw`/`from_raw` 配对）、`catch_boundary`（`catch_unwind` + `AssertUnwindSafe`）
- `dispatch.rs`：`cfg(target_os)` 选择平台后端；对 Linux 补齐 **Tier 3 CLI 回退**（`gsettings` 读壁纸）
- `wakelocks.rs`：`Mutex<HashMap<u64, LockEntry>>` 句柄注册表；RAII guard 无法跨 FFI 表达，故由注册表持有 guard，`release` 时取出并 drop 以触发释放闭包；CLI tier 持有一个 `systemd-inhibit` 子进程，释放即 `kill` + `wait` 回收

**安全红线落实：**
- 全部导出函数体内置于 `catch_boundary`，panic 无法越过 `extern "C"` 边界（否则是 UB）
- 所有出参指针先 `is_null()` 检查再解引用；字符串转换不假设长度，读到 null 即止
- 生产代码 0 处 `unwrap()`/`expect()`/`panic!`；10 处 unsafe 块全部带紧邻安全说明
- `uda_status_message` 曾返回裸 `&str` 指针（无 null 结尾，C 侧越界读 `.rodata`），改为缓存 `CString` 后修复

**多语言示例：**
- `examples/python/uda.py`：纯 `ctypes` 封装，显式声明 `argtypes`/`restype`；`UDA_LIBRARY` 环境变量优先，其次 `cargo metadata` 报告的真实 target 目录，再尝试仓库常见构建目录，最后系统搜索路径
- `examples/python/demo.py`：主题检测 → 壁纸读取 → 2 秒常亮锁
- `examples/nodejs/demo.js`：`koffi` 零编译绑定 + `package.json`

**示例层加固（Step 13 复跑时发现并修复）：**
- **动态库定位缺陷**：两个示例原本硬编码 `../../target/{debug,release}/`，而本机 `cargo metadata` 报告 `target_directory = /mnt/d/uda_cache/target`（外置缓存），导致示例找不到已构建的库。现改为优先向 `cargo metadata` 询问真实 target 目录；Node 侧额外推导 `<target>/<triple>/<profile>` 以覆盖 WSL 交叉编译场景
- **koffi 3.x 出参写法（实测结论，曾导致进程直接崩溃）**：
  - `koffi.as(v, koffi.types.int32)` → 只接受 pointer/string 类型，报错
  - `koffi.out()/inout()` → 同样只接受指针类型
  - `koffi.pointer(int32)` → 返回类型描述符而非指针值
  - **正确写法**：`koffi.alloc(类型, 1)` 得到可写的 BigInt 地址，调用后用 `koffi.decode(地址, 类型)` 读回
  - `koffi.types.void` 的 size 为 0，`alloc` 拒绝分配；`char **` 出参需用 `koffi.pointer(koffi.types.char/void)`
- **`char **out` 必须调用两次（关键内存安全结论）**：按 `char*` 读回时 koffi 会立即解码为 JS 字符串、原始地址丢失，无法释放；按 `void*` 读回地址再 `koffi.decode(address, 'char *')` 会直接让进程崩溃（无 JS 异常）。因此 `getWallpaper` 采用「第一次取字符串 + 第二次取地址并 `uda_free_string`」的策略；由于库每次都新分配缓冲区，释放第二次返回的地址是正确且必要的。实测连续 200 轮读取+释放无崩溃
- **Windows DLL 导出验证**：`x86_64-w64-mingw32-nm` 对 Rust 产出的 DLL 报「无符号」，故新增 `scripts/pe_exports.py` 直接解析 PE 导出目录，确认 8 个 `uda_*` 符号全部导出

**双平台实跑结果：**

| 平台 | 命令 | 结果 |
| |------|------|
| Linux (WSL) | `python3 examples/python/demo.py` | ✅ 主题浅色、壁纸 `/usr/share/backgrounds/gnome/adwaita-l.jpg`、句柄 1 获取/释放 |
| Windows | `node.exe examples/nodejs/demo.js` + `uda_ffi.dll` | ✅ 主题深色、壁纸 `c:\users\administrator\downloads\20260204-120321.png`、句柄 1 获取/释放 |

### FFI 阶段新建文件（Step 13）
路径 | 说明 |
|------|------|
`crates/uda-ffi/src/lib.rs` | 8 个 `#[no_mangle] extern "C"` 导出 + 完整 ABI 文档 |
`crates/uda-ffi/src/error.rs` | `UdaStatus` 常量、`Failure`、`UdaError` → 状态码映射、`status_message` |
`crates/uda-ffi/src/util.rs` | C 字符串安全转换、`catch_boundary` panic  containment、thread-local 错误槽 |
`crates/uda-ffi/src/dispatch.rs` | 平台后端选择 + Linux Tier-3 CLI 回退 |
`crates/uda-ffi/src/wakelocks.rs` | 句柄注册表 + 原生/CLI 两种释放策略 |
`include/uda.h` | 标准 C 头文件（常量、原型、内存所有权、线程安全、ABI 稳定性） |
`examples/python/uda.py` | 纯 `ctypes` 封装（无第三方依赖） |
`examples/python/demo.py` | 主题/壁纸/常亮锁演示 |
`examples/nodejs/demo.js` | `koffi` 零编译绑定演示 |
`examples/nodejs/package.json` | Node 示例依赖清单 |
`scripts/pe_exports.py` | 解析 PE 导出目录，交叉验证 Windows DLL 的 `uda_*` 符号 |

### Step 11：跨平台编译保证与验证

**硬性约束落实：**
- `crates/uda-platform-windows/src/lib.rs` 保留 `#![cfg(windows)]`，且置于模块列表**之前**，确保任何模块声明都不会在非 Windows 目标被编译
- 新增 `#![deny(unsafe_op_in_unsafe_fn)]`
- `Cargo.toml` 的 `windows` 依赖保持 `[target.'cfg(windows)'.dependencies]` 作用域
- 补充 `async-trait.workspace = true` 与 `Win32_Security`、`Win32_System_SystemInformation` feature

**验证结果：**

| 验证项 | 命令 | 结果 |
|--------|------|------|
| Windows 单元测试 | `cargo test -p uda-platform-windows --target x86_64-pc-windows-gnu` | **51 passed / 0 failed**（Step 12 修复后由 45 增至 51） |
| Windows 全目标检查 | `cargo check -p uda-platform-windows --target x86_64-pc-windows-gnu --all-targets` | 零错误零警告 |
| Linux 回归 | `./scripts/test-linux-mock.sh` | **23 passed / 0 failed** |
| Linux 工作区检查 | `cargo check --workspace --all-targets` | 零警告（0 条 warning/error 行） |
| cfg 守卫生效 | `ls target/debug/deps/ \| grep uda_platform_windows` | 0 个产物（完全排除） |
| 生产代码 unwrap/expect/panic | grep 统计 | 0 处（仅测试断言用 `panic!`） |
| unsafe SAFETY 注释 | grep 统计 | 14/14 全覆盖 |
| 注册表写入类型 | grep `write_dword` | 0 处调用（写入路径已统一为 `write_string`/`REG_SZ`） |

**环境说明：** 宿主机原先已装 `x86_64-pc-windows-gnu` target 但无链接器，MSVC 目标亦无 `link.exe`。已安装 `mingw-w64` 交叉工具链，Windows 测试改用 gnu 目标执行。

## 4. 文件清单与依赖

### 新建文件
| 路径 | 说明 |
|------|------|
| `crates/uda-core/src/error.rs` | UdaError 枚举 |
| `crates/uda-core/src/capability.rs` | Capability/SupportLevel/Theme/RgbaColor |
| `crates/uda-core/src/appearance.rs` | AppearanceManager trait |
| `crates/uda-core/src/wallpaper.rs` | WallpaperManager trait、FillMode、WallpaperOptions |
| `crates/uda-core/src/notification.rs` | NotificationManager trait、Notification、Urgency |
| `crates/uda-platform-linux/src/detection.rs` | DesktopEnvironment/EnvironmentInfo 与 detect_environment |
| `crates/uda-platform-linux/src/appearance.rs` | LinuxAppearanceManager |
| `crates/uda-platform-linux/src/error.rs` | UdaError 重导出 |
| `crates/uda-platform-linux/src/wallpaper.rs` | LinuxWallpaperManager |
| `crates/uda-platform-linux/src/notification.rs` | LinuxNotificationManager |

### 依赖变更
- `Cargo.toml`：workspace 新增 `bitflags = "2"`、`async-trait = "0.1"`
- `crates/uda-core/Cargo.toml`：新增 `bitflags.workspace = true`、`async-trait.workspace = true`
- `crates/uda-platform-linux/Cargo.toml`：新增 `zvariant = "4.2"`、`async-trait.workspace = true`
- `crates/uda-cli/Cargo.toml`：新增 `uda-platform-linux = { path = "../uda-platform-linux" }`

### 新建文件
| 路径 | 说明 |
|------|------|
| | `crates/uda-core/src/wakelock.rs` | WakeLockManager trait、WakeLockType、WakeLockGuard |
| | `crates/uda-platform-linux/src/wakelock.rs` | LinuxWakeLockManager |

### Windows 阶段新建文件（Step 10）
路径 | 说明 |
|------|------|
`crates/uda-platform-windows/src/appearance.rs` | `WindowsAppearanceManager`，注册表 `AppsUseLightTheme` 两段式查询 |
`crates/uda-platform-windows/src/wallpaper.rs` | `WindowsWallpaperManager`，`SystemParametersInfoW` + `WallpaperStyle`/`TileWallpaper`（Step 12 起改为 `REG_SZ` 写入） |
`crates/uda-platform-windows/src/wakelock.rs` | `WindowsWakeLockManager`、`WindowsWakeLockGuard`，`SetThreadExecutionState` |
`crates/uda-platform-windows/src/detection.rs` | `WindowsRelease`/`WindowsEnvironmentInfo`，`RtlGetVersion` |
`crates/uda-platform-windows/src/notification.rs` | `WindowsNotificationManager`，WinRT Toast + `availability()` |

### 修改文件（Step 9 / Step 11）
路径 | 修改内容 |
|------|---------|
`crates/uda-platform-linux/src/detection.rs` | 测试增加 `ENV_LOCK` + `EnvGuard` 串行化，消除并行环境变量污染 |
`crates/uda-platform-windows/src/lib.rs` | 补齐 5 个模块声明、`#![deny(unsafe_op_in_unsafe_fn)]`、回退层级文档 |
`crates/uda-platform-windows/Cargo.toml` | 新增 `async-trait`、`Win32_Security`、`Win32_System_SystemInformation` |

### 修改文件（Step 12：Code Review 缺陷修复）
路径 | 修改内容 |
|------|---------|
`crates/uda-platform-windows/src/wallpaper.rs` | 废弃 `write_dword`，改为 `write_string`（`RegSetValueExW` + `REG_SZ`，字节长度含 UTF-16 null 终止符）；`wallpaper_style_values` 由 `(u32, u32)` 改为 `(&'static str, &'static str)` 精确映射；导入由 `REG_DWORD` 换为 `REG_SZ`；模块文档补映射表 |
`plans/phase1_plan.md` | 修正 §10.2 映射表、§5 风险表、§6.2 验收标准与 §7.2 测试基线（45 → 51） |

### Windows 阶段依赖变更
- `crates/uda-platform-windows/Cargo.toml`：新增 `async-trait.workspace = true`；`windows` features 补齐 `Win32_Security`（`HKEY` 与 `RegCreateKeyExW` 所在 feature）、`Win32_System_SystemInformation`（`OSVERSIONINFOW`）

## 5. 风险与缓解
| 风险 | 缓解 |
|------|------|
| `gsettings` 返回格式非预期（如 `'Default'` 而非数字） | 使用字符串匹配，兼容 `color-scheme` 与 `gtk-theme` 两种路径 |
| XFCE 检测在无 `xfconf-query` 时失败 | 严格按用户要求，XFCE 失败后返回 `Theme::Light` |
| `kreadconfig5` 可能不存在于最小化 KDE 安装 | 命令失败视为该 tier 不可用，降级到下一 tier |
| `RgbaColor` 解析 accent-color 字符串失败 | 返回 `UdaError::NotSupported` |
| zbus API 版本差异 | 已适配 `zbus 4.x` / `zvariant 4.x` API |
| `dbus-run-session` 下并行测试互相干扰 | 通知/外观模块测试避免在单元测试中建立真实 D-Bus 连接，仅做纯逻辑验证 |
| **Linux 测试用 `env::set_var` 导致并行污染**（Step 9 实际发生） | `ENV_LOCK` 互斥锁 + `EnvGuard` RAII 清理，锁中毒时 `into_inner()` 恢复 |
| **Windows 注册表值类型被改写导致字节误读** | 读取路径强制校验 `REG_DWORD` 且长度恰 4 字节，否则忽略（适用于 `AppsUseLightTheme`） |
| **壁纸样式键被写成 `REG_DWORD`，Explorer 无法识别**（Step 12 Code Review 实际发生） | `WallpaperStyle`/`TileWallpaper` 属 REG_SZ 字符串值；已废弃 `write_dword`，统一经 `write_string` 以 `REG_SZ` 写入，字节长度含 UTF-16 null 终止符；测试断言 `REG_SZ` 常量 |
| **`FillMode::Fit` 被错误降级为平铺**（Step 12 Code Review 实际发生） | Windows 7+ 原生支持 Fit；严格映射 Fill/Crop→`"10"`、Fit→`"6"`、Stretch→`"2"`，`TileWallpaper` 恒为 `"0"`；测试覆盖全部四种模式与"仅限已知模式"约束 |
| **壁纸样式与壁纸设置顺序颠倒导致沿用旧样式** | 固定为"先写 `WallpaperStyle`/`TileWallpaper`，后调 `SystemParametersInfoW`" |
| **Win32 执行状态非引用计数，多 guard 相互干扰** | 文档化为单锁模型；`WindowsWakeLockGuard` 包装 core guard，保证只释放一次 |
| **非打包进程无 AppUserModelID 导致 Toast 失败**（Step 10.5 实际发生） | 识别 `0x80070490` 并映射为 `NotSupported`；提供 `availability()` 供调用方探测 |
| **`GetVersionExW` 在无兼容性清单时虚报版本** | 改用 `RtlGetVersion` |
| **HRESULT 手工十进制转换错误**（首次实现即发生） | 常量写为 `0x8007_0490_u32 as i32`，测试断言同一 hex 字面量 |
| **宿主机无 Windows 链接器导致无法跑测试** | 安装 `mingw-w64`，使用 `x86_64-pc-windows-gnu` 目标 |

## 6. 完成标准（Done Criteria）

### 6.1 Linux 侧（Step 1–8）
1. `uda-core` 中 `UdaError`、`Capability`、`SupportLevel`、`Theme`、`RgbaColor` 与 `AppearanceManager`、`WallpaperManager`、`NotificationManager` 定义完整，`cargo build -p uda-core` 零错误零警告。
2. `uda-platform-linux` 中 `detect_environment()` 可返回桌面环境与会话类型。
3. `uda-platform-linux` 中 `LinuxAppearanceManager` 实现 `detect_theme()`，并按 **Portal → GNOME → KDE → XFCE → Light** 回退链执行。
4. `detection.rs` 包含 `DesktopEnvironment::infer()` 及 >= 5 个单元测试，覆盖 GNOME/KDE/Xorg/Wayland/Unknown。
5. `uda-platform-linux` 中 `LinuxWallpaperManager` 实现 `set_wallpaper()`，并按 **GNOME → KDE → Hyprland/Sway → X11** 回退链执行。
6. `uda-platform-linux` 中 `LinuxNotificationManager` 实现 `send()`，调用 `org.freedesktop.Notifications.Notify`，支持 actions 展平。
7. `uda-platform-linux` 中 `LinuxWakeLockManager` 实现 `acquire()`，调用 `org.freedesktop.ScreenSaver.Inhibit`，并在 `WakeLockGuard` 的 `Drop` 中调用 `UnInhibit` 释放。
8. 全工作区 `cargo build --workspace` 通过，`./scripts/test-linux-mock.sh` 全部 PASS。
9. 计划文档留存于 `plans/phase1_plan.md`。

### 6.2 Windows 侧（Step 9–11）
10. `WindowsAppearanceManager` 通过 `RegOpenKeyExW` + `RegQueryValueExW` 读取 `AppsUseLightTheme`，0=Dark / 1=Light，并做值类型与长度校验。
11. `WindowsWallpaperManager` 先写 `WallpaperStyle`/`TileWallpaper`（**`REG_SZ` UTF-16 字符串**），再调 `SystemParametersInfoW(SPI_SETDESKWALLPAPER, ..., SPIF_UPDATEINIFILE | SPIF_SENDCHANGE)`，并支持 `SPI_GETDESKWALLPAPER` 读回。
12. `WindowsWakeLockManager` 获取时设 `ES_DISPLAY_REQUIRED | ES_SYSTEM_REQUIRED | ES_CONTINUOUS`，`Drop` 时以 `ES_CONTINUOUS` 恢复默认。
13. `uda-platform-windows/src/lib.rs` 保留 `#![cfg(windows)]` 守卫，Linux 下 `cargo check --workspace` 零警告且该 crate 零编译产物。
14. Windows target `cargo check --all-targets` 零错误零警告，单元测试 51 项全部通过。
15. Linux 下 `./scripts/test-linux-mock.sh` 回归通过（23 项）。
16. 生产代码零 `unwrap()`/`expect()`/`panic!`；全部 unsafe 块带 SAFETY 注释。

## 7. 当前进度总览

### 7.1 Phase 1 模块完成度

| 模块 | `uda-core` Trait | Linux | Windows | C-ABI |
|------|-----------------|-------|---------|-------|
| Detection | — | ✅ `detect_environment()` | ✅ `detect_environment()` | — |
| Appearance | ✅ `AppearanceManager` | ✅ Portal→GNOME→KDE→XFCE→Light | ✅ 注册表 + 降级 | ✅ `uda_detect_theme` |
| Wallpaper | ✅ `WallpaperManager` | ✅ GNOME→KDE→Hyprland/Sway→X11 | ✅ SPI + 样式键（REG_SZ） | ✅ `uda_set/get_wallpaper` |
| Notification | ✅ `NotificationManager` | ✅ D-Bus Notify | ✅ WinRT Toast | ⏸ Phase 1 未导出（toast 需打包应用身份） |
| WakeLock | ✅ `WakeLockManager` | ✅ ScreenSaver Inhibit + CLI 降级 | ✅ SetThreadExecutionState | ✅ `uda_wakelock_acquire/release` |
| FFI 导出层 | — | ✅ `libuda_ffi.so` | ✅ `uda_ffi.dll` | ✅ 8 个符号 + `include/uda.h` |
| 多语言示例 | — | ✅ Python `ctypes`（实跑通过） | ✅ Node.js `koffi`（实跑通过） | ✅ 双平台 demo 均成功 |
| CLI 诊断工具 | — | ✅ `uda-cli` | ⏸ 硬编码依赖 Linux crate | — |

### 7.2 测试基线（2026-09-21）

| 目标 | 命令 | 测试数 | 状态 |
|------|------|--------|------|
| `x86_64-unknown-linux-gnu` | `./scripts/test-linux-mock.sh` | 23 | ✅ 全通过 |
| `x86_64-pc-windows-gnu` | `cargo test -p uda-platform-windows --target x86_64-pc-windows-gnu` | 51 | ✅ 全通过 |
| `uda-ffi`（Linux） | `cargo test -p uda-ffi` | 46 | ✅ 全通过 |
| `uda-ffi`（Windows gnu） | `cargo test -p uda-ffi --target x86_64-pc-windows-gnu` | 46 | ✅ 全通过 |
| `uda-core` | `cargo test -p uda-core` | 4 | ✅ 全通过 |
| 工作区检查 | `cargo check --workspace --all-targets` | — | ✅ 零警告 |
| 动态库产物（Linux） | `cargo build -p uda-ffi` → `libuda_ffi.so` | — | ✅ 68 MB，`nm -D` 确认 8 个 `uda_*` 符号 |
| 动态库产物（Windows） | `cargo build -p uda-ffi --target x86_64-pc-windows-gnu` → `uda_ffi.dll` | — | ✅ 33 MB，`scripts/pe_exports.py` 确认 8 个 `uda_*` 符号 |
| Python 示例实跑（Linux） | `python3 examples/python/demo.py` | — | ✅ 主题浅色 + 壁纸 + 常亮锁全部成功 |
| Node.js 示例实跑（Windows） | `node.exe examples/nodejs/demo.js` + `uda_ffi.dll` | — | ✅ 主题深色 + 壁纸 + 常亮锁全部成功 |
| 内存泄漏抽查 | 2 万次 `uda_get_wallpaper` | — | ✅ RSS 增量 256 KB，无泄漏 |
| koffi 出参稳定性抽查 | 200 轮 `uda_get_wallpaper` + `uda_free_string` | — | ✅ 无崩溃、无泄漏 |
| `cargo fmt --all -- --check` | 格式化门禁 | — | ✅ 零 diff |
| `cargo clippy --workspace --all-targets -- -D warnings` | 静态分析门禁 | — | ✅ 零警告 |
| 交叉编译卫生（AGENTS.md Principle 3） | `cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` | — | ✅ 通过 |
| CI 工作流合法性 | `yaml.safe_load(".github/workflows/ci.yml")` | 5 jobs | ✅ 解析通过 |

### 7.2.1 CI 工作流重构（Step 14）

**修复前缺陷**：`.github/workflows/ci.yml` 整体不是合法 YAML —— 文件首行是 `mkdir -p .github/workflows`，第 3 行是 `cat << 'EOF' > .github/workflows/ci.yml`，末尾残留 `EOF`。即当初生成文件的 shell heredoc 被原样写盘而未执行，GitHub Actions 无法解析该工作流，CI 从未真正运行过。

**重写后的 5 个 job**：

| Job | 作用 | 关键点 |
|-----|------|--------|
| `lint` | 格式化 + 静态分析 + 编译检查 | `rustfmt`、`clippy -D warnings`、`check --locked`；先跑最快失败 |
| `cross-check` | 交叉编译卫生 | 新增 `x86_64-pc-windows-gnu` target + `mingw-w64` 链接器，验证 Windows crate 不破坏 Linux 宿主编译 |
| `linux-test` | Linux 全量测试 | 按 crate 拆分（`uda-core` / `uda-platform-linux`@dbus-run-session / `uda-ffi`），构建 `.so` 后实跑 Python 示例 |
| `windows-test` | Windows 全量测试 | `uda-core` / `uda-platform-windows` / `uda-ffi`，构建 DLL 后 `pe_exports.py` 校验符号并实跑 Python 示例 |
| `release` | v0.1.0 发布 | `if: startsWith(github.ref, 'refs/tags/v')`，`needs` 前置 4 个 job，双平台 release 产物 + 符号校验 + 打包 + `softprops/action-gh-release` |

**同时修复的隐患**：
1. 全部构建命令补 `--locked`，防止 CI 与本地 `Cargo.lock` 解析不一致
2. 每个 job 独立 `Swatinem/rust-cache@v2`，并新增 `permissions: contents: read` / `concurrency` 取消同分支旧任务
3. 原 Windows job 缺 `python3-dbusmock` 等无关依赖已剔除；新增 mingw 链接器与 `zip` 安装（且修正"先打包后装 zip"的顺序错误）
4. `UDA_LIBRARY` 改用正斜杠路径，避免 Windows 反斜杠在 `ctypes` / bash 中的歧义

**为让新门禁真实通过而顺带修复的代码问题**（此前 9 处 clippy 警告 + 19 个文件格式 diff，任何 `-D warnings` 门禁都会红）：
- `uda-core/src/notification.rs`：`Urgency` 手写 `impl Default` 改为 `#[derive(Default)]` + `#[default]`
- `uda-ffi/src/lib.rs`：`uda_detect_theme` 内多余的 `let code = ...; let _ = code;` 单元值绑定
- `uda-platform-linux/src/appearance.rs`：`portal_color_scheme` 两处 `return` 改尾表达式
- `uda-platform-linux/src/notification.rs`：D-Bus `Notify` 9 参为协议固定签名，加 `#[allow(clippy::too_many_arguments)]` 并注释说明；测试用 `vec!` 改数组
- `uda-platform-linux/src/wallpaper.rs`：`&[...]` 借来的临时数组改为直接传数组
- 全工作区 `cargo fmt --all`

**README 同步**：测试徽章改为 CI 动态徽章（`actions/workflows/ci.yml/badge.svg`），不再写死数字（仓库实测 124 个测试：core 4 + linux 23 + windows 51 + ffi 46；Linux 宿主可运行 73 个）。全文改为英文（保留 Slogan 与机构名），新增支持矩阵、三语言 Quickstart、架构分层、v0.2.0 Planned 路线图、Community & Sponsorship。Python 示例补注 `set_wallpaper()` 在无后端会话下会返回 `UdaError`，与 capability-driven 契约一致。

**遗留**：仓库尚无 `LICENSE` 文件，徽章暂链接至 opensource.org；建议补 MIT/Apache-2.0 双许可文本。

### 7.2.2 v0.1.0 收官：许可证与中英双语文档（Step 15）

**新增文件**：
- `LICENSE-MIT`：MIT 许可全文，署名 `Copyright (c) 2026 United Desktop Association`
- `LICENSE-APACHE`：Apache License 2.0 全文（自 `http://www.apache.org/licenses/LICENSE-2.0.txt` 官方原文下载，202 行），附录版权声明处填入 `2026 United Desktop Association`，无残留占位符
- `README_CN.md`：中文版 README，与英文版章节一一对应

**修改**：
- 5 个 crate 的 `Cargo.toml` 均补 `license = "MIT OR Apache-2.0"`，已用 `cargo metadata` 验证字段可被 Cargo 正确解析
- `README.md` / `README_CN.md` 顶部徽章区与底部许可区各放一处互链（**English** · 简体中文），共 4 个入口可切换语言
- 英文版许可区改为同时链接 `LICENSE-APACHE` 与 `LICENSE-MIT`，不再指向 opensource.org（原因为仓库缺许可文件，现已补齐）

**验证**：
- 两份 README 的相对链接、锚点链接、代码围栏语言标签全部通过脚本校验，无死链
- 锚点 `#why-uda` / `#为什么需要-uda` 均落在无 emoji 的纯文本标题上，GitHub slug 行为可预期
- `cargo metadata --no-deps` 对 5 个 crate 的 license 字段解析正常；`cargo check --workspace --all-targets --locked` 通过

### 7.3 后续待办（Phase 2 及 Phase 1 遗留）
- [ ] **CLI 跨平台化**：`crates/uda-cli/src/main.rs` 目前硬编码依赖 `uda_platform_linux`，需按 `cfg` 分流以支持 Windows 诊断
- [ ] **FFI 通知导出**：`uda_send_notification` 尚未导出；需先解决 toast 在非打包进程下的 AppUserModelID 问题
- [ ] **FFI 事件回调**：主题变更、壁纸变更等监听型 API 需要 C 函数指针回调或轮询接口，尚未设计
- [ ] **Node.js 示例的 Linux 侧验证**：宿主机无 Linux 版 `node`，当前仅用 WSL 互通的 `node.exe` + 交叉编译 DLL 验证过；Linux 原生 `node` + `libuda_ffi.so` 组合尚未实跑
- [ ] **Windows 通知增强**：toast action 按钮需打包应用注册激活处理器；`app_icon` 需 `appImage` 内容
- [ ] **Windows 多显示器壁纸**：需私有 `IDesktopWallpaper` COM 接口（Win8+）
- [ ] **Windows 强调色**：`HKCU\Software\Microsoft\Windows\DWM` 的 `AccentColor`（ABGR 打包）布局跨版本不稳定，需实测后补充
- [ ] **System Tray**：Phase 1 路线图项，Linux `org.kde.StatusNotifierItem` + Windows `Shell_NotifyIconW` 尚未实现
- [ ] **cbindgen 自动化**：`include/uda.h` 目前手工维护，可引入 `cbindgen` 从 Rust 源码生成以防漂移
- [ ] **Phase 2**：Media Control / Clipboard / Audio / Brightness / Session Lifecycle
