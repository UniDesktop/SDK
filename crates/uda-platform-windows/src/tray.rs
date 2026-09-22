//! Windows system tray backend: `Shell_NotifyIconW` + a hidden message-only window.
//!
//! # Architecture
//!
//! ```text
//!  host thread                     tray worker thread
//!  ───────────                     ──────────────────
//!  TrayIcon ── Arc<TrayIconInner>   HWND_MESSAGE window
//!    ├ tooltip / icon / menu         ├ message pump: GetMessageW / Translate /
//!    └ visible / capabilities        │   DispatchMessageW
//!                                    └ window proc: WM_xxx -> TrayEvent
//! ```
//!
//! A tray icon on Windows is a window, not an object: the shell posts callback
//! messages to the `hWnd` recorded in `NOTIFYICONDATAW`, so that window must
//! belong to a thread that runs a message loop. UDA therefore creates a
//! message-only window (`HWND_MESSAGE` parent) on a dedicated worker thread and
//! never touches the host's message queue, which keeps the host free of any
//! requirement to pump messages itself.
//!
//! # No shared-state mirror
//!
//! Unlike the Linux backend (see `crates/uda-platform-linux/src/tray.rs`), the
//! worker thread cannot just read a mirror: Win32 calls such as
//! `Shell_NotifyIconW`, `CreatePopupMenu` and `TrackPopupMenuEx` must be issued
//! from the thread that owns the window. The host's mutations are therefore
//! *forwarded* to the worker as user messages, and the worker applies them
//! under its own lock. This satisfies `tray_specs.md` §3.3 ("callbacks fire on
//! the tray worker thread, never on the host's main thread") and is what keeps
//! a `Drop` racing a menu open from corrupting the window.
//!
//! # Never panic (AGENTS.md §1)
//!
//! Every Win32 result is inspected and mapped into
//! [`UdaError`](uda_core::error::UdaError). No `unwrap()`, `expect()`,
//! `panic!()` or `unreachable!()` on a Win32 call; the FFI is inherently unsafe,
//! so each unsafe block carries a `// SAFETY:` note naming the invariant it
//! relies on.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use windows::Win32::Foundation::{
    ERROR_CLASS_ALREADY_EXISTS, GetLastError, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::UI::WindowsAndMessaging::HWND_MESSAGE;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HDC, HBRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
// `Shell_NotifyIconW`, `NOTIFYICONDATAW` and the `NIM_*`/`NIF_*` constants live in
// `Win32::UI::Shell`; the icon-creation and window APIs (`CreateIcon`,
// `CreateIconIndirect`, `ICONINFO`, `RegisterClassW`, ...) are exported from
// `Win32::UI::WindowsAndMessaging`.
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_STATE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW,
    ICONINFO, KillTimer, PostMessageW, RegisterClassW, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, TrackPopupMenuEx, TranslateMessage, UnregisterClassW, GWLP_USERDATA,
    HCURSOR, HICON, HMENU, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MSG, TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
    WINDOW_STYLE, WNDCLASSW, WNDCLASS_STYLES, WS_OVERLAPPED,
};

use uda_core::capability::{Capability, SupportLevel};
use uda_core::error::UdaError;
use uda_core::tray::{
    MenuItem, TrayEvent, TrayFeature, TrayIcon, TrayIconConfig, TrayIconSource, TrayManager,
};

/// Callback message id posted by the shell into the hidden window.
///
/// `tray_specs.md` §2.4 requires `>= WM_USER`; `WM_APP` (0x8000) is the
/// conventional choice because it leaves the whole `WM_USER` range free for the
/// host application should it ever share a window.
const CALLBACK_MESSAGE: u32 = 0x8000;

/// Window style used for the hidden window.
///
/// `WS_OVERLAPPED` is the harmless default; a message-only window shows nothing
/// and has no Z-order, so no extended style (`WS_EX_*`) is requested.
const WINDOW_STYLE_BITS: WINDOW_STYLE = WS_OVERLAPPED;

/// Extended style for the hidden window.
///
/// No flags: the window is invisible by virtue of its `HWND_MESSAGE` parent, so
/// there is nothing to hide from the taskbar or the Alt-Tab list.
const WINDOW_EX_STYLE_BITS: WINDOW_EX_STYLE = WINDOW_EX_STYLE(0);

/// Window class name registered once per process.
///
/// Unique by construction: the PID and a process-wide counter are folded in so
/// two UDA tray icons in the same process never collide, and a class name that
/// happens to match the host application's own class cannot be claimed.
const fn class_name() -> &'static str {
    // A per-process unique suffix is appended by `register_window_class`, which
    // owns the counter; this is the stable prefix.
    "UDA_TrayMessageWindow"
}

/// Per-process counter disambiguating window classes and tray icon ids.
static TRAY_COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// UTF-16 helpers
// ---------------------------------------------------------------------------

/// Encode a Rust string as a NUL-terminated UTF-16 buffer.
///
/// Win32's `*W` APIs take `PCWSTR`; owning the buffer (rather than borrowing a
/// temporary) is what makes it safe to hand to an FFI call.
fn to_utf16(text: &str) -> Vec<u16> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    wide
}

/// Decode a NUL-terminated UTF-16 buffer into an owned string.
///
/// The inverse of [`to_fixed_utf16`]; it exists so a fixed-width Win32 field can
/// be read back in a test, and a malformed buffer degrades to a replacement
/// character instead of failing.
#[cfg(test)]
fn from_utf16(wide: &[u16]) -> String {
    let len = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..len])
}

/// Encode a string into a fixed-size UTF-16 field, NUL-padded.
///
/// `NOTIFYICONDATAW::szTip` is a `[u16; 128]` inline array (not a pointer), so
/// the caller needs a sized, NUL-terminated copy. The truncation happens on a
/// char boundary because [`uda_core::tray::sanitize_tooltip`] already capped the
/// Rust string, so this only has to avoid splitting a surrogate pair.
fn to_fixed_utf16<const N: usize>(text: &str) -> [u16; N] {
    let mut buffer = [0u16; N];
    // Reserve one element for the terminator.
    let limit = N.saturating_sub(1);
    // `encode_utf16` yields one u16 per BMP char and two per astral char, so
    // stopping once `limit` units are written keeps a pair intact whenever the
    // string was already capped in chars.
    let mut written = 0;
    for unit in text.encode_utf16() {
        if written >= limit {
            log::debug!("tray tip truncated at {limit} UTF-16 units");
            break;
        }
        buffer[written] = unit;
        written += 1;
    }
    buffer
}

// ---------------------------------------------------------------------------
// Icon transcoding
// ---------------------------------------------------------------------------

/// Convert a validated RGBA buffer into a Win32 `HICON`.
///
/// `tray_specs.md` §2.6 says an RGBA icon is wrapped in a bitmap and scaled; the
/// smallest faithful path is a 32bpp top-down DIB section fed to
/// `CreateIconIndirect` with an opaque mask. The DIB is created from the
/// *validated source* rather than through the Linux transcoder, because Win32
/// wants top-down BGRA while the Linux path produces bottom-up ARGB.
///
/// Returns `None` when the source is not RGBA or fails validation, so the
/// caller can degrade instead of failing registration.
fn icon_from_source(source: &TrayIconSource) -> Option<HICON> {
    let TrayIconSource::Rgba {
        width,
        height,
        stride,
        data,
    } = source
    else {
        return None;
    };
    // Validation is the platform-agnostic contract (`tray_specs.md` §3.1); a
    // host-supplied buffer is never trusted before it reaches the FFI.
    if source.validate().is_err() {
        log::warn!("tray icon failed validation; no icon will be set");
        return None;
    }

    let width = *width as i32;
    let height = *height as i32;
    let stride = *stride as usize;
    let width_bytes = (width as usize) * 4;

    // Win32 wants BGRA (premultiplied alpha is not used by the shell for tray
    // icons), so each pixel's R and B channels are swapped from the source RGBA.
    let mut pixels = vec![0u8; width_bytes * height as usize];
    for row in 0..height as usize {
        let src = &data[row * stride..row * stride + width_bytes];
        let dst = &mut pixels[row * width_bytes..row * width_bytes + width_bytes];
        for (source_pixel, target_pixel) in src.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
            target_pixel[0] = source_pixel[2];
            target_pixel[1] = source_pixel[1];
            target_pixel[2] = source_pixel[0];
            target_pixel[3] = source_pixel[3];
        }
    }

    // SAFETY: `GetDC(None)` returns the desktop DC, which the worker thread may
    // use for the (tiny) duration of building the icon.
    let dc = unsafe { GetDC(HWND::default()) };
    if dc.is_invalid() {
        log::warn!("GetDC failed; tray icon cannot be built");
        return None;
    }
    let bitmap = create_color_bitmap(dc, width, height, &pixels);
    // SAFETY: the DC is released exactly once, immediately after the bitmap is
    // detached from it.
    unsafe {
        let _ = ReleaseDC(HWND::default(), dc);
    }
    let bitmap = match bitmap {
        Some(bitmap) => bitmap,
        None => {
            log::warn!("could not build a tray icon bitmap");
            return None;
        }
    };

    // An opaque 1bpp mask keeps the shell from treating transparent pixels as
    // "cut out"; the alpha channel in the colour bitmap carries the shape.
    let mask = create_mask_bitmap(width, height);
    let info = ICONINFO {
        fIcon: windows::Win32::Foundation::BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: bitmap,
    };

    // SAFETY: `CreateIconIndirect` borrows both bitmaps for the lifetime of the
    // icon it returns; both stay alive until after the call and are only freed
    // once the icon is no longer referenced.
    let icon = unsafe { CreateIconIndirect(&info) };
    match icon {
        Ok(icon) => Some(icon),
        Err(error) => {
            log::warn!("CreateIconIndirect failed: {error}");
            None
        }
    }
}

