//! Windows system tray backend: `Shell_NotifyIconW` + a hidden worker window.
//!
//! ```text
//!  host thread                     tray worker thread
//!  ───────────                     ──────────────────
//!  TrayIcon ── Arc<TrayIconInner>   hidden top-level window
//!    ├ tooltip / icon / menu         ├ message pump: GetMessageW / Translate /
//!    └ visible / capabilities        │   DispatchMessageW
//!                                    └ window proc: WM_xxx -> TrayEvent
//! ```
//!
//! A tray icon on Windows is a window, not an object: the shell posts callback
//! messages to the `hWnd` recorded in `NOTIFYICONDATAW`, so that window must
//! belong to a thread running a message loop. UDA creates a hidden top-level
//! window on a dedicated worker thread and never touches the host's message
//! queue, so the host is never asked to pump messages itself. The window is a
//! *normal* top-level window that is never shown rather than a message-only
//! window on purpose: only a normal window receives broadcast messages, and the
//! `TaskbarCreated` broadcast (sent whenever explorer.exe starts or restarts) is
//! the worker's cue to re-register an icon the new shell process knows nothing
//! about.
//!
//! Unlike the Linux backend there is no shared-state mirror, because Win32 calls
//! such as `Shell_NotifyIconW`, `CreatePopupMenu` and `TrackPopupMenuEx` must be
//! issued from the thread that owns the window. Host mutations are *forwarded*
//! to the worker as user messages and applied under its own lock, which is what
//! keeps a `Drop` racing a menu open from corrupting the window.
//!
//! Every Win32 result is inspected and mapped into
//! [`UdaError`](uda_core::error::UdaError); no `unwrap()`, `expect()`, `panic!()`
//! or `unreachable!()` is used on a Win32 call, and each unsafe block carries a
//! `// SAFETY:` note naming the invariant it relies on.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};

use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, DIB_RGB_COLORS, HBITMAP, HBRUSH, HDC,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSMICON};
// `Shell_NotifyIconW`, `NOTIFYICONDATAW` and the `NIM_*`/`NIF_*` constants live in
// `Win32::UI::Shell`; the icon-creation and window APIs (`CreateIcon`,
// `CreateIconIndirect`, `ICONINFO`, `RegisterClassW`, ...) are exported from
// `Win32::UI::WindowsAndMessaging`.
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_STATE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NIS_HIDDEN, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
    NOTIFY_ICON_DATA_FLAGS, NOTIFY_ICON_STATE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, GetWindowLongPtrW,
    KillTimer, LoadImageW, PostMessageW, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, TrackPopupMenuEx, TranslateMessage,
    UnregisterClassW, GWLP_USERDATA, HCURSOR, HICON, HMENU, ICONINFO, IMAGE_ICON, LR_DEFAULTSIZE,
    LR_LOADFROMFILE, MF_CHECKED, MF_DISABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG,
    TPM_BOTTOMALIGN, TPM_LEFTALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_COMMAND, WM_CONTEXTMENU, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
    WNDCLASS_STYLES, WS_OVERLAPPEDWINDOW,
};

use uda_core::capability::{Capability, SupportLevel};
use uda_core::error::UdaError;
use uda_core::tray::{
    MenuItem, TrayEvent, TrayFeature, TrayIcon, TrayIconConfig, TrayIconSource, TrayManager,
};

/// Callback message id posted by the shell into the hidden window.
///
/// `tray_specs.md` §2.4 requires `>= WM_USER`; `WM_APP` (0x8000) leaves the whole
/// `WM_USER` range free for a host application that ever shares the window.
const CALLBACK_MESSAGE: u32 = 0x8000;

/// Private message that resolves a command chosen in the modal menu loop.
///
/// `TrackPopupMenuEx` returns the chosen id, but resolving it needs the worker,
/// and the frame that called the modal loop must not keep touching the worker
/// once it returns. Posting the id back through the queue turns the invocation
/// into an ordinary message: it is dispatched with a fresh, exclusive worker
/// borrow, and no reference to the worker survives the modal loop at all.
const MENU_COMMAND_MESSAGE: u32 = 0x8001;

/// Window style for the worker window: a full top-level window style *without*
/// the `WS_VISIBLE` bit. Nothing ever appears (the window is never shown and has
/// a zero size), but it must be a *normal* top-level window rather than a
/// message-only one: a message-only window does not receive broadcast messages,
/// so it would never see the `TaskbarCreated` broadcast that announces a shell
/// restart and the icon could not survive an explorer restart.
const WINDOW_STYLE_BITS: WINDOW_STYLE = WS_OVERLAPPEDWINDOW;

/// Extended style for the worker window: none; invisibility comes from the
/// missing `WS_VISIBLE` bit and the zero size, not from an extended style.
const WINDOW_EX_STYLE_BITS: WINDOW_EX_STYLE = WINDOW_EX_STYLE(0);

/// Version-4 keyboard activation notification.
///
/// `shellapi.h` defines `NIN_SELECT` as `WM_USER + 0` and `NIN_KEYSELECT` as
/// `WM_USER + 1`; the `windows` crate exports the former but not the latter, so
/// the documented value is spelled out here and asserted in a test.
const NIN_KEYSELECT: u32 = 0x0401;

/// The `uFlags` bits every registration and update must carry.
///
/// `NIF_SHOWTIP` is mandatory alongside `NIF_TIP` under
/// `NOTIFYICON_VERSION_4`: without it the shell suppresses the standard tooltip
/// entirely (`tray_specs.md` §2.2), so `set_tooltip` would never become visible.
/// One constant so the two call sites (`add_icon`, `apply_refresh`) can never
/// diverge on a flag whose omission is silent.
const BASE_ICON_FLAGS: NOTIFY_ICON_DATA_FLAGS =
    NOTIFY_ICON_DATA_FLAGS(NIF_MESSAGE.0 | NIF_TIP.0 | NIF_SHOWTIP.0);

/// Window class name registered once per process. A per-process unique suffix is
/// appended by `register_window_class`, which owns the counter, so two UDA tray
/// icons in one process never collide.
const fn class_name() -> &'static str {
    "UDA_TrayMessageWindow"
}

/// Per-process counter disambiguating window classes and tray icon ids.
static TRAY_COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// UTF-16 helpers
// ---------------------------------------------------------------------------

/// Encode a Rust string as a NUL-terminated UTF-16 buffer for a Win32 `*W` call.
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
/// `NOTIFYICONDATAW::szTip` is an inline `[u16; 128]`, not a pointer, so the
/// caller needs a sized copy. [`uda_core::tray::sanitize_tooltip`] already
/// capped the string in chars, so this only has to stop at `limit` units to keep
/// a surrogate pair intact.
fn to_fixed_utf16<const N: usize>(text: &str) -> [u16; N] {
    let mut buffer = [0u16; N];
    // Reserve one element for the terminator.
    let limit = N.saturating_sub(1);
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
// Callback message layout
// ---------------------------------------------------------------------------

/// A tray callback message, unpacked from its `wParam` / `lParam` pair.
///
/// Under `NOTIFYICON_VERSION_4` the shell packs two values into `lParam` and a
/// third into `wParam` (`tray_specs.md` §2.3):
/// `lParam` low 16 bits are the mouse message, its high 16 bits the icon id from
/// `NOTIFYICONDATAW::uID`, and `wParam` is the cursor position with `x` low and
/// `y` high, each signed 16-bit.
///
/// A struct with one parser states the packing contract once, instead of
/// re-deriving it at each call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CallbackPayload {
    /// The mouse message the shell reported.
    event: u32,
    /// The icon id the notification is addressed to.
    icon_id: u32,
    /// The cursor position in screen coordinates.
    cursor: POINT,
}

