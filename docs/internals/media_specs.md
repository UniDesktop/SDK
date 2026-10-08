# Media Control Backend Specifications

跨平台媒体播控的协议字典。UDA 把它收敛成 [`MediaManager`](../../crates/uda-core/src/media.rs)
trait 与三个 C-ABI 入口；本文记录两端各自"怎么做到"，是唯一事实来源。

## 1. 模型映射（先看这里）

| UDA 概念 | Linux (MPRIS v2) | Windows (SMTC) |
|---|---|---|
| `PlaybackStatus::Playing` | `PlaybackStatus` == `"Playing"` | `GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing` |
| `PlaybackStatus::Paused` | `"Paused"` | `Paused` |
| `PlaybackStatus::Stopped` | `"Stopped"` | `Stopped` |
| `PlaybackStatus::Unknown` | 属性缺失 / 无法识别 | 会话无焦点或属性不可读 |
| `MediaMetadata::title` | `Metadata["xesam:title"]`（`String`） | `TryGetMediaPropertiesAsync().Title` |
| `MediaMetadata::artist` | `Metadata["xesam:artist"]`（`as` 数组，**拼接**） | `.Artist` |
| `MediaMetadata::album` | `Metadata["xesam:album"]` | `.AlbumTitle` |
| `MediaMetadata::duration_ms` | `Metadata["mpris:length"]`（**微秒**，需 /1000） | `.Duration`（`TimeSpan`，100ns 单位） |
| `MediaCommand::Play` | `Player.Play()` | `TryPlayAsync()` |
| `MediaCommand::Pause` | `Player.Pause()` | `TryPauseAsync()` |
| `MediaCommand::TogglePlayPause` | `Player.PlayPause()` | `TryTogglePlayPauseAsync()` |
| `MediaCommand::Next` | `Player.Next()` | `TrySkipNextAsync()` |
| `MediaCommand::Previous` | `Player.Previous()` | `TrySkipPreviousAsync()` |
| `MediaCommand::Stop` | `Player.Stop()` | `TryStopAsync()` |

两端都没有"上一曲/下一曲"之外的 seek 语义进入本 Step 的范围；`position_ms`
在 Linux 侧由 `Position` 属性给出（微秒），Windows 侧 SMTC 不暴露稳定位置，
故两端都可能为 `None`。

## 2. Linux：`org.mpris.MediaPlayer2.Player`

### 2.1 发现播放器

MPRIS 没有"当前播放器"的中心注册表。约定做法是**扫描 session bus 上所有
以 `org.mpris.MediaPlayer2.` 开头的独占名**：

```
org.freedesktop.DBus -> ListNames()
```

播放器的总名就是 `org.mpris.MediaPlayer2.<instance>`。多个播放器同时在线是
常态（浏览器与本地播放器各自一个），因此需要挑选一个：

1. 过滤出前缀匹配的名字；
2. 逐个读 `org.mpris.MediaPlayer2.Player.PlaybackStatus`，取第一个
   `Playing` 的；
3. 全是 `Paused`/`Stopped` 时，取第一个能应答的（保留"上次听过什么"）；
4. 一个都取不到 -> `Ok(None)`。

**严禁**把 `playerctld`（`org.mpris.MediaPlayer2.playerctld`）当作普通播放器：
它是一个多路复用守护进程，选中它会把指令转发到别的播放器，导致状态与用户
看到的界面不一致。它没有 `xesam:` 元数据，可直接识别并跳过。

### 2.2 接口与方法

| 成员 | 签名 / 类型 |
|---|---|
| `PlaybackStatus` (property) | `s`：`"Playing"` / `"Paused"` / `"Stopped"` |
| `Metadata` (property) | `a{sv}`，键为 `xesam:*` / `mpris:length` |
| `Position` (property) | `x`（微秒，i64） |
| `PlayPause()` | method |
| `Play()` / `Pause()` / `Stop()` | method |
| `Next()` / `Previous()` | method |

### 2.3 `Metadata` 字典踩坑（critical）

- `xesam:artist` 与 `xesam:genre` 是 **`as`（字符串数组）**，不是单个字符串。
  多个艺术家必须拼接成一个展示串，UDA 用 `", "` 连接；直接 `downcast::<String>()`
  会失败并让整首曲子退化成空标题。
- `mpris:length` 是 **微秒**（`x`，i64）。乘以/除以 1000 才是毫秒。把微秒当
  毫秒会让一首 3 分钟的歌唱成 50 小时。
- `xesam:title` 可能是 **数组**（Chromium 系历史上这么发过）。取首个元素即可，
  否则标题整段丢失。
- 任何键都可能缺席（直播流、无元数据的本地文件）。缺席 == 空字符串，不是错误。
- `mpris:length` <= 0 表示直播或未知时长，映射为 `None` 而不是 0。

### 2.4 错误分层

`zbus` 调用失败要分开对待：`ServiceUnknown`（播放器刚好退出）与
`UnknownProperty`（该播放器没实现那个属性）都是**可预期的竞态**，降级为
`Ok(None)` 或该字段为空；只有连 session bus 都连不上才是
`UdaError::DetectionFailed`。

