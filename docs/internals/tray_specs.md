# System Tray Module - Protocol Specifications

> Reference dictionary for the U tray module. Consult before writing any
> platform-specific tray logic. Mirrors the layout of `wallpaper_specs.md` and
> `appearance_specs.md`.

---

## 0. Design Constraints (from AGENTS.md)

| Constraint | Consequence for this module |
|---|---|
| **Never panic** (Principle 1) | No `unwrap()`/`expect()` on FFI, D-Bus, env vars, or user-supplied icon data. Every fallible step returns `Result<_, UdaError>`. |
| **Capability-driven** (Principle 1) | `TrayManager::capabilities()` returns a `Capability` set; `SupportLevel` distinguishes `Full` / `Partial` (e.g. no double-click event) / `None` (no tray backend at all). |
| **Cascading fallback** (Principle 2) | Linux: StatusNotifierItem -> AppIndicator -> typed `UdaError::NotSupported`. Windows: `Shell_NotifyIconW`. |
| **Zero-bloat** (Principle 3) | No Qt/GTK. Linux uses `zbus`; Windows uses `windows-rs`. |
| **Non-blocking** (AGENTS.md Phase 2) | A dedicated worker thread owns the platform message loop. The host's main thread is never blocked or polled. |

---

## 1. Linux: StatusNotifierItem (SNI) - Tier 1

