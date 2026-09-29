<div align="center">

# <image src="./icons/UniDesktop_3D_transparent_mini.png" height="30"/>  UniDesktop API (UDA)

*The missing bottom half of Qt — Unifying fragmented Linux desktop environments & Windows with a single, type-safe API.*

[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-2021%20edition-orange.svg)](https://www.rust-lang.org)
[![CI](https://github.com/UniDesktop/SDK/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/UniDesktop/SDK/actions/workflows/ci.yml)
![Platform: Linux](https://img.shields.io/badge/platform-Linux%20(GNOME%20%2F%20KDE%20%2F%20Wayland)-lightgrey.svg)
![Platform: Windows](https://img.shields.io/badge/platform-Windows%2010%20%2F%2011-lightgrey.svg)

**English** · [简体中文](README_CN.md)


</div>

---

> [!IMPORTANT]
> 🚀 **v0.2.0 Release Candidate Branch (`develop`)**
>
> **Phase 2 (Interactive Shell & System Integration) is complete** on this branch: the system tray, media playback control, and the session/power lifecycle have all been verified against real backends and are about to be tagged **v0.2.0**. For the long-lived stable line, switch to [`main`](https://github.com/UniDesktop/SDK/tree/main) or check the [v0.1.0 release](https://github.com/UniDesktop/SDK).

## Why UDA?

Building a cross-platform desktop application today means writing the same feature five times: `SystemParametersInfoW` on Windows, then `gsettings`, `qdbus`, `swww`, and `hyprpaper` on Linux — each with its own failure modes, session assumptions, and no shared type system. UDA replaces that fragmentation with one capability-driven Rust API that probes the environment, picks the working backend, and degrades gracefully instead of crashing.

- **Portal-first** — every Linux feature tries the XDG Desktop Portal before touching anything distro-specific.
- **Cascading Fallback** — a strict tier chain: Portal → native DE IPC → CLI tools → a strongly typed `UdaError::Unsupported`.
- **Capability-driven** — features advertise `SupportLevel::Full` / `Restricted` / `Unsupported`, so your app can branch before it breaks.
- **Zero heavy runtime dependencies** — pure Rust `zbus` on Linux, `windows-rs` on Windows. No Qt, no GTK, no bundled toolkit.

---

## 🖥️ Desktop & Platform Support Matrix

| Platform / DE | Theme | Wallpaper | Notification | WakeLock | Tray | Media | Session |
| --- | :---: | :---: | :---: | :---: | :---: | :---: | :---: |
| **Windows 10 / 11** | ✅ | ✅ | ✅ [^1] | ✅ | ✅ | ✅ | ✅ |
| **GNOME 42+** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **KDE Plasma 5 / 6** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **XFCE** | ✅ | ✅ | ✅ | ✅ | ⚠️ | ✅ | ✅ |
| **Hyprland** | ⚠️ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ | ✅ |
| **Sway** | ⚠️ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ | ✅ |
| **Generic X11** | ⚠️ | ✅ | ⚠️ | ⚠️ | ✅ | ✅ | ✅ |

> **Shipped in this release:** light/dark appearance detection with live system-follow support · wallpaper management with `Crop` / `Fill` / `Fit` / `Stretch` modes, multi-monitor targeting, and dark/light pairing · native notifications with icon and urgency hints · keep-awake locks (`PreventDisplaySleep` / `PreventSystemIdle`) · **system tray** with a cross-platform menu model (text items, checkboxes, separators, submenus) · **media control** with metadata read and playback commands · **session & power lifecycle** (lock, logout, suspend, hibernate, reboot, shutdown).
>
> `✅` verified against the real backend · `⚠️` best-effort fallback — the exact capability is reported at runtime through `capabilities()`.

[^1]: Windows notifications carry two platform restrictions. On a **Microsoft Store–installed runtime** (e.g. the Store build of Python or Node.js), the toast source is displayed as the host application's *package family name* (for example `PythonSoftwareFoundation.Python.3.13_qbz5n2kfra8p0`) because Package Identity overrides the AppUserModelID; UDA cannot override that binding. In an **unpackaged script host**, interactive *action buttons* silently degrade to a read-only text card, because toast buttons require a COM activator registered by an MSIX package. See [`docs/internals/notification_specs.md`](docs/internals/notification_specs.md) §3.

---

## 🚀 Quickstart in 3 Languages

UDA ships three entry points: the native Rust crates, and a stable C-ABI (`include/uda.h`) reachable from Python and Node.js.

### Rust

```rust
use uda_core::appearance::AppearanceManager;
use uda_core::wallpaper::{FillMode, WallpaperManager, WallpaperOptions};
use uda_platform_linux::appearance::LinuxAppearanceManager;
use uda_platform_linux::wallpaper::LinuxWallpaperManager;

fn main() -> Result<(), uda_core::error::UdaError> {
    let theme = LinuxAppearanceManager::new().detect_theme()?;
    println!("system theme: {theme:?}");
    LinuxWallpaperManager::new().set_wallpaper(
        "/usr/share/backgrounds/gnome/adwaita-l.jpg",
        &WallpaperOptions { fill_mode: FillMode::Fit, ..Default::default() },
    )
}
```

Add the crates you need:

```toml
uda-core = { path = "crates/uda-core" }
uda-platform-linux = { path = "crates/uda-platform-linux" }   # or uda-platform-windows
```

Run the bundled diagnostic CLI to see every backend on your machine:

```bash
cargo run -p uda-cli
```

### Python

A ready-made `ctypes` wrapper lives in [`examples/python/uda.py`](examples/python/uda.py). It resolves the shared library through `cargo metadata`, so it works with a redirected `target/` directory and honours `UDA_LIBRARY`.

```python
from uda import Uda, FillMode

with Uda() as uda:
    print(uda.detect_theme())                        # 'dark' | 'light' | 'unknown'
    print(uda.get_wallpaper())                       # current wallpaper path, or None
    uda.set_wallpaper("/usr/share/backgrounds/gnome/adwaita-l.jpg", FillMode.FIT)
```

> `set_wallpaper()` returns `UdaError` when the running session offers no usable backend (no Portal, no GNOME/KDE, and neither `feh` nor `nitrogen` on `PATH`). Catch it and check the reported capability instead of assuming success — that is the capability-driven contract described in [Why UDA?](#why-uda).

Run the full demo:

```bash
cargo build -p uda-ffi
python3 examples/python/demo.py
```

### Node.js

[`examples/nodejs/demo.js`](examples/nodejs/demo.js) drives the same C-ABI through [`koffi`](https://koffi.dev/) — no `node-gyp`, no native compilation step.

```bash
cd examples/nodejs && npm install
cargo build -p uda-ffi
node demo.js
```

```javascript
const koffi = require('koffi');

const lib = koffi.load(process.env.UDA_LIBRARY ?? 'uda_ffi.dll');
const setWallpaper = lib.func('uda_set_wallpaper', 'int32', ['const char *', 'int32']);
const detectTheme = lib.func('uda_detect_theme', 'int32', ['void *']);

// Fill modes: UDA_FILL_CROP=0, UDA_FILL_FILL=1, UDA_FILL_FIT=2, UDA_FILL_STRETCH=3
setWallpaper('C:\\Users\\me\\Pictures\\wall.png', 1);

const slot = koffi.alloc(koffi.types.int32, 1);
if (detectTheme(slot) === 0) {
  console.log('theme code:', koffi.decode(slot, koffi.types.int32)); // 1 = dark, 2 = light
}
```

> **Out-parameters:** `koffi.alloc(type, 1)` returns a writable BigInt address and `koffi.decode(address, type)` reads it back. Strings returned by UDA must be released with `uda_free_string()` — see [`examples/nodejs/demo.js`](examples/nodejs/demo.js) for the verified pattern.

---

## 🏗️ Architecture & Project Layout

UDA is a Cargo workspace split into a platform-agnostic contract layer, one implementation crate per operating system, and an FFI shim for non-Rust consumers.

```text
uda/
├── crates/
│   ├── uda-core/              # Traits, enums, Capability bitflags, unified UdaError
│   ├── uda-platform-linux/    # Portal + D-Bus + Wayland IPC + CLI fallbacks
│   ├── uda-platform-windows/  # Win32 / COM / WinRT (fully #![cfg(windows)] gated)
│   ├── uda-ffi/               # C-ABI exports → libuda_ffi.so / uda_ffi.dll
│   └── uda-cli/               # Local diagnostic CLI
├── examples/                  # Python (ctypes) and Node.js (koffi) demos
├── include/                   # uda.h — the stable C header
├── docs/internals/            # Protocol specifications per platform & feature
└── scripts/                   # Headless validation helpers
```

| Crate | Responsibility |
| --- | --- |
| [`uda-core`](crates/uda-core) | Defines the public contract: `AppearanceManager`, `WallpaperManager`, `NotificationManager`, `WakeLockManager`, `TrayManager`, `MediaManager`, `SessionManager`, plus `Capability`, `SupportLevel`, `Theme`, `FillMode`, `WallpaperOptions`, and `UdaError`. Contains no platform code. |
| [`uda-platform-linux`](crates/uda-platform-linux) | Implements the contract over `zbus`, the XDG Desktop Portal, GNOME `gsettings`, KDE `plasmashell`, Hyprland/Sway sockets, X11 CLI tools (`feh`, `nitrogen`), and MPRIS v2 / SNI / logind. |
| [`uda-platform-windows`](crates/uda-platform-windows) | Implements the contract over the registry, `SystemParametersInfoW`, `SetThreadExecutionState`, WinRT toasts, `Shell_NotifyIconW`, WinRT SMTC, and the Win32 session/power APIs. Gated by `#![cfg(windows)]` so a Linux host never breaks the build. |
| [`uda-ffi`](crates/uda-ffi) | Wraps both backends behind `#[no_mangle] extern "C"` functions with panic containment at the boundary, and a thread-local last-error slot. |

---

## 🧩 New in v0.2.0 — Tray, Media & Session

### System Tray

Cross-platform menu model — text items, checkboxes, separators, submenus, and disabled states. On Linux the icon is exported over SNI (`org.kde.StatusNotifierItem` + DBusMenu); on Windows it uses `Shell_NotifyIconW` driven by a dedicated worker thread with its own message pump, so the host's event loop is never hijacked.

**Rust**

```rust
use uda_core::tray::{TrayIcon, TrayMenu, TrayIconSource};
use uda_platform_linux::tray::LinuxTrayManager;

let menu = TrayMenu::new()
    .text("设置", || log::info!("settings clicked"))
    .checkbox("自动启动", true, |on| log::info!("autostart = {on}"))
    .separator()
    .text("退出", || std::process::exit(0));

let icon = LinuxTrayManager::new()
    .create(TrayIcon::builder()
        .name("UDA Demo")
        .tooltip("UDA Tray")
        .icon(TrayIconSource::Path("icons/UniDesktop_3D_transparent_mini.png".into()))
        .menu(menu)
        .build())?;

icon.wait();          // block until the icon is destroyed
drop(icon);           // or let Drop unregister it from the tray
```

**Python**

```python
from uda import Uda

with Uda() as uda:
    menu = uda.create_tray_menu()
    menu.add_text("设置", lambda: print("settings"))
    menu.add_checkbox("自动启动", True, lambda on: print("autostart", on))
    menu.add_separator()

    icon = uda.create_tray_icon("UDA Demo", tooltip="UDA Tray")
    icon.menu = menu
    icon.wait()
```

**Node.js**

```javascript
const { Uda } = require('./uda');

const uda = new Uda();
const menu = uda.createTrayMenu();
menu.addText('设置', () => console.log('settings'));
menu.addCheckbox('自动启动', true, (on) => console.log('autostart', on));
menu.addSeparator();

const icon = uda.createTrayIcon('UDA Demo', { tooltip: 'UDA Tray' });
icon.menu = menu;
await icon.wait();
icon.destroy();
```

### Media Playback Control

Read what is playing and drive the player. Linux speaks MPRIS v2 over the session bus; Windows uses WinRT SMTC.

**Rust**

```rust
use uda_core::media::{MediaCommand, MediaManager};
use uda_platform_linux::media::LinuxMediaManager;

let manager = LinuxMediaManager::new().await?;
if let Some(track) = manager.active_metadata()? {
    println!("{} — {}", track.title, track.artists.join(", "));
}
manager.send_command(MediaCommand::PlayPause)?;
```

**Python**

```python
with Uda() as uda:
    track = uda.media.now_playing          # None when no player is running
    if track:
        print(f"{track.title} — {', '.join(track.artists)}")
    print(uda.media.status)                # 'playing' | 'paused' | 'stopped'
    uda.media.send("play_pause")
```

**Node.js**

```javascript
const uda = new Uda();
const now = uda.media.nowPlaying;         // null when no player is running
if (now) console.log(`${now.title} — ${now.artists.join(', ')}`);
console.log(uda.media.status);
uda.media.send('play_pause');
```

### Session & Power Lifecycle

Lock, logout, suspend, hibernate, reboot and shutdown from one call. Windows acquires `SeShutdownPrivilege` through a proper token dance before `ExitWindowsEx`, and a refusal is reported as a typed error rather than a panic.

**Rust**

```rust
use uda_core::session::{SessionAction, SessionManager, perform};
use uda_platform_windows::session::WindowsSessionManager;

let manager = WindowsSessionManager::new();

// Ask first: locking is the only action considered safe to automate.
for action in [SessionAction::Lock, SessionAction::Suspend] {
    if manager.capabilities()?.contains(action.capability()) {
        perform(&manager, action)?;
    }
}
```

**Python**

```python
with Uda() as uda:
    caps = uda.session.capabilities()          # {'lock': True, 'reboot': False, ...}
    if caps["lock"]:
        uda.session.lock()                     # safe to automate
    # uda.session.reboot()                    # destructive — opt in explicitly
```

**Node.js**

```javascript
const uda = new Uda();
console.log(uda.session.capabilities);        // { lock: true, reboot: false, ... }
if (uda.session.supports('lock')) uda.session.lock();
// uda.session.reboot();                     // destructive — opt in explicitly
```

> **Safety contract:** only `lock` is non-destructive. Every other action is gated by a distinct capability bit, so a UI can query before it draws an entry that could shut the machine down.

---

## 🗺️ Roadmap

**Phase 2 — Interactive Shell & System Integration (v0.2.0) is complete.** System tray, media playback control and the session/power lifecycle have all shipped and been verified against real backends.

**Phase 3 (v0.3.0) — Shell Extensions & Window Topology** is the current target:

| Feature | Focus |
| --- | --- |
| Global shortcuts | Key combinations across the Wayland Portal, X11 and Win32 |
| Advanced clipboard | Multi-format MIME read/write with a change listener |
| Audio endpoint routing | Default output device switching and master volume |
| Display brightness | ACPI backlight plus DDC/CI for external monitors |

Later phases cover display topology and HiDPI, taskbar badges and progress bars, native file dialogs, virtual desktops, window aesthetics (Mica / Acrylic / KWin blur), input simulation, screen capture, and idle detection.

---

## 🤝 Community & Sponsorship

**Organization:** Universal Desktop Community

UDA is developed and maintained under the Universal Desktop Community, an open community focused on lowering the cost of cross-platform desktop integration.

- 💛 **Sponsor:** [https://afdian.com/a/srinternet](https://afdian.com/a/srinternet)
- 🌐 **Official website:** [https://unidesktop.sr-studio.cn](https://unidesktop.sr-studio.cn)

---

<div align="center">

Licensed under either of **Apache License 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE)) or **MIT License** ([`LICENSE-MIT`](LICENSE-MIT)), at your option.

**English** · [简体中文](README_CN.md)

</div>
