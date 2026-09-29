# Session & Power Lifecycle Backend Specifications

跨平台会话与电源生命周期管理的协议字典。UDA 把它收敛成
[`SessionManager`](../../crates/uda-core/src/session.rs) trait 与六个 C-ABI
入口；本文记录两端各自"怎么做到"，是唯一事实来源。

> **安全前提**：六个动作里五个会结束用户会话或停止机器。本模块把
> `capabilities()` 设计成**静态、无副作用**的查询，把六个动作设计成"返回
> `Ok` 的瞬间就已不可撤销"。只有 `Lock` 可以自动化，其余必须由宿主应用自己
> 弹确认框。自动化测试**严禁**触发真实电源动作，见
> §7 测试红线。

## 1. 模型映射（先看这里）

| UDA 概念 | Linux | Windows |
|---|---|---|
| `SessionAction::Lock` | `org.freedesktop.ScreenSaver.Lock()`（session bus）→ `loginctl lock-session` | `user32!LockWorkStation()` |
| `SessionAction::Logout` | `org.freedesktop.login1.Manager.TerminateSession("")` → DE SessionManager | `ExitWindowsEx(EWX_LOGOFF, 0)` |
| `SessionAction::Suspend` | `org.freedesktop.login1.Manager.Suspend(false)` | `powrprof!SetSuspendState(false, false, false)` |
| `SessionAction::Hibernate` | `org.freedesktop.login1.Manager.Hibernate(false)` | `powrprof!SetSuspendState(true, false, false)` |
| `SessionAction::Reboot` | `org.freedesktop.login1.Manager.Reboot(false)` | 提权 `SeShutdownPrivilege` → `ExitWindowsEx(EWX_REBOOT \| EWX_FORCEIFHUNG, 0)` |
| `SessionAction::Shutdown` | `org.freedesktop.login1.Manager.PowerOff(false)` | 提权 `SeShutdownPrivilege` → `ExitWindowsEx(EWX_POWEROFF \| EWX_FORCEIFHUNG, 0)` |
| `Capability::SESSION_MANAGEMENT` | logind 或 `loginctl` 可用 | Win32 会话/电源 API 存在 |
| `Capability::LOCK` / `LOGOUT` / ... | 对应代码路径存在 | 对应代码路径存在 |

### 1.1 能力位（ABI 稳定）

位号与数值属于 C ABI 的一部分（见 `include/uda.h`），**不得重排**：

| 能力 | 位 | 值 |
|---|---|---|
| `SESSION_MANAGEMENT` | 16 | `0x00010000` |
| `LOCK` | 17 | `0x00020000` |
| `LOGOUT` | 18 | `0x00040000` |
| `SUSPEND` | 19 | `0x00080000` |
| `HIBERNATE` | 20 | `0x00100000` |
| `REBOOT` | 21 | `0x00200000` |
| `SHUTDOWN` | 22 | `0x00400000` |

**未实现的动作绝不能暴露与已实现动作相同的能力位**——否则调用方会在桌面上
画出一个点了没反应的按钮。`uda_core::session::perform()` 是唯一闸门：
先比对能力位，缺位即返回 `UdaError::NotSupported`，**根本不会调到后端**。
这让"平台不支持"与"试了但失败"可区分，也让"一个能力位不授权另一个动作"。

## 2. Linux：systemd-logind + FreeDesktop ScreenSaver

两条总线，职责不同，**不可混用**：

| 总线 | 承载 | 原因 |
|---|---|---|
| `zbus::Connection::system()` | `org.freedesktop.login1` | 电源与注销属于系统级，只有 system bus 上才有 login1 |
| `zbus::Connection::session()` | `org.freedesktop.ScreenSaver`、各 DE SessionManager | 锁屏与桌面会话属于用户会话级 |

### 2.1 login1 电源与注销

```
dest     = org.freedesktop.login1
path     = /org/freedesktop/login1
interface= org.freedesktop.login1.Manager
bus      = system
```

