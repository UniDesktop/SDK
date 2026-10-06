# GOTCHAS — 踩坑记录与隐性规范

> **性质**：本文件不是设计文档，而是**事故备忘录**。`*_specs.md` 记载"应该怎么做"，
> 本文件记载"没想到会这样"和"别再这么干"。
>
> **来源**：全部条目来自实际会话中的真实踩坑、源码交叉核对与使用者纠偏，非凭空编写。
> 每条都标明出处，便于回溯验证。
>
> **阅读对象**：后续接手本仓库的 AI Agent 与人类贡献者。改动涉及对应模块前先读这里。

---

## 目录

- [1. 人类纠偏与红线](#1-人类纠偏与红线)
- [2. 操作系统底层踩坑记录](#2-操作系统底层踩坑记录)
- [3. 工程与测试默契](#3-工程与测试默契)
- [4. 未完成的隐患与待办](#4-未完成的隐患与待办)

---

## 1. 人类纠偏与红线

> 使用者在会话中明确说"不要"、"严禁"、"为什么这么做"的地方。踩到即返工。

### 1.1 严禁用测试伪造事实

- **严禁 `assert!(true)` / 空测试体**冒充"已实现测试"。测试必须真正断言行为。
- **严禁为了让测试通过而放宽断言**。断言是契约，不是可调参数。
- 来源：项目测试约定；`crates/uda-platform-linux/src/tray.rs:23` 明确要求模块内
  `No unwrap(), expect(), panic!, unreachable!()`——同样适用于测试代码的意图。

### 1.2 严禁凭空推断"历史约定"

- **踩坑实例**：升级 v0.2.1 时，我看到 `git show v0.1.0:` 与 `git show v0.2.0:` 里
  crate 的 `Cargo.toml` 都是 `0.1.0`，据此推断"本仓库约定 Cargo.toml 不跟随发布版本"，
  于是**没有**改 crate 版本号。
- **使用者纠正**："同步更新 crate 版本号至 v0.2.1（此前 v0.2.0 发布时遗漏）"——
  这是**发布事故**，不是约定。
- **教训**：当使用者明确指出某处"缺失/遗漏"时，那是待修复的缺陷；不要用 git 历史
  为其编造合理性。历史只能解释过去，不能豁免当下。

### 1.3 严禁在文档里写与代码不一致的内容

- 使用者反馈的 issue 原话精神：README 与 README_CN 的平台支持矩阵必须与 Wiki **完全一致**；
  AGENTS.md、`uda-core/src/error.rs`、Wiki 三处对 support level 的表述必须统一。
- **规则**：文档出现歧义时，**以深入调研 SDK 真实代码为唯一依据**；调研后仍无法判定，
  才停下来提问。禁止擅自作决定，禁止提交模棱两可的修改。
- **权威链**：平台支持矩阵以 **Wiki 为唯一权威**，README（含 README_CN）据此修正。

### 1.4 严禁擅自扩大改动范围

- 使用者布置任务 A，就只交付任务 A。发现邻近问题（如双语不同步）应**先报告**，
  得到指示后再动手。
- **踩坑实例**：我只更新了 `en/reference/python-sdk.md` 的版本串，漏了中文版。
  使用者问"是因为原本双语不同步吗"——确实是历史遗留，但正确做法是先问再做，
  而不是按自己判断跳过。得到"补齐中文版缺失的 4 节（取并集原则）"的指示后才动手。

### 1.5 严禁用破坏性命令删文件

- 严格禁止 `rm` / `rmdir` / `del` / `erase` / `Remove-Item` / `git clean -fd`。
- 需要删除时一律用 `trash <path>`（进回收站，可恢复）；清理未跟踪文件用
  `git safe-clean` 或 `git stash --include-untracked`；删除已跟踪文件用
  `git rm --cached <file>` 后 `trash <file>`。
- 来源：`file-deletion-policy-strict`，违反会触发人工安全闸口，中断自动化流程。

---

## 2. 操作系统底层踩坑记录

### 2.1 Linux 托盘：`IconPixmap` 的字节序与行序

**出处**：`crates/uda-platform-linux/src/tray.rs:130`、`docs/internals/tray_specs.md:82`

SNI 的 `IconPixmap` 类型签名是 `a(iiay)`，字面写作 `[width, height, arggb_bytes]`。
这是**文档措辞与实际不符**的经典陷阱：

- **不是** `A, R, G, B` 的**字节**顺序，而是 **B, G, R, A**。写成 A,R,G,B 会让
  红色与蓝色互换，测试 `red_and_blue_are_not_swapped` 专门守这条。
- **行序是 bottom-up**（源第 0 行落到目标末尾），列内顺序保持不变。
- **必须处理 `stride`**（每行字节数，`>= width * 4`）。若 `stride` 大于像素宽度，
  多出的填充字节要**跳过而非拷贝**——拷进去会读到 `0xAA/0xBB/0xCC` 之类的脏值。
- **不做预乘 alpha**：shell 不做 premultiply，所以全透明像素必须保留其 RGB 通道。
  测试 `a_fully_transparent_pixel_keeps_its_colour_channels` 守这条。

缓冲区校验必须是"硬失败"而非 panic：`stride < width * 4`、`data.len() < stride * height`、
零尺寸、`Path` 类型（无像素）等情况一律返回 `None`，让上层降级到 `image-missing` 占位名。

### 2.2 Linux 托盘：SNI 的 `Menu` 属性必须是对象路径

**出处**：`crates/uda-platform-linux/src/tray.rs:1662` 测试注释

宿主通过 SNI 的 `Menu` 属性发现菜单。**一个与路径文本相同的字符串不是对象路径**——
如果返回 `s` 而不是 `o`，Plasma 会回退到 `ContextMenu()`，菜单直接失效。

测试用 `zbus` 的 `D-Bus Properties.Get` 实拉一次，断言
`value.value_signature().as_str() == "o"`，再尝试转成 `zvariant::ObjectPath`。

### 2.3 Linux 托盘：`Menu` 属性的上下文菜单坐标被丢弃

**出处**：`crates/uda-platform-linux/src/tray.rs:640`

SNI 的 `ContextMenu(x, y)` / `Scroll` / `SecondaryActivate` 在 UDA 跨平台模型里没有
对应语义：dbusmenu 自己携带几何信息、由 shell 渲染，所以这两个方法只记日志。
不要误以为漏实现了交互。

### 2.4 Linux 托盘：双击是"合成"的，不是真实信号

**出处**：`crates/uda-platform-linux/src/tray.rs:41`、`1179`

SNI 协议**没有**双击信号。UDA 把窗口期内（复刻 `GetDoubleClickTime()`）的两次
`Activate` 合成为 `TrayEvent::DoubleClick`。

**因此 Linux 绝不广告 `TRAY_DOUBLE_CLICK` 能力位**——宣示了就是撒谎，
违反"诚实能力"契约。Windows 有真实 `WM_LBUTTONDBLCLK`，是矩阵里唯一可以诚实宣示它的后端。

合成窗口在每次激活后**重置**，所以三连击读作"单击 + 双击"，而不是"一次长双击"。

### 2.5 Linux 托盘：dbusmenu 布局签名固定为 `(ia{sv}av)`

**出处**：`crates/uda-platform-linux/src/tray.rs:325`、`1609`

`MenuNode = (i32, OwnedProps, MenuChildren)`，wire signature 必须是 `(ia{sv}av)`。
测试直接断言 `MenuNode::signature()` 并逐个子节点检查 `value_signature()`，
防止嵌套子菜单签名漂移。

- **根 id 固定为 0**（协议哨兵值），真实行从 1 开始单调分配。
- **非递归请求也必须点名子节点**，这样 shell 才能按需请求子菜单。
- 没有挂菜单时返回**空根**而不是报错——让 shell 什么都不画，好过抛错。

### 2.6 Linux 托盘：无菜单时 `GetProperty` 返回 `InvalidArgs`

**出处**：`crates/uda-platform-linux/src/tray.rs:852`

未挂菜单时调 `GetProperty` / `GetGroupProperties` 会得到 `InvalidArgs`，
未知 item id 或未知属性名同样返回 `InvalidArgs`。
**未知 id 要被跳过而非回报空条目**（测试 `group_properties_filters_to_the_requested_names` 守这条）。

### 2.7 Linux 托盘：dbusmenu 事件必须串行

**出处**：`crates/uda-platform-linux/src/tray.rs:904`

`event_group` 的批量事件**必须顺序应用**。并发执行会打乱顺序，
dbusmenu 依赖事件有序。回调在状态锁**释放之后**调用，
这样宿主可以在自己的回调里改图标/菜单而不会死锁。

### 2.8 Windows 托盘：`lParam` / `wParam` 的打包规则

**出处**：`crates/uda-platform-windows/src/tray.rs:1146`

`NOTIFYICON_VERSION_4` 下，这是最容易踩的 Win32 坑：

| 字段 | 内容 |
|---|---|
| `lParam` **低 16 位** | 鼠标消息（`WM_LBUTTONUP` 等） |
| `lParam` **高 16 位** | 图标 id（`NOTIFYICONDATAW::uID`） |
| `wParam` | 光标**屏幕坐标**，`x` 在低字、`y` 在高字，各为有符号 16 位 |

**`wParam` 不是 id。** 拿它跟 `icon_id` 比，永远不相等——症状就是
"图标出现了但点击毫无反应"。

`wParam` 仍要转发给 `show_menu`，因为弹窗需要锚点坐标。
部分 shell 对键盘调用（`Shift+F10`）会给出 `(0, 0)`，此时应回退到实时
`GetCursorPos`，而不是把菜单钉在屏幕原点。

### 2.9 Windows 托盘：消息泵与线程归属

**出处**：`crates/uda-platform-windows/src/tray.rs:12`

托盘图标在 Windows 上**是一个窗口而非对象**：shell 往 `NOTIFYICONDATAW` 记录的
`hWnd` 投递回调消息，所以那个窗口必须属于一个跑消息循环的线程。

- UDA 在**专用 worker 线程**上建 `HWND_MESSAGE` 消息窗口，**绝不碰宿主的消息队列**。
- 宿主不得被要求自己泵消息。
- `Shell_NotifyIconW`、`CreatePopupMenu`、`TrackPopupMenuEx` 全部必须在拥有窗口的
  线程上调用；Linux 后端那套"共享状态镜像"在 Windows **不适用**，宿主改动用用户消息
  **转发**给 worker，在 worker 自己的锁下应用。
- 每个 unsafe 块都要带 `// SAFETY:` 注释说明依赖的不变量；Win32 调用上
  **禁止** `unwrap()/expect()/panic!()/unreachable!()`。

### 2.10 Windows 托盘：command id 与菜单焦点舞蹈

**出处**：`crates/uda-platform-windows/src/tray.rs:476`、`1220`

- `WM_COMMAND` 只带 **16 位 id**，所以维护 `id -> row` 表，**每次菜单变更就重建**
  （保证过期 id 绝不会触发过期回调）。id 从 **1** 起分配，`0` 留给
  `TPM_RETURNCMD` 的"用户取消"返回值。
- 菜单**焦点舞蹈是强制的**：没有 `SetForegroundWindow`，弹窗不会因外击而关闭；
  结尾没有 `WM_NULL`，某些 shell 会让菜单卡在打开状态。
- 禁用项必须同时给 `MF_DISABLED` 和 `MF_GRAYED`——前者只是不触发，后者才真正变灰。
- **`NIM_DELETE` 必须在销毁窗口之前**，否则 shell 持有悬空 `hWnd`，可能显示
  幽灵图标直到注销。

### 2.11 Windows 托盘：tooltip 的 127 字符上限是跨平台共用的

**出处**：`crates/uda-core/src/tray.rs:79`

`NOTIFYICONDATAW::szTip` 是内联 `[u16; 128]`，**含 NUL 终止符**，所以上限是 **127**。
这个数字被提升为跨平台常量 `TOOLTIP_MAX_CHARS = 127`，Linux 侧同样遵守。
截断必须按 **char 边界**而非字节，否则会切断 UTF-8 代理对。

### 2.12 Windows 通知：裸路径必须转成 `file://` URI

**出处**：`crates/uda-core/src/notification.rs:60`、`crates/uda-platform-windows/src/notification.rs:42`

`toast` 模板的 `<image src>` 属性是在 **shell 的上下文**里解析的，不是发送方的工作目录。
所以 `C:\pics\a.png` 即使文件就在那儿也可能定位失败——必须规范化成 `file:///C:/pics/a.png`。

- **已带 scheme 的值原样通过**，否则宿主自己构造的 URI 会被二次前缀成 `file:///file:///...`。
- **`has_uri_scheme` 的判定规则是坑**：RFC 3986 要求 scheme 以字母开头，而驱动器号
  只以 `C:` 两字符形式出现，所以规则是"**冒号前至少两个字符且全为合法 scheme 字符**"
  ——正是这条把 `C:/pics` 留在"需要转换"的路径上。
- **只判语法，不检查存在性**：拼错路径不该是硬失败（平台自己遇到读不了的图就是"画一张没图的卡片"）。

**UNC 网络路径注意**：`\\server\share\a.png` 这种双反斜杠前缀必须先转成正斜杠，
且其前两个字符 `\` 不是合法 scheme 字符，会被正确判为"需转换"，产出
`file://server/share/a.png`。这一段没有专门测试覆盖，见 §4.1。

### 2.13 Windows 通知：模板选择跟着图标走

**出处**：`crates/uda-platform-windows/src/notification.rs:162`

有 `app_icon` 用 `ToastImageAndText02`（带 `<image>` 节点），没有就降级到
`ToastText02`（无 `<image>` 节点）。

**不能对 `ToastText02` 调 `SelectSingleNode("/toast/visual/binding/image")`**——
XPath 匹配不到任何东西，整个通知发送会失败。`IMAGE_XPATH` 只填 image 模板。

### 2.14 Windows 通知：AUMID 是进程级一次性状态

**出处**：`crates/uda-platform-windows/src/notification.rs:229`

`SetCurrentProcessExplicitAppUserModelID` 是让未打包 Win32 进程收到 toast 的
正规途径，但：

- **首次调用即登记，之后不再覆盖**。宿主在 UDA 之前已注册过自己的 AUMID，
  就必须保留它（覆盖会破坏其激活路由）。用 `GetCurrentProcessExplicitAppUserModelID`
  先探测，非空就保留并用 `OnceLock` 记住。
- **空/纯空白名不能注册**（Win32 拒绝），必须先替换成
  `FALLBACK_AUMID = "UniDesktop.Notification"`。用点分反写域名形式，避免裸词与别家应用冲突。
- `create_notifier` 失败的原因通常是"进程没有可解析的 toast 身份"，
  报错信息里已带 `ELEMENT_NOT_FOUND` 的说明。

### 2.15 壁纸后端**从不**调用 Portal

**出处**：`crates/uda-platform-linux/src/wallpaper.rs:13`、AGENTS.md Principle 2 例外说明

四级降级链是**通则而非铁律**。壁纸模块**完全绕过 Tier 1**（`org.freedesktop.portal.Wallpaper`
存在但从未被调用），从 Tier 2 起：

1. GNOME：`gsettings` CLI（`org.gnome.desktop.background`，`picture-uri` / `picture-uri-dark`）
2. KDE Plasma：D-Bus `org.kde.plasmashell` → `/PlasmaShell` → `evaluateScript`
3. Hyprland / Sway：`hyprpaper` / `swww` CLI
4. X11：`feh` / `nitrogen` CLI
5. 都没有：`UdaError::NotSupported`

同理**唤醒锁只用 `org.freedesktop.ScreenSaver.Inhibit`**，不用
`org.freedesktop.portal.Inhibit`。外观检测（`org.freedesktop.portal.Settings` →
`Read("org.freedesktop.appearance", "color-scheme")`）是**目前唯一**走 Tier 1 的模块。

### 2.16 壁纸的文件 URI 转义是手工白名单

**出处**：`crates/uda-platform-linux/src/wallpaper.rs:27`

Linux 侧 `file_uri` 只转义 `%`、空格、`#`、`?`（顺序上 `%` 必须最先替换，否则二次转义）。
**这不是完整的 URI 百分号编码**——测试只覆盖了这几个字符。见 §4.2。

### 2.17 唤醒锁在 Windows 上不是引用计数的

**出处**：`crates/uda-platform-windows/src/wakelock.rs:13`

`SetThreadExecutionState` 是**线程级、进程范围的状态，且每次调用整体替换前一状态**，
不是带引用计数的句柄：

- `ES_CONTINUOUS` **必须**在场，请求才会持续到下一次调用；缺了它只保一个挂起周期。
- 释放 = 恢复默认 `ES_CONTINUOUS`，**影响整个进程**。所以 UDA 明确把它做成
  **单锁模型**并写进文档，而不是悄悄堆叠多个 guard。
- guard **不可 Clone**，且**不该跨线程释放**（线程作用域）：在别的线程释放，
  清不掉真正持锁那个线程上的标志。它仍然是 `Send`，以便移到持锁线程上。
- 返回值是**先前状态**，`0` 表示失败（只发生在"从电源通知回调里改线程状态"时）。

Linux 侧相反：`ScreenSaver.Inhibit` 返回一个 **cookie**，据此 `UnInhibit`，
是可叠加的。

### 2.18 主题检测的兜底必须是 `Unknown` 而非 `Light`

**出处**：`crates/uda-platform-linux/src/appearance.rs:22`

在平铺窗口管理器（Hyprland / Sway / X11 裸环境）上，**没有任何桌面组件发布配色方案**。
此时诚实的回答是"无法确定"（`Theme::Unknown`）。

返回 `Theme::Light` 等于告诉宿主一件后端根本不知道的事——这正是 v0.2.1 修掉的缺陷。

注意 `Theme::Unknown` 与 `Theme::Auto` 是**两个不同的东西**：`Auto` 是"跟随系统"的偏好，
`Unknown` 是"没能判定"。C ABI 里两者都映射到 `UDA_THEME_UNKNOWN = 0`。

---

## 3. 工程与测试默契

### 3.1 三条测试命令，一条不能少

每次代码改动必须全部通过：

```bash
cargo check --workspace --all-targets    # 编译，零告警零错误
./scripts/test-linux-mock.sh             # 隔离 D-Bus 会话里跑 Linux 模块测试
cargo test -p uda-ffi                    # C ABI 契约测试
```

`scripts/test-linux-mock.sh` 内部就是 `dbus-run-session -- cargo test -p uda-platform-linux`。

### 3.2 Linux D-Bus 测试必须跑在 `dbus-run-session` 里

**出处**：`crates/uda-platform-linux/src/tray.rs:1628` 测试注释、`scripts/test-linux-mock.sh`

测试 `get_layout_survives_session_bus_transport` 的注释说得很清楚：
**本地消息往返抓不到"总线上因载荷非法而直接断开连接"这类错误**，
必须连上一个真实的 session bus 才能暴露。

`dbus-run-session` 创建一个临时空 D-Bus 实例，测完自动销毁。**没有它，测试等于没测。**

### 3.3 Windows 宿主上有一条已知的环境性失败

**踩坑实例**：`get_layout_survives_session_bus_transport` 在 Windows 开发机上报
`Error: Address("Unable to open shared memory")`。

已用 `git stash` 验证：**改动前改动后都失败，是环境既有问题，不是回归**。
根因是该机器没有 D-Bus session bus。用 `dbus-run-session` 也无效（退出码 127，未安装）。

**汇报测试结果时必须区分"我的回归"与"宿主环境限制"**，不要拿环境失败当挡箭牌，
也不要把环境失败算到自己头上。Linux CI 上这条是通过的。

### 3.4 跨编译卫生：Windows 代码必须 `#[cfg(windows)]` 隔离

`crates/uda-platform-windows` 整体加 `#![cfg(windows)]` 守卫，
保证 Linux 开发者执行 `cargo check --workspace` 永不因 Windows 代码断裂。
在 Linux 侧新增代码时，**不得**引用任何 Windows 专属类型或 API。

### 3.5 能力必须诚实：`Partial` 要带原因

**出处**：`crates/uda-core/src/capability.rs`

v0.2.1 起 `SupportLevel` 收窄为三态，且 `Partial` **携带降级原因**：

```rust
pub enum SupportLevel {
    None,
    Partial(String),   // 携带人类可读的降级原因
    Full,
}
impl SupportLevel {
    pub fn reason(&self) -> Option<&str> { /* ... */ }
}
```

**为什么**：宿主需要向用户解释"为什么这个功能是残的"，不该被迫去猜。
没有原因的 `Partial` 等于什么都没说——这正是 v0.2.1 补上的契约。

`SupportLevel` **不再派生 `Copy`**（`String` 不可 `Copy`）——改动它时注意调用点。

### 3.6 错误模型只用五个变体

**出处**：`crates/uda-core/src/error.rs`

```rust
pub enum UdaError {
    NotSupported(String),
    DetectionFailed(String),
    CommandFailed(String),
    Io(#[from] std::io::Error),
    Internal(String),
}
```

**禁止**在文档或代码里发明 `FeatureUnsupported` / `Unsupported` / `Restricted` /
`DBusError` / `IoError` ——这些都不存在。AGENTS.md Tier 4 的正确写法是
`UdaError::NotSupported`。

### 3.7 锁中毒要恢复而不是传播

**出处**：`crates/uda-platform-linux/src/tray.rs:73`

托盘状态是**按字段重写的纯数据**，某个回调里 panic 之后，继续服务严格好过
让此后每次更新全部失败。所以用 `lock_or_recover()` 而不是 `?` 传播毒化。
两端的托盘后端都遵循同一约定。

### 3.8 测试必须覆盖"降级路径"而不只是"正常路径"

从现有测试命名可以看出本项目的取向：`an_invalid_icon_degrades_instead_of_panicking`、
`conversion_rejects_inconsistent_buffers`、`a_worker_sees_the_host_drop_as_shutdown`、
`bus_names_are_unique_and_sanitised`、`advertised_capabilities_omit_double_click`。

新增测试时应自问：**这条守的是哪一条契约？** 名字要能直接回答这个问题。

### 3.9 示例数据里的版本串要跟发布走

`examples/python/uda.py` 的通知示例文案曾硬编码 `"v0.2.0 已发布"`，v0.2.1 时同步改为
`"v0.2.1 已发布"`。**发布新版本时，示例代码与示例数据里的版本串一并更新**——
它们会出现在文档引用里，对不上很显眼。

### 3.10 双语文档遵循"取并集"原则

**出处**：本次会话使用者指示"补齐中文版缺失的 4 节，与英文版对齐（取并集原则）"

中文页从英文页取并集补齐，而不是删英文页的多余内容。节标题允许语义等价的译名差异
（`Constants` → `常量类`、`Loading the library` → `安装与导入`、
`Media and session` → `媒体与会话`、`See also` → `相关文档`），
但**章节集合必须一致**。

行内链接两种语言用各自约定：中文页用**不带 `/en/` 前缀**的根相对路径
（`/reference/status-codes/`），英文页带 `/en/` 前缀。**不要互相"纠正"对方的前缀。**

### 3.11 Starlight 站点结构与页数校验

- `defaultLocale: 'root'`；**中文在 `src/content/docs/` 根下，英文在 `src/content/docs/en/` 下。**
- 完整站点应为 **51 个 HTML 页面**。构建后数字不对，说明有页被漏加或误删。
- 校验双语对齐的可靠做法：`grep -an "^## " <file>` 数两侧 `##` 标题数，
  再去 `dist` 里确认新章节真的落盘、新链接真的解析成功。

### 3.12 版本号引用要区分"发布版本"与"历史陈述"

v0.2.1 升级时：**发布版本引用**（徽章、`package.json`、crate `version` 字段）统一改；
**历史陈述**（"Fixed in v0.2.0"、"Phase 2 shipped in v0.2.0"、路线图里的 "v0.1.0 RELEASED"）
**必须原样保留**。不要一把 `sed` 全替换——那会篡改历史。

### 3.13 环境特定操作系统的 CLI 注意事项

**出处**：本会话实际操作经验

- **`node` 在 git bash 里被 alias 成 `winpty node.exe`**，会要求 TTY 并吞掉管道 stdout。
  需要捕获输出时用全路径 `"/c/Program Files/nodejs/node.exe"`。
- **`findstr` / `dir` 的反斜杠路径在 git bash 里会被吃掉**。用带正斜杠的 `grep` 替代。
- **cmd.exe 下调 `sed`/`grep`/`awk`/`cat`/`rm`** 不可用，用 `type`/`find`/`findstr`，
  或整体改用 PowerShell。
- 跨仓库操作时注意 `git -C <repo>` 指向真正的仓库根（`wiki/` 是 SDK 仓库内的**子目录**，
  不是独立 git 仓库）。

---

## 4. 未完成的隐患与待办

> 本次会话中察觉到、但**未彻底根治**的微小缺陷。每条都标注了复现方式与建议处置。

### 4.1 UNC 网络路径的 toast 图标缺乏测试覆盖

- **现象**：`image_source(r"\\server\share\a.png")` 的逻辑路径会先转成正斜杠，
  再因前两字符非合法 scheme 字符而进入 `file_uri` 分支，产出
  `file://server/share/a.png`。**但没有任何测试覆盖这条输入。**
- **复现**：给 `uda-core` 加一个断言 `\\server\share\a.png` → `file://server/share/a.png`
  的测试，看它是否与预期一致。
- **建议**：补测试；若实际行为不符合预期，在 `uda-core/src/notification.rs` 里修 `file_uri`。
- **优先级**：低（UNC 场景罕见，且失败也只是"没有图标"这种降级）。

### 4.2 Linux `file_uri` 的百分号编码不是完整实现

- **现象**：`crates/uda-platform-linux/src/wallpaper.rs:32` 只替换 `%`、空格、`#`、`?`。
  其余需要转义的 URI 字符（如 `[`、`]`、非 ASCII 的 CJK 之外的某些字符）没有处理。
- **复现**：构造一个含 `&` 或 `+` 等字符的壁纸路径，观察生成的 URI。
- **建议**：要么补全成完整的 percent-encoding，要么在文档里明确这是**有限白名单**。
  前者更彻底，但需评估现有测试 asserted 的具体输出是否会变。
- **优先级**：低（受影响的字符组合罕见；且 `&`/`+` 在 fragment 解析之外通常无害）。

### 4.3 `get_layout_survives_session_bus_transport` 依赖外部 `dbus-run-session`

- **现象**：该测试在无 systemd/dbus 的开发机（如纯 Windows 宿主）上无法运行，
  只能依赖 Linux CI。本地开发者若在容器外跑，会误判为失败。
- **建议**：考虑用 `#[cfg(all(test, target_os = "linux"))]` 之外的方式做环境探测，
  或在测试里 skip 并给出明确提示；至少保证 CI 覆盖不打折。
- **优先级**：中（影响本地开发体验，不影响正确性保证）。

### 4.4 Wiki 仓库与 SDK 仓库的版本发布节奏需手工对齐

- **现象**：SDK 发布 v0.2.1 时，`wiki/docs_repo/Website/package.json` 与
  `package-lock.json` 需人工同步；本次是两个独立提交。
- **建议**：若希望原子化，可在 SDK 仓库加一个脚本同时 bump 两侧；或至少在
  发布 checklist 里写明"别忘了 wiki"。
- **优先级**：低（一次性手工操作，出错可见）。

### 4.5 平台支持矩阵仍靠人工与源码保持一致

- **现象**：本次修正暴露了 README/Wiki/AGENTS.md 三处矩阵与 support level 表述
  曾互相矛盾。目前已统一，但**没有任何机制阻止它们再次漂移**。
- **建议**：考虑从 `Capability` 位标志与各后端 `support_level()` 实现生成一份
  机器可读的能力表，文档与 README 从它渲染。这是根治，但工作量不小。
- **优先级**：中（这类文档漂移已经发生过一次，且用户以 issue 形式反馈）。

### 4.6 Windows 托盘图标与菜单变更通过轮询同步

- **出处**：`crates/uda-platform-windows/src/tray.rs:845`（`SYNC_INTERVAL_MS = 200`）

宿主对 tooltip / icon / menu / visible 的修改，worker 通过 200 ms 定时器**轮询**发现，
而不是即时推送（仅 shutdown 走共享标志）。两侧对齐了 Linux 轮询间隔以保持一致延迟。

**潜在问题**：宿主若在 200 ms 内连续改两次，中间状态可能被跳过（看到的是最新值，
通常无害，但若某次改动被设计成"闪烁一次"之类的瞬时效果，会丢失）。

**建议**：若要消灭轮询，需引入真正的变更通知（如 Windows 事件）——工作量中等，
且要保证 Drop 路径仍然是"纯标志写入"而不能变成可能阻塞的跨线程发送。

---

## 附：快速对照表

| # | 一句话结论 |
|---|---|
| 1.1 | 测试不许伪造，也不许为了让绿而放宽断言 |
| 1.2 | 别用 git 历史把"缺陷"论证成"约定" |
| 1.3 | 文档与代码冲突时，以深挖 SDK 代码为唯一依据 |
| 1.4 | 任务 A 就只交付 A；发现别的问题先报告 |
| 1.5 | 删文件只用 `trash`，禁 `rm`/`del`/`git clean -fd` |
| 2.1 | SNI `IconPixmap` 字节序是 B,G,R,A，行序 bottom-up，要处理 stride |
| 2.2 | SNI `Menu` 属性必须返回对象路径（`o`），不是字符串 |
| 2.4 | Linux 托盘双击是合成的，绝不广告 `TRAY_DOUBLE_CLICK` |
| 2.5 | dbusmenu 布局签名固定 `(ia{sv}av)`，根 id 为 0 |
| 2.8 | Win32 V4 托盘 `wParam` 是光标坐标，不是 id |
| 2.9 | Win32 托盘是窗口，必须专用线程跑消息泵 |
| 2.10 | command id 从 1 起；焦点舞蹈强制；`NIM_DELETE` 先于销毁窗口 |
| 2.11 | tooltip 127 字符跨平台共用，按 char 边界截断 |
| 2.12 | toast 图标裸路径必须转 `file://`；UNC 路径无测试覆盖（§4.1） |
| 2.13 | `ToastText02` 上没有 `<image>` 节点，别去 SelectSingleNode |
| 2.14 | AUMID 进程级首次登记即锁定，空名回退到通用身份 |
| 2.15 | 壁纸与唤醒锁**不走 Portal**，从 Tier 2 起；外观检测是唯一 Tier 1 |
| 2.16 | Linux 壁纸 URI 转义是有限白名单，不是完整编码（§4.2） |
| 2.17 | Windows 唤醒锁非引用计数、单锁、线程作用域 |
| 2.18 | 主题兜底是 `Unknown`，不是 `Light`；`Unknown` ≠ `Auto` |
| 3.1 | 三条测试命令一条不能少 |
| 3.2 | Linux D-Bus 测试必须在 `dbus-run-session` 里跑 |
| 3.3 | Windows 宿主上那条 D-Bus 失败是既有环境问题，别算成回归 |
| 3.4 | Windows 代码 `#[cfg(windows)]` 隔离，Linux 上不引用 Win32 类型 |
| 3.5 | `Partial` 必须带原因；已不再派生 `Copy` |
| 3.6 | `UdaError` 只有五个变体，别发明新的 |
| 3.7 | 托盘锁中毒要恢复而非传播 |
| 3.8 | 测试要守降级路径，名字要说清守哪条契约 |
| 3.9 | 发布时示例代码里的版本串一并更新 |
| 3.10 | 双语文档取并集；中英文各自保留自己的链接前缀风格 |
| 3.11 | 中文在 docs 根、英文在 docs/en；完整站点 51 页 |
| 3.12 | 版本替换只改"发布版本引用"，历史陈述原样保留 |
| 3.13 | git bash 里 `node` 走 winpty、反斜杠会被吃、Unix 工具不可用 |
| 4.1 | UNC 网络路径 toast 图标缺测试 |
| 4.2 | Linux 壁纸 URI 百分号编码不完整 |
| 4.3 | 跨主机 D-Bus 测试受 `dbus-run-session` 可用性限制 |
| 4.4 | 两侧版本号靠人工对齐，暂无脚本 |
| 4.5 | 能力矩阵暂无机器可读单一来源，可能再次漂移 |
| 4.6 | Windows 托盘靠 200ms 轮询同步，瞬时状态可能被跳过 |