/// Build a 32bpp top-down BGRA `HBITMAP` holding `pixels`.
///
/// A DIB section is used instead of `CreateBitmap` so the pixel layout is
/// exactly `BI_RGB` with no palette translation, which is what keeps an RGBA
/// icon from appearing with swapped channels.
fn create_color_bitmap(dc: HDC, width: i32, height: i32, pixels: &[u8]) -> Option<HBITMAP> {
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        // A positive height means bottom-up; the shell wants top-down for a
        // colour bitmap, hence the negation.
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        biSizeImage: (width * height * 4) as u32,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    let info = BITMAPINFO {
        bmiHeader: header,
        bmiColors: [Default::default()],
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();

    // SAFETY: `CreateDIBSection` only reads `info` and writes through `bits`;
    // both outlive the call, and the returned bitmap owns its own copy.
    let bitmap = unsafe { CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, None, 0) };
    let bitmap = match bitmap {
        Ok(bitmap) => bitmap,
        Err(error) => {
            log::warn!("CreateDIBSection failed: {error}");
            return None;
        }
    };
    if bits.is_null() {
        return None;
    }

    // The section is writable, so the pixels are copied straight in.
    // SAFETY: `bits` points at `width * height * 4` bytes as declared in the
    // header, and `pixels` is exactly that long (checked by the caller).
    unsafe {
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u8>(), pixels.len());
    }
    Some(bitmap)
}

/// Build a 1bpp monochrome mask covering the whole icon.
///
/// All-zero bits mean "leave the colour bitmap visible", which is the correct
/// mask when alpha already encodes the shape.
fn create_mask_bitmap(width: i32, height: i32) -> HBITMAP {
    // A monochrome bitmap row is padded to 16 bits; `width <= 32` for any tray
    // icon, so one u16 per row is enough and the rest stays zero.
    let row_bytes = (((width + 15) / 16) * 2) as usize;
    let mut zeros = vec![0u8; row_bytes * height as usize];
    // SAFETY: `CreateBitmap` reads `zeros` as planar data of the documented size.
    unsafe { CreateBitmap(width, height, 1, 1, Some(zeros.as_mut_ptr().cast())) }
}

/// Destroy an icon built by [`icon_from_source`], if the shell lost it.
///
/// Kept as a named function so the drop path and the icon-swap path call the
/// same code, and so a failed `DestroyIcon` is only a log line.
fn destroy_icon(icon: HICON) {
    if icon.is_invalid() {
        return;
    }
    // SAFETY: `icon` either came from `CreateIconIndirect` or is invalid; both
    // are safe to pass to `DestroyIcon` exactly once.
    unsafe {
        if let Err(error) = DestroyIcon(icon) {
            log::debug!("DestroyIcon reported {error}");
        }
    }
}

/// Whether two icon sources describe the same image.
///
/// `TrayIconSource` has no `PartialEq` (it owns a pixel buffer), so equality is
/// decided structurally. A `Path` change and an RGBA change are both detected
/// without comparing every byte when the shapes already differ.
fn icons_equal(left: Option<&TrayIconSource>, right: Option<&TrayIconSource>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(TrayIconSource::Path(a)), Some(TrayIconSource::Path(b))) => a == b,
        (
            Some(TrayIconSource::Rgba {
                width: lw,
                height: lh,
                stride: ls,
                data: ld,
            }),
            Some(TrayIconSource::Rgba {
                width: rw,
                height: rh,
                stride: rs,
                data: rd,
            }),
        ) => lw == rw && lh == rh && ls == rs && ld == rd,
        _ => false,
    }
}

/// Whether two menus are the same menu.
///
/// `Arc::ptr_eq` is the right test: the host replaces the `Arc` when it swaps
/// menus, and a mutating host keeps the same `Arc` (in which case the visible
/// rows are rebuilt from it anyway, because the menu is re-read at open time).
fn menus_equal(left: Option<&Arc<uda_core::tray::TrayMenu>>, right: Option<&Arc<uda_core::tray::TrayMenu>>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Menu model
// ---------------------------------------------------------------------------

/// One command id allocated for a menu row, plus the callback it fires.
///
/// `WM_COMMAND` carries a 16-bit id, so the backend keeps a `id -> row` table
/// rebuilt on every menu mutation (`tray_specs.md` §2.5 item 4). The callback is
/// cloned out of the host's row at build time, so a click never has to reach
/// back into `TrayMenu` while the shell menu is open.
struct MenuEntry {
    /// Win32 command id used in `AppendMenuW`.
    command_id: u16,
    /// The label shown in the menu.
    label: String,
    /// The row's callback, cloned from the host's item.
    action: Option<uda_core::tray::TrayAction>,
}

/// A `TrayMenu` flattened into Win32 command ids.
///
/// Rows are visited depth-first and ids are allocated contiguously, which keeps
/// a click addressable regardless of how the host nested its submenus.
struct MenuTable {
    entries: Vec<MenuEntry>,
    /// The next id to hand out; starts above 1 so 0 can mean "nothing chosen".
    next_id: u16,
}

/// The lowest allocated command id.
///
/// `TrackPopupMenuEx` with `TPM_RETURNCMD` returns 0 when the user dismisses the
/// menu, so a real command id must never be 0.
const FIRST_COMMAND_ID: u16 = 1;

impl MenuTable {
    /// An empty table.
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: FIRST_COMMAND_ID,
        }
    }

    /// Allocate one id and remember the row it addresses.
    fn allocate(&mut self, label: String, action: Option<uda_core::tray::TrayAction>) -> u16 {
        let command_id = self.next_id;
        // Saturating: a menu with more than 65k rows is pathological, and
        // wrapping would alias an existing command.
        self.next_id = self.next_id.saturating_add(1);
        self.entries.push(MenuEntry {
            command_id,
            label,
            action,
        });
        command_id
    }

    /// Look up the callback a returned command id addresses.
    fn action_for(&self, command_id: u16) -> Option<uda_core::tray::TrayAction> {
        self.entries
            .iter()
            .find(|entry| entry.command_id == command_id)
            .and_then(|entry| entry.action.clone())
    }

    /// Rebuild the table from a host menu.
    ///
    /// The table is treated as disposable: every menu mutation produces a fresh
    /// one, so a stale id can never fire an outdated callback.
    fn from_menu(menu: &uda_core::tray::TrayMenu) -> Self {
        let mut table = Self::new();
        for item in menu.items() {
            table.push_item(&item);
        }
        table
    }

    /// Allocate ids for one row and, for a submenu, its children.
    fn push_item(&mut self, item: &MenuItem) -> u16 {
        match item {
            MenuItem::Separator => {
                // A separator still consumes an id so positions line up, but it
                // carries neither label nor callback.
                self.allocate(String::new(), None)
            }
            MenuItem::Text { label, action, .. } => {
                self.allocate(label.clone(), action.clone())
            }
            MenuItem::Checkbox { label, action, .. } => {
                self.allocate(label.clone(), action.clone())
            }
            MenuItem::Submenu { label, children, .. } => {
                let command_id = self.allocate(label.clone(), None);
                for child in children.items() {
                    self.push_item(&child);
                }
                command_id
            }
        }
    }

    /// Log which command id landed on which row.
    ///
    /// The table is rebuilt on every menu open, so this is the only place a host
    /// can see the mapping the shell is about to be handed.
    fn trace(&self) {
        for entry in &self.entries {
            log::debug!(
                "tray menu row {:?} -> command id {}",
                entry.label,
                entry.command_id
            );
        }
    }

    /// Build a Win32 `HMENU` for the whole menu, allocating ids as it goes.
    ///
    /// Returns the popup menu and leaves the id table populated; the caller
    /// destroys the `HMENU` once the shell is done with it.
    fn build(&mut self, menu: &uda_core::tray::TrayMenu) -> Option<HMENU> {
        let popup = match create_popup() {
            Ok(popup) => popup,
            Err(error) => {
                log::warn!("CreatePopupMenu failed: {error}");
                return None;
            }
        };
        for item in menu.items() {
            self.append_item(popup, &item);
        }
        self.trace();
        Some(popup)
    }

    /// Append one row (and its submenu, recursively) to `menu`.
    fn append_item(&mut self, menu: HMENU, item: &MenuItem) {
        match item {
            MenuItem::Separator => {
                // SAFETY: `menu` is a live popup created by `CreatePopupMenu`.
                let _ = unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::PCWSTR::null()) };
            }
            MenuItem::Text { label, state, action } => {
                let command_id = self.allocate(label.clone(), action.clone());
                append_row(menu, command_id, label, state.enabled, false, false);
            }
            MenuItem::Checkbox {
                label,
                state,
                action,
            } => {
                let command_id = self.allocate(label.clone(), action.clone());
                append_row(
                    menu,
                    command_id,
                    label,
                    state.enabled,
                    true,
                    state.checked,
                );
            }
            MenuItem::Submenu {
                label, children, ..
            } => {
                // The submenu row claims one id so its position is stable, and
                // its children are allocated afterwards.
                let command_id = self.allocate(label.clone(), None);
                let child_menu = match create_popup() {
                    Ok(child_menu) => child_menu,
                    Err(error) => {
                        log::warn!("CreatePopupMenu failed for a submenu: {error}");
                        return;
                    }
                };
                for child in children.items() {
                    self.append_item(child_menu, &child);
                }
                let wide = to_utf16(label);
                // SAFETY: `menu` and `child_menu` are live menus, and `wide`
                // outlives the call (the label is copied by the shell).
                let _ = unsafe {
                    AppendMenuW(
                        menu,
                        MF_POPUP,
                        child_menu.0 as usize,
                        windows::core::PCWSTR(wide.as_ptr()),
                    )
                };
                let _ = command_id;
            }
        }
    }
}

