# AGENTS.md - UniDesktop API (UDA) Engineering Guidelines

> **Target Audience:** Autonomous AI Code Agents (Claude Code, Cursor, Windsurf, Devin, etc.)  
> **Project Scope:** Cross-platform Desktop Shell Integration SDK for Windows 10/11 & Modern Linux (GNOME, KDE, Wayland Tiling, X11).  
> **Primary Goal:** Fill the "missing bottom half of Qt" by providing deep, unified OS desktop control.

---

## 1. Core Architecture & Guiding Principles

As an AI agent working on UDA, you MUST adhere to the following principles. Any code violating these rules will be rejected.

### Principle 1: Capability-Driven Architecture (Never Panic)
Linux desktop environments are highly fragmented. Wayland strictly restricts certain capabilities (e.g., getting global window coordinates).
- **NEVER use `.unwrap()` or `.expect()`** on system calls, D-Bus invocations, or environment variables.
- All high-level features must expose a **`Capability`** check returning `SupportLevel::Full`, `SupportLevel::Restricted(Reason)`, or `SupportLevel::Unsupported`.
- Always degrade gracefully. If a modern feature fails, fall back to known alternatives before returning an error.

### Principle 2: The Cascading Fallback Engine
When executing an OS desktop action on Linux, strictly follow this fallback hierarchy:
1. **Tier 1 (XDG Desktop Portal):** Check if `org.freedesktop.portal.*` is available.
2. **Tier 2 (Native DE D-Bus/IPC):** Query `$XDG_CURRENT_DESKTOP` and invoke DE-specific D-Bus methods (GNOME, KDE Plasma) or Unix Domain Sockets (Hyprland, Sway).
3. **Tier 3 (CLI Tool Fallback):** If D-Bus is unavailable, probe system `PATH` for standard utilities (`swww`, `hyprpaper`, `feh`, `xfconf-query`).
4. **Tier 4 (Graceful Error):** Return a strongly typed `UdaError::FeatureUnsupported`.

### Principle 3: Zero-Bloat Systems Philosophy
- Core libraries MUST remain lightweight. **DO NOT** pull in Qt, GTK, or heavy GUI frameworks.
- On Linux, use pure-Rust `zbus` for D-Bus IPC.
- On Windows, use official `windows-rs` APIs. Ensure `crates/uda-platform-windows` uses `#![cfg(windows)]` guards so `cargo check --workspace` never breaks on Linux.

---

## 2. Workspace & Crate Structure

The repository is a Rust Workspace divided into distinct layers of abstraction:

```text
uda/
├── crates/
│   ├── uda-core/              # Public Traits, Enums, Capability Flags, Unified Errors
│   ├── uda-platform-linux/    # Linux Implementations (D-Bus, Wayland IPC, X11)
│   ├── uda-platform-windows/  # Windows Implementations (Win32, COM, WinRT)
│   ├── uda-ffi/               # C-ABI export layer (for Node.js, Python, C++)
│   └── uda-cli/               # Diagnostic CLI tool for local developer manual testing
├── docs/internals/            # SPECIFICATION DICTIONARY (Reference book for protocols)
├── scripts/                   # Test automation & headless validation scripts
└── Cargo.toml                 # Workspace manifest
```

---

## 3. Protocol Specification Reference (The Cheat Book)

Before writing any platform-specific logic, consult the specifications stored in `docs/internals/`:

- **Wallpaper Backend:** See `docs/internals/wallpaper_specs.md`
  - GNOME: GSettings over D-Bus (`org.gnome.desktop.background picture-uri` / `picture-uri-dark`).
  - KDE: D-Bus `org.kde.plasmashell` -> `/PlasmaShell` -> `evaluateScript`.
  - Hyprland: IPC socket via `hyprpaper` or `swww`.
  - Windows: Win32 `SystemParametersInfoW(SPI_SETDESKWALLPAPER)`.
- **System Appearance:** See `docs/internals/appearance_specs.md`
  - Linux: `org.freedesktop.portal.Settings` -> `Read("org.freedesktop.appearance", "color-scheme")`.
  - Windows: Registry `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`.
- **System Notifications:**
  - Linux: `org.freedesktop.Notifications` over Session D-Bus.
  - Windows: WinRT `ToastNotificationManager`.
- **System Tray:**
  - Linux: `org.kde.StatusNotifierItem` (SNI) protocol.
  - Windows: `Shell_NotifyIconW`.

---

## 4. Phased Roadmap & Deliverables

