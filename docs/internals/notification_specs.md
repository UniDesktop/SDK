# 通知（Notification）规格说明

## 1. Linux：FreeDesktop Notifications

```text
Bus      : Session Bus
Dest     : org.freedesktop.Notifications
Path     : /org/freedesktop/Notifications
Interface: org.freedesktop.Notifications
```

### Notify

```text
UInt32 Notify(
    String app_name,        // 发送方应用名，由通知服务器自行决定是否展示
    UInt32 replaces_id,     // 0 = 新通知；否则替换同 id 的旧通知
    String app_icon,        // 图标：文件路径 / URI / 图标主题名 / 空
    String summary,         // 单行标题
    String body,            // 多行正文
    Array<(String)> actions,// 扁平化动作对：[key, label, key, label, ...]
    Dict<String,Variant> hints, // 附加提示，UDA 写入 urgency（Byte）
    Int32 expire_timeout    // 毫秒；0 = 服务器默认，-1 = 永不过期
)
```

| 字段 | UDA 行为 |
|------|----------|
| `actions` | 由 `Vec<(String, String)>` 展平为 `[key, label, ...]`；末尾落单的 key 因无 label 被丢弃，避免渲染出空白按钮 |
| `hints` | 仅 `urgency`（`0` Low / `1` Normal / `2` Critical），按规范以 `Byte` 类型传递 |
| `app_icon` | 原样传递给服务器，由服务器解释（路径、URI 或主题名） |

返回值为服务器分配的 `notification id`。`replaces_id != 0` 时该 id 通常等于被替换的通知。

### 能力判定

只要 `Connection::session()` 成功即报告 `SEND_NOTIFICATION`。通知守护进程缺失属于运行时错误，由 `Notify` 调用本身抛出，不在能力位中预判。

## 2. Windows：WinRT ToastNotificationManager

未打包（unpackaged）进程的两个必要步骤，缺一不可：

1. `SetCurrentProcessExplicitAppUserModelID(app_name)`：把 AUMID 记录到进程上，`app_name` 为空或纯空白时替换为回退值 `UniDesktop.Notification`（Win32 拒绝空字符串）。
2. `ToastNotificationManager::CreateToastNotifierWithId(app_name)`：显式寻址。参数版的 `CreateToastNotifier()` 从进程解析身份，对未打包二进制恒返回 `ELEMENT_NOT_FOUND`（`0x80070490`），即使已注册 AUMID 也不会改变。

两者都通过 `ensure_app_identity()` 每进程只执行一次（`OnceLock`），且若宿主机在 UDA 之前已设置自己的 AUMID，则通过 `GetCurrentProcessExplicitAppUserModelID` 检测到并原样保留——覆盖它会破坏宿主机自身的激活路由。

`app_icon` 通过 `uda_core::notification::image_source()` 规范化后写入模板的 `<image src>` 节点：裸路径转为 `file://` URI，已带 scheme 的值原样通过。无图标时降级到 `ToastText02` 模板（无 `<image>` 节点），而不是让 `SelectSingleNode` 落空。

## 3. Windows 平台限制（ Pitfalls ）

### 3.1 Package Identity 与通知来源显示

Windows 的 toast 身份由 **Package Identity** 决定，而不是由调用方传入的应用名。这带来两类不同表现：

| 运行环境 | toast 顶部来源显示 | UDA 能做的 |
|---|---|---|
| 未打包进程（`node script.js`、系统安装的 Python） | `app_name` 注册的 AUMID，即调用方给定的名字 | 直接生效 |
| **带包身份的运行环境**（Microsoft Store 安装的 Python/Node 等） | **宿主应用的包名**，形如 `PythonSoftwareFoundation.Python.3.13_qbz5n2kfra8p0` | 无法覆盖 |

**根因**：商店版运行时被打包为 MSIX，进程已被 shell 绑定到包身份。`SetCurrentProcessExplicitAppUserModelID` 不能覆盖既有绑定，`CreateToastNotifierWithId(custom_id)` 也不改变已解析的身份，因此通知卡片顶部显示的是包族名（Package Family Name）而不是调用方名字。

**后果与对策**：这是平台策略，不是 UDA 的缺陷。调用方应当预期到在商店环境下通知来源会暴露宿主运行时包名；若必须展示自有品牌名，唯一可靠途径是把宿主程序自身打包为 MSIX 并注册自己的身份。

### 3.2 交互式动作按钮（Action Buttons）需要 COM 组件注册

FreeDesktop 规范的 `actions`（按钮对）在 Windows 上**不会**渲染为可点击按钮：

- toast 按钮要求 toast XML 内含 `actions` 内容，并且有一个**已注册的 COM 激活器**在用户点击时被唤醒；
- 该激活器依赖注册表中的 `CLSID` / `AppUserModelID` 关联，只有 MSIX 打包应用可以可靠注册；
- 未打包的脚本环境无法完成注册。

**UDA 的降级策略**：接受 `Notification::actions`（保证 trait 与 C-ABI 跨平台一致），但在 Windows 后端**不呈现按钮**，通知以只读文本卡片正常显示，`send()` 仍返回成功。调用方可以通过 `capabilities()` 得知后端存在，但按钮可用性无法在发送前探测——它取决于宿主是否打包，而非取决于系统能力。

清单：

- Linux：`actions` 完整支持。
- Windows 未打包：动作按钮静默降级，仅文本可见。
- Windows MSIX 打包：按钮可用，宿主需自行注册激活器；UDA 不注入 COM 组件。

## 4. C-ABI 入口

```c
int32_t uda_notify(const char *app_name,
                   const char *title,
                   const char *body,
                   const char *icon,
                   const char *actions,   // 扁平化 "key\nlabel\nkey\nlabel"
                   uint32_t *out_id);
```

`app_name` 为空时 FFI 层替换为 `UniDesktop.Notification`；`icon` 为空表示不设置；`actions` 中末尾落单的记录被丢弃。

## 5. 相关文件

| 路径 | 职责 |
|------|------|
| `crates/uda-core/src/notification.rs` | `Notification` 模型、`image_source` 图标规范化 |
| `crates/uda-platform-linux/src/notification.rs` | D-Bus `Notify` 调用 |
| `crates/uda-platform-windows/src/notification.rs` | AUMID 注册、ToastNotifier、模板填充 |
| `crates/uda-ffi/src/notify.rs` | C-ABI 字段组装 |