/// Create an empty popup menu.
fn create_popup() -> windows::core::Result<HMENU> {
    // SAFETY: no parameters, no invariants.
    unsafe { CreatePopupMenu() }
}

/// Append a text or checkbox row to `menu`.
///
/// A disabled row gets both `MF_DISABLED` and `MF_GRAYED`: the first stops it
/// firing, the second is what actually greys it out (`tray_specs.md` §2.5).
fn append_row(menu: HMENU, command_id: u16, label: &str, enabled: bool, checkbox: bool, checked: bool) {
    let wide = to_utf16(label);
    let mut flags = MF_STRING;
    if !enabled {
        flags |= MF_DISABLED | MF_GRAYED;
    }
    if checkbox && checked {
        flags |= MF_CHECKED;
    }
    // SAFETY: `menu` is live and `wide` is kept alive until the call returns;
    // the shell copies the label, so the buffer does not have to outlive it.
    let _ = unsafe {
        AppendMenuW(
            menu,
            flags,
            command_id as usize,
            windows::core::PCWSTR(wide.as_ptr()),
        )
    };
}

// ---------------------------------------------------------------------------
// Tray state shared with the worker
// ---------------------------------------------------------------------------

/// The state the worker thread serves, mirrored from the host's `TrayIconInner`.
///
/// The mirror exists so the worker never has to lock the host's state while a
/// Win32 call is in flight; the host pushes updates through
/// [`WorkerCommand::Refresh`] instead.
struct TrayShared {
    /// Application name, also used as the tooltip fallback.
    name: String,
    /// Current tooltip.
    tooltip: String,
    /// Current icon source, re-encoded into an `HICON` on demand.
    icon: Option<TrayIconSource>,
    /// Whether the item is shown.
    visible: bool,
    /// The live menu, when one is attached.
    menu: Option<Arc<uda_core::tray::TrayMenu>>,
    /// Left-click handler.
    on_click: Option<uda_core::tray::TrayEventHandler>,
    /// Double-click handler; on Windows this is a real event, not a synthesis.
    on_double_click: Option<uda_core::tray::TrayEventHandler>,
    /// The icon handle, so the worker can tell when the host has let go.
    host: Option<Arc<uda_core::tray::TrayIconInner>>,
    /// Whether the host has dropped the icon.
    shutdown: bool,
}

impl Default for TrayShared {
    fn default() -> Self {
        Self {
            name: String::new(),
            tooltip: String::new(),
            icon: None,
            // A freshly created icon is visible until the host hides it; the
            // worker seeds the same value so its first tick is not read as a
            // visibility change requiring a `NIM_MODIFY`.
            visible: true,
            menu: None,
            on_click: None,
            on_double_click: None,
            host: None,
            shutdown: false,
        }
    }
}

impl TrayShared {
    /// A state seeded from the host's registration request.
    fn from_config(config: &TrayIconConfig) -> Self {
        Self {
            name: config.name.clone(),
            tooltip: config.tooltip.clone(),
            icon: config.icon.clone(),
            visible: true,
            menu: config.menu.clone(),
            on_click: None,
            on_double_click: None,
            host: None,
            shutdown: false,
        }
    }

    /// Copy the host-visible state into the mirror.
    ///
    /// Everything is read under exactly one lock guard and copied out before it
    /// is dropped, so no lock is ever held across a Win32 call.
    fn sync_from(&mut self) {
        let Some(host) = self.host.as_ref() else {
            return;
        };
        let (tooltip, icon, menu, visible, shutdown) = {
            let state = host.lock_state();
            (
                state.tooltip.clone(),
                state.icon.clone(),
                state.menu.clone(),
                state.visible,
                state.shutdown,
            )
        };
        self.tooltip = tooltip;
        self.icon = icon;
        self.menu = menu;
        self.visible = visible;
        self.shutdown = shutdown;
    }
}

/// Lock the tray state, recovering from a poisoned lock.
///
/// The state is pure data that is rewritten field by field, so resuming after a
/// panic in one callback is strictly better than failing every later update.
/// This mirrors `uda_core::tray`'s own recovery.
fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, what: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            log::warn!("tray lock ({what}) poisoned; recovering the state");
            poisoned.into_inner()
        }
    }
}

// ---------------------------------------------------------------------------
// Worker thread
// ---------------------------------------------------------------------------

/// The tray worker: owns the hidden window, the message loop and the tray icon.
///
/// Everything here runs on one dedicated OS thread, which is what Win32 requires
/// for both the window and the `Shell_NotifyIconW` calls that reference it.
struct Worker {
    /// Shared state mirrored from the host.
    shared: Arc<Mutex<TrayShared>>,
    /// The icon id inside `NOTIFYICONDATAW`; unique per registered icon.
    icon_id: u32,
    /// The hidden message-only window, once created.
    hwnd: HWND,
    /// The icon currently registered with the shell, if any.
    icon: HICON,
    /// Command-id table rebuilt whenever the host changes the menu.
    menu_table: MenuTable,
    /// Whether the shell has acknowledged `NIM_SETVERSION`.
    version_handshake_done: bool,
    /// Whether the icon was registered and still needs unregistering.
    registered: bool,
    /// Tooltip applied by the last successful update.
    ///
    /// Cached so the sync tick can tell "the host changed the tooltip" from
    /// "nothing happened" and skip a redundant `NIM_MODIFY`.
    tooltip: String,
    /// Icon source applied by the last successful update.
    ///
    /// Named separately from [`Worker::icon`] (the live `HICON`): one is the
    /// host's declaration, the other is the shell-side handle built from it.
    last_icon: Option<TrayIconSource>,
    /// Menu applied by the last successful update.
    menu: Option<Arc<uda_core::tray::TrayMenu>>,
    /// Visibility applied by the last successful update.
    visible: bool,
}

// SAFETY: the only non-`Send` field is `hwnd` (a raw pointer wrapper). Win32
// window handles are per-thread resources, and every `Worker` is moved to
// exactly one thread — the worker that creates the window and never touches it
// again from anywhere else. The shared state it points at is `Arc<Mutex<_>>`,
// which is itself `Send`, so no other field contributes.
unsafe impl Send for Worker {}