### 1.1 Protocol overview
The **StatusNotifierItem** specification (freedesktop.org, "StatusNotifierItem and
StatusNotifierWatcher") replaces the legacy XEmbed system tray. It is D-Bus based,
which is why it is the only mechanism that works uniformly on Wayland.

**Required bus name** (must be unique per process, hence the PID):
```
org.kde.StatusNotifierItem-<pid>-<counter>
```

**Required object path:**
```
/StatusNotifierItem
```

### 1.2 Watcher registration (must happen first)
The item must announce itself to the watcher that owns the tray area. The watcher
is normally started by the desktop shell and owns a well-known name:

- **Primary**: `org.kde.StatusNotifierWatcher` on `/StatusNotifierWatcher`
- **Legacy fallback**: `org.freedesktop.StatusNotifierWatcher` (same interface)

Registration method:
```text
org.kde.StatusNotifierWatcher.RegisterStatusNotifierItem(service: STRING)
```
`service` is the **bus name** (not the object path) that the item owns. On failure
the watcher is absent -> fall through to Tier 2.

### 1.3 Implemented interface: `org.kde.StatusNotifierItem`

**Methods**
| Method | Signature | Purpose |
|---|---|---|
| `Activate` | `(x: i32, y: i32)` | Primary activation. Reported as the left-click event. |
| `SecondaryActivate` | `(x: i32, y: i32)` | Secondary activation (middle click on most shells). |
| `ContextMenu` | `(x: i32, y: i32)` | Shell requests the menu at (x, y). |
| `Scroll` | `(i: i32, delta: STRING)` | Wheel input; `delta` is `"up"`/`"down"`/`"left"`/`"right"`. |
| `ProvideXdgActivationToken` | `(s: STRING)` | Wayland activation token handshake. |

**Signals** (sent *by the item*)
| Signal | Signature | When |
|---|---|---|
| `NewTitle` | `()` | Tooltip changed. |
| `NewIcon` | `()` | Icon pixmap changed. |
| `NewAttentionIcon` | `()` | Attention icon changed. |
| `NewStatus` | `(s: STRING)` | Status changed; `"Passive"` / `"Active"` / `"NeedsAttention"`. |
| `XAyatanaLabelChanged` | `(i, s)` | Ayatana label group update. |
| `XAyatanaLabelGuideChanged` | `(s)` | Ayatana label guide update. |

**Properties** (the shell reads these)
| Property | Type | Notes |
|---|---|---|
| `Category` | `s` | `"ApplicationStatus"` or `"SystemServices"`. |
| `Id` | `s` | Unique item id; DBus menu path is usually `"/MenuBar"`-ish for this id. |
| `Title` | `s` | Tooltip **title** line. |
| `Status` | `s` | `"Active"`/`"Passive"`/`"NeedsAttention"`. |
| `WindowId` | `u` | 0 when the item has no X window (Wayland case). |
| `IconName` | `s` | Freedesktop icon-theme name; empty when a pixmap is supplied. |
| `IconPixmap` | `a(iiay)` | **32bpp rows**, bottom-to-top: `[width, height, arggb_bytes]`. Byte order per pixel is **B, G, R, A** — not A, R, G, B. |
| `OverlayIconName` / `OverlayIconPixmap` | `s` / pixmap | Optional overlay badge. |
| `AttentionIconName` / `AttentionIconPixmap` | `s` / pixmap | Optional attention icon. |
| `ToolTip` | `(sa(iiay)ss)` | `[icon_name, pixmap, title, description]`. |
| `Menu` | `o` | Object path of the `com.canonical.dbusmenu`. |
| `ItemIsMenu` | `b` | `true` when `Activate`/`SecondaryActivate` open menus instead of firing events. |

### 1.4 Icon encoding gotcha (critical)
`IconPixmap` is `a(iiay)` where each row is **32 bits per pixel, little-endian,
bottom-up**. The "ARGB32" in the spec refers to a 32-bit pixel, but the **bytes
of each pixel are ordered B, G, R, A**.
UDA's `TrayIcon::from_rgba()` takes straight **RGBA top-down** bytes, so the
platform backend must:

1. validate `width`, `height`, `stride` and `data.len()` (see 3.1);
2. reorder each pixel from R, G, B, A to **B, G, R, A** (this swaps R with B;
   alpha keeps its place at the end);
3. reverse row order (top-down -> bottom-up);
4. honour `stride` (bytes per row, `>= width * 4`); if the stride is larger than
   the pixel width, the padding bytes must be skipped, not copied.

Do **not** write A, R, G, B: that keeps the red and blue channels in each
other's slots, so a red icon arrives blue and vice versa. The regression test
`red_and_blue_are_not_swapped` in `uda-platform-linux/src/tray.rs` pins this
down with a single asymmetric pixel.

### 1.5 Menus: `com.canonical.dbusmenu`

The menu is exported as a **separate object** at the path advertised by the `Menu`
property, usually `/MenuBar`.

**Interface:** `com.canonical.dbusmenu`

| Member | Signature | Notes |
|---|---|---|
| **Methods** `GetLayout` | `(i parentId, i recursionDepth, as propertyNames) -> (u revision, (ia{sv}ia{sv}v))` | `recursionDepth` of 0 means "unlimited". Returns the item tree. |
| `GetGroupProperties` | `(ai ids, as propertyNames) -> a(ia{sv})` | For incremental updates. |
| `GetProperty` | `(i id, s name) -> v` | |
| `Event` | `(i id, s eventId, v data, u timestamp)` | `eventId` is `"clicked"` or `"hovered"`. |
| `EventGroup` | `(a(isvu) events)` | Batched events. |
| `AboutToShow` | `(i id) -> b` | Returns whether the submenu content actually changed. |
| **Signals** `ItemsPropertiesUpdated` | `(a(ia{sv}), a(is))` | Changed/removed properties. |
| `LayoutUpdated` | `(u revision, i parentId)` | Full re-read required. |
| `ItemActivationRequested` | `(i id, u timestamp)` | Ask the shell to open the item. |

**Item property names** (`a{sv}` keys) that UDA maps:
| Key | Type | UDA meaning |
|---|---|---|
| `type` | `s` | `"standard"` / `"separator"` (Checkbox and Submenu are modelled by combining `toggle-type` with `standard`). |
| `label` | `s` | Text item label; checkbox prefix (`"\u2713 "`) is added by the shell on GNOME, but UDA ships the glyph so the model stays cross-platform. |
| `enabled` | `b` | !Disabled |
| `visible` | `b` | Always `true` in UDA. |
| `icon-name` / `icon-data` | `s` / `ay` | Not used in Step 1. |
| `toggle-type` | `s` | `"checkmark"` for Checkbox items. |
| `toggle-state` | `i` | 0 = off, 1 = on. |
| `children-display` | `s` | `"submenu"` for Submenu items. |
| `disposition` | `s` | `"informative"` / `"warning"` / `"alert"`; not used in Step 1. |

**Gotcha:** `Event` carries an integer item id, not a path. The backend must keep a
stable `id -> MenuItemId` map and rebuild it whenever the tree is mutated, because
the shell caches layout revisions.

### 1.6 AppIndicator (Tier 2 - legacy fallback)

Used when no `org.kde.StatusNotifierWatcher` is available but an AppIndicator
compatibility layer exists (older Ubuntu, some XFCE/LXDE setups, XEmbed trays).

- **Bus name:** `org.kde.StatusNotifierItem-<pid>` is *not* used; AppIndicator
  registers via its own service or exports the same `org.kde.StatusNotifierItem`
  interface on a requested name.
- **Detection order:** try `org.kde.StatusNotifierWatcher` first; if the bus error
  is `ServiceUnknown`, try `org.freedesktop.StatusNotifierWatcher`; if that also
  fails, probe `PATH` for `ayatana-indicator-*` / `nm-applet` style hosts before
  returning `UdaError::NotSupported`.
- **Menu:** the same `com.canonical.dbusmenu` interface is reused, so the menu
  model is shared with Tier 1. This is the reason UDA exports the menu as a
  separate object rather than embedding it in the item.

### 1.7 linux capability matrix
| Capability | Full when | Partial when | None when |
|---|---|---|---|
| Show icon | SNI or AppIndicator reachable | - | no watcher, no D-Bus session |
| Tooltip | `ToolTip` accepted | shell ignores `ToolTip` (still shows `Title`) | - |
| `on_click` | `Activate` delivered | shell never sends `Activate` | - |
| `on_double_click` | **never Full on Linux** - SNI has no double-click event | UDA synthesises it from two `Activate` calls within ~500 ms | - |
| Context menu | `com.canonical.dbusmenu` exported | shell shows no menu | - |
| Checkbox items | `toggle-type` honoured | shell renders as plain text | - |

> **Key limitation:** SNI defines *no* double-click event. UDA reports
> `SupportLevel::Partial` on Linux and synthesises double-click from two
> `Activate` signals within a short window. This must be documented in the
> capability query so hosts can degrade.

---

## 2. Windows: `Shell_NotifyIconW` - Tier 1 (only tier)

### 2.1 Core API
```text
BOOL Shell_NotifyIconW(DWORD dwMessage, NOTIFYICONDATAW *pnid);
```
Messages: `NIM_ADD`, `NIM_MODIFY`, `NIM_DELETE`, `NIM_SETFOCUS`, `NIM_SETVERSION`.

`NOTIFYICONDATAW` sizing rules: initialise with `size = std::mem::size_of::<NOTIFYICONDATAW>()`,
set `hWnd`, `uID`, `uFlags`, and only the union members implied by `uFlags`.

### 2.2 Version handshake (required for modern behaviour)
```text
Shell_NotifyIconW(NIM_SETVERSION, &NOTIFYICONDATAW { uVersion = NOTIFYICON_VERSION_4, .. })
```
`NOTIFYICON_VERSION_4` (Vista+) makes the shell use `WM_MENUSELECT`-free callback
messages and respect balloon timeouts. Without it the shell falls back to the
Windows 95 behaviour, which is why the handshake is mandatory, not optional.

### 2.3 Callback messages (posted to `hWnd` with `uCallbackMessage` in `uFlags`)
| Message | UDA event |
|---|---|
| `WM_LBUTTONDOWN` | press (tracked for click vs double-click) |
| `WM_LBUTTONUP` | **`on_click`** |
| `WM_LBUTTONDBLCLK` | **`on_double_click`** |
| `WM_RBUTTONUP` | request context menu |
| `WM_RBUTTONDOWN` | request context menu (some shells) |
| `WM_MOUSEMOVE` | hover (used to re-arm double-click tracking) |
| `WM_CONTEXTMENU` | request context menu at (x, y) |
| `NIN_BALLOONUSERCLICK` / `NIN_BALLOONTIMEOUT` / `NIN_BALLOONHIDE` | balloon feedback |

### 2.4 Window requirements
- The `hWnd` must belong to a thread that runs a **message loop**
  (`GetMessageW` / `TranslateMessage` / `DispatchMessageW`). UDA therefore creates
  a **hidden message-only window** (`HWND_MESSAGE` parent) on the worker thread.
- The window class must be registered once per process; use a unique class name to
  avoid clashing with the host application's own classes.
- The callback message id (`uCallbackMessage`) must be `>= WM_USER` (typically
  `WM_APP + n`).

### 2.5 Menu rendering
Windows has no object-path protocol. The menu must be materialised natively:

1. Build an `HMENU` recursively from `TrayMenu` via `CreatePopupMenu()` /
   `AppendMenuW()` / `InsertMenuItemW()`.
2. Map `MenuItem` variants: Text -> `MF_STRING`, Checkbox -> `MF_STRING` with
   `MFS_CHECKED`/`MFS_UNCHECKED` + `MFT_RADIOCHECK` when exclusive, Separator ->
   `MF_SEPARATOR`, Submenu -> `MF_POPUP` with a nested `HMENU`, Disabled ->
   `MF_DISABLED` (and `MF_GRAYED`).
3. Before showing: `SetForegroundWindow(hwnd)` then `TrackPopupMenuEx(...)` with
   `TPM_RIGHTBUTTON`; the foreground call is **mandatory**, otherwise the popup
   does not dismiss on an outside click.
4. Command ids: `WM_COMMAND` carries a 16-bit id, so the backend keeps a
   `id -> MenuItemId` table rebuilt on each menu mutation.
5. After `TrackPopupMenuEx` returns, post a harmless `WM_NULL` to dismiss.

### 2.6 Icon limits and degradation
| Limit | Value | UDA degradation |
|---|---|---|
| Icon size | 16x16 @96dpi for the tray (GetSystemMetrics(SM_CXSMICON)) | `LoadImageW` with `LR_DEFAULTSIZE`; RGBA bytes are wrapped in `CreateIcon`/`CreateBitmap` and scaled with `StretchDIBits` |
| Tooltip (`szTip`) | **128 UTF-16 code units incl. NUL** on <Vista; **128** is still the safe cap under `NOTIFYICONDATAW` | Truncate to 127 chars at a UTF-8 char boundary; never truncate a surrogate pair; log `debug!` on truncation |
| Balloon (`szInfo`) | 256 chars, `szInfoTitle` 64 chars | Same boundary-safe truncation |

### 2.7 Lifecycle
- `Shell_NotifyIconW(NIM_DELETE, ...)` **must** be called before the `hWnd` is
  destroyed, otherwise the shell keeps a dangling `hWnd` and explorer may show a
  ghost icon until restart.
- UDA's `Drop` therefore: unregister icon -> stop the message pump -> destroy the
  hidden window -> unregister the window class (once the last icon is gone).
- `Drop` is guarded by an internal `AtomicBool`/`Mutex` so a double drop, a drop
  during teardown, or a drop racing a menu open can never double-free.

---

## 3. Cross-platform model rules

### 3.1 RGBA validation (shared by both backends)
`from_rgba(width, height, stride, data)` is rejected (`UdaError::NotSupported`)
when any of:
- `width == 0 || height == 0`
- `stride < width * 4`
- `data.len() < stride * height`
- `width * height * 4` overflows `usize`

### 3.2 Tooltip policy
- Target length: **80 chars**, safe cap: **127 UTF-16 code units** (Windows).
- Truncate on a `char` boundary; append no ellipsis (ellipsis chars may not encode
  under a legacy console and would break the ASCII-only script rule).
- Report `SupportLevel::Partial` when the tooltip had to be truncated.

### 3.3 Event delivery guarantee
- Callbacks fire on the **tray worker thread**, never on the host's main thread.
- A callback must never call back into `TrayIcon` synchronously: mutations are
  forwarded through the worker's message queue. See
  `crates/uda-core/src/tray.rs` for the queue design.

### 3.4 Drop idempotency contract
1. Drop takes an internal `shutdown` flag; the first caller wins.
2. The worker receives a `Shutdown` message and performs: delete icon -> quit
   message loop -> destroy window -> join.
3. Drop joins the worker thread with a bounded wait; on timeout it logs `warn!`
   and returns rather than hanging the host's exit path.
4. No path may `unwrap()` the join handle or the internal `Mutex`.

---

## 4. Capability reporting summary

| Capability | Windows | Linux (SNI) | Linux (AppIndicator) |
|---|---|---|---|
| Icon show/hide | Full | Full | Full |
| Tooltip | Full | Full | Full |
| `on_click` | Full | Full | Full |
| `on_double_click` | **Full** | **Partial** (synthesised) | **Partial** (synthesised) |
| Context menu | Full | Full | Full |
| Checkbox | Full | Full | Full |
| Dynamic menu update | Full (rebuild + TrackPopupMenu) | Full (LayoutUpdated signal) | Full |