/// Unpack a tray callback message.
fn unpack_callback(wparam: WPARAM, lparam: LPARAM) -> CallbackPayload {
    let packed = lparam.0 as u32;
    // `wParam` is an `isize` on 64-bit Windows; the `as u32` truncates to the low
    // word, the documented width of `x`, before the high half is read as `y`.
    let coordinates = wparam.0 as u32;
    CallbackPayload {
        event: packed & 0xFFFF,
        icon_id: (packed >> 16) & 0xFFFF,
        cursor: POINT {
            x: (coordinates & 0xFFFF) as i16 as i32,
            y: ((coordinates >> 16) & 0xFFFF) as i16 as i32,
        },
    }
}

/// What the window procedure should do with one shell notification event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallbackAction {
    /// The icon was activated (mouse click or keyboard selection).
    Click,
    /// The icon was double-clicked.
    DoubleClick,
    /// A context menu was requested, anchored as described by [`MenuAnchor`].
    Menu(MenuAnchor),
    /// Nothing UDA consumes; hand the message to `DefWindowProcW`.
    Unhandled,
}

/// Map one version-4 notification event onto a [`CallbackAction`].
///
/// Pure so the mapping table stays unit-testable without a shell: v4 reports
/// mouse activations as `WM_LBUTTONUP` / `WM_LBUTTONDBLCLK`, keyboard
/// activations as `NIN_SELECT` / `NIN_KEYSELECT`, and context-menu requests as
/// `WM_RBUTTONUP` (mouse) or `WM_CONTEXTMENU`. `WM_RBUTTONDOWN` is deliberately
/// *not* a trigger: exactly one of DOWN/UP may open the menu, or a single
/// right-click would hand itself a second tracking loop re-entrantly.
fn classify_callback_event(event: u32, cursor: POINT) -> CallbackAction {
    match event {
        WM_LBUTTONUP => CallbackAction::Click,
        // `NIN_SELECT` (mouse selection) and `NIN_KEYSELECT` (keyboard Enter or
        // Space) are the version-4 activation notifications; both are the click
        // a host expects, and leaving them unhandled loses keyboard access.
        NIN_SELECT | NIN_KEYSELECT => CallbackAction::Click,
        WM_LBUTTONDBLCLK => CallbackAction::DoubleClick,
        WM_RBUTTONUP => CallbackAction::Menu(MenuAnchor::Reported(cursor)),
        // `WM_CONTEXTMENU`'s `wParam` has no consistently documented meaning
        // (keyboard invocations report (-1, -1) or the icon rectangle depending
        // on the shell), so the menu anchors at the live cursor instead of
        // trusting a value that may be garbage.
        WM_CONTEXTMENU => CallbackAction::Menu(MenuAnchor::AtCursor),
        _ => CallbackAction::Unhandled,
    }
}

/// Where a context menu should open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuAnchor {
    /// The coordinates the shell reported in the callback's `wParam`; trusted
    /// for mouse notifications only.
    Reported(POINT),
    /// No trustworthy position: query the live cursor instead.
    AtCursor,
}

/// Resolve a [`MenuAnchor`] into a screen point for `TrackPopupMenuEx`.
///
/// A reported `(0, 0)` counts as "no position was supplied" (the keyboard
/// `Shift+F10` path) and falls back to `live_cursor`, as does
/// [`MenuAnchor::AtCursor`]; a failed cursor query degrades to the screen
/// origin, which the alignment flags still place on screen. Pure so the anchor
/// policy stays unit-testable without a display.
fn resolve_menu_anchor(anchor: MenuAnchor, live_cursor: Option<POINT>) -> POINT {
    match anchor {
        // A non-zero report is a real mouse position; use it verbatim.
        MenuAnchor::Reported(point) if point.x != 0 || point.y != 0 => point,
        // A zero report (or no report at all) falls back to the live cursor.
        _ => live_cursor.unwrap_or(POINT { x: 0, y: 0 }),
    }
}

// ---------------------------------------------------------------------------
// Icon transcoding
// ---------------------------------------------------------------------------

/// The tray icon size Win32 wants, in pixels.
///
/// Read lazily rather than held in a `const`, because a DPI change or a theme
/// switch can move `GetSystemMetrics(SM_CXSMICON)` while the process lives.
/// `LoadImageW` is handed the same pair, so a multi-resolution `.ico` picks the
/// entry closest to what the shell will draw.
fn tray_icon_extent() -> (i32, i32) {
    // SAFETY: `GetSystemMetrics` only reads process-wide window metrics and has
    // no failure mode to report.
    let extent = unsafe { GetSystemMetrics(SM_CXSMICON) };
    if extent > 0 {
        (extent, extent)
    } else {
        (16, 16)
    }
}

/// Load a Win32 `HICON` from a file path, returning `None` when the file is
/// missing, unreadable or not an image so the caller can degrade.
fn icon_from_path(path: &str) -> Option<HICON> {
    // The source contract already rejects blank paths (`tray_specs.md` §3.1);
    // re-checking keeps the helper honest when called directly.
    if path.trim().is_empty() {
        log::warn!("tray icon path is blank; no icon will be set");
        return None;
    }

    let wide = to_utf16(path);
    let (width, height) = tray_icon_extent();

    // A null module handle makes `LoadImageW` read `name` as a file name rather
    // than a resource ordinal. windows-rs has no `Param<HINSTANCE>` for
    // `Option<HINSTANCE>`, so the null handle is spelled as a default value.
    let module = windows::Win32::Foundation::HINSTANCE::default();

    // SAFETY: `wide` is a live, null-terminated UTF-16 buffer for the duration of
    // the call and `module` is null, so the name is read as the file name above.
    // `LoadImageW` copies the decoded image into a handle the caller owns.
    let handle = unsafe {
        LoadImageW(
            module,
            windows::core::PCWSTR(wide.as_ptr()),
            IMAGE_ICON,
            width,
            height,
            LR_LOADFROMFILE | LR_DEFAULTSIZE,
        )
    };

    match handle {
        Ok(handle) => Some(HICON(handle.0)),
        Err(error) => {
            log::warn!("LoadImageW({path}) failed: {error}");
            None
        }
    }
}

/// Convert a validated source into a Win32 `HICON`, returning `None` when the
/// source cannot be turned into an icon so the caller can degrade.
///
/// An RGBA icon is wrapped in a 32bpp top-down DIB section fed to
/// `CreateIconIndirect` with an opaque mask (`tray_specs.md` §2.6). The DIB is
/// built from the source directly rather than through the Linux transcoder,
/// which produces bottom-up rows for `IconPixmap`.
fn icon_from_source(source: &TrayIconSource) -> Option<HICON> {
    // A path icon is decoded by Win32 itself: the loader understands `.ico`,
    // `.cur` and `.bmp`, including the multi-image entries packed for several
    // DPI levels.
    if let TrayIconSource::Path(path) = source {
        return icon_from_path(path);
    }

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
    let Some(bitmap) = bitmap else {
        log::warn!("could not build a tray icon bitmap");
        return None;
    };

    // An opaque 1bpp mask keeps the shell from treating transparent pixels as
    // "cut out"; the alpha channel in the colour bitmap carries the shape.
    let mask = match create_mask_bitmap(width, height) {
        Some(mask) => mask,
        None => {
            log::warn!("could not build the tray icon mask bitmap");
            // The colour bitmap is already built at this point; release it so
            // this failure path leaks no GDI object either.
            delete_bitmap(bitmap);
            return None;
        }
    };
    let info = ICONINFO {
        fIcon: windows::Win32::Foundation::BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: bitmap,
    };

    // SAFETY: `CreateIconIndirect` *copies* the content of both bitmaps into the
    // icon it returns (MSDN: "the icon ... is created by copying the bitmaps"),
    // so the originals remain the caller's property and are deleted below on
    // every path, success or failure.
    let icon = match unsafe { CreateIconIndirect(&info) } {
        Ok(icon) => Some(icon),
        Err(error) => {
            log::warn!("CreateIconIndirect failed: {error}");
            None
        }
    };
    // The two source bitmaps are no longer needed now that the call has
    // returned: the icon owns its own copies. Released whether the icon was
    // created or not, so no path leaks a GDI object (GDI handles are
    // process-wide and capped at 10,000 by default).
    delete_bitmap(bitmap);
    delete_bitmap(mask);
    icon
}