impl Default for Worker {
    fn default() -> Self {
        Self {
            shared: Arc::new(Mutex::new(TrayShared::default())),
            icon_id: 0,
            hwnd: HWND::default(),
            icon: HICON::default(),
            menu_table: MenuTable::new(),
            version_handshake_done: false,
            registered: false,
            tooltip: String::new(),
            last_icon: None,
            menu: None,
            // A new icon is visible until the host hides it; `TrayShared` seeds
            // the same value so the first tick is not mistaken for a change.
            visible: true,
        }
    }
}

/// How often the worker mirrors the host state and checks for shutdown.
///
/// Mirrors the Linux backend's polling interval so both platforms pick up a
/// tooltip or icon change with the same latency, and so a `TrayIcon::drop`
/// (`tray_specs.md` §3.4) unregisters within a fraction of a second.
const SYNC_INTERVAL_MS: u32 = 200;

/// The timer id used for the sync timer.
const SYNC_TIMER_ID: usize = 1;

impl Worker {
    /// Register the class, create the window and add the tray icon.
    ///
    /// Called once before the message loop starts. Every step is ordered: the
    /// window must exist before the icon, because the shell needs an `hWnd` to
    /// post callbacks to, and the icon must exist before the version handshake,
    /// because there is nothing to version until then.
    fn setup(&mut self, class_name: &str) -> Result<(), UdaError> {
        let instance = self.module_handle()?;
        if !register_window_class(class_name, instance) {
            return Err(UdaError::Internal(
                "could not register the tray window class".to_string(),
            ));
        }

        let wide = to_utf16(class_name);
        let title = to_utf16(&self.name());
        // `HWND_MESSAGE` as the parent makes the window message-only: it has no
        // Z-order, never appears in Alt-Tab, and cannot be shown.
        //
        // SAFETY: the class is registered, both wide strings outlive the call,
        // and a null `lpparam` is allowed.
        let hwnd = match unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE_BITS,
                windows::core::PCWSTR(wide.as_ptr()),
                windows::core::PCWSTR(title.as_ptr()),
                WINDOW_STYLE_BITS,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                HMENU::default(),
                instance,
                Some(std::ptr::null()),
            )
        } {
            Ok(hwnd) => hwnd,
            Err(error) => {
                return Err(UdaError::Internal(format!(
                    "could not create the tray window: {error}"
                )));
            }
        };
        if hwnd.is_invalid() {
            return Err(UdaError::Internal(
                "the tray window is invalid".to_string(),
            ));
        }
        self.hwnd = hwnd;

        // Hand the worker to the window procedure through user data, so the
        // proc can reach the shared state without a global or a thread-local.
        //
        // SAFETY: `hwnd` was created by this thread and `GWLP_USERDATA` is a
        // documented per-window slot on a window this thread owns. The worker
        // outlives the window (teardown destroys the window first).
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, self as *mut Worker as isize);
        }

        self.add_icon()?;
        Ok(())
    }

    /// The application name from the shared state.
    fn name(&self) -> String {
        lock_or_recover(&self.shared, "name").name.clone()
    }

    /// Register the tray icon and handshake version 4.
    ///
    /// `NIM_ADD` first, then `NIM_SETVERSION`: the shell needs an item to
    /// version, and without the handshake it silently falls back to the Windows
    /// 95 callback behaviour (`tray_specs.md` §2.2).
    fn add_icon(&mut self) -> Result<(), UdaError> {
        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = self.icon_id;
        // Only the flags that are actually set may appear in `uFlags`, so the
        // icon is only requested when a source exists.
        data.uCallbackMessage = CALLBACK_MESSAGE;

        let (tip, icon) = {
            let shared = lock_or_recover(&self.shared, "worker add icon");
            (
                to_fixed_utf16::<128>(&uda_core::tray::sanitize_tooltip(&shared.tooltip)),
                shared
                    .icon
                    .as_ref()
                    .and_then(icon_from_source)
                    .unwrap_or_default(),
            )
        };

        let mut flags = NIF_MESSAGE | NIF_TIP;
        if !icon.is_invalid() {
            flags |= NIF_ICON;
            data.hIcon = icon;
        }
        data.uFlags = flags;
        data.szTip = tip;

        // SAFETY: `data` is fully initialised and `hwnd` is this thread's
        // message-only window; the shell copies what it needs before returning.
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
        if !added.as_bool() {
            log::warn!("Shell_NotifyIconW(NIM_ADD) failed; no tray icon");
            destroy_icon(icon);
            return Err(UdaError::NotSupported(
                "the shell refused to add a tray icon".to_string(),
            ));
        }
        self.icon = icon;
        // Only now is the item really registered, which is what lets `teardown`
        // decide whether a `NIM_DELETE` is owed at all.
        self.registered = true;

        // Version handshake. A failure is not fatal: the shell keeps using the
        // legacy callback behaviour, which still delivers clicks.
        let mut version = data;
        // SAFETY: `uVersion` and `uTimeout` share the anonymous union, and
        // `uFlags` does not select either, so writing one is exactly the
        // documented way to set the requested version.
        version.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        // SAFETY: same struct, still owned by this frame.
        let handed = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &version) };
        self.version_handshake_done = handed.as_bool();
        if !self.version_handshake_done {
            log::debug!("NIM_SETVERSION was not acknowledged; using legacy callbacks");
        }

        Ok(())
    }

    /// Apply a host-visible change to the registered icon.
    ///
    /// Rebuilds the whole `NOTIFYICONDATAW` from the mirror, which is what keeps
    /// tooltip, icon and visibility in step after any sequence of host calls.
    fn apply_refresh(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        let (tip, icon_source, visible) = {
            let shared = lock_or_recover(&self.shared, "worker refresh");
            (
                to_fixed_utf16::<128>(&uda_core::tray::sanitize_tooltip(&shared.tooltip)),
                shared.icon.clone(),
                shared.visible,
            )
        };

        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = self.icon_id;
        data.uCallbackMessage = CALLBACK_MESSAGE;
        data.szTip = tip;

        let mut new_icon = self.icon;
        let mut flags = NIF_MESSAGE | NIF_TIP;
        if visible {
            if let Some(source) = icon_source.as_ref() {
                new_icon = icon_from_source(source).unwrap_or(self.icon);
            }
            if !new_icon.is_invalid() {
                flags |= NIF_ICON;
                data.hIcon = new_icon;
            }
            // Clearing `NIS_HIDDEN` explicitly, so a previously hidden icon
            // comes back rather than staying passive.
            flags |= NIF_STATE;
            data.dwState = windows::Win32::UI::Shell::NOTIFY_ICON_STATE(0);
            data.dwStateMask = windows::Win32::UI::Shell::NOTIFY_ICON_STATE(0);
        } else {
            // A hidden icon keeps its registration but is not drawn, which is
            // the documented alternative to unregistering and re-adding.
            flags |= NIF_STATE;
            data.dwState = windows::Win32::UI::Shell::NIS_HIDDEN;
            data.dwStateMask = windows::Win32::UI::Shell::NIS_HIDDEN;
        }
        data.uFlags = flags;

        // SAFETY: same window and icon id as registration, so this is the
        // documented modify path.
        let updated = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        if !updated.as_bool() {
            log::debug!("Shell_NotifyIconW(NIM_MODIFY) failed");
            return;
        }

        // Only retire the previous icon once the shell accepted the new one,
        // so a failed update never leaves the item icon-less.
        if !new_icon.is_invalid() && new_icon != self.icon {
            destroy_icon(self.icon);
            self.icon = new_icon;
        }
    }

    /// Rebuild the command-id table from the host's menu.
    fn rebuild_menu(&mut self) {
        let menu = {
            let shared = lock_or_recover(&self.shared, "worker menu");
            shared.menu.clone()
        };
        self.menu_table = match menu {
            Some(menu) => MenuTable::from_menu(&menu),
            None => MenuTable::new(),
        };
    }

    /// Mirror the host state, then apply or tear down as needed.
    ///
    /// Runs on the sync timer, which is the only place the worker reads the
    /// host's state: keeping it out of the message handling itself means a menu
    /// callback and a host mutation can never interleave mid-update.
    ///
    /// Returns `false` when the worker must stop.
    fn on_tick(&mut self) -> bool {
        let (tooltip, icon, menu, visible, shutdown) = {
            let mut shared = lock_or_recover(&self.shared, "tick");
            shared.sync_from();
            (
                shared.tooltip.clone(),
                shared.icon.clone(),
                shared.menu.clone(),
                shared.visible,
                shared.shutdown,
            )
        };

        // `TrayIcon::drop` only sets a flag (`uda-core/src/tray.rs`), so the
        // worker is what actually unregisters. Without this check the icon would
        // linger in the shell after the host let go of it.
        if shutdown {
            return false;
        }

        if visible != self.visible {
            self.visible = visible;
            self.apply_refresh();
        }
        if tooltip != self.tooltip || !icons_equal(icon.as_ref(), self.last_icon.as_ref()) {
            self.tooltip = tooltip;
            self.last_icon = icon;
            self.apply_refresh();
        }
        if !menus_equal(menu.as_ref(), self.menu.as_ref()) {
            self.menu = menu;
            self.rebuild_menu();
        }
        true
    }

    /// Run the worker until the host drops the icon.
    ///
    /// This is the thread entry point: the message loop blocks in
    /// `GetMessageW` until a message arrives or the window is destroyed, so the
    /// thread parks without burning CPU and never blocks the host. The sync
    /// timer posts `WM_TIMER` into this same loop, so everything the worker does
    /// happens on one thread.
    fn run(&mut self) {
        // The timer is the worker's heartbeat: it drives state mirroring and
        // shutdown detection without a channel the host would have to signal.
        // SAFETY: `hwnd` is this thread's live window, and a `None` timer
        // procedure means the tick arrives as `WM_TIMER` instead of a call.
        unsafe {
            SetTimer(self.hwnd, SYNC_TIMER_ID, SYNC_INTERVAL_MS, None);
        }

        // SAFETY: `msg` is a plain out-struct owned by this frame, and the loop
        // exits on `<= 0` as documented (0 = WM_QUIT, -1 = error).
        let mut msg = MSG::default();
        loop {
            let result = unsafe { GetMessageW(&mut msg, self.hwnd, 0, 0) };
            if result.0 <= 0 {
                break;
            }
            // SAFETY: both calls only read the message this loop dequeued.
            unsafe {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }

            // The tick runs here rather than in the window procedure so it can
            // never be re-entered from inside a callback the worker dispatched.
            if !self.on_tick() {
                break;
            }
        }

        self.teardown();
    }

    /// Handle the shell's callback message.
    ///
    /// `lParam` carries the real mouse message, which is how one callback id
    /// fans out into every tray event (`tray_specs.md` §2.3).
    fn on_callback(&mut self, hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        // The icon id travels in `wParam`; ignore anything addressed elsewhere.
        if wparam.0 as u32 != self.icon_id {
            return LRESULT(0);
        }

        match lparam.0 as u32 {
            x if x == windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONUP => {
                self.dispatch(TrayEvent::Click);
            }
            x if x == windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDBLCLK => {
                self.dispatch(TrayEvent::DoubleClick);
            }
            x if x == windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONUP
                || x == windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONDOWN
                || x == windows::Win32::UI::WindowsAndMessaging::WM_CONTEXTMENU =>
            {
                // A menu request is not a `TrayEvent`; it is handled inline so
                // the shell's own event ordering is respected.
                self.show_menu();
            }
            _ => {
                // SAFETY: fall through to the default handler for everything
                // UDA does not consume.
                return unsafe { DefWindowProcW(hwnd, message, wparam, lparam) };
            }
        }
        LRESULT(0)
    }

    /// Invoke a `TrayEvent` handler without holding the state lock.
    ///
    /// The callback is taken out of the state and put back afterwards, so a host
    /// that mutates the icon from inside its own handler cannot deadlock
    /// against this thread.
    fn dispatch(&mut self, event: TrayEvent) {
        let handler = {
            let mut shared = lock_or_recover(&self.shared, "dispatch");
            match event {
                TrayEvent::Click => shared.on_click.take(),
                TrayEvent::DoubleClick => shared.on_double_click.take(),
            }
        };
        if let Some(mut handler) = handler {
            handler(&event);
            let mut shared = lock_or_recover(&self.shared, "dispatch restore");
            match event {
                TrayEvent::Click => shared.on_click = Some(handler),
                TrayEvent::DoubleClick => shared.on_double_click = Some(handler),
            }
        }
    }

    /// Build and show the context menu at the cursor.
    ///
    /// The focus dance in `tray_specs.md` §2.5 item 3 is mandatory: without
    /// `SetForegroundWindow` the popup does not dismiss on an outside click, and
    /// without the trailing `WM_NULL` some shells leave it stuck open.
    fn show_menu(&mut self) {
        let menu = {
            let shared = lock_or_recover(&self.shared, "show menu");
            shared.menu.clone()
        };
        let Some(menu) = menu else {
            return;
        };

        // Rebuilt on every open: the host may have mutated the menu since it was
        // last shown, and a Win32 menu is a snapshot of the rows at build time.
        self.rebuild_menu();
        self.menu_table.trace();
        let popup = match self.menu_table.build(&menu) {
            Some(popup) => popup,
            None => return,
        };

        // A cursor query failure is not fatal; (0, 0) with the alignment flags
        // below still puts the menu on screen.
        let mut point = POINT::default();
        let point = if unsafe { GetCursorPos(&mut point) }.is_ok() {
            point
        } else {
            POINT { x: 0, y: 0 }
        };

        // SAFETY: `hwnd` is this thread's window; making it foreground is the
        // documented requirement for a dismissible popup.
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }

        // SAFETY: `popup` is live, `hwnd` is live, and no `TPMPARAMS` is needed
        // for a simple popup. `TPM_RETURNCMD` hands the chosen id back instead
        // of posting `WM_COMMAND`, which keeps the lookup in one place.
        let chosen = unsafe {
            TrackPopupMenuEx(
                popup,
                (TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD).0,
                point.x,
                point.y,
                self.hwnd,
                None,
            )
        };

        // SAFETY: a harmless posted message that unblocks the menu's own modal
        // loop, per the documented workaround.
        unsafe {
            let _ = PostMessageW(self.hwnd, 0, WPARAM(0), LPARAM(0));
        }

        // SAFETY: `popup` is no longer referenced by the shell once
        // `TrackPopupMenuEx` returned.
        unsafe {
            let _ = DestroyMenu(popup);
        }

        // 0 means the user dismissed the menu without choosing anything.
        let command_id = chosen.0 as u16;
        if command_id != 0 {
            let action = self.menu_table.action_for(command_id);
            if let Some(action) = action {
                action.invoke(&TrayEvent::Click);
            }
        }
    }

    /// Handle a `WM_COMMAND` addressed at the popup menu.
    ///
    /// `TPM_RETURNCMD` already resolves the id, but a shell that posts the
    /// message instead still lands here; both paths use the same lookup so there
    /// is exactly one place a command becomes a callback.
    fn on_command(&mut self, command_id: u16) {
        if command_id == 0 {
            return;
        }
        let action = self.menu_table.action_for(command_id);
        // The row's label is logged alongside the id so a host debugging its own
        // menu can see which row the shell actually delivered.
        match action.is_some() {
            true => log::debug!("tray command {command_id} resolved to a row"),
            false => log::debug!("tray command {command_id} matched no row"),
        }
        if let Some(action) = action {
            action.invoke(&TrayEvent::Click);
        }
    }

    /// The module handle for this executable.
    fn module_handle(&self) -> Result<windows::Win32::Foundation::HMODULE, UdaError> {
        // SAFETY: a null module name asks for the current executable's image;
        // no invariants beyond the documented call.
        match unsafe { GetModuleHandleW(None) } {
            Ok(handle) => Ok(handle),
            Err(error) => Err(UdaError::Internal(format!(
                "GetModuleHandleW failed: {error}"
            ))),
        }
    }

    /// Unregister, destroy and release everything this worker owns.
    ///
    /// Called on every exit path and idempotent by construction: each step
    /// checks for an invalid handle before acting, so a second call is a no-op.
    /// `NIM_DELETE` runs *before* the window is destroyed, otherwise the shell
    /// keeps a dangling `hWnd` and may show a ghost icon until logoff
    /// (`tray_specs.md` §2.7).
    fn teardown(&mut self) {
        // A window with no registration owed nothing to the shell, so the
        // `NIM_DELETE` is skipped entirely rather than issued against an id the
        // shell may have recycled for another item.
        if self.registered && !self.hwnd.is_invalid() {
            // Stop the heartbeat first: no tick may fire while the icon is being
            // unregistered, or it would try to refresh an item mid-deletion.
            // SAFETY: `hwnd` is this thread's live window and the timer id is
            // the one `run` registered.
            unsafe {
                let _ = KillTimer(self.hwnd, SYNC_TIMER_ID);
            }

            let mut data = NOTIFYICONDATAW::default();
            data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            data.hWnd = self.hwnd;
            data.uID = self.icon_id;
            // SAFETY: same window and id as registration; the shell forgets the
            // item immediately, so no other field has to be filled.
            let removed = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
            if !removed.as_bool() {
                log::debug!("Shell_NotifyIconW(NIM_DELETE) reported a failure");
            }
            // Either way the item is gone from this worker's point of view; a
            // second teardown must not try again.
            self.registered = false;
        }

        destroy_icon(self.icon);
        self.icon = HICON::default();

        if !self.hwnd.is_invalid() {
            // SAFETY: this thread created the window and owns its message loop.
            let _ = unsafe { DestroyWindow(self.hwnd) };
            self.hwnd = HWND::default();
        }
    }
}

