# WakeLock / Inhibit 规格说明

## 1. FreeDesktop ScreenSaver（Linux 首选）

```text
Service: org.freedesktop.ScreenSaver
Path  : /org/freedesktop/ScreenSaver
Interface: org.freedesktop.ScreenSaver
```

### Inhibit

```text
UInt32 Inhibit(String app_name, String reason, UInt32 flags)
```

| 参数 | 说明 |
|------|------|
| app_name | 调用方应用名 |
| reason | 申请锁的原因说明 |
| flags | 8 = InhibitIdle（阻止屏幕休眠）；12 = InhibitIdle | InhibitSuspend（阻止系统挂起） |

返回值 `cookie: u32` 是后续 `UnInhibit` 所需的句柄。

### UnInhibit

```text
void UnInhibit(UInt32 cookie)
```

收到 `UnInhibit(cookie)` 后，会话管理器应立即解除对应的抑制状态。

## 2. XDG Desktop Portal（备选）

Portal 未在当前 Phase 1 实现中启用，保留为下一 Phase 的可选升级路径。

## 3. RAII 语义

`WakeLockGuard` 实现 `Drop`，确保进程崩溃或提前返回时锁必然释放，避免残留 cookie 导致系统长期被抑制。