## 3. Windows：WinRT SMTC

`GlobalSystemMediaTransportControlsSessionManager` 是 Windows 10 1703+ 的系统级
媒体会话中枢，任何遵循 SMTC 的播放器（Groove、Spotify UWP、Edge、VLC 的 WinRT
后端）都会注册进来。

### 3.1 获取管理器

```rust
let manager = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()?
    .get()?;                       // 阻塞直到 WinRT 异步完成
```

`RequestAsync()` 返回 `IAsyncOperation`，`.get()` 同步等待结果。这是 Rust 侧
唯一需要的异步桥接——不需要整条 tokio 运行时，`windows::Foundation::AsyncStatus`
配合即可。

### 3.2 获取当前会话

```rust
let session = manager.GetCurrentSession()?;   // Option<GlobalSystemMediaTransportControlsSession>
```

`GetCurrentSession()` 返回"当前拥有媒体焦点"的会话——即用户在系统 UI
（音量浮层、键盘媒体键）上操作的那一个。返回 `None` 表示系统当前没有播放会话，
此时 `active_metadata()` 报 `Ok(None)`、`playback_status()` 报
`PlaybackStatus::Unknown`，都不是错误。

### 3.3 属性与控制

| 成员 | 说明 |
|---|---|
| `TryGetMediaPropertiesAsync().get()` | `GlobalSystemMediaTransportControlsSessionMediaProperties` |
| `.Title` / `.Artist` / `.AlbumTitle` | `HSTRING`，可能为空 |
| `.Duration` | `TimeSpan`（100ns 单位），`Duration::zero()` 表示未知 |
| `GetPlaybackInfo()?.PlaybackStatus()` | 会话级播放状态 |
| `TryTogglePlayPauseAsync()` | 返回 `bool`：**false 表示被拒**，不是错误 |
| `TryPlayAsync()` / `TryPauseAsync()` / `TryStopAsync()` | 同上 |
| `TrySkipNextAsync()` / `TrySkipPreviousAsync()` | 同上 |

### 3.4 关键踩坑（critical）

- **时间单位**：SMTC 的 `TimeSpan` 是 **100 纳秒**。毫秒 = ticks / 10000。
  写成 `/1_000_000` 会把 3 分钟算成 18 秒。
- **`Duration` 为 0 不代表 0 毫秒**：SMTC 用 `TimeSpan::zero()` 表达"未知
  时长"，必须映射为 `None`。直播流、无时长信息的来源都会给 0。
- **控制方法是 `Try*`**：返回 `false` 说明播放器拒绝（例如已暂停时再次
  Pause）。这是正常语义，映射为 `Ok(())` 而非错误；只有 HRESULT 失败才是
  `UdaError::Internal`。
- **没有 JIT 属性**：所有 SMTC 调用都是 WinRT 异步，Rust 侧统一 `.get()` 同步
  等待，避免把 async 泄漏到 C-ABI。

## 4. C-ABI 入口

三个导出，与其它模块同样经 `catch_boundary` 包裹（见 `crates/uda-ffi/src/util.rs`）：

| 函数 | 出参 | 说明 |
|---|---|---|
| `uda_media_get_status(out_status: *mut i32)` | `0..3` 状态码 | 无会话时写 `3`（`Unknown`）并返回 `UDA_OK` |
| `uda_media_get_metadata(out_title, out_artist, out_album: *mut *mut c_char, out_duration_ms, out_position_ms: *mut u64)` | 三个字符串 + 时长/进度槽 | 字符串由库分配，调用方用 `uda_free_string()` 释放；无元数据或字段未发布时写 NULL；`out_duration_ms` 写 0 表示未知；仅 `out_position_ms` 允许传 `NULL` 跳过 |
| `uda_media_send_command(command: i32)` | 无 | `0..5` 命令码；无播放器返回 `UDA_ERR_NOT_SUPPORTED` |

`out_title` / `out_artist` / `out_album` / `out_duration_ms` 四个出参指针必须非空（传 `NULL` 返回 `UDA_ERR_INVALID_ARGUMENT`）；未发布的字段以 NULL 指针（字符串）/ 0（数值）回填，而非空字符串。

## 5. 能力与降级

`Capability::MEDIA_CONTROL` 表示"本平台存在可用的媒体播控后端"。即使置位，
运行时仍可能无播放器——所以 `active_metadata()` 返回 `Ok(None)` 是**正常
答案**而非失败，调用方必须按"现在没在放"处理，与 AGENTS.md Principle 1
（Never Panic）一致。

## 6. 测试基线

- 元数据解析（数组拼接、单位换算、空字段）必须是纯函数，可在无 D-Bus /
  无 WinRT 的 CI 里测——`docs/../crates/uda-platform-linux/src/media.rs` 的
  `metadata_from_dbus` 与 `crates/uda-platform-windows/src/media.rs` 的
  `duration_from_ticks` 都是为此设计的。
- 无播放器场景：`active_metadata()` == `Ok(None)`，不 panic、不 unwrap。