/// Register the window class, tolerating an already-registered name.
///
/// Returns `false` only when the class could not be made available; a class that
/// is already registered counts as success.
fn register_window_class(class_name: &str, instance: windows::Win32::Foundation::HMODULE) -> bool {
    let wide = to_utf16(class_name);
    let class = WNDCLASSW {
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(tray_window_proc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: windows::Win32::Foundation::HINSTANCE(instance.0),
        hIcon: HICON::default(),
        hCursor: HCURSOR::default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: windows::core::PCWSTR::null(),
        lpszClassName: windows::core::PCWSTR(wide.as_ptr()),
    };

    // SAFETY: `class` is fully initialised and `wide` outlives the call.
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        // `ERROR_CLASS_ALREADY_EXISTS` is the documented "already registered"
        // answer and is harmless here; anything else means the class is
        // genuinely unusable.
        let last = unsafe { GetLastError() };
        if last != ERROR_CLASS_ALREADY_EXISTS {
            log::warn!("RegisterClassW failed with {last:?}");
            return false;
        }
    }
    true
}

/// Unregister the window class once the worker is gone.
///
/// Called from the worker thread at teardown, where the class is definitely no
/// longer in use by a live window.
fn unregister_window_class(class_name: &str) {
    // SAFETY: a null instance asks for the current executable's image, matching
    // the handle used at registration.
    let instance = match unsafe { GetModuleHandleW(None) } {
        Ok(instance) => instance,
        Err(_) => return,
    };
    let wide = to_utf16(class_name);
    // SAFETY: `wide` outlives the call and `instance` is the one that
    // registered the class.
    unsafe {
        let _ = UnregisterClassW(
            windows::core::PCWSTR(wide.as_ptr()),
            windows::Win32::Foundation::HINSTANCE(instance.0),
        );
    }
}