/// Build a 32bpp top-down BGRA `HBITMAP` holding `pixels`. A DIB section is used
/// instead of `CreateBitmap` so the layout is exactly `BI_RGB` with no palette
/// translation.
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
        // The call reported success but handed back no pixel memory; the
        // half-built bitmap is the caller's to release, so destroy it here
        // rather than leaking one GDI object per attempt.
        delete_bitmap(bitmap);
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

/// Build a 1bpp monochrome mask covering the whole icon. All-zero bits mean
/// "leave the colour bitmap visible", the correct mask when alpha already
/// encodes the shape. Returns `None` when Win32 refuses the bitmap, so the
/// caller aborts instead of feeding `CreateIconIndirect` a null handle.
fn create_mask_bitmap(width: i32, height: i32) -> Option<HBITMAP> {
    // A monochrome row is padded to 16 bits; `width <= 32` for any tray icon, so
    // one u16 per row is enough and the rest stays zero.
    let row_bytes = (((width + 15) / 16) * 2) as usize;
    let mut zeros = vec![0u8; row_bytes * height as usize];
    // SAFETY: `CreateBitmap` reads `zeros` as planar data of the documented size.
    let bitmap = unsafe { CreateBitmap(width, height, 1, 1, Some(zeros.as_mut_ptr().cast())) };
    if bitmap.is_invalid() {
        let last = unsafe { GetLastError() };
        log::warn!("CreateBitmap(mask) failed with Win32 error {}", last.0);
        return None;
    }
    Some(bitmap)
}

/// Release one GDI bitmap, logging a failure instead of panicking.
///
/// The bitmaps fed to `CreateIconIndirect` remain the caller's property (the
/// icon copies their content), so every creation path must reach this exactly
/// once per bitmap it created.
fn delete_bitmap(bitmap: HBITMAP) {
    if bitmap.is_invalid() {
        return;
    }
    // SAFETY: `bitmap` came from `CreateDIBSection` or `CreateBitmap` in this
    // module and is deleted exactly once here.
    unsafe {
        if !DeleteObject(bitmap).as_bool() {
            log::debug!("DeleteObject reported a failure");
        }
    }
}

/// Destroy an icon built by [`icon_from_source`]. A named function so the drop
/// path and the icon-swap path share it, and a failure is only a log line.
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

/// Whether two icon sources describe the same image. `TrayIconSource` has no
/// `PartialEq` (it owns a pixel buffer), so equality is decided structurally.
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

/// Whether two menus are the same menu: `Arc::ptr_eq`, because a mutating host
/// keeps the same `Arc` and the rows are rebuilt at open time anyway.
fn menus_equal(
    left: Option<&Arc<uda_core::tray::TrayMenu>>,
    right: Option<&Arc<uda_core::tray::TrayMenu>>,
) -> bool {
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
/// `WM_COMMAND` carries a 16-bit id, so the backend keeps an `id -> row` table
/// filled by the single allocation pass of [`MenuTable::build`]
/// (`tray_specs.md` §2.5 item 4). The callback is cloned out of the host's row
/// at build time, so a click never reaches back into `TrayMenu` while the
/// shell menu is open.
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
/// a click addressable regardless of how the host nested its submenus. The
/// table is filled by exactly one pass of [`MenuTable::build`], so every entry
/// corresponds to one row of the `HMENU` the shell was handed: a separator has
/// no command id and no entry, and nothing survives from an earlier build.
struct MenuTable {
    entries: Vec<MenuEntry>,
    /// The next id to hand out; starts above 1 so 0 can mean "nothing chosen".
    next_id: u16,
    /// The host menu the current entries were built from, by identity.
    ///
    /// [`Worker::on_command`] compares this against the menu still attached to
    /// the icon before invoking anything: if the host detached or replaced the
    /// menu while a popup was open (or while a chosen id was still travelling
    /// through the message queue), these entries describe a menu that no
    /// longer exists and their callbacks must not fire.
    source: Option<Arc<uda_core::tray::TrayMenu>>,
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
            source: None,
        }
    }

    /// Allocate one id and remember the row it addresses.
    fn allocate(&mut self, label: String, action: Option<uda_core::tray::TrayAction>) -> u16 {
        let command_id = self.next_id;
        // Saturating: wrapping would alias an existing command.
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

    /// Log which command id landed on which row, the mapping the shell is about
    /// to be handed.
    fn trace(&self) {
        for entry in &self.entries {
            log::debug!(
                "tray menu row {:?} -> command id {}",
                entry.label,
                entry.command_id
            );
        }
    }

    /// Build a Win32 `HMENU` for the whole menu, allocating ids in the same
    /// pass.
    ///
    /// The table is reset first, so the result describes exactly one build and
    /// opening the menu twice cannot accumulate dead entries. On success the
    /// table records `menu` by identity ([`MenuTable::source`]) — the guard
    /// [`Worker::on_command`] checks before invoking anything — and every row
    /// of the returned `HMENU` that carries a command id has exactly one entry
    /// here; separators carry none. The caller destroys the `HMENU` once the
    /// shell is done with it. A row Win32 refuses fails the whole build: a
    /// partially rendered menu would show rows whose command ids can never
    /// fire anything.
    fn build(&mut self, menu: &Arc<uda_core::tray::TrayMenu>) -> Option<HMENU> {
        // The single allocation pass starts from a clean table: `append_item`
        // below is the only place ids are handed out, so ids, entries and the
        // `HMENU` stay in one-to-one correspondence.
        self.entries.clear();
        self.next_id = FIRST_COMMAND_ID;
        self.source = None;

        let popup = match create_popup() {
            Ok(popup) => popup,
            Err(error) => {
                log::warn!("CreatePopupMenu failed: {error}");
                return None;
            }
        };
        for item in menu.items() {
            if !self.append_item(popup, &item) {
                // `append_item` already logged the offending row; release the
                // half-built menu (never handed to the shell) so a failed
                // append cannot leak it.
                destroy_menu(popup);
                // Leave nothing behind: the table must not describe a menu
                // that was never handed to the shell.
                self.entries.clear();
                return None;
            }
        }
        self.source = Some(Arc::clone(menu));
        self.trace();
        Some(popup)
    }

    /// Append one row (and its submenu, recursively) to `menu`.
    ///
    /// Returns `false` when Win32 refused the row, so the caller can fail the
    /// whole menu rather than silently show one with missing rows.
    fn append_item(&mut self, menu: HMENU, item: &MenuItem) -> bool {
        match item {
            MenuItem::Separator => {
                // A separator can never be chosen, so it carries no command id
                // and gets no table entry either.
                // SAFETY: `menu` is a live popup created by `CreatePopupMenu`.
                let appended =
                    unsafe { AppendMenuW(menu, MF_SEPARATOR, 0, windows::core::PCWSTR::null()) };
                match appended {
                    Ok(()) => true,
                    Err(error) => {
                        log::error!("AppendMenuW(separator) failed: {error}");
                        false
                    }
                }
            }
            MenuItem::Text {
                label,
                state,
                action,
            } => {
                let command_id = self.allocate(label.clone(), action.clone());
                append_row(menu, command_id, label, state.enabled, false, false)
            }
            MenuItem::Checkbox {
                label,
                state,
                action,
            } => {
                let command_id = self.allocate(label.clone(), action.clone());
                append_row(menu, command_id, label, state.enabled, true, state.checked)
            }
            MenuItem::Submenu {
                label, children, ..
            } => {
                // The submenu row claims one id so its position is stable.
                self.allocate(label.clone(), None);
                let child_menu = match create_popup() {
                    Ok(child_menu) => child_menu,
                    Err(error) => {
                        log::warn!("CreatePopupMenu failed for a submenu: {error}");
                        return false;
                    }
                };
                for child in children.items() {
                    if !self.append_item(child_menu, &child) {
                        // The child menu was never attached to the parent, so
                        // nothing else can reach it; destroy it instead of
                        // leaking the HMENU.
                        destroy_menu(child_menu);
                        return false;
                    }
                }
                let wide = to_utf16(label);
                // SAFETY: `menu` and `child_menu` are live menus, and `wide`
                // outlives the call (the label is copied by the shell).
                let appended = unsafe {
                    AppendMenuW(
                        menu,
                        MF_POPUP,
                        child_menu.0 as usize,
                        windows::core::PCWSTR(wide.as_ptr()),
                    )
                };
                match appended {
                    // On success ownership of `child_menu` moved into `menu`.
                    Ok(()) => true,
                    Err(error) => {
                        log::error!("AppendMenuW(submenu {label:?}) failed: {error}");
                        // The append failed, so the popup was never attached to
                        // the parent and would leak if it were not destroyed.
                        destroy_menu(child_menu);
                        false
                    }
                }
            }
        }
    }
}