| 动作 | 方法 | 实参 |
|---|---|---|
| Suspend | `Suspend` | `interactive = false` |
| Hibernate | `Hibernate` | `interactive = false` |
| Reboot | `Reboot` | `interactive = false` |
| Shutdown | **`PowerOff`** | `interactive = false` |
| Logout | `TerminateSession` | `id = ""` |

**关键点：**

- **关机的方法是 `PowerOff`，不是 `Shutdown`。** login1 没有 `Shutdown`
  方法，写错名字只会得到一个 `UnknownMethod` 错误，看起来像"权限不够"。
- **`interactive` 恒为 `false`。** 传 `true` 会让 logind 尝试与用户交互
  （polkit 对话框可能由 logind 自己弹），而 UDA 的调用方往往已经是无人值守的
  服务进程；`false` 让 polkit 自己决定，行为可预期。
- **注销用 `TerminateSession("")`。** 空 id 表示"调用者自己所在的会话"，
  这是唯一不需要 `sd_session_get_self()` 查 id 的写法。它终止的是**登录会话**
  里的所有进程（`KillUserProcesses=yes` 的发行版上更彻底），比"退出图形会话"
  语义更强，因此在 systemd 桌面上优先于 DE SessionManager。
- 每次 D-Bus 调用都包 `tokio::time::timeout(5s)`。polkit 代理、logind 重启、
  无响应守护进程都不会把调用方挂死（AGENTS.md Principle 1：Never Panic）。

### 2.2 锁屏（session bus）

```
dest     = org.freedesktop.ScreenSaver
path     = /org/freedesktop/ScreenSaver
interface= org.freedesktop.ScreenSaver
bus      = session
method   = Lock()
```

`Lock()` 无参无返回。GNOME、KDE、XFCE 都实现了这个 FreeDesktop 接口；
Hyprland/Sway 的 `swayidle`/`hypridle` 也导出它。Tier 3 回退是
`loginctl lock-session`（CLI），这条路径**不需要任何 screen saver 服务**，
因此 bare WM 上锁屏依然可用。

### 2.3 注销回退链（Tier 2 → Tier 3）

`TerminateSession` 失败（无 systemd、被 polkit 拒、方法不存在）时，按桌面环境
逐个尝试各自的注销入口：

| 桌面 | dest | path | interface | method |
|---|---|---|---|---|
| GNOME | `org.gnome.SessionManager` | `/org/gnome/SessionManager` | `org.gnome.SessionManager` | `Logout(0)` |
| KDE | `org.kde.Shutdown` | `/Shutdown` | `org.kde.Shutdown` | `logout` |
| XFCE | `org.xfce.Session` | `/org/xfce/Session/Manager` | `org.xfce.Session.Manager` | `Logout` |

每次尝试都带超时；全部失败则返回最后一次的语义错误，绝不静默成功。

### 2.4 错误映射（D-Bus）

| logind / D-Bus 错误 | `UdaError` | 含义 |
|---|---|---|
| `org.freedesktop.DBus.Error.AccessDenied` | `NotSupported` | polkit 拒绝：无权限，重试无用 |
| `...NotAuthorized...` / `Unauthorized` | `NotSupported` | 同上，CLI 层同样识别 |
| `InteractiveAuthorizationRequired` | `NotSupported` | polkit 规则要弹对话框，而 UDA 宿主通常不是已注册的 polkit subject，弹不出来；同样不是"再试一次"能解决的 |
| `org.freedesktop.systemd1.NoSuchOperation` | `NotSupported` | 机器缺这个能力（最常见：没开 swap 时休眠） |
| `...NotSupported...` | `NotSupported` | 同上 |
| 连接 system bus 失败 / 超时 | `DetectionFailed` | 环境问题，不是功能缺失 |
| 其它 | `CommandFailed` | 带原文透传，便于排查 |

分类是**对已渲染错误文本**做的纯函数（`map_login1_error`），因此可在无 D-Bus
的 CI 里单测，也不依赖某个版本的 zbus 如何格式化方法错误。`AccessDenied`
归到 `NotSupported` 而非 `CommandFailed`：调用方需要能说"请找管理员授权"，
而不是"再试一次"。

