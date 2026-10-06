<div align="center">

# <image src="./icons/UniDesktop_3D_transparent_mini.png" height="30"/>  UniDesktop API (UDA)

*Unifying fragmented Linux desktop environments and Windows behind a single, type-safe API.*

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/UniDesktop/SDK/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/UniDesktop/SDK/actions/workflows/ci.yml)
[![Version](https://img.shields.io/badge/release-v0.2.1-brightgreen)](https://github.com/UniDesktop/SDK/releases)
![Platform: Linux](https://img.shields.io/badge/platform-Linux%20(GNOME%20%2F%20KDE%20%2F%20Wayland)-lightgrey.svg)
![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-lightgrey.svg)

**English** · [简体中文](README_CN.md)

</div>

---

## Why UDA?

Building a cross-platform desktop application today means writing the same feature five times: `SystemParametersInfoW` on Windows, then `gsettings`, `qdbus`, `swww` and `hyprpaper` on Linux — each with its own failure modes, session assumptions, and no shared type system. UDA replaces that fragmentation with one capability-driven Rust API that probes the environment, picks the working backend, and **degrades gracefully instead of panicking**.

| Principle | What it means |
|---|---|
| **Cascading fallback** | XDG Desktop Portal → native DE IPC (`$XDG_CURRENT_DESKTOP`) → CLI tools (`swww`, `hyprpaper`, …) → a typed `UdaError::NotSupported`. Never a panic. Not every module uses Tier 1: wallpaper and wake locks start at Tier 2, and appearance detection is the only module that engages the portal, through `org.freedesktop.portal.Settings`. |
| **Capability-driven** | Features advertise `SupportLevel::Full` / `Partial(reason)` / `None`, so your app branches *before* it breaks. `Partial` carries the reason, so a degradation can be explained rather than guessed at. |
| **Zero heavy runtime dependencies** | Pure Rust `zbus` on Linux, `windows-rs` on Windows. No Qt, no GTK, no bundled toolkit. |

## 🖥️ Platform Support Matrix

| Feature | Windows 10/11 | GNOME 42+ | KDE Plasma 5/6 | XFCE | Hyprland | Sway | Generic X11 |
|------|:---:|:---:|:---:|:---:|:---:|:---:|:---:|
| Theme detection | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ |
| Accent colour | ✅ | ✅ | ⚠️ | ⚠️ | ❌ | ❌ | ❌ |
| Wallpaper | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Notification | ✅ [^1] | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ |
| Notification icon | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Notification actions | ⚠️ [^2] | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Wake lock | ✅ | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ |
| System tray | ✅ | ✅ | ✅ | ⚠️ | ✅ | ✅ | ✅ |
| Media control | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Session — lock | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Session — logout | ✅ | ✅ | ✅ | ⚠️ | ⚠️ | ⚠️ | ⚠️ |
| Session — suspend / hibernate | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| Session — reboot / shutdown | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |

`✅` verified against the real backend · `⚠️` best-effort fallback or an extra component · `❌` the platform has no such concept. The exact capability is always reported at runtime through `capabilities()` / `support_level()` — never assume from this table.

[^1]: On a **Microsoft Store–installed runtime**, the toast source is shown as the host's package family name because Package Identity overrides the AppUserModelID. See [`docs/internals/notification_specs.md`](docs/internals/notification_specs.md) §3.1.
[^2]: In an **unpackaged script host**, interactive *action buttons* degrade to a read-only text card, because toast buttons require a COM activator registered by an MSIX package. See the same document §3.2.

The per-cell backend, the ⚠️ behaviour and what each environment reports: **[Platform support](https://unidesktop.github.io/en/reference/platform-support/)**.

## 🚀 Quickstart in 3 Languages

Three entry points: the native Rust crates, and a stable C-ABI (`include/uda.h`) reachable from Python and Node.js.

```rust
// Rust — native traits, no FFI layer
let manager = uda_platform_linux::LinuxWallpaperManager::new();
manager.set_wallpaper("~/Pictures/a.png", FillMode::Fill)?;
```

```python
# Python — examples/python/uda.py (ctypes, zero dependencies beyond the stdlib)
from uda import Uda
with Uda() as uda:
    print(uda.theme)                      # 'dark' | 'light' | 'unknown'
    uda.notify("标题", "正文内容")          # native notification
```

```javascript
// Node.js — examples/nodejs/uda.js (koffi, zero-compile)
const { Uda } = require('./uda');
const uda = new Uda();
uda.notify('标题', '正文内容');
uda.dispose();
```

Full install steps, capability guides and the complete API reference: **[unidesktop.github.io](https://unidesktop.github.io/en)**.

## 🏗️ Architecture & Project Layout

```text
uda/
├── crates/
│   ├── uda-core/              # Public traits, enums, Capability flags, unified errors
│   ├── uda-platform-linux/    # Linux backends: D-Bus (zbus), Wayland IPC, X11
│   ├── uda-platform-windows/  # Windows backends: Win32, COM, WinRT
│   ├── uda-ffi/               # C-ABI export layer (Python ctypes / Node.js koffi)
│   └── uda-cli/               # Diagnostic CLI for manual probing
├── docs/internals/            # Protocol specification dictionary (the cheat book)
├── examples/                  # 01_appearance … 07_session, aligned in 3 languages
└── plans/                     # Per-phase implementation plans
```

| Module | Linux backend | Windows backend |
|---|---|---|
| Appearance | FreeDesktop portal `Settings` → GSettings → `kreadconfig` → `xfconf-query`; `Theme::Unknown` when nothing answers | Registry `AppsUseLightTheme` / `SystemUsesLightTheme` |
| Wallpaper | GSettings → KDE `plasmashell` D-Bus → `hyprpaper` / `swww` → `feh` / `nitrogen`. No portal: `org.freedesktop.portal.Wallpaper` exists but is never called | `SystemParametersInfoW` |
| Notification | `org.freedesktop.Notifications` | WinRT `ToastNotificationManager` |
| WakeLock | `org.freedesktop.ScreenSaver` `Inhibit` | `SetThreadExecutionState` |
| System tray | `org.kde.StatusNotifierItem` + `com.canonical.dbusmenu` | `Shell_NotifyIconW` + worker-thread message pump |
| Media | MPRIS v2 over the session bus | WinRT SMTC |
| Session & power | `systemd-logind` + `org.freedesktop.ScreenSaver` | Win32 power & shutdown APIs |

The **C-ABI** returns an `int32_t` status per call (`0` success, `-1` invalid argument, `-2` unsupported, `-3` detection failed, `-4` I/O error, `-5` internal, `-6` a contained panic) and never unwinds across the boundary — every export body runs inside `catch_unwind`.

## 🗺️ Roadmap

| Phase | Scope | Status |
|---|---|---|
| **1** | Core types, appearance, wallpaper, notifications, wake locks, C-ABI + SDKs | **v0.1.0 released** |
| **2** | System tray, media playback control, session & power lifecycle | **v0.2.0 released** |
| **3** | Global shortcuts, advanced clipboard, audio endpoint routing, display brightness | in progress |
| **4** | Virtual desktops, external window control, window aesthetics (Mica / Acrylic / KWin blur) | planned |
| **5** | Input simulation, eyedropper, screen capture, idle detection | planned |

See [`plans/`](plans/) for the per-phase breakdown and [`AGENTS.md`](AGENTS.md) for the engineering rules.

## 🤝 Community & Sponsorship

Issues and pull requests are welcome on [GitHub](https://github.com/UniDesktop/SDK). The project is dual-licensed under [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE).
