# Changelog

本文件记录本项目的全部值得注意的变更。
This file records every notable change to this project.

格式基于 [Keep a Changelog](https://keepachangelog.com/en/1.0.0/)，且本项目遵循 [语义化版本](https://semver.org/spec/v2.0.0.html)。
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

> 完整文档见 [unidesktop.github.io](https://unidesktop.github.io/)。
> Full documentation lives at [unidesktop.github.io](https://unidesktop.github.io/).

## [v0.2.2] - Cross-Review Hardening · 2026-10-10

> 对应 PR：交叉审查清单（issue #4，50 项）中 v0.2.1 未覆盖的 48 项全部在本节落地。
> Corresponds to the cross-review checklist (issue #4): the 48 findings not already
> shipped in v0.2.1 all land in this section.
> 本节同时收录 cbindgen 生成 C 头文件（issue #6 / PR #7）带来的 ABI 契约收紧。
> This section also covers the ABI-contract tightening from the cbindgen-generated
> C header (issue #6 / PR #7).

### ✨ Added

- ✨ **`uda_last_error_message` 返回可释放的 `char *`**：该导出的返回类型由 `const char *` 改为 `char *`，C/C++ 宿主可以直接把它交给 `uda_free_string()`，无需 `const_cast`；二进制 ABI 不变，但这是源码级签名变化，依赖旧 `const char *` 函数指针类型的代码需要适配。
- ✨ **`uda_last_error_message` returns a freeable `char *`**: the export now returns `char *` instead of `const char *`, so C/C++ hosts can pass it straight to `uda_free_string()` without a cast. The binary ABI is unchanged, but this is a source-level signature change for code that stores the old `const char *` function-pointer type.
- ✨ **`uda_tray_capabilities` C 导出**：托盘能力位首次对 C 宿主可见，新增 8 个 `UDA_TRAY_CAP_*` 常量（append-only，位与核心 `Capability` 一一对应），宿主可以按 AGENTS.md 的要求"先查能力再降级"。C ABI 没有点击回调注册，因此导出中 `UDA_TRAY_CAP_CLICK` / `UDA_TRAY_CAP_DOUBLE_CLICK` 恒为 0；Python / Node 绑定尚未封装该导出（以及 `uda_tray_support_level` / `uda_tray_support_reason`）。
- ✨ **A `uda_tray_capabilities` C export**: tray capability bits become visible to C hosts for the first time, with eight new `UDA_TRAY_CAP_*` constants (append-only, mapping 1:1 onto the core `Capability` bits), so hosts can follow "query capabilities, then degrade". The C ABI has no click-callback registration, so `UDA_TRAY_CAP_CLICK` / `UDA_TRAY_CAP_DOUBLE_CLICK` are always cleared in this export; the Python and Node bindings do not wrap it (nor `uda_tray_support_level` / `uda_tray_support_reason`) yet.
- ✨ **托盘降级状态对宿主可见**：新增 `uda_tray_support_level` / `uda_tray_support_reason` 两个 C 导出与 `UDA_TRAY_SUPPORT_LEVEL_NONE / PARTIAL / FULL` 常量。watcher 缺失等降级（图标已导出但 shell 可能看不到）不再只进日志：Linux 侧把原因记到图标状态上，`LinuxTrayManager::support_level` 返回 `Partial(reason)`，宿主可经新导出按句柄查询级别与原因（原因串归调用方所有，无降级时为 NULL）；Windows 无降级路径，恒返回 FULL / NULL。
- ✨ **Tray degradation becomes host-visible**: two new C exports, `uda_tray_support_level` / `uda_tray_support_reason`, plus the `UDA_TRAY_SUPPORT_LEVEL_NONE / PARTIAL / FULL` constants. A degraded registration (the item is exported but the shell may never see it - a missing watcher) no longer vanishes into logs: Linux records the reason on the icon state, `LinuxTrayManager::support_level` reports `Partial(reason)`, and hosts query level and reason per handle through the new exports (the reason string is caller-owned, NULL when not degraded); Windows has no degradation path and always reports FULL / NULL.

### 🛠️ Changed

- 🛠️ **`UdaError` 新增 `InvalidArgument` 变体并映射 `UDA_ERR_INVALID_ARGUMENT`（`-1`）**：空标签、非法图标等调用方输入错误不再伪装成"平台不支持"（`-2`），`include/uda.h` 与绑定文档同步。
- 🛠️ **`UdaError` gains an `InvalidArgument` variant mapped to `UDA_ERR_INVALID_ARGUMENT` (`-1`)**: caller mistakes such as an empty label or an invalid icon no longer masquerade as "platform not supported" (`-2`); `include/uda.h` and the binding docs say the same.
- 🛠️ **`Theme`、`SupportLevel`、`UdaError` 标记 `#[non_exhaustive]`**：这三个枚举在补丁版本中就可能新增变体（`Unknown`、`Partial(reason)`、`InvalidArgument` 均是先例）；下游的穷尽 `match` 需要通配臂，新增变体不再构成编译期破坏。crate 内所有跨包 `match` 已补通配兜底。
- 🛠️ **`Theme`, `SupportLevel` and `UdaError` are now `#[non_exhaustive]`**: these enums gain variants in patch releases (`Unknown`, `Partial(reason)` and `InvalidArgument` are all precedents); downstream exhaustive `match`es need a wildcard arm, so future additions stop being compile-time breaks. Every cross-crate `match` in this workspace carries the wildcard now.
- 🛠️ **关于 FFI 导出的 `unsafe extern "C"`（兼容性说明）**：本节没有翻转任何导出的安全等级——凡接受裸指针的导出自引入起就是 `unsafe extern "C"`（托盘 / 通知 / 强调色 / 媒体 / 会话各导出随 v0.2.0 一并引入），新增的 `uda_tray_capabilities` 同样如此；全库仅有 8 个普通 `extern "C"` 导出（两个消息助手，以及仅收数值句柄的托盘 / wakelock 调用）。该分类对调用方的影响如实说明：C 调用方完全不受影响；经 `extern "C" { ... }` 块声明调用的 Rust 宿主在**任何 edition** 都不受定义侧 `unsafe` 关键字影响（块内声明的函数本来就要求调用方持 `unsafe`，与定义侧写法无关）；仅当把 `uda-ffi` 当 rlib 直接依赖时，指针类导出才要求 `unsafe` 块——这与 v0.2.0 以来一致。指针类导出的 `unsafe` 是健全分类而非可移除的障碍：传入非空的悬垂指针属于调用方 UB，安全函数不能接受。安全/普通分类现由 `export_safety_classification_is_pinned` 测试钉死，未来任何重分类都必须显式更新该列表与 CHANGELOG。
- 🛠️ **On the `unsafe extern "C"` FFI exports (compatibility note)**: nothing in this section flips an export's safety - every export that takes a raw pointer has been `unsafe extern "C"` since it was introduced (the tray, notification, accent-colour, media and session exports arrived that way with v0.2.0), and the new `uda_tray_capabilities` follows suit; the library has exactly eight plain `extern "C"` exports (the two message helpers, plus the tray / wakelock calls that take only numeric handles). The impact on callers, stated precisely: C callers are unaffected; Rust hosts that declare the functions in `extern "C" { ... }` blocks are unaffected **on every edition** (the definition-side `unsafe` keyword cannot break an extern-block declaration - block items already require the caller's own `unsafe` regardless); only a host consuming `uda-ffi` as an rlib sees an `unsafe` requirement on the pointer-taking exports, the same as since v0.2.0. The `unsafe` on pointer-taking exports is a sound classification, not a removable obstacle: a caller passing a dangling non-null pointer is UB, which a safe function cannot accept. The split is now pinned by the `export_safety_classification_is_pinned` test, so any future reclassification must update that list and the changelog deliberately.
- 🛠️ **同步 API 桥的构建顺序**：`uda-platform-linux` 的 `run_async` 与 `uda-ffi` 的 `run_sync` 现在都是"先判定运行时上下文、后构建"，环境运行时分支不再把runtime 搬去工作线程。
- 🛠️ **Sync-bridge build order**: `run_async` (`uda-platform-linux`) and `run_sync` (`uda-ffi`) both branch on the ambient runtime *before* building anything, so the nested branch no longer ships a runtime across threads.
- 🛠️ **C 头文件自动化生成**：`include/uda.h` 改为用 [cbindgen](https://github.com/mozilla/cbindgen) 从 `crates/uda-ffi` 的 Rust 源码生成（[UniDesktop/SDK#6](https://github.com/UniDesktop/SDK/issues/6)），Rust 侧成为 C-ABI 签名、常量与文档的唯一事实源。
- 🛠️ **Generated C header**: `include/uda.h` is now generated by [cbindgen](https://github.com/mozilla/cbindgen) from the Rust sources in `crates/uda-ffi` ([UniDesktop/SDK#6](https://github.com/UniDesktop/SDK/issues/6)), making the Rust side the single source of truth for C-ABI signatures, constants and documentation.
  - 新增 `crates/uda-ffi/src/abi.rs`：全部 `UDA_*` 常量与托盘回调类型集中于此（顺带消除了 `lib.rs` 与 `error.rs` 之间的一份重复定义）；头文件中的 API 文档全部合并进导出函数的 Rust doc comment。
  - A new `crates/uda-ffi/src/abi.rs` centralizes every `UDA_*` constant and tray callback type (also removing a duplicated definition between `lib.rs` and `error.rs`); the header's API documentation was folded into the Rust doc comments of the exported functions.
  - 新增 `scripts/gen-header.sh`（支持 `--check`）；CI 的 Cross-Compile Hygiene job 增加「重新生成 + diff」门禁，改了 Rust 侧忘记重新生成头文件会直接红灯。
  - A new `scripts/gen-header.sh` (with `--check`) adds a regenerate-and-diff gate to the Cross-Compile Hygiene CI job: a Rust change that forgets to regenerate the header now fails the build.
  - **迁移本身不改变 ABI 签名与常量**：生成的 `include/uda.h` 逐项复刻此前的头文件——包括本版本先落地的 8 个 `UDA_TRAY_CAP_*`、3 个 `UDA_TRAY_SUPPORT_LEVEL_*` 常量与 `uda_tray_capabilities` / `uda_tray_support_level` / `uda_tray_support_reason` 三个导出；`scripts/header_symbols.py` 与 `pe_exports.py` 的「头文件 ↔ 动态库符号」校验保持不变。唯一的语义差异是下面 `uda_media_get_metadata` 的 NULL 回填，那是有意的行为修正，不在「签名与常量不变」的承诺范围内。
  - **The migration itself changes no ABI signatures or constants**: the generated `include/uda.h` reproduces the previous header item for item - including the eight `UDA_TRAY_CAP_*` and three `UDA_TRAY_SUPPORT_LEVEL_*` constants plus the `uda_tray_capabilities` / `uda_tray_support_level` / `uda_tray_support_reason` exports that landed earlier in this release; the `header_symbols.py` / `pe_exports.py` header-to-library symbol checks are unchanged. The one semantic difference is the `uda_media_get_metadata` NULL backfill below, which is a deliberate behaviour fix and therefore outside the "signatures and constants unchanged" promise.
- 🛠️ **`uda_media_get_metadata` 回填 NULL，`MediaMetadata` 区分「未发布」与「发布为空」**：未发布字段与「无播放器」现在稳定写回 NULL（时长 / 进度写 0，状态仍是 `UDA_OK`），与 `docs/internals/media_specs.md` 的 C-ABI 契约一致。此前 `MediaMetadata` 的字段是 `String`，C 侧根本无法表达「未发布」：本节前面按「空串即未发布」把空值折叠成 NULL，于是「播放器发布了一个空值」与「播放器没发布该字段」在 C-ABI 上不可区分。现在字符串字段改为 `Option<String>`，Linux 上 `None` → NULL、`Some("")` → 空 C 串，两者可区分；Windows SMTC 线上没有「未发布」标记，空读取折叠为 `None`（即 NULL），可区分性仅 Linux 成立。因此「空串一律折叠为 NULL」的通用规则适用于壁纸路径、托盘降级原因等「空串没有独立语义」的导出，而媒体字段是其有意例外：只有未发布字段是 NULL。宿主在两个平台上都应把 NULL 当作「跳过该字段」；`uda_free_string(NULL)` 是空操作，三个出参可无条件释放。
- 🛠️ **`uda_media_get_metadata` backfills NULL, and `MediaMetadata` tells "published empty" from "unpublished"**: unpublished fields and "no player" now reliably write NULL (duration / position write 0 and the status stays `UDA_OK`), matching the C-ABI contract in `docs/internals/media_specs.md`. `MediaMetadata`'s fields used to be `String`, which cannot express "unpublished" at all: the "empty means unpublished" rule earlier in this section folded every empty value to NULL, so "the player published an empty value" and "the player did not publish the field" were indistinguishable across the C ABI. The fields are now `Option<String>`: on Linux `None` maps to NULL and `Some("")` to an empty C string, so the two cases differ; Windows SMTC has no "unpublished" marker on the wire, so an empty read folds to `None` (i.e. NULL) and the distinction holds on Linux only. The general "empty folds to NULL" rule therefore covers the exports where an empty string carries no separate meaning (a wallpaper path, a tray degradation reason), with the media fields the deliberate exception: only an unpublished field is NULL. A host must treat NULL as "skip this field" on both platforms; `uda_free_string(NULL)` is a no-op, so all three out-strings can be freed unconditionally.

### ⚠️ Breaking

- ⚠️ **`MediaMetadata` 字符串字段改为 `Option<String>`（Rust API）**：`uda-core::media::MediaMetadata` 的 `title` / `artist` / `album` 由 `String` 变为 `Option<String>`，`is_empty()` 相应改判 `is_none()`。外部 Rust 代码中 `metadata.title`（期望 `String`）、`metadata.title.clone()` 或 `format!("{}", metadata.title)` 等写法将编译失败，需改为 `metadata.title.as_deref().unwrap_or("")` 之类。C-ABI 不变；Python/Node 绑定仍把 NULL 与空 C 串一并解码成 `""`，其公开行为不受影响。动机：C 宿主需要区分「未发布」（NULL）与「发布为空」（空串），`String` 无法表达前者。
- ⚠️ **`MediaMetadata` string fields become `Option<String>` (Rust API)**: `uda-core::media::MediaMetadata`'s `title` / `artist` / `album` change from `String` to `Option<String>`, with `is_empty()` now testing `is_none()`. External Rust code reading `metadata.title` as a `String`, cloning it or formatting it with `{}` will stop compiling; adapt with `metadata.title.as_deref().unwrap_or("")` and similar. The C-ABI is unchanged; the Python/Node bindings still decode both NULL and an empty C string to `""`, so their public behaviour does not change. Motivation: a C host must tell "not published" (NULL) from "published empty" (empty string), which a bare `String` cannot express.

### 🧰 Fixed

- 🧰 **Windows 托盘七项**：`hide()`→`show()` 往返失效（`dwStateMask=0`）；GDI 位图全路径不释放（含 `CreateIconIndirect` 前后、mask 失败早退）；模态菜单泵重入窗口过程造成同一 `Worker` 两个 `&mut`（别名 UB，菜单流程拆为两段并改为 per-worker 重入守卫）；`NIM_MODIFY` 失败泄漏新 HICON；更新失败被记为已应用后永不重试；`AppendMenuW` 结果一律丢弃（子菜单失败泄漏 HMENU）；explorer 重启后图标永久消失（改隐藏顶层窗口 + `TaskbarCreated` 重加）。
- 🧰 **Seven Windows tray fixes**: `hide()`→`show()` never restoring (`dwStateMask=0`); GDI bitmaps leaked on every path (around `CreateIconIndirect` and the mask-failure early exit); the modal menu pump re-entering the window proc with a second `&mut Worker` (aliasing UB — the menu flow is now two-phase with a per-worker reentrancy guard); a leaked HICON when `NIM_MODIFY` fails; updates recorded as applied without being so and never retried; `AppendMenuW` results discarded wholesale (leaking the submenu HMENU); and the icon gone forever after an explorer restart (now a hidden top-level window that re-registers on `TaskbarCreated`).
- 🧰 **Windows 托盘窗口不再幽灵化**：工作窗口改 `WS_POPUP` + `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`，不再出现在 Alt+Tab 切换列表中（`EnumWindows` 会枚举隐藏的顶层窗口，不受样式影响；按 Alt+Tab 语义过滤的窗口列表需自行剔除工具窗口）；`WM_NCDESTROY` 现在清 `GWLP_USERDATA` 并 `PostQuitMessage`，外部销毁路径不再悬空解引用。
- 🧰 **The Windows worker window stops being a ghost**: it is now `WS_POPUP` with `WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE`, keeping it out of the Alt+Tab list (`EnumWindows` enumerates hidden top-level windows regardless of style, so an Alt+Tab-style window list must filter tool windows out on its own); `WM_NCDESTROY` clears `GWLP_USERDATA` and posts a quit message, so an externally destroyed window can no longer leave a dangling pointer behind.
- 🧰 **Windows AUMID 两项**：提前返回泄漏 `CoTaskMem`；`availability()` 用空 app_name 抢注进程身份导致通知归属分叉。
- 🧰 **Two Windows AUMID fixes**: the early-return path leaked `CoTaskMem`, and `availability()` claimed the process identity with an empty app_name, splitting notification ownership.
- 🧰 **Windows 通知补齐**：`NIF_SHOWTIP`（v4 下标准 tooltip 被抑制）、`NIN_SELECT`/`NIN_KEYSELECT` 键盘激活、`WM_CONTEXTMENU` 锚点语义、`SetTimer` / `SetForegroundWindow` 结果检查。
- 🧰 **Windows notifications completed**: `NIF_SHOWTIP` (v4 suppressed the standard tooltip), `NIN_SELECT`/`NIN_KEYSELECT` keyboard activation, `WM_CONTEXTMENU` anchor semantics, and `SetTimer` / `SetForegroundWindow` result checks.
- 🧰 **Linux 托盘六项**：dbusmenu 信号从不发射 + `Arc::as_ptr` 变更检测失效（现按内容指纹发射真实 `LayoutUpdated`，并补规范要求的 `NewToolTip`）；`GetLayout` 忽略 `parent_id` 且曲解 `recursionDepth`；dbusmenu id 按遍历位置重排（现持久稳定映射，退役 id 不复用）；worker 初始化失败仍宣称全套能力（现就绪握手，失败返回错误）；未注册双击回调时第二次点击被吞（现回退 `on_click`）；200ms 每 tick 无条件重转码图标。
- 🧰 **Six Linux tray fixes**: dbusmenu signals never emitted plus a blind `Arc::as_ptr` change detector (now a content fingerprint with a real `LayoutUpdated`, plus the spec-mandated `NewToolTip`); `GetLayout` ignoring `parent_id` and misreading `recursionDepth`; dbusmenu ids re-shuffled per snapshot (now a persistent stable map whose retired ids are never reused); a failed worker init still advertising full capabilities (now a readiness handshake that fails `create()`); the second click swallowed when no double-click handler exists (now falls back to `on_click`); and an unconditional per-tick icon transcode.
- 🧰 **Linux 平台四项**：Hyprland 壁纸链路不可用（hyprpaper 改走 `hyprctl`、swww 守护进程探测/拉起、全程超时、失败上报）；同步 API 内联 `block_on` 在 tokio 上下文 panic（现统一 `try_current` 分流桥）；logout Tier-3 回退服务名/参数签名错误；GNOME 47+ 强调色枚举解析恒失败（映射 HIG 调色板）。
- 🧰 **Four Linux platform fixes**: an unusable Hyprland wallpaper chain (hyprpaper now driven via `hyprctl`, swww daemon probed/spawned, everything under timeouts, failures reported); sync APIs panicking under an ambient tokio runtime (now one `try_current`-based bridge); logout Tier-3 fallback with wrong service names and argument signatures; and GNOME 47+ accent-colour enums never parsing (now mapped onto the HIG palette).
- 🧰 **FFI 契约六项**：`uda_tray_menu_destroy` 后点击触发已释放回调（先从所有存活图标解挂再删记录，`set_menu` 单锁线性化）；`uda_last_error_message()` 所有权三处文档互相矛盾、绑定每次失败泄漏 CString（现统一为"新分配、调用方经 `uda_free_string()` 释放"，文档与两种绑定同步）；`set_action` 失败仍返回 `UDA_OK`；复选框蹦床 `Arc` 自引用环（改 `Weak`）；菜单行 id 用 `entries().last()` 反推（现直接使用 `push` 返回值）；句柄注册表"摘除-回插"两次加锁的短暂失活窗口（现单锁内完成判型）。
- 🧰 **Six FFI contract fixes**: clicks reaching freed callbacks after `uda_tray_menu_destroy` (the menu is detached from every live icon before the record goes, with `set_menu` linearised under one lock); `uda_last_error_message()` ownership stated three contradictory ways while the bindings leaked a CString on every failure (now uniformly "freshly allocated, released by the caller via `uda_free_string()`", with docs and both bindings aligned); `set_action` failures still reporting `UDA_OK`; the checkbox trampoline's `Arc` self-reference cycle (now `Weak`); menu row ids reverse-engineered from `entries().last()` (now taken straight from `push`); and the remove/re-insert double-lock window in the handle registry (type checks now complete under one lock).
- 🧰 **工程门禁与媒体契约**：非 Linux/Windows 目标（如 macOS）上 `cargo check --workspace` 直接编译失败（cfg 备援不完整）；播放器明确拒绝媒体命令时返回 `-5` 而非 `uda.h` 承诺的 `-2`。
- 🧰 **Build gate and media contract**: `cargo check --workspace` failed outright on non-Linux/Windows targets such as macOS (incomplete cfg fallbacks); and a player explicitly refusing a media command reported `-5` instead of the `-2` promised by `uda.h`.
- 🧰 **无头环境的诚实答案（第二轮复审）**：主题检测不再把从未设置过的 `gtk-theme` schema 默认值（如 WSL 的 'Adwaita'）当成浅色答案——`-dark` 后缀才判深色，浅色需要 `dconf read` 证明显式设置，否则落到 `Theme::Unknown`；wakelock 的 Tier-3 `systemd-inhibit` 兜底在系统总线无 `org.freedesktop.login1` 时诚实地返回 `NotSupported`（锁无人受理就不再假装成功），wakelock 的能力位与 `support_level` 改由 logind / ScreenSaver 实际可达性驱动，可达性受限时报 `Partial(reason)`。
- 🧰 **Honest answers on headless environments (second review round)**: theme detection no longer treats a never-configured `gtk-theme` schema default (WSL's 'Adwaita') as a light-theme answer - only a `-dark` suffix answers Dark, a Light answer requires `dconf read` to prove the key was explicitly set, and everything else falls to `Theme::Unknown`; the wakelock Tier-3 `systemd-inhibit` fallback now fails with `NotSupported` when `org.freedesktop.login1` is absent from the system bus (an unlockable lock is no longer reported as success), and the wake-lock capability bits plus `support_level` are driven by real logind / ScreenSaver reachability, reporting `Partial(reason)` when only a degraded tier is reachable.
- 🧰 **会话能力改为运行时探测**：`LinuxSessionManager::capabilities` 不再硬编码 7 个能力位——现在按 `org.freedesktop.login1`（系统总线）与 `org.freedesktop.ScreenSaver`（会话总线）的实际可达性渲染矩阵：探测分两层，manager 可达给电源位，调用者会话可解析（`ListSessions` 非空 + `GetSession` 对象级校验）才给会话管理位；systemd 已启用的 WSL2 有 daemon 和 PTY 会话但解析不出调用者会话，因此只报电源位；ScreenSaver 可达时 LOCK 独立置位（lock 的 Tier-2）；GNOME/KDE/XFCE 桌面会话管理器可达时 `LOGOUT`/`SESSION_MANAGEMENT` 独立置位（logout 的 Tier-3，探测与执行体同源），覆盖无 logind seat 的传统 X11 桌面。探测结果进程级缓存；`include/uda.h` 的能力位语义说明同步改写。（终审轮进一步把锁屏/注销探测按 `auto` 与 `GetSession("")` 拆分，并覆盖按需激活的桌面 manager，见下。）
- 🧰 **Session capabilities are probed at runtime**: `LinuxSessionManager::capabilities` no longer hardcodes the seven capability bits - the matrix is rendered from the actual reachability of `org.freedesktop.login1` (system bus) and `org.freedesktop.ScreenSaver` (session bus): a reachable manager claims the power actions, and the session-management bits additionally require the caller's session to be resolvable (a non-empty `ListSessions` plus an object-level `GetSession` check); a systemd-enabled WSL2 host runs the daemon with a PTY session in the list yet cannot resolve the caller's session, so it answers with the power bits alone; LOCK is claimed independently whenever the screen saver is reachable (lock's Tier 2), and the logout pair is claimed independently whenever a GNOME/KDE/XFCE desktop session manager is reachable (logout's Tier 3, probed through the same service names the fallback dials), covering traditional X11 desktops without a logind seat session. The probe is cached per process; the capability-bit wording in `include/uda.h` was rewritten to match. (The final review round further split the lock/logout probes onto `auto` vs `GetSession("")` and covered activatable desktop managers - see below.)
- 🧰 **会话动作失败回归 `-2` 契约**：`loginctl`/login1/ScreenSaver 各层的尝试失败（命令非零退出、polkit 拒绝、服务不应答、CLI 缺失）现在全部映射为 `UDA_ERR_NOT_SUPPORTED`（诊断信息保留在消息里），不再以 `CommandFailed` 报 `-5`——`include/uda.h` 承诺的"按 `-2` 降级"从此真实成立。
- 🧰 **Session attempt failures honour the `-2` contract**: attempt failures across the `loginctl` / login1 / ScreenSaver tiers (non-zero exits, polkit refusals, silent services, missing CLIs) now map to `UDA_ERR_NOT_SUPPORTED` with the diagnostic kept in the message, instead of `CommandFailed` reporting `-5` - `include/uda.h`'s "degrade on `-2`" promise now holds.
- 🧰 **示例不再无条件宣称成功**：`07_session`（Python 与 Node）改为先经 `uda_session_capabilities` 实时探测再渲染六项动作的支持矩阵，探测不支持的动作为明确标注"当前环境不支持（探测结果）"并不调用；Node 绑定新增 `UdaError` 类（携带 `status` 状态码）与全部状态码常量导出，宿主可按 `-1 / -2 / -5` 分流，与 Python 绑定的错误语义对齐。
- 🧰 **Examples stop claiming unconditional success**: `07_session` (Python and Node) now probes `uda_session_capabilities` first and renders the six-action support matrix from that result, labelling unsupported actions "not supported in this environment (probed)" and never calling them; the Node binding gains a `UdaError` class (carrying the numeric `status`) plus exported status-code constants, so hosts can branch on `-1 / -2 / -5` exactly like the Python binding.
- 🧰 **示例退出语义对齐**：Node 示例补齐与 Python `_bootstrap.run` 相同的顶层失败呈现——环境不可用时打印一行 `UDA 调用失败: …` 并以退出码 1 结束（此前为未捕获调用栈），07_session 的"探测抛错 → 1、全部不支持 → 0"两侧行为一致。
- 🧰 **Example exit semantics aligned**: the Node examples now render failures exactly like Python's `_bootstrap.run` - one `UDA 调用失败: …` line and exit code 1 when the environment cannot answer (previously an uncaught stack trace), with 07_session agreeing on "probe throws → 1, everything unsupported → 0" on both sides.
- 🧰 **杂项**：空壁纸路径未被拒绝；空字符串字段返回非 NULL 空串（媒体元数据字段例外——本版本为它们保留了「已发布但为空」与「未发布」的可区分性，见 Changed 中的 NULL/空串契约）；Tier-3 壁纸探测不区分桌面环境；dbusmenu 根节点 `GetProperty(0)` 恒 `InvalidArgs` 且缺标准属性；点击回调 panic 后 `on_click` 永久失效；wakelock `expires_at` 死状态无回收；D-Bus 调用无超时上限（5 个模块）；`GET_WALLPAPER` 能力位与实现错位；`uda-cli` 裸 unwrap / 硬编码路径 / 破坏性副作用；Node `new Uda(path)` 参数被丢弃、`_readWallpaper` 双调用、行添加失败泄漏 koffi 槽位；Python 兜底诊断写死状态码 0、非 NULL 空串跳过释放。
- 🧰 **Assorted**: empty wallpaper paths not rejected; empty string fields returning non-NULL pointers (the media metadata fields are the exception - this release keeps "published empty" distinguishable from "unpublished" for them, see the NULL/empty contract under Changed); the Tier-3 wallpaper probe ignoring the desktop environment; `GetProperty(0)` always `InvalidArgs` with standard root properties missing; `on_click` gone for good after a panicking handler; the dead `expires_at` with no reaper; missing D-Bus timeouts across five modules; the `GET_WALLPAPER` capability/implementation mismatch; `uda-cli`'s bare unwraps, hardcoded path and destructive side effects; Node dropping the `new Uda(path)` argument, double-reading the wallpaper and leaking koffi slots on failed row adds; Python reporting status code 0 in fallback diagnostics and skipping the release of non-NULL empty strings.
- 🧰 **锁屏与注销探测按各自执行体拆分（终审轮）**：`LOCK` 与 `LOGOUT` 不再共用同一探测——`LOCK` 跟随 `GetSession("auto")`（logind 自带"回落到该用户 display 会话"语义，`loginctl lock-session` 的真实目标），`LOGOUT`/`SESSION_MANAGEMENT` 跟随 `GetSession("")`（只解析调用者自身会话，systemd 源码语义即如此）；探测与执行从此一一对应，用户服务进程里"有 display 会话但无自身会话"的环境不再虚报注销能力。
- 🧰 **Lock and logout probes follow their own executors (final review round)**: `LOCK` and `LOGOUT` no longer share one probe - `LOCK` follows `GetSession("auto")` (logind's own "fall back to the user's display session" semantics, the true target of `loginctl lock-session`), while `LOGOUT`/`SESSION_MANAGEMENT` follow `GetSession("")` (which resolves only the caller's own session, per systemd's documented behaviour); probe and executor now agree one-to-one, so a user-service process with a display session but no session of its own no longer over-promises logout.
- 🧰 **桌面注销探测覆盖按需激活的服务（终审轮）**：`desktop_logout_manager_present()` 现合并 `ListNames` 与 `ListActivatableNames`——Plasma 的 `org.kde.Shutdown` 是 D-Bus 可激活服务（`kdbusaddons` 生成的 service 文件），进程未启动时不会出现在已持有名单里，只查名单会把真实可用的 KDE 注销回退挡在核心能力门禁之外；两个列表各自失败独立降级，探测绝不主动激活服务。
- 🧰 **The desktop logout probe covers activatable services (final review round)**: `desktop_logout_manager_present()` now unions `ListNames` with `ListActivatableNames` - Plasma's `org.kde.Shutdown` is a D-Bus activatable service (a `kdbusaddons`-generated service file) that is absent from the owned-names list until its process starts, so a names-only probe used to block the perfectly usable KDE logout fallback behind the core capability gate; each listing degrades independently on failure, and the probe never activates a service.
- 🧰 **托盘菜单销毁的回调门（终审轮）**：`uda_tray_menu_destroy` 现为每个菜单维护共享回调门——成功销毁后，任何已克隆进后端快照的行动作都变成静默 no-op（旧实现只摘主状态，Linux/Windows 工作线程镜像里的动作在 200ms 同步窗口内仍可触达宿主函数指针）；若销毁时恰有回调在执行，destroy 返回 `UDA_ERR_INTERNAL` 且菜单保持完整，宿主在回调返回后重试即可（等待放在宿主侧，避免与托盘工作线程互等死锁，回调内自销毁同样安全）；Python/Node 绑定仅在销毁成功后才释放各自的蹦床，`include/uda.h` 契约同步。
- 🧰 **A callback gate for tray menu destruction (final review round)**: `uda_tray_menu_destroy` now keeps a shared callback gate per menu - after a successful destroy, every row action already cloned into a backend snapshot becomes a silent no-op (the old implementation detached only the host state, and actions inside the Linux/Windows worker mirrors stayed reachable for the 200 ms sync window); when a callback is in flight, the destroy reports `UDA_ERR_INTERNAL` and leaves the menu fully intact, so the host retries after its callback returns (the wait lives on the host side, which is what keeps the caller from deadlocking against the tray worker, and a callback destroying its own menu stays safe); the Python and Node bindings release their trampolines only after a successful destroy, and the `include/uda.h` contract says the same.
- 🧰 **Windows 托盘窗口过程生命期重构（终审轮）**：`Worker` 改为堆上钉住、全程裸指针访问的持有对象——消息循环、`Shell_NotifyIconW`、`TrackPopupMenuEx` 模态循环与宿主回调等每个可重入边界处都不再存活任何 `&mut Worker`，窗口过程与各阶段均从裸指针构建短借用；菜单重入守卫改由独立持有的 `AtomicBool` 承担。此前的"两阶段 + `&mut bool` 守卫"拆分仍留有模态循环期间的可变借用别名，本轮以生命周期不变式彻底闭合。
- 🧰 **The Windows tray window-procedure lifecycle rebuilt (final review round)**: the `Worker` is now a heap-pinned owner accessed through raw pointers - no `&mut Worker` is ever alive across a reentrancy boundary (the message pump, `Shell_NotifyIconW`, the `TrackPopupMenuEx` modal loop, host callbacks); the window procedure and every phase build short-lived accesses from the pinned pointer instead, and the menu-reentrancy guard is an independently owned `AtomicBool`. The previous "two-phase + `&mut bool` guard" split still aliased a mutable borrow across the modal loop; this closes that for good with a lifecycle invariant.
- 🧰 **Windows 唤醒锁绑定到持有线程（终审轮）**：`SetThreadExecutionState` 的请求随线程死亡，而 FFI 桥在存在 ambient tokio 运行时会把 acquire 的 future 派到临时工作线程并在返回前 join——请求随桥接线程退出即失效，宿主却拿到 `UDA_OK` 句柄。现在每个锁由专属持有线程承载：acquire 经会合通道上报结果，线程停在释放通道上，释放闭包（或 guard 被 drop 关闭通道）才让它恢复 `ES_CONTINUOUS` 并退出；并发多锁各自独立，互不清除。
- 🧰 **Windows wake locks are pinned to an owner thread (final review round)**: a `SetThreadExecutionState` request dies with its thread, and the FFI bridge runs the acquire future on an ephemeral worker whenever an ambient tokio runtime exists - joining that worker took the request with it while the host kept an `UDA_OK` handle. Each lock now lives on a dedicated owner thread: acquire reports through a rendezvous channel, the thread parks on the release channel, and only the release closure (or a dropped guard closing the channel) lets it restore `ES_CONTINUOUS` and exit; concurrent locks are independent and never clear one another.
- 🧰 **壁纸读取与 hyprpaper 回复语义（终审轮）**：hyprctl 的 IPC 传输成功与 hyprpaper 接受请求是两回事（Hyprland v0.49 对错误回复仍退出 0）——现在专用调用器捕获 stdout、要求 `ok` 回复，被拒后继续 swww 回退并保留诊断；GNOME 读取先按 GVariant 字符串语法解码（转义、`\uXXXX`）、再按 URI 规则还原本地路径（空 authority/`localhost`），远端 authority、畸形百分号编码、NUL 等如实拒绝；FFI 不再重复执行平台已完成的 gsettings 读取（旧路径无超时且解析宽松，会绕过平台层边界），Unity 会话判定移入平台层。
- 🧰 **Wallpaper reads and hyprpaper reply semantics (final review round)**: a successful `hyprctl` IPC transfer is not an accepted request (Hyprland v0.49 exits 0 even when hyprpaper replies with an error) - a dedicated caller now captures stdout and requires the `ok` reply, falling through to swww with the daemon's diagnosis on refusal; GNOME reads decode the GVariant string first (escapes, `\uXXXX`) and then resolve the local file URI (empty authority / `localhost`), rejecting remote authorities, malformed percent escapes and embedded NULs honestly; the FFI no longer re-runs the gsettings read the platform already performed (the old duplicate had no timeout and a lenient parser, bypassing the platform's boundaries), and Unity-session detection moved into the platform layer.
- 🧰 **绑定侧托盘生命期与空回调（第二轮复审）**：Python 的 `TrayMenu.destroy` 失败时不再把整个菜单留在模块级集合里（菜单持有 `Uda`，曾使常亮锁与托盘图标永不释放），改为只钉住 ctypes 蹦床本身，重试成功后再按 identity 摘除；无回调行改传同 `CFUNCTYPE` 类型的空指针（直接传 `None` 会被 ctypes 以 `ArgumentError` 拒绝，"静默行"此前根本无法创建）；Node 的 `TrayIcon.destroy` 失败时先置 `_stopped` 再抛出（`wait()` 不再永久挂起），`Uda.dispose()` 把销毁失败的图标/菜单钉进进程级数组（`keepalives` 是 koffi 蹦床的最后引用，丢掉即 UAF）。
- 🧰 **Binding-side tray lifetimes and null callbacks (second review round)**: a failed Python `TrayMenu.destroy` no longer parks the whole menu in a module-level set (the menu owns `Uda`, so one failed destroy kept wake locks and tray icons from ever being released) - only the ctypes trampolines are pinned, and they are removed by identity after a successful retry. Rows without a callback now pass a null pointer of the same `CFUNCTYPE` type (passing `None` directly is rejected by ctypes with `ArgumentError`, so "silent rows" could not be created at all). On the Node side, a failed `TrayIcon.destroy` sets `_stopped` before rethrowing (`wait()` no longer hangs forever), and `Uda.dispose()` pins icons/menus whose destroy failed in a process-lifetime array (`keepalives` is the last reference to the koffi trampolines; dropping it would be a UAF).
- 🧰 **`uda_tray_set_icon_rgba` 的畸形尺寸改为参数错误**：宽或高为 0、`stride < width * 4`、`width * 4` 溢出这三种调用方违约现在返回 `UDA_ERR_INVALID_ARGUMENT`（`-1`），不再透传 `uda-core` 的 `TrayIcon::set_icon` 返回的 `UDA_ERR_NOT_SUPPORTED`（`-2`）；`len < stride * height` 此前已由 FFI 层返回 `-1`。这些校验同时保留在 `uda-core` 的 `TrayIconSource::validate` 中，FFI 侧这层只负责给出正确的状态码与诊断信息；头文件也不再声称 `data` 可在 `len == 0` 时为 NULL（实现一贯拒绝 NULL）。
- 🧰 **`uda_tray_set_icon_rgba` reports malformed geometry as an argument error**: a zero width or height, `stride < width * 4`, or a `width * 4` overflow now returns `UDA_ERR_INVALID_ARGUMENT` (`-1`) instead of surfacing `UDA_ERR_NOT_SUPPORTED` (`-2`) from `uda-core`'s `TrayIcon::set_icon`; `len < stride * height` already returned `-1` from the FFI layer. The same checks remain in `uda-core`'s `TrayIconSource::validate`, with the FFI layer owning only the status code and the diagnosis; the header also no longer claims `data` may be NULL when `len == 0` (the implementation has always rejected NULL).

### 🧪 Testing

- 🧪 **生成头文件的 C/C++ 回归脚本与符号门禁**：新增 `scripts/test-header.py`，用真实的 C11 / C++11 编译器把生成头文件的契约钉死：`UDA_SESSION_CAP_*` 必须是 32 位无符号移位常量（`UdaStatus` 不得泄漏到 C 侧）、能力位与 `crates/uda-ffi/src/abi.rs` 的规范位图逐位一致、两个回调 typedef 在 C++ 下可直接赋值，以及 `scripts/gen-header.sh` 在 cbindgen 版本不符时于生成前退出且不改动头文件；该脚本已接入 CI 的 Linux job。`cargo test -p uda-ffi` 另有 `the_generated_header_declares_exactly_the_exported_symbols` 门禁：每个 `#[no_mangle]` 导出、`pub UDA_*` 常量与回调 typedef 都必须与 `include/uda.h` 双向一致，因此漏跑 `gen-header.sh` 在本地测试就会红灯，而不只依赖 CI 的重新生成 diff。
- 🧪 **A C/C++ regression script and a symbol gate for the generated header**: a new `scripts/test-header.py` compiles real C11 / C++11 assertions against the generated header - `UDA_SESSION_CAP_*` must stay 32-bit unsigned shift constants (`UdaStatus` must not leak into C), the capability bits must match the canonical map in `crates/uda-ffi/src/abi.rs` bit for bit, both callback typedefs must be assignable from C++, and `scripts/gen-header.sh` must exit before generating (leaving the header untouched) when the cbindgen version does not match; it runs in the Linux CI job. `cargo test -p uda-ffi` also carries `the_generated_header_declares_exactly_the_exported_symbols`, which compares the header against the crate's sources in both directions - every `#[no_mangle]` export, `pub UDA_*` constant and callback typedef must appear on both sides - so forgetting `gen-header.sh` fails a plain local test run, not only the CI regenerate-and-diff step.

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

[v0.2.2]: https://github.com/UniDesktop/SDK/compare/v0.2.1...HEAD
[v0.2.1]: https://github.com/UniDesktop/SDK/releases/tag/v0.2.1
[v0.2.0]: https://github.com/UniDesktop/SDK/releases/tag/v0.2.0
[v1-alpha1]: https://github.com/UniDesktop/SDK/releases/tag/v1-alpha1
