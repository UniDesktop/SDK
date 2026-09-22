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

> [!WARNING]
> **🚧 Active Development Branch (`develop`) **
> 
> You are viewing the **unstable development branch** for upcoming **v0.2.0**. Code here is actively undergoing experimental integration (System Tray, DBusMenu, Win32 Message Loops) and APIs may break without notice. For the battle-tested stable release, please switch to the [`main`](https://github.com/UniDesktop/SDK/tree/main) branch or check the [latest release (v0.1.0)](https://github.com/UniDesktop/SDK/releases).

## Why UDA?

Building a cross-platform desktop application today means writing the same feature five times: `SystemParametersInfoW` on Windows, then `gsettings`, `qdbus`, `swww`, and `hyprpaper` on Linux — each with its own failure modes, session assumptions, and no shared type system. UDA replaces that fragmentation with one capability-driven Rust API that probes the environment, picks the working backend, and degrades gracefully instead of crashing.

- **Portal-first** — every Linux feature tries the XDG Desktop Portal before touching anything distro-specific.
- **Cascading Fallback** — a strict tier chain: Portal → native DE IPC → CLI tools → a strongly typed `UdaError::Unsupported`.
- **Capability-driven** — features advertise `SupportLevel::Full` / `Restricted` / `Unsupported`, so your app can branch before it breaks.
- **Zero heavy runtime dependencies** — pure Rust `zbus` on Linux, `windows-rs` on Windows. No Qt, no GTK, no bundled toolkit.

---

## 🖥️ Desktop & Platform Support Matrix

| Platform / DE | Theme Detection | Wallpaper | Notification | WakeLock |
| --- | :---: | :---: | :---: | :---: |
| **Windows 10 / 11** | ✅ | ✅ | ✅ | ✅ |
| **GNOME 42+** | ✅ | ✅ | ✅ | ✅ |
| **KDE Plasma 5 / 6** | ✅ | ✅ | ✅ | ✅ |
| **XFCE** | ✅ | ✅ | ✅ | ✅ |
| **Hyprland** | ⚠️ | ✅ | ⚠️ | ⚠️ |
| **Sway** | ⚠️ | ✅ | ⚠️ | ⚠️ |
| **Generic X11** | ⚠️ | ✅ | ⚠️ | ⚠️ |

> **Shipped in this release:** light/dark appearance detection with live system-follow support · wallpaper management with `Crop` / `Fill` / `Fit` / `Stretch` modes, multi-monitor targeting, and dark/light pairing · native notifications with action buttons and urgency hints · keep-awake locks (`WakeLockType::PreventDisplaySleep` / `PreventSystemIdle`).
>
> `✅` verified against the real backend · `⚠️` best-effort fallback — the exact capability is reported at runtime through `capabilities()`.

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
| [`uda-core`](crates/uda-core) | Defines the public contract: `AppearanceManager`, `WallpaperManager`, `NotificationManager`, `WakeLockManager`, plus `Capability`, `SupportLevel`, `Theme`, `FillMode`, `WallpaperOptions`, and `UdaError`. Contains no platform code. |
| [`uda-platform-linux`](crates/uda-platform-linux) | Implements the contract over `zbus`, the XDG Desktop Portal, GNOME `gsettings`, KDE `plasmashell`, Hyprland/Sway sockets, and X11 CLI tools (`feh`, `nitrogen`). |
| [`uda-platform-windows`](crates/uda-platform-windows) | Implements the contract over the registry, `SystemParametersInfoW`, `SetThreadExecutionState`, and WinRT toasts. Gated by `#![cfg(windows)]` so a Linux host never breaks the build. |
| [`uda-ffi`](crates/uda-ffi) | Wraps both backends behind eight `#[no_mangle] extern "C"` functions with panic containment at the boundary, and a thread-local last-error slot. |

---

## 🗺️ Roadmap to v0.2.0

The following are **planned** and not yet implemented.

| Feature | Target |
| --- | --- |
| System Tray (`org.kde.StatusNotifierItem` + `Shell_NotifyIconW`) | Planned |
| MPRIS v2 media control & metadata listener (SMTC on Windows) | Planned |
| Global shortcut registration (Portal / X11 / Win32) | Planned |
| Wayland input simulation via `libei` | Planned |

Later phases cover clipboard, audio routing, display brightness, session lifecycle, virtual desktops, and screen capture.

---

## 🤝 Community & Sponsorship

**Organization:** United Desktop Association

UDA is developed and maintained under the United Desktop Association, an open community focused on lowering the cost of cross-platform desktop integration.

- 💛 **Sponsor:** [https://afdian.com/a/srinternet](https://afdian.com/a/srinternet)
- 🌐 **Official website:** [https://unidesktop.sr-studio.cn](https://unidesktop.sr-studio.cn)

---

<div align="center">

Licensed under either of **Apache License 2.0** ([`LICENSE-APACHE`](LICENSE-APACHE)) or **MIT License** ([`LICENSE-MIT`](LICENSE-MIT)), at your option.

**English** · [简体中文](README_CN.md)

</div>