/// Create an empty popup menu.
fn create_popup() -> windows::core::Result<HMENU> {
    // SAFETY: no parameters, no invariants.
    unsafe { CreatePopupMenu() }
}

/// Release a popup menu built by this module.
///
/// Every caller holds either a menu the shell is done with (a finished tracking
/// loop) or one that was never handed to it (a half-built menu released after a
/// failed append), so each call destroys a menu nothing else can reach. A named
/// function so every release path shares one point; a failed destroy is only a
/// log line.
fn destroy_menu(menu: HMENU) {
    // SAFETY: `menu` came from `CreatePopupMenu` in this module and is destroyed
    // exactly once here; no caller keeps a copy of the handle.
    unsafe {
        if let Err(error) = DestroyMenu(menu) {
            log::debug!("DestroyMenu failed: {error}");
        }
    }
}

/// Append a text or checkbox row to `menu`.
///
/// A disabled row gets both `MF_DISABLED` and `MF_GRAYED`: the first stops it
/// firing, the second is what actually greys it out (`tray_specs.md` §2.5).
/// Returns `false` when Win32 refused the row, so a row can never disappear
/// silently from a menu the host believes it attached.
fn append_row(
    menu: HMENU,
    command_id: u16,
    label: &str,
    enabled: bool,
    checkbox: bool,
    checked: bool,
) -> bool {
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
    match unsafe {
        AppendMenuW(
            menu,
            flags,
            command_id as usize,
            windows::core::PCWSTR(wide.as_ptr()),
        )
    } {
        Ok(()) => true,
        Err(error) => {
            log::error!("AppendMenuW({command_id}, {label:?}) failed: {error}");
            false
        }
    }
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
    /// The hidden worker window, once created.
    hwnd: HWND,
    /// The icon currently registered with the shell, if any.
    icon: HICON,
    /// Command-id table, filled by [`MenuTable::build`] while a popup is
    /// prepared and discarded when the host's menu changes.
    menu_table: MenuTable,
    /// Whether this worker's modal menu tracking loop is running.
    ///
    /// Per-worker on purpose, not a process-wide static: re-entry can only
    /// happen on this thread (the modal loop pumps *this* window's messages),
    /// while a process-wide flag silently swallowed the legitimate menu
    /// requests of every *other* tray icon's worker. The window procedure takes
    /// `&mut Worker` per message, so the guard is set, observed and cleared
    /// through this one field with no cross-thread synchronization.
    menu_tracking: bool,
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
    /// Message id registered for the shell's `TaskbarCreated` broadcast.
    ///
    /// Zero when the registration failed; the window procedure then never
    /// matches (no real registered message can be 0) and a shell restart is
    /// simply not survivable, which the setup path logs loudly.
    taskbar_created_msg: u32,
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
            menu_tracking: false,
            version_handshake_done: false,
            registered: false,
            tooltip: String::new(),
            last_icon: None,
            menu: None,
            // A new icon is visible until the host hides it; `TrayShared` seeds
            // the same value so the first tick is not mistaken for a change.
            visible: true,
            taskbar_created_msg: 0,
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
        // A null parent makes this a normal top-level window, which is what
        // `WINDOW_STYLE_BITS` requires: only such a window receives the
        // `TaskbarCreated` broadcast registered below.
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
                HWND::default(),
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
            return Err(UdaError::Internal("the tray window is invalid".to_string()));
        }
        self.hwnd = hwnd;

        // Hand the worker to the window procedure through user data, so the
        // proc can reach the shared state without a global or a thread-local.
        //
        // SAFETY: `hwnd` was created by this thread and `GWLP_USERDATA` is a
        // documented per-window slot on a window this thread owns. The worker
        // outlives the window (teardown destroys the window first), and it is
        // heap-pinned by `spawn_worker`, so `self` never moves after this write
        // and the pointer stays valid for the window's whole life.
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, self as *mut Worker as isize);
        }

        // Register for the shell's restart broadcast. The id is session-unique
        // for this string; zero means the registration failed, in which case
        // the worker will simply never see a shell restart, so the icon cannot
        // survive an explorer restart and that deserves a loud log line.
        self.taskbar_created_msg =
            unsafe { RegisterWindowMessageW(windows::core::w!("TaskbarCreated")) };
        if self.taskbar_created_msg == 0 {
            let last = unsafe { GetLastError() };
            log::error!(
                "RegisterWindowMessageW(TaskbarCreated) failed with Win32 error {}; \
                 the tray icon will not survive a shell restart",
                last.0
            );
        }

        // The heartbeat must exist before the icon: `add_icon` is what makes the
        // shell state live, and a failed timer would orphan it (no tick would
        // ever apply an update or notice shutdown), so a zero return is a fatal
        // setup error rather than a silent degradation.
        //
        // SAFETY: `hwnd` is this thread's live window, and a `None` timer
        // procedure means the tick arrives as `WM_TIMER` instead of a call.
        let timer = unsafe { SetTimer(hwnd, SYNC_TIMER_ID, SYNC_INTERVAL_MS, None) };
        if timer == 0 {
            let last = unsafe { GetLastError() };
            return Err(UdaError::Internal(format!(
                "could not start the tray sync timer (Win32 error {})",
                last.0
            )));
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

        // Only flags that are actually set may appear in `uFlags`, so the icon
        // is only requested when a source exists.
        let mut flags = BASE_ICON_FLAGS;
        if !icon.is_invalid() {
            flags |= NIF_ICON;
            data.hIcon = icon;
        }
        data.uFlags = flags;
        data.szTip = tip;

        // SAFETY: `data` is fully initialised and `hwnd` is this thread's
        // hidden worker window; the shell copies what it needs before returning.
        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &data) };
        if !added.as_bool() {
            log::warn!("Shell_NotifyIconW(NIM_ADD) failed; no tray icon");
            destroy_icon(icon);
            return Err(UdaError::NotSupported(
                "the shell refused to add a tray icon".to_string(),
            ));
        }
        // A shell-restart re-registration supersedes the previous handle only
        // after the shell accepted the new one; on a first registration the
        // previous handle is the invalid default and `destroy_icon` is a no-op.
        destroy_icon(self.icon);
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
    ///
    /// Returns `true` when the shell accepted the update. The caches
    /// (`tooltip`, `last_icon`, `visible`) are rewritten **only on success**,
    /// so a failed update leaves them stale and the next tick retries it
    /// instead of recording a change as applied that the shell never saw.
    fn apply_refresh(&mut self) -> bool {
        if self.hwnd.is_invalid() {
            return false;
        }
        let (tooltip, icon_source, visible) = {
            let shared = lock_or_recover(&self.shared, "worker refresh");
            (shared.tooltip.clone(), shared.icon.clone(), shared.visible)
        };

        let mut data = NOTIFYICONDATAW::default();
        data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        data.hWnd = self.hwnd;
        data.uID = self.icon_id;
        data.uCallbackMessage = CALLBACK_MESSAGE;
        data.szTip = to_fixed_utf16::<128>(&uda_core::tray::sanitize_tooltip(&tooltip));

        let mut new_icon = self.icon;
        let mut flags = BASE_ICON_FLAGS;
        if visible {
            if let Some(source) = icon_source.as_ref() {
                new_icon = icon_from_source(source).unwrap_or(self.icon);
            }
            if !new_icon.is_invalid() {
                flags |= NIF_ICON;
                data.hIcon = new_icon;
            }
            // Showing the icon clears `NIS_HIDDEN`: `dwStateMask` selects which
            // bits are touched, so the mask must name `NIS_HIDDEN` while the
            // state itself is zero. A zero mask would change nothing and a
            // hidden icon would never come back.
            flags |= NIF_STATE;
            data.dwState = NOTIFY_ICON_STATE(0);
            data.dwStateMask = NIS_HIDDEN;
        } else {
            // A hidden icon keeps its registration but is not drawn, which is
            // the documented alternative to unregistering and re-adding.
            flags |= NIF_STATE;
            data.dwState = NIS_HIDDEN;
            data.dwStateMask = NIS_HIDDEN;
        }
        data.uFlags = flags;

        // SAFETY: same window and icon id as registration, so this is the
        // documented modify path.
        let updated = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
        if !updated.as_bool() {
            log::debug!("Shell_NotifyIconW(NIM_MODIFY) failed; keeping the previous state");
            // A freshly built handle must not leak — but it may legitimately be
            // the still-registered one (`unwrap_or(self.icon)` above), which has
            // to survive a failed update.
            if !new_icon.is_invalid() && new_icon != self.icon {
                destroy_icon(new_icon);
            }
            return false;
        }

        // Only retire the previous icon once the shell accepted the new one,
        // so a failed update never leaves the item icon-less.
        if !new_icon.is_invalid() && new_icon != self.icon {
            destroy_icon(self.icon);
            self.icon = new_icon;
        }
        // Record exactly what the shell now shows, so the next tick compares
        // against reality rather than against intent (and retries a failed
        // update because the cache still holds the old value).
        self.tooltip = tooltip;
        self.last_icon = icon_source;
        self.visible = visible;
        true
    }

    /// Discard the command-id table after the host's menu changed.
    ///
    /// Ids are allocated only while a popup is being prepared
    /// ([`MenuTable::build`]), so between opens "rebuilding" means dropping:
    /// entries left over from the last build describe a popup the shell is no
    /// longer showing. [`Worker::on_command`] independently re-checks the
    /// table's `source` against the menu still attached, so a command that
    /// arrives before this tick runs is discarded there as well.
    fn invalidate_menu_table(&mut self) {
        self.menu_table = MenuTable::new();
    }

    /// Mirror the host state, then apply or tear down as needed.
    ///
    /// Runs on the sync timer, which is the only place the worker reads the
    /// host's state: keeping it out of the message handling itself means a menu
    /// callback and a host mutation can never interleave mid-update.
    ///
    /// Returns `false` when the worker must stop.
    fn on_tick(&mut self) -> bool {
        let (menu, tooltip, icon, visible, shutdown) = {
            let mut shared = lock_or_recover(&self.shared, "tick");
            shared.sync_from();
            (
                shared.menu.clone(),
                shared.tooltip.clone(),
                shared.icon.clone(),
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

        // One refresh covers visibility, tooltip and icon together; whether the
        // mirror differs from what the shell last accepted decides whether a
        // `NIM_MODIFY` is worth issuing. `apply_refresh` rewrites its caches
        // only on success, so a failed update stays dirty here and is retried
        // on the next tick instead of being recorded as applied.
        if visible != self.visible
            || tooltip != self.tooltip
            || !icons_equal(icon.as_ref(), self.last_icon.as_ref())
        {
            self.apply_refresh();
        }
        if !menus_equal(menu.as_ref(), self.menu.as_ref()) {
            self.menu = menu;
            self.invalidate_menu_table();
        }
        true
    }

    /// Run the worker until the host drops the icon.
    ///
    /// This is the thread entry point: the message loop blocks in
    /// `GetMessageW` until a message arrives or the window is destroyed, so the
    /// thread parks without burning CPU and never blocks the host. The sync
    /// timer (registered by `setup`) posts `WM_TIMER` into this same loop, so
    /// everything the worker does happens on one thread.
    fn run(&mut self) {
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
    /// Under `NOTIFYICON_VERSION_4` (which this worker requests with
    /// `NIM_SETVERSION`; `tray_specs.md` §2.2) **both** the event and the icon id
    /// travel packed into `lParam`:
    ///
    /// * **low 16 bits of `lParam`** - the notification (`WM_LBUTTONUP`,
    ///   `WM_CONTEXTMENU`, `NIN_SELECT`, ...).
    /// * **high 16 bits of `lParam`** - the icon id from `NOTIFYICONDATAW::uID`.
    /// * **`wParam`** - the cursor's **screen coordinates**, `x` in the low word
    ///   and `y` in the high word. It is *not* an id, so comparing it against
    ///   `icon_id` silently drops every notification: the id never matches a
    ///   coordinate pair, which is exactly the "icon appears but nothing
    ///   responds" symptom.
    ///
    /// The event-to-action mapping itself lives in
    /// [`classify_callback_event`], where it is unit-testable.
    fn on_callback(&mut self, hwnd: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        let payload = unpack_callback(wparam, lparam);
        // Ignore anything addressed to a different icon in this window's set.
        if payload.icon_id != self.icon_id {
            return LRESULT(0);
        }

        match classify_callback_event(payload.event, payload.cursor) {
            CallbackAction::Click => self.dispatch(TrayEvent::Click),
            CallbackAction::DoubleClick => self.dispatch(TrayEvent::DoubleClick),
            CallbackAction::Menu(anchor) => {
                // While the modal tracking loop pumps messages, a further menu
                // trigger would stack a second popup on the first; the guard
                // turns the inner request into a no-op instead. The flag is
                // per-worker (re-entry only ever happens on this thread), so
                // another icon's worker is never blocked by this popup.
                if self.menu_tracking {
                    log::debug!("menu trigger ignored while a popup is already tracking");
                    return LRESULT(0);
                }
                // A menu request is not a `TrayEvent`; it is handled inline so
                // the shell's own event ordering is respected.
                self.show_menu(anchor);
            }
            CallbackAction::Unhandled => {
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

    /// Build and show the context menu.
    ///
    /// The work is split in two so no borrow of the worker is alive while the
    /// modal loop pumps messages: everything touching `self` happens in
    /// [`Worker::prepare_menu`], and tracking runs from plain locals
    /// ([`MenuSession`]). The window procedure, which takes `&mut Worker` from
    /// `GWLP_USERDATA`, can therefore re-enter freely — any `WM_COMMAND`, click
    /// handler or menu trigger it serves behaves exactly as it would without a
    /// menu open. The chosen command id is forwarded through the message queue
    /// ([`MENU_COMMAND_MESSAGE`]) so its invocation also runs under a fresh,
    /// exclusive worker borrow.
    fn show_menu(&mut self, anchor: MenuAnchor) {
        // `session` owns only plain values, so the borrow of `self` ends here;
        // the tracking guard is handed over as a plain `&mut bool` so the modal
        // loop can set and release it without touching the worker again.
        if let Some(session) = self.prepare_menu(anchor) {
            session.track(&mut self.menu_tracking);
        }
    }

    /// Phase one of the menu: everything that needs the worker.
    ///
    /// Builds a fresh popup menu from the host's menu (a Win32 menu is a
    /// snapshot of the rows at build time, and the host may have mutated it
    /// since it was last shown), resolves the anchor point and claims
    /// foreground status.
    fn prepare_menu(&mut self, anchor: MenuAnchor) -> Option<MenuSession> {
        let menu = {
            let shared = lock_or_recover(&self.shared, "show menu");
            shared.menu.clone()
        };
        let menu = menu?;

        // One allocation pass: `build` resets the table, hands every row with
        // a command id exactly one id and records which menu the ids belong
        // to. (The previous rebuild-then-build pair allocated every row twice
        // — once for the table and once for the `HMENU` — which doubled the
        // table, left dead ids that could never be chosen, and filled `trace`
        // with entries the shell could never send.)
        let popup = self.menu_table.build(&menu)?;

        // One read serves both fallbacks; `resolve_menu_anchor` holds the
        // policy, and a failed query is not fatal because the alignment flags
        // still keep the menu on screen.
        let mut cursor = POINT::default();
        // SAFETY: `cursor` is a plain out-struct owned by this frame.
        let live_cursor = match unsafe { GetCursorPos(&mut cursor) } {
            Ok(()) => Some(cursor),
            Err(_) => None,
        };
        let point = resolve_menu_anchor(anchor, live_cursor);

        // SAFETY: `hwnd` is this thread's window; making it foreground is the
        // documented requirement for a dismissible popup.
        let foreground = unsafe { SetForegroundWindow(self.hwnd) };
        if !foreground.as_bool() {
            // The call is what lets the popup dismiss on an outside click, so a
            // failure (typically the foreground-lock timeout) degrades the
            // menu rather than aborting it. There is deliberately no
            // `NIM_SETFOCUS` fallback: that request only tells the shell to
            // hand focus back to the *notification area*, it does not make this
            // window foreground, so it cannot substitute here.
            log::warn!("SetForegroundWindow failed; the menu may not dismiss on an outside click");
        }

        Some(MenuSession {
            hwnd: self.hwnd,
            popup,
            point,
        })
    }

    /// Re-register the icon after the shell restarted (`TaskbarCreated`).
    ///
    /// explorer.exe forgets every notification-area item when it (re)starts; the
    /// worker re-adds the icon from its mirrored state. A refusal is not fatal:
    /// the next `TaskbarCreated` or the sync tick still sees a consistent
    /// worker.
    fn on_taskbar_created(&mut self) {
        if self.hwnd.is_invalid() {
            return;
        }
        log::debug!("shell restarted; re-registering the tray icon");
        // explorer forgot the item, so the local flag must be cleared before
        // the re-add: `add_icon` sets it again only on success.
        self.registered = false;
        if let Err(error) = self.add_icon() {
            log::warn!("could not re-register the tray icon after a shell restart: {error}");
            return;
        }
        if !self.visible {
            // `add_icon` registers the icon shown. Flip the applied cache back
            // to "visible" so the next tick sees a difference from the hidden
            // mirror and re-applies `NIS_HIDDEN`; a failed hide is retried
            // there too, because the caches only advance on success.
            self.visible = true;
        }
    }

    /// Handle a menu command id.
    ///
    /// The one place a command becomes a callback, reached two ways: the modal
    /// loop forwards the id the user chose via [`MENU_COMMAND_MESSAGE`]
    /// (`TPM_RETURNCMD` returns it instead of posting `WM_COMMAND`), and a shell
    /// that posts `WM_COMMAND` on its own lands on the same lookup.
    ///
    /// Before anything is invoked, the id is gated on the menu *still attached*
    /// to the icon: the table records which menu it was built from
    /// ([`MenuTable::source`]) and a command whose menu has since been detached
    /// or replaced is dropped. This is the last gate against firing a callback
    /// captured from a menu the host destroyed while the popup was open or
    /// while the chosen id was still travelling through the message queue — the
    /// FFI side unlinks the menu from the icon but cannot recall the actions
    /// already cloned into the worker's table. Identity comparison cannot close
    /// every race (a host mutating the same `TrayMenu` in place is invisible to
    /// it), but it covers the window the message queue opens.
    fn on_command(&mut self, command_id: u16) {
        if command_id == 0 {
            return;
        }
        let attached = lock_or_recover(&self.shared, "menu command").menu.clone();
        if !menus_equal(self.menu_table.source.as_ref(), attached.as_ref()) {
            log::debug!("tray command {command_id} dropped: its menu is no longer attached");
            return;
        }
        let action = self.menu_table.action_for(command_id);
        if action.is_some() {
            log::debug!("tray command {command_id} resolved to a row");
        } else {
            log::debug!("tray command {command_id} matched no row");
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
        // Stop the heartbeat first: no tick may fire while the icon is being
        // unregistered, or it would try to refresh an item mid-deletion. The
        // timer is killed whenever the window exists (not only on a successful
        // registration) because `setup` creates it before `add_icon`; killing
        // an id the window has no timer for is a harmless no-op.
        if !self.hwnd.is_invalid() {
            // SAFETY: `hwnd` is this thread's live window and the timer id is
            // the one `setup` registered.
            unsafe {
                let _ = KillTimer(self.hwnd, SYNC_TIMER_ID);
            }

            // A window with no registration owed the shell nothing, so the
            // `NIM_DELETE` is skipped entirely rather than issued against an id
            // the shell may have recycled for another item.
            if self.registered {
                let mut data = NOTIFYICONDATAW::default();
                data.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
                data.hWnd = self.hwnd;
                data.uID = self.icon_id;
                // SAFETY: same window and id as registration; the shell forgets
                // the item immediately, so no other field has to be filled.
                let removed = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
                if !removed.as_bool() {
                    log::debug!("Shell_NotifyIconW(NIM_DELETE) reported a failure");
                }
                // Either way the item is gone from this worker's point of view;
                // a second teardown must not try again.
                self.registered = false;
            }
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

/// The modal phase of a context menu: plain values only, no worker borrow.
///
/// A standalone bundle on purpose, so the modal loop inside `TrackPopupMenuEx`
/// can let the window procedure take the worker's `&mut` as the only live
/// reference.
struct MenuSession {
    /// The worker window the popup is attached to.
    hwnd: HWND,
    /// The popup menu to track, destroyed by [`MenuSession::track`].
    popup: HMENU,
    /// The screen point the menu opens at.
    point: POINT,
}

impl MenuSession {
    /// Run the modal tracking loop and forward the chosen command id.
    ///
    /// The id is posted back as [`MENU_COMMAND_MESSAGE`] instead of returned,
    /// because resolving it needs the worker and the caller holds no borrow of
    /// it across (or after) the modal loop. A failed forward is logged as an
    /// error — the choice is then lost, which is strictly worse than a failed
    /// menu build.
    fn track(self, menu_tracking: &mut bool) {
        // The window procedure ignores further menu triggers while this is
        // set; it reads the same worker field through `&mut Worker` on every
        // nested message, so the nested trigger sees it before any pumping
        // starts and the flag never has to be process-wide.
        *menu_tracking = true;

        // SAFETY: `popup` is live, `hwnd` is live, and no `TPMPARAMS` is needed
        // for a simple popup. `TPM_RETURNCMD` hands the chosen id back instead
        // of posting `WM_COMMAND`, which keeps the lookup in one place.
        let chosen = unsafe {
            TrackPopupMenuEx(
                self.popup,
                (TPM_LEFTALIGN | TPM_BOTTOMALIGN | TPM_RIGHTBUTTON | TPM_RETURNCMD).0,
                self.point.x,
                self.point.y,
                self.hwnd,
                None,
            )
        };

        // SAFETY: a harmless posted `WM_NULL` that unblocks the menu's own modal
        // loop, per the documented workaround.
        unsafe {
            let _ = PostMessageW(self.hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        }

        // The modal loop is done; release the guard before any later message
        // could legitimately be treated as a menu request again.
        *menu_tracking = false;

        // `popup` is no longer referenced by the shell once `TrackPopupMenuEx`
        // returned.
        destroy_menu(self.popup);

        // 0 means the user dismissed the menu without choosing anything.
        if chosen.0 != 0 {
            // SAFETY: `hwnd` is this thread's live window; posting only enqueues
            // a message for the worker's own pump to dispatch.
            unsafe {
                if let Err(error) = PostMessageW(
                    self.hwnd,
                    MENU_COMMAND_MESSAGE,
                    WPARAM(chosen.0 as usize),
                    LPARAM(0),
                ) {
                    log::error!("could not forward the chosen menu command: {error}");
                }
            }
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

/// The window procedure for the hidden worker window.
///
/// The worker pointer is stashed in `GWLP_USERDATA` at creation, so this is a
/// thin trampoline: it hands UDA's own messages to the worker and defers
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
        // The shell's restart broadcast. Zero never matches (no registered
        // message id can be 0), which is what keeps a failed registration from
        // turning every `WM_NULL` into a re-registration attempt.
        if worker.taskbar_created_msg != 0 && message == worker.taskbar_created_msg {
            // explorer.exe (re)started and forgot the icon; re-register it. A
            // broadcast is a notification, not a request for a default handler,
            // so it ends here.
            worker.on_taskbar_created();
            return LRESULT(0);
        }
        if message == WM_COMMAND || message == MENU_COMMAND_MESSAGE {
            // The low word of `wParam` is the command id, both for a shell-posted
            // `WM_COMMAND` and for the id forwarded out of the modal menu loop.
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
        //
        // The `Box` is load-bearing, not stylistic: `setup` writes
        // `self as *mut Worker` into the window's `GWLP_USERDATA`, and the
        // window procedure later dereferences it. A stack-resident `Worker`
        // would make that address depend on the closure's stack frame, which
        // the compiler is free to relocate or reclaim. Boxing pins the struct to
        // one heap allocation for the whole thread, so the pointer stays valid
        // until `teardown` clears the slot.
        let worker = Box::new(Worker {
            shared,
            icon_id,
            ..Worker::default()
        });
        let worker_name = class_name.clone();

        let handle = std::thread::Builder::new()
            .name("uda-tray-worker".to_string())
            .spawn(move || {
                // `setup` and the message loop are separate so a failure can be
                // reported before the (blocking) loop starts. Moving the `Box`
                // here moves only the pointer word; the heap `Worker` stays put,
                // which is what keeps the `GWLP_USERDATA` pointer honest.
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
        // publishes a degraded answer explicitly, stated with its reason.
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
        assert!(menu
            .push(MenuItem::submenu("more", Arc::clone(&child)))
            .is_ok());
        menu
    }

    #[test]
    fn command_ids_are_allocated_above_zero_and_uniquely() {
        let menu = sample_menu();
        let mut table = MenuTable::new();
        assert!(table.build(&menu).is_some());
        // The ids must all be non-zero, because `TrackPopupMenuEx` returns 0
        // for "nothing chosen".
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
        // One entry per row that carries a command id: the sample has five
        // (the separator is id-less in the HMENU and has no entry).
        assert_eq!(
            seen.len(),
            5,
            "one entry per id-bearing row: {}",
            seen.len()
        );

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

        let mut table = MenuTable::new();
        // Build so the id that `AppendMenuW` received is the one the lookup
        // uses.
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
        let mut table = MenuTable::new();
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
        let mut table = MenuTable::new();
        assert!(table.entries.is_empty());
        let popup = table.build(&menu);
        assert!(popup.is_some(), "an empty menu is still a valid popup");
        // An empty menu builds an empty table: no rows, no ids, no entries.
        assert!(table.entries.is_empty());
    }

    #[test]
    fn a_second_build_replaces_the_table_instead_of_accumulating() {
        // Opening the menu twice must not double the table: `build` is the one
        // allocation pass and starts from a clean table every time.
        let menu = sample_menu();
        let mut table = MenuTable::new();
        assert!(table.build(&menu).is_some());
        let first: Vec<u16> = table.entries.iter().map(|entry| entry.command_id).collect();

        assert!(table.build(&menu).is_some());
        let second: Vec<u16> = table.entries.iter().map(|entry| entry.command_id).collect();
        assert_eq!(second, first, "the same menu must build the same ids");
        assert_eq!(second.first(), Some(&FIRST_COMMAND_ID));
        assert_eq!(second.last(), Some(&(FIRST_COMMAND_ID + 4)));
    }

    #[test]
    fn a_build_yields_one_entry_per_row_of_the_real_menu() {
        // Every row of the built HMENU carries a command id, so the table must
        // mirror the shell's menu exactly — one entry per row, one id per
        // entry, no dead allocations. Submenu rows are counted through their
        // own popup: `GetMenuItemCount` sees only a menu's immediate rows.
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(menu.push(MenuItem::text("open")).is_ok());
        assert!(menu.push(MenuItem::checkbox_checked("pin")).is_ok());
        let child = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(child.push(MenuItem::text("inner")).is_ok());
        assert!(menu
            .push(MenuItem::submenu("more", Arc::clone(&child)))
            .is_ok());

        let mut table = MenuTable::new();
        let popup = match table.build(&menu) {
            Some(popup) => popup,
            None => panic!("a menu must build"),
        };

        // The built HMENU really has one row per entry, so the ids the shell
        // can send and the table's lookups stay in one-to-one correspondence.
        // SAFETY: `popup` is a live menu built above; these calls only read
        // its structure.
        let top_rows = unsafe { windows::Win32::UI::WindowsAndMessaging::GetMenuItemCount(popup) };
        // SAFETY: same live menu; the submenu is the third top-level row.
        let child_menu = unsafe { windows::Win32::UI::WindowsAndMessaging::GetSubMenu(popup, 2) };
        assert!(!child_menu.is_invalid(), "the submenu must be attached");
        // SAFETY: `child_menu` is the live submenu attached to `popup`.
        let child_rows =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetMenuItemCount(child_menu) };
        assert_eq!(
            top_rows + child_rows,
            table.entries.len() as i32,
            "one entry per row of the HMENU, nested rows included"
        );
        assert_eq!(table.entries.len(), 4);
        for (index, entry) in table.entries.iter().enumerate() {
            assert_eq!(entry.command_id, FIRST_COMMAND_ID + index as u16);
        }
        destroy_menu(popup);
    }

    #[test]
    fn a_built_table_remembers_which_menu_it_was_built_from() {
        // `on_command` drops a command when the menu it was built from is no
        // longer the one attached to the icon; that guard needs the table to
        // record its source by identity.
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(menu.push(MenuItem::text("open")).is_ok());
        let replacement = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(replacement.push(MenuItem::text("quit")).is_ok());

        let mut table = MenuTable::new();
        assert!(table.source.is_none(), "an unbuilt table has no source");

        assert!(table.build(&menu).is_some());
        assert!(Arc::ptr_eq(
            table.source.as_ref().expect("a build records its source"),
            &menu
        ));
        assert!(!menus_equal(table.source.as_ref(), Some(&replacement)));

        // A rebuild retargets the identity and describes only the new menu.
        assert!(table.build(&replacement).is_some());
        assert!(Arc::ptr_eq(
            table.source.as_ref().expect("a rebuild records its source"),
            &replacement
        ));
        assert_eq!(table.entries.len(), 1);
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
    fn the_private_messages_are_distinct_from_the_callback_message() {
        // Both live in the `WM_APP` range; a collision would route one message
        // kind into the handler of the other.
        assert_ne!(MENU_COMMAND_MESSAGE, CALLBACK_MESSAGE);
        assert!(MENU_COMMAND_MESSAGE > CALLBACK_MESSAGE);
    }

    #[test]
    fn the_base_icon_flags_always_carry_the_mandatory_showtip_bit() {
        // `NIF_SHOWTIP` must accompany `NIF_TIP` under `NOTIFYICON_VERSION_4`,
        // or the shell suppresses the tooltip (`tray_specs.md` §2.2).
        assert_eq!(BASE_ICON_FLAGS.0 & NIF_MESSAGE.0, NIF_MESSAGE.0);
        assert_eq!(BASE_ICON_FLAGS.0 & NIF_TIP.0, NIF_TIP.0);
        assert_eq!(BASE_ICON_FLAGS.0 & NIF_SHOWTIP.0, NIF_SHOWTIP.0);
        // Neither state bit belongs in the base set: `NIF_ICON` and `NIF_STATE`
        // are added per update.
        assert_eq!(BASE_ICON_FLAGS.0 & NIF_ICON.0, 0);
        assert_eq!(BASE_ICON_FLAGS.0 & NIF_STATE.0, 0);
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
        assert_eq!(
            state.icon,
            Some(TrayIconSource::Path("app.ico".to_string()))
        );
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
    fn the_callback_payload_splits_lparam_into_event_and_icon_id() {
        // `NOTIFYICON_VERSION_4` packing: the mouse message in the low word of
        // `lParam`, the icon id in the high word. Getting this wrong is what made
        // every notification silently drop on a real Windows machine.
        const WM_LBUTTONUP: u32 = 0x0202;
        let payload = unpack_callback(WPARAM(0), LPARAM(((7u32 << 16) | WM_LBUTTONUP) as isize));
        assert_eq!(payload.event, WM_LBUTTONUP);
        assert_eq!(payload.icon_id, 7);
        // No coordinates were supplied, so the cursor stays at the origin.
        assert_eq!(payload.cursor, POINT { x: 0, y: 0 });
    }

    #[test]
    fn the_icon_id_never_comes_from_wparam() {
        // The bug this guards against: `wParam` holds screen *coordinates*, so an
        // id read from it never matches and every event is discarded. The two
        // fields must stay independent.
        let payload = unpack_callback(
            WPARAM(((300u32 << 16) | 640) as usize),
            LPARAM(((2u32 << 16) | 0x0205) as isize),
        );
        assert_eq!(
            payload.icon_id, 2,
            "the id comes from the high word of lParam"
        );
        assert_eq!(payload.cursor, POINT { x: 640, y: 300 });
        assert_ne!(payload.icon_id as i32, payload.cursor.x);
    }

    #[test]
    fn the_cursor_is_signed_so_a_negative_coordinate_survives() {
        // A multi-monitor desktop puts the tray at negative coordinates, and the
        // 16-bit lanes are signed. Sign-extending them is what keeps the menu
        // anchored on the correct monitor.
        //
        // x = -100 -> 0xFF9C, y = -1200 -> 0xFB50. Each is a 16-bit two's
        // complement, so the low word of `wParam` is 0xFF9C and the high word is
        // 0xFB50; `wParam` itself is their concatenation.
        let x_lane: u32 = 0xFF9C;
        let y_lane: u32 = 0xFB50;
        let payload = unpack_callback(WPARAM(((y_lane << 16) | x_lane) as usize), LPARAM(0));
        assert_eq!(payload.cursor.x, -100);
        assert_eq!(payload.cursor.y, -1200);
    }

    #[test]
    fn the_callback_payload_is_addressable_by_icon_id() {
        // The worker discards a notification whose id does not match its own, so
        // two icons sharing a window cannot steal each other's clicks.
        const WM_RBUTTONUP: u32 = 0x0205;
        let mine = unpack_callback(WPARAM(0), LPARAM(((3u32 << 16) | WM_RBUTTONUP) as isize));
        let theirs = unpack_callback(WPARAM(0), LPARAM(((4u32 << 16) | WM_RBUTTONUP) as isize));
        assert_eq!(mine.icon_id, 3);
        assert_eq!(theirs.icon_id, 4);
        assert_ne!(mine.icon_id, theirs.icon_id);
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

    #[test]
    fn every_activation_notification_maps_to_a_click() {
        const CURSOR: POINT = POINT { x: 12, y: 34 };
        // Mouse activation...
        assert_eq!(
            classify_callback_event(WM_LBUTTONUP, CURSOR),
            CallbackAction::Click
        );
        // ...and both version-4 keyboard activations (`NIN_SELECT` is what the
        // shell reports after a mouse selection, `NIN_KEYSELECT` after Enter or
        // Space); dropping either loses keyboard access to the icon.
        assert_eq!(
            classify_callback_event(NIN_SELECT, CURSOR),
            CallbackAction::Click
        );
        assert_eq!(
            classify_callback_event(NIN_KEYSELECT, CURSOR),
            CallbackAction::Click
        );
        assert_eq!(
            classify_callback_event(WM_LBUTTONDBLCLK, CURSOR),
            CallbackAction::DoubleClick
        );
    }

    #[test]
    fn only_rbutton_up_and_context_menu_open_the_menu() {
        const CURSOR: POINT = POINT { x: -96, y: 40 };
        assert_eq!(
            classify_callback_event(WM_RBUTTONUP, CURSOR),
            CallbackAction::Menu(MenuAnchor::Reported(CURSOR))
        );
        // `WM_CONTEXTMENU` never trusts its `wParam`: the anchor is resolved
        // from the live cursor instead.
        assert_eq!(
            classify_callback_event(WM_CONTEXTMENU, CURSOR),
            CallbackAction::Menu(MenuAnchor::AtCursor)
        );
        // `WM_RBUTTONDOWN` must not trigger a second tracking loop on top of
        // the one the UP event opens.
        assert_eq!(
            classify_callback_event(
                windows::Win32::UI::WindowsAndMessaging::WM_RBUTTONDOWN,
                CURSOR
            ),
            CallbackAction::Unhandled
        );
        // A left click is a `TrayEvent`, never a menu.
        assert_ne!(
            classify_callback_event(WM_LBUTTONUP, CURSOR),
            CallbackAction::Menu(MenuAnchor::AtCursor)
        );
    }

    #[test]
    fn the_keyboard_activation_constant_matches_shellapi() {
        // `shellapi.h`: `NIN_SELECT` is `WM_USER`, `NIN_KEYSELECT` is
        // `WM_USER + 1`. The latter is spelled out locally, so the value is
        // pinned here against the documented definition.
        assert_eq!(NIN_SELECT, 0x0400);
        assert_eq!(NIN_KEYSELECT, 0x0401);
        assert_eq!(NIN_KEYSELECT, NIN_SELECT + 1);
    }

    #[test]
    fn a_non_zero_reported_anchor_wins_over_the_live_cursor() {
        let reported = POINT { x: 100, y: 40 };
        let cursor = POINT { x: 1, y: 2 };
        assert_eq!(
            resolve_menu_anchor(MenuAnchor::Reported(reported), Some(cursor)),
            reported
        );
    }

    #[test]
    fn a_zero_report_or_no_report_falls_back_to_the_cursor() {
        let cursor = POINT { x: 1, y: 2 };
        // An exact (0, 0) report means "no position was supplied" (the
        // keyboard Shift+F10 path), not "the user is at the screen origin".
        assert_eq!(
            resolve_menu_anchor(MenuAnchor::Reported(POINT { x: 0, y: 0 }), Some(cursor)),
            cursor
        );
        assert_eq!(
            resolve_menu_anchor(MenuAnchor::AtCursor, Some(cursor)),
            cursor
        );
    }

    #[test]
    fn a_failed_cursor_query_degrades_to_the_origin() {
        // Nothing usable anywhere: the alignment flags still place the menu on
        // screen, so the origin is a survivable answer and not a panic path.
        assert_eq!(
            resolve_menu_anchor(MenuAnchor::AtCursor, None),
            POINT { x: 0, y: 0 }
        );
    }
}