### Phase 1: MVP Core Foundation (v0.1.0 RELEASED) [COMPLETED]
- [x] **Core Types & Traits:** Error model, `Capability`, `SupportLevel`, `Theme`, `WallpaperOptions`.
- [x] **Appearance Module:** Detect & listen to Dark/Light theme, Accent color (Portal + Registry).
- [x] **Wallpaper Module:** Set & get wallpaper with FillMode (GNOME, KDE, Hyprland, Sway, X11, Win32).
- [x] **Notification Module:** Native notifications with actions, urgency, and timeout.
- [x] **WakeLock Module:** Prevent display/system sleep (Inhibit portal, ScreenSaver, Win32).
- [x] **C-ABI & FFI:** `crates/uda-ffi` + Python & Node.js examples + CI/CD all green.

### Phase 2: Interactive Shell & System Integration (v0.2.0 TARGET) [IN PROGRESS]
- [ ] **System Tray Module (Priority #1):**
  - Cross-platform menu model: Text items, Checkboxes, Separators, Submenus, Disabled states.
  - Linux: `org.kde.StatusNotifierItem` (SNI) via D-Bus + `com.canonical.dbusmenu`.
  - Windows: `Shell_NotifyIconW` + internal hidden worker thread for message pump.
  - Non-blocking: Tray must run on internal worker threads, never hijacking the host's event loop.
  - Lifecycle: `TrayIcon` implements `Drop` to automatically remove icon on shutdown.
- [ ] **Media Playback Controls (MPRIS v2 + SMTC):** Dual-way media status listener and player control.
- [ ] **Global Shortcuts:** Register global key combinations across Portal, X11, and Win32.
- [ ] **Clipboard Enhancements:** Multi-format MIME clipboard listener.

### Phase 3: Shell Extensions & Window Topology
- [ ] **Display Topology:** Screen geometry, HiDPI scale factor, refresh rate, hotplug events (`QScreen` parity).
- [ ] **Global Shortcuts:** Listen to key combinations across Wayland Portal, X11, and Win32.
- [ ] **Taskbar Enhancements:** Taskbar icon badge numbers, progress bars, and JumpLists.
- [ ] **Native Dialogs:** Portal FileChooser and Win32 IFileDialog abstractions.
- [ ] **Live Wallpaper Engine (Foundation):** Transparent window layer on Wayland (`layer-shell`) and Windows (`WorkerW`).

### Phase 4: Window Orchestration & Workspaces
- [ ] **Virtual Desktops:** Enumerate, switch, and move windows across workspaces.
- [ ] **External Window Control:** List active windows, minimize, focus, set always-on-top.
- [ ] **Window Aesthetics:** Request Mica, Acrylic, or KWin blur hints.

### Phase 5: Automation & Input Simulation
- [ ] **Input Simulation:** Smooth mouse move, click, and keystroke injection (`libei` + `SendInput`).
- [ ] **Eyedropper:** Color picker abstraction.
- [ ] **Screen Capture:** Low-latency frame streaming via PipeWire Portal and Desktop Duplication API.
- [ ] **User Idle Detection:** Query elapsed seconds since last user input.

---

## 5. Testing & Verification Rules

Every code change must be strictly verified. You must never claim a task is complete without running and passing verification scripts.

### Test Execution Command
Every code change must pass:
1. `cargo check --workspace --all-targets`
2. `./scripts/test-linux-mock.sh`
3. `cargo test -p uda-ffi`

### Verification Criteria
1. **Compilation Check:** `cargo check --workspace` must pass with zero warnings/errors.
2. **D-Bus Mock Tests:** Unit tests in `uda-platform-linux` must run inside `dbus-run-session` using `python3-dbusmock` fixtures.
3. **Cross-Compilation Hygiene:** The Linux agent must not break Windows compilation. Ensure all Windows code is gated behind `#[cfg(windows)]`.

---

## 6. Coding Conventions & Patterns

1. **Error Handling:** Use `thiserror` for library-internal errors:
   ```rust
   #[derive(thiserror::Error, Debug)]
   pub enum UdaError {
       #[error("Feature unsupported on this desktop: {0}")]
       Unsupported(String),
       #[error("D-Bus communication error: {0}")]
       DBusError(#[from] zbus::Error),
       #[error("Platform IO error: {0}")]
       IoError(#[from] std::io::Error),
   }
   ```
2. **Asynchronous Runtime:** When async is required (e.g., listening to D-Bus signals or registry events), use `tokio`. Provide synchronous wrapper methods where practical.
3. **Logging:** Use the `log` crate (`log::debug!`, `log::warn!`). Never use raw `println!` in library crates.