## 3. Windows：Win32 会话与电源 API

| 动作 | API | 需要提权 |
|---|---|---|
| Lock | `user32!LockWorkStation()` | 否 |
| Logout | `ExitWindowsEx(EWX_LOGOFF, 0)` | 否 |
| Suspend | `powrprof!SetSuspendState(bHibernate=false, bForce=false, bWakeupEventsDisabled=false)` | 否 |
| Hibernate | `SetSuspendState(true, false, false)` | 否 |
| Reboot | `ExitWindowsEx(EWX_REBOOT \| EWX_FORCEIFHUNG, 0)` | **是** |
| Shutdown | `ExitWindowsEx(EWX_POWEROFF \| EWX_FORCEIFHUNG, 0)` | **是** |

Reboot 与 Shutdown 共用 `exit_power_off_path()`（提权 + 退出），Logout 与它们
共用 `exit_flags_for()` 查表取 flag。运行代码与测试读的是**同一张表**，因此改动
映射不可能"测试通过但发布的是别的 flag"。

### 3.1 `SeShutdownPrivilege` 提权舞步（critical）

`ExitWindowsEx` 带 `EWX_REBOOT`/`EWX_POWEROFF` 时，如果进程令牌**没有持有或
没有启用** `SeShutdownPrivilege`，会以 `ERROR_PRIVILEGE_NOT_HELD`(1314) 失败。
两步缺一不可，且两步失败的表面现象完全一样，因此必须都做对：

1. `OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &token)`
   —— 打开进程令牌。`TOKEN_QUERY` 是必需的：`AdjustTokenPrivileges` 通过
   `previousstate` 出参汇报它**实际做了什么**。
2. `LookupPrivilegeValueW(None, SE_SHUTDOWN_NAME, &mut luid)` 拿到 LUID，再
   `AdjustTokenPrivileges(token, false, &privileges, ...)` 启用。

**最大的坑：`AdjustTokenPrivileges` 即使一个权限都没授予也返回成功。**
唯一可靠的判据是随后检查 `GetLastError() == ERROR_NOT_ALL_ASSIGNED`(1300)。
不检查就会出现"提权函数返回 Ok，紧接着 ExitWindowsEx 报 1314"，而错误信息
指向了错误的位置。

令牌句柄由 `TokenGuard` 持有，`Drop` 调 `CloseHandle`——从
`OpenProcessToken` 到操作结束之间的每一条提前返回路径都会释放句柄。
（无 unwrap/expect：每个 `BOOL`/`HRESULT` 都显式判败并入错误链。）

### 3.2 为什么 logout 不加 `EWX_FORCEIFHUNG`

`EWX_FORCEIFHUNG` 会强行结束挂起的应用。对注销来说，这会**丢掉用户没保存的
东西**——而注销本来是可以被桌面环境"礼貌地"拒绝或提示的。只有 reboot /
shutdown 两个不可撤销动作才用它（那两个本来就要结束一切）。

### 3.3 Win32 错误映射

| Win32 错误 | 来源 | `UdaError` |
|---|---|---|
| 1314 `ERROR_PRIVILEGE_NOT_HELD` | `ExitWindowsEx` | `NotSupported`（提示以管理员运行） |
| 1300 `ERROR_NOT_ALL_ASSIGNED` | `AdjustTokenPrivileges` | `NotSupported`（账户无 `SeShutdownPrivilege`） |
| 1 `ERROR_INVALID_FUNCTION` / 2 `ERROR_FILE_NOT_FOUND` | `SetSuspendState` | `NotSupported`（休眠文件不存在 = 休眠被关闭） |
| 其它 `HRESULT`/`Err` | 全部三个 API | `Internal` / `CommandFailed`，消息含 Win32 码 |

`SetSuspendState` 返回 `BOOLEAN` 而非 `HRESULT`，所以原因只能从
`GetLastError()` 取——这也是 `last_error_code()` 辅助函数存在的理由。

## 4. C-ABI 入口

七个导出（一个能力查询 + 六个动作），与其它模块同样经 `catch_boundary` 包裹：