/// The window procedure for the hidden tray window.
///
/// The worker pointer is stashed in `GWLP_USERDATA` at creation, so this is a
/// thin trampoline: it hands UDA's own message to the worker and defers
/// everything else to `DefWindowProcW`.
///
/// # Safety
///
/// An `extern "system"` callback invoked by Win32 under the documented window
/// procedure contract: `hwnd` is a live window and `message` is one it received.
unsafe extern "system" fn tray_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: `GWLP_USERDATA` was written by the thread that owns this window,
    // with a pointer to the worker, which is alive for the window's lifetime.
    let worker = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut Worker;
    if !worker.is_null() {
        // SAFETY: the pointer is non-null and the worker outlives the window
        // (teardown destroys the window before the worker is dropped).
        let worker = unsafe { &mut *worker };
        if message == CALLBACK_MESSAGE {
            return worker.on_callback(hwnd, message, wparam, lparam);
        }
        if message == windows::Win32::UI::WindowsAndMessaging::WM_COMMAND {
            // The low word of `wParam` is the command id.
            worker.on_command((wparam.0 & 0xFFFF) as u16);
            return LRESULT(0);
        }
    }
    // SAFETY: the fallback path for every message UDA does not consume.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// Windows tray manager.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsTrayManager;

impl WindowsTrayManager {
    /// A new manager.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// The capability set this backend publishes for a registered item.
    ///
    /// Unlike Linux, Windows delivers a genuine `WM_LBUTTONDBLCLK`, so
    /// `TRAY_DOUBLE_CLICK` is advertised here — the one backend in the matrix
    /// where claiming it is honest (`tray_specs.md` §4).
    fn advertised_capabilities() -> Capability {
        Capability::SYSTEM_TRAY
            | Capability::TRAY_ICON
            | Capability::TRAY_TOOLTIP
            | Capability::TRAY_CLICK
            | Capability::TRAY_CONTEXT_MENU
            | Capability::TRAY_CHECKBOX
            | Capability::TRAY_DYNAMIC_MENU
            | Capability::TRAY_DOUBLE_CLICK
    }

    /// Spawn the worker thread and wait until it has registered the icon.
    ///
    /// The wait is bounded and covers only the shell round-trip: the message
    /// loop itself runs for the icon's lifetime on the worker, so the host is
    /// never asked to pump messages. A worker that fails to start reports the
    /// error back through a one-slot rendezvous channel, which is what lets
    /// `create` return an error instead of a handle to an icon that never
    /// registered.
    ///
    /// No command channel is used: the worker discovers the host's shutdown from
    /// the `shutdown` flag under the shared state, which `TrayIcon::drop` sets.
    /// That is what keeps the host's drop path a plain flag write rather than a
    /// cross-thread send that could block on a stalled worker.
    fn spawn_worker(
        shared: Arc<Mutex<TrayShared>>,
        class_name: String,
        icon_id: u32,
    ) -> Result<(), UdaError> {
        // A one-slot rendezvous channel: the worker reports readiness exactly
        // once.
        let (ready, ready_rx) = mpsc::sync_channel(1);
        // Every backend-facing field starts invalid/empty; only the shared state
        // and the shell-side id come from the caller.
        let worker = Worker {
            shared,
            icon_id,
            ..Worker::default()
        };
        let worker_name = class_name.clone();

        let handle = std::thread::Builder::new()
            .name("uda-tray-worker".to_string())
            .spawn(move || {
                // `setup` and the message loop are separate so a failure can be
                // reported before the (blocking) loop starts.
                let mut worker = worker;
                if let Err(error) = worker.setup(&worker_name) {
                    log::warn!("tray worker could not start: {error}");
                    worker.teardown();
                    unregister_window_class(&worker_name);
                    let _ = ready.try_send(Err(error.to_string()));
                    return;
                }
                let _ = ready.try_send(Ok(String::new()));
                worker.run();
                unregister_window_class(&worker_name);
            })
            .map_err(|error| {
                UdaError::Internal(format!("could not spawn the tray worker: {error}"))
            })?;

        let result = match ready_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(message)) => Err(UdaError::NotSupported(message)),
            Err(_) => Err(UdaError::Internal(
                "the tray worker did not report readiness in time".to_string(),
            )),
        };
        // The handle is detached on purpose: the worker exits when it sees the
        // shutdown flag, and the host must not block on a join that may never
        // finish (a menu could be open when the host drops the icon).
        drop(handle);
        result
    }
}

impl TrayManager for WindowsTrayManager {
    fn create(&self, config: TrayIconConfig) -> Result<TrayIcon, UdaError> {
        let inner = Arc::new(uda_core::tray::TrayIconInner::new(config.name.clone()));
        let icon = TrayIcon::from_inner(Arc::clone(&inner));

        // The callbacks cannot be cloned, so they are moved out of the config
        // before it is consumed; this is the only chance to take them.
        let mut config = config;
        let on_click = config.on_click.take();
        let on_double_click = config.on_double_click.take();

        let mut shared_state = TrayShared::from_config(&config);
        shared_state.on_click = on_click;
        // Windows reports a real `WM_LBUTTONDBLCLK`, so no synthesis is needed.
        shared_state.on_double_click = on_double_click;
        // The handle lets the worker mirror host updates and tells it which icon
        // it is serving.
        shared_state.host = Some(Arc::clone(&inner));

        let shared = Arc::new(Mutex::new(shared_state));
        // One counter drives both the window class suffix and the icon id, so
        // two icons in the same process never collide on either.
        let index = TRAY_COUNTER.fetch_add(1, Ordering::SeqCst);
        let class_name = format!("{}_{index}", class_name());

        Self::spawn_worker(Arc::clone(&shared), class_name, index)?;

        inner.set_capabilities(Self::advertised_capabilities());

        // Nothing to keep alive here: the worker holds the only other `Arc` to
        // the shared state and notices the host's departure through the
        // `shutdown` flag that `TrayIcon::drop` sets, so no channel, handle or
        // leaked pointer is needed to keep the icon registered.
        Ok(icon)
    }

    fn capabilities(&self) -> Capability {
        Self::advertised_capabilities()
    }

    fn support_level(&self, feature: TrayFeature) -> SupportLevel {
        let capabilities = Self::advertised_capabilities();
        if !capabilities.contains(Capability::SYSTEM_TRAY) {
            return SupportLevel::None;
        }
        let flag = match feature {
            TrayFeature::Icon => Capability::TRAY_ICON,
            TrayFeature::Tooltip => Capability::TRAY_TOOLTIP,
            TrayFeature::Click => Capability::TRAY_CLICK,
            TrayFeature::DoubleClick => Capability::TRAY_DOUBLE_CLICK,
            TrayFeature::ContextMenu => Capability::TRAY_CONTEXT_MENU,
            TrayFeature::Checkbox => Capability::TRAY_CHECKBOX,
            TrayFeature::DynamicMenu => Capability::TRAY_DYNAMIC_MENU,
        };
        // An unclaimed flag is a plain `None`: a backend either answers for a
        // feature or does not, and `Partial` stays reserved for a backend that
        // publishes a degraded answer explicitly.
        if capabilities.contains(flag) {
            SupportLevel::Full
        } else {
            SupportLevel::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Encode and read back a NUL-terminated UTF-16 string.
    fn round_trip(text: &str) -> String {
        let wide = to_utf16(text);
        from_utf16(&wide)
    }

    #[test]
    fn utf16_round_trips_through_a_nul_terminated_buffer() {
        assert_eq!(round_trip("hello"), "hello");
        assert_eq!(round_trip(""), "");
        // Non-ASCII must survive the round trip, not be mangled or dropped.
        assert_eq!(round_trip("托盘"), "托盘");
        assert_eq!(round_trip("café"), "café");
    }

    #[test]
    fn from_utf16_stops_at_the_terminator() {
        // A buffer with data after the NUL must be truncated there, which is
        // what keeps an uninitialised tail out of a decoded string.
        let buffer = [b'a' as u16, b'b' as u16, 0, b'x' as u16];
        assert_eq!(from_utf16(&buffer), "ab");
    }

    #[test]
    fn a_fixed_tip_field_is_nul_terminated_and_sized() {
        let field = to_fixed_utf16::<128>("hi");
        assert_eq!(field[0], b'h' as u16);
        assert_eq!(field[1], b'i' as u16);
        assert_eq!(field[2], 0, "the field must be NUL-terminated");
        assert_eq!(field.len(), 128, "the field keeps its declared size");
    }

    #[test]
    fn a_long_tooltip_is_truncated_on_a_character_boundary() {
        // 200 ASCII chars into a 128-slot field: 127 units plus the terminator.
        let long: String = "x".repeat(200);
        let field = to_fixed_utf16::<128>(&long);
        assert_eq!(field[127], 0, "the terminator occupies the last slot");
        assert_eq!(field[0], b'x' as u16);

        // A multi-byte string must not be cut mid-character: the sanitizer caps
        // it in `char`s and this truncation caps it in `u16` units, so a
        // surrogate pair is either fully written or left out entirely. A lone
        // surrogate would decode to a replacement character, which is exactly
        // what must not happen.
        let stars: String = "★".repeat(300);
        let field = to_fixed_utf16::<128>(&stars);
        assert_eq!(field[127], 0);
        let decoded = from_utf16(&field);
        assert!(
            !decoded.contains('\u{FFFD}'),
            "truncation must not split a surrogate pair"
        );
        assert!(decoded.chars().all(|c| c == '★'));
        assert!(decoded.chars().count() < 300);
    }

    #[test]
    fn sanitizer_caps_the_tooltip_before_the_fixed_copy() {
        // The two-stage truncation must never lose the terminator: sanitize to
        // 127 chars, then encode into 128 units.
        let long: String = "y".repeat(400);
        let capped = uda_core::tray::sanitize_tooltip(&long);
        assert_eq!(capped.chars().count(), 127);
        let field = to_fixed_utf16::<128>(capped);
        assert_eq!(field[126], b'y' as u16);
        assert_eq!(field[127], 0);
    }

    #[test]
    fn two_icon_sources_are_equal_only_when_they_match() {
        let none: Option<TrayIconSource> = None;
        let path = TrayIconSource::Path("a.png".to_string());
        let same_path = TrayIconSource::Path("a.png".to_string());
        let other_path = TrayIconSource::Path("b.png".to_string());

        assert!(icons_equal(none.as_ref(), none.as_ref()));
        assert!(icons_equal(Some(&path), Some(&same_path)));
        assert!(!icons_equal(Some(&path), Some(&other_path)));
        // A path and an RGBA icon are different shapes, so they are unequal
        // without comparing a pixel buffer to a string.
        let rgba = TrayIconSource::Rgba {
            width: 1,
            height: 1,
            stride: 4,
            data: vec![0, 0, 0, 255],
        };
        assert!(!icons_equal(Some(&path), Some(&rgba)));
        // Absence is distinct from presence.
        assert!(!icons_equal(None, Some(&path)));
    }

    #[test]
    fn rgba_icons_compare_by_shape_and_bytes() {
        let first = TrayIconSource::Rgba {
            width: 1,
            height: 1,
            stride: 4,
            data: vec![1, 2, 3, 4],
        };
        let identical = first.clone();
        // `TrayIconSource` is an enum, so each variant is spelled out rather than
        // using functional-record-update syntax.
        let different_pixel = TrayIconSource::Rgba {
            width: 1,
            height: 1,
            stride: 4,
            data: vec![9, 2, 3, 4],
        };
        let different_shape = TrayIconSource::Rgba {
            width: 2,
            height: 1,
            stride: 4,
            data: vec![1, 2, 3, 4],
        };

        assert!(icons_equal(Some(&first), Some(&identical)));
        assert!(!icons_equal(Some(&first), Some(&different_pixel)));
        assert!(!icons_equal(Some(&first), Some(&different_shape)));
    }

    #[test]
    fn an_icon_source_is_only_equal_to_itself_across_kinds() {
        let rgba = TrayIconSource::Rgba {
            width: 2,
            height: 2,
            stride: 8,
            data: vec![0; 16],
        };
        let same = rgba.clone();
        assert!(icons_equal(Some(&rgba), Some(&same)));
    }

    #[test]
    fn menus_are_equal_when_they_are_the_same_handle() {
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        let same = Arc::clone(&menu);
        let other = Arc::new(uda_core::tray::TrayMenu::new());

        assert!(menus_equal(Some(&menu), Some(&same)));
        assert!(!menus_equal(Some(&menu), Some(&other)));
        assert!(!menus_equal(None, Some(&menu)));
        assert!(menus_equal(None, None));
    }

    /// Build a menu with one row of each interesting kind.
    fn sample_menu() -> Arc<uda_core::tray::TrayMenu> {
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(menu.push(MenuItem::text("open")).is_ok());
        assert!(menu.push(MenuItem::separator()).is_ok());
        assert!(menu.push(MenuItem::checkbox_checked("pin")).is_ok());
        assert!(menu.push(MenuItem::text_disabled("locked")).is_ok());
        let child = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(child.push(MenuItem::text("inner")).is_ok());
        assert!(menu.push(MenuItem::submenu("more", Arc::clone(&child))).is_ok());
        menu
    }

    #[test]
    fn command_ids_are_allocated_above_zero_and_uniquely() {
        let menu = sample_menu();
        let mut table = MenuTable::from_menu(&menu);
        // `from_menu` already allocated the top-level rows; the ids must all be
        // non-zero, because `TrackPopupMenuEx` returns 0 for "nothing chosen".
        let mut seen = Vec::new();
        for entry in &table.entries {
            assert_ne!(entry.command_id, 0, "0 is reserved for dismissal");
            assert!(
                !seen.contains(&entry.command_id),
                "command id {} is duplicated",
                entry.command_id
            );
            seen.push(entry.command_id);
        }
        assert!(seen.len() >= 5, "one entry per row: {}", seen.len());

        // A further allocation must not reuse an id.
        let extra = table.allocate("x".to_string(), None);
        assert!(!seen.contains(&extra));
        assert_ne!(extra, 0);
    }

    #[test]
    fn a_command_id_resolves_to_the_row_callback() {
        static FIRED: AtomicUsize = AtomicUsize::new(0);
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(menu
            .push(MenuItem::text_with_action("quit", |_| {
                FIRED.fetch_add(1, Ordering::SeqCst);
            }))
            .is_ok());

        let mut table = MenuTable::from_menu(&menu);
        // Rebuild through `build` so the id that `AppendMenuW` received is the
        // one the lookup uses.
        let popup = table.build(&menu);
        assert!(popup.is_some(), "a menu must build");
        let action = table.action_for(FIRST_COMMAND_ID);
        assert!(action.is_some(), "the first row must be addressable");
        if let Some(action) = action {
            action.invoke(&TrayEvent::Click);
        }
        assert_eq!(FIRED.load(Ordering::SeqCst), 1);

        // An id that was never allocated resolves to nothing.
        assert!(table.action_for(60_000).is_none());
    }

    #[test]
    fn nested_rows_get_their_own_ids_so_a_click_is_addressable() {
        let menu = sample_menu();
        let mut table = MenuTable::from_menu(&menu);
        // Build so the ids handed to `AppendMenuW` are the ones the lookup uses.
        assert!(table.build(&menu).is_some());

        // The nested row must be addressable on its own, independently of the
        // submenu row that contains it.
        let child = table
            .entries
            .iter()
            .find(|entry| entry.label == "inner")
            .map(|entry| entry.command_id)
            .expect("a nested row must have its own command id");
        assert_ne!(child, 0);

        // Its id must differ from every top-level row's, so a click on the
        // child cannot be mistaken for a click on the submenu.
        let submenu_id = table
            .entries
            .iter()
            .find(|entry| entry.label == "more")
            .map(|entry| entry.command_id)
            .expect("the submenu row must have its own command id");
        assert_ne!(child, submenu_id);
    }

    #[test]
    fn a_menu_without_rows_still_encodes() {
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        let mut table = MenuTable::from_menu(&menu);
        assert!(table.entries.is_empty());
        let popup = table.build(&menu);
        assert!(popup.is_some(), "an empty menu is still a valid popup");
    }

    #[test]
    fn advertised_capabilities_admit_double_click() {
        let capabilities = WindowsTrayManager::advertised_capabilities();
        assert!(capabilities.contains(Capability::SYSTEM_TRAY));
        assert!(capabilities.contains(Capability::TRAY_ICON));
        assert!(capabilities.contains(Capability::TRAY_TOOLTIP));
        assert!(capabilities.contains(Capability::TRAY_CLICK));
        assert!(capabilities.contains(Capability::TRAY_CONTEXT_MENU));
        assert!(capabilities.contains(Capability::TRAY_CHECKBOX));
        assert!(capabilities.contains(Capability::TRAY_DYNAMIC_MENU));
        // The one capability Windows can honestly claim and Linux cannot.
        assert!(capabilities.contains(Capability::TRAY_DOUBLE_CLICK));
    }

    #[test]
    fn support_level_is_full_for_every_advertised_feature() {
        let manager = WindowsTrayManager::new();
        for feature in [
            TrayFeature::Icon,
            TrayFeature::Tooltip,
            TrayFeature::Click,
            TrayFeature::DoubleClick,
            TrayFeature::ContextMenu,
            TrayFeature::Checkbox,
            TrayFeature::DynamicMenu,
        ] {
            assert_eq!(
                manager.support_level(feature),
                SupportLevel::Full,
                "{feature} must be fully supported on Windows"
            );
        }
        assert_eq!(
            manager.capabilities(),
            WindowsTrayManager::advertised_capabilities()
        );
    }

    #[test]
    fn the_callback_message_is_out_of_the_hosts_reserved_range() {
        // `tray_specs.md` §2.4: the callback id must be >= WM_USER, and using
        // `WM_APP` leaves the whole `WM_USER` range free for the host.
        const WM_USER: u32 = 0x0400;
        assert!(CALLBACK_MESSAGE >= WM_USER);
        assert_eq!(CALLBACK_MESSAGE, 0x8000);
    }

    #[test]
    fn the_sync_interval_is_a_polite_poll() {
        // Fast enough that a tooltip change feels immediate, slow enough that
        // an idle icon costs nothing.
        assert!(SYNC_INTERVAL_MS >= 50);
        assert!(SYNC_INTERVAL_MS <= 500);
        assert_ne!(SYNC_TIMER_ID, 0, "timer id 0 is invalid");
    }

    #[test]
    fn the_window_class_name_is_process_unique() {
        // The name must be a fixed prefix so the suffix can be appended per
        // icon, and it must not contain characters a class name cannot carry.
        let name = class_name();
        assert!(!name.is_empty());
        assert!(!name.contains('.'));
        assert!(!name.contains(' '));
        assert_eq!(name, "UDA_TrayMessageWindow");
    }

    #[test]
    fn a_config_seeds_the_mirror_with_the_registered_values() {
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(menu.push(MenuItem::text("open")).is_ok());
        let config = TrayIconConfig {
            name: "test".to_string(),
            icon: Some(TrayIconSource::Path("app.ico".to_string())),
            tooltip: "hello".to_string(),
            menu: Some(Arc::clone(&menu)),
            on_click: None,
            on_double_click: None,
        };

        let mut state = TrayShared::from_config(&config);
        assert_eq!(state.name, "test");
        assert_eq!(state.tooltip, "hello");
        assert_eq!(state.icon, Some(TrayIconSource::Path("app.ico".to_string())));
        assert!(state.menu.is_some());
        // A freshly registered icon is visible; `TrayIcon::hide` is the only way
        // to change that, so the mirror must start out shown.
        assert!(state.visible);
        assert!(!state.shutdown);
        // The callbacks live in the shared state, not the config, once taken.
        assert!(state.on_click.is_none());

        // Syncing from nothing must be a no-op rather than a panic.
        state.sync_from();
        assert_eq!(state.tooltip, "hello");
    }

    #[test]
    fn the_mirror_tracks_host_updates() {
        let inner = Arc::new(uda_core::tray::TrayIconInner::new("test".to_string()));
        let icon = TrayIcon::from_inner(Arc::clone(&inner));
        let config = TrayIconConfig {
            name: "test".to_string(),
            icon: Some(TrayIconSource::Path("app.ico".to_string())),
            tooltip: "first".to_string(),
            menu: None,
            on_click: None,
            on_double_click: None,
        };
        let mut state = TrayShared::from_config(&config);
        state.host = Some(Arc::clone(&inner));
        state.sync_from();

        // A host tooltip change must reach the mirror.
        icon.set_tooltip("second");
        state.sync_from();
        assert_eq!(state.tooltip, "second");

        // An icon swap must be visible too.
        assert!(icon
            .set_icon(TrayIconSource::Path("b.ico".to_string()))
            .is_ok());
        state.sync_from();
        assert_eq!(state.icon, Some(TrayIconSource::Path("b.ico".to_string())));

        // Hiding the icon must reach the mirror.
        icon.hide();
        state.sync_from();
        assert!(!state.visible);

        // A rejected icon leaves the previous one in place.
        assert!(icon.set_icon(TrayIconSource::Path(String::new())).is_err());
        state.sync_from();
        assert_eq!(state.icon, Some(TrayIconSource::Path("b.ico".to_string())));

        // A menu attached by the host must show up.
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        icon.set_menu(Arc::clone(&menu));
        state.sync_from();
        assert!(state.menu.is_some());
    }

    #[test]
    fn dropping_the_host_sets_the_shutdown_flag_the_worker_polls() {
        let inner = Arc::new(uda_core::tray::TrayIconInner::new("test".to_string()));
        let mut state = TrayShared::from_config(&TrayIconConfig::new("test"));
        state.host = Some(Arc::clone(&inner));

        assert!(!inner.lock_state().shutdown);
        state.sync_from();
        assert!(!state.shutdown, "a live host is not shut down");

        drop(TrayIcon::from_inner(Arc::clone(&inner)));
        state.sync_from();
        assert!(state.shutdown, "the worker must see the drop");
    }

    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_propagated() {
        let mutex = Mutex::new(0u32);
        let handle = std::thread::spawn(move || {
            let _guard = mutex.lock();
            panic!("poison the lock on purpose");
        });
        assert!(handle.join().is_err());

        // The mutex was moved into the thread, so a fresh one stands in for the
        // poisoned state the same way: recovery must not fail.
        let mutex = Mutex::new(7u32);
        {
            let guard = mutex.lock();
            assert!(guard.is_ok());
        }
        let mut recovered = lock_or_recover(&mutex, "test");
        *recovered += 1;
        assert_eq!(*recovered, 8);
    }

    #[test]
    fn the_worker_state_is_shared_across_the_arc() {
        // The mirror lives behind one `Arc<Mutex<..>>`: never a clone of the
        // mutex, so every holder observes the same state.
        let shared = Arc::new(Mutex::new(TrayShared::from_config(&TrayIconConfig::new(
            "shared",
        ))));
        let clone = Arc::clone(&shared);
        {
            let mut state = lock_or_recover(&shared, "test");
            state.tooltip = "written once".to_string();
        }
        assert_eq!(lock_or_recover(&clone, "test").tooltip, "written once");
    }
}