| 函数 | 出参 | 说明 |
|---|---|---|
| `uda_session_capabilities(out: *mut u32)` | 能力位掩码 | 无副作用，可随时调用；0 表示无后端 |
| `uda_session_lock()` | 无 | **唯一可自动化的动作** |
| `uda_session_logout()` | 无 | 危险 |
| `uda_session_suspend()` | 无 | 危险 |
| `uda_session_hibernate()` | 无 | 危险 |
| `uda_session_reboot()` | 无 | 危险，Windows 需管理员 |
| `uda_session_shutdown()` | 无 | 危险，Windows 需管理员 |

六个动作函数都**不返回内容**，只返回状态码：动作本身要么发生要么没发生。
返回值语义：

| 状态 | 含义 |
|---|---|
| `UDA_OK` (0) | 动作已被系统接受（对 Shutdown 而言进程随即消失） |
| `UDA_ERR_NOT_SUPPORTED` (-2) | 平台/账户无此能力，或 polkit 拒绝 |
| `UDA_ERR_INTERNAL` (-5) | API 调用失败，详见 `uda_last_error_message()` |

**为什么不是单一 `uda_session_perform(action_code)`**：六个命名函数让每个调用点
在源码里就看得见它触发哪个系统动作，也让漏链接符号的绑定在链接期就失败，
而不是运行时才炸。

## 5. 能力与降级

- **能力位表达"代码路径存在"，不是"账户被允许"。** 关掉休眠的机器仍上报
  `Capability::HIBERNATE`；真正拒绝发生在运行时，成为
  `UdaError::NotSupported`。这与 Linux 的 polkit、Windows 的 1314 完全对称。
- Linux 后端无条件上报全部七位：任何装了 systemd 的机器都有 logind，而锁屏
  的 `loginctl` 回退连 screen saver 服务都不需要。
- Windows 后端同样无条件上报全部七位：Win10/11 都有这四个 API；权限是运行时
  问题（1300/1314），不是能力问题。
- 非 Linux / 非 Windows 目标：`capabilities()` 返回空集，六个动作一律
  `UDA_ERR_NOT_SUPPORTED`。

## 6. 演示程序的安全姿态

`examples/python/07_session.py` 与 `examples/nodejs/07_session.js` 默认只打印
当前平台的能力矩阵（一次只读查询），随后仅对用户**显式确认**的锁屏调用
`uda.session.lock()`。关机/重启等危险动作只以代码示例形式打印，标签写明
"需要另行显式确认后才可调用"，演示**不会**执行它们。

## 7. 测试红线

1. **任何自动化测试与 CI 都不得触发真实关机、重启、注销、睡眠、休眠。**
   Windows 上一条 `ExitWindowsEx` 就能让 CI runner 关机。
2. 测试只覆盖：能力位映射、参数解析（exit flags / login1 方法名 /
   `interactive` 常量）、错误分类（polkit、NoSuchOperation、1314、1300）、
   超时行为，以及核心闸门 `perform()` 的路由。
3. 路由测试用实现 `SessionManager` 的 **mock**（`RecordingManager` /
   `RefusingManager`）：记录被调用的方法名、上报固定能力集。断言"被拒绝的
   动作根本没有到达后端"（`calls().is_empty()`），这是"不会误执行"的证明。
4. **不得在运行后残留 mock / verify / e2e 脚本或日志**；临时产物只进 `.tmp/`。
5. 真实桌面上的手工验证只允许锁屏，且必须在人看着的时候做。

## 8. 相关文件

| 文件 | 作用 |
|---|---|
| `crates/uda-core/src/session.rs` | `SessionAction`、`SessionManager`、`perform()` 闸门 |
| `crates/uda-core/src/capability.rs` | 七个会话能力位（16..22） |
| `crates/uda-platform-linux/src/session.rs` | login1 / ScreenSaver / 回退链 |
| `crates/uda-platform-windows/src/session.rs` | Win32 API + 提权舞步 |
| `crates/uda-ffi/src/session.rs` | `Failure` 映射与 mock 测试 |
| `include/uda.h` | C 侧常量与函数声明 |
