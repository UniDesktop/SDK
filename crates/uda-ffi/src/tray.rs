//! Tray handle registry shared by the C exports.
//!
//! # Why a registry exists
//!
//! The same argument as [`crate::wakelocks`], one level deeper: a C caller
//! cannot hold an `Arc<TrayIcon>` or an `Arc<TrayMenu>`, so both live in a
//! process-wide table keyed by an opaque `uint64_t`. Handles start at `1` so
//! `0` can mean "no handle" in C, which is what makes a zeroed out-parameter
//! recognisable as "the call failed" rather than as a live resource.
//!
//! # Ownership model
//!
//! * **Tray icons** are owned exclusively by the registry. `uda_tray_destroy`
//!   removes the entry and drops the `Arc`, which runs `TrayIcon::drop` and
//!   unregisters the icon from the shell.
//! * **Menus** are shared records. `uda_tray_set_menu` clones the `Arc` into the
//!   icon, so a menu handle outliving its tray icon is valid, and dropping the
//!   menu handle after attaching it leaves the tray working. Both entries must
//!   eventually be destroyed, or the process exits and releases everything.
//!
//! # Callback threading model
//!
//! Every C callback registered through this module is invoked on the **tray
//! worker thread** that the platform backend owns, never on the thread that
//! called `uda_tray_menu_add_*`. The callback must therefore be cheap, must not
//! block, and must not touch host UI state directly - forward the event into the
//! host's own loop instead. This is the same contract `uda_core::tray` documents
//! for `TrayAction`, and it is stated again in `include/uda.h` because the C
//! caller is the one who has to honour it.
//!
//! # Panic containment
//!
//! The registry never unwraps. A lock is recovered from poisoning, a missing or
//! mis-typed handle is reported as `UDA_ERR_INVALID_ARGUMENT`, and an oversized
//! icon is rejected before a single byte is read from the caller's buffer.

use std::collections::HashMap;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use uda_core::tray::{
    MenuItem, TrayAction, TrayEvent, TrayIcon, TrayIconBuilder, TrayIconSource, TrayMenu,
};

use crate::dispatch;
use crate::error::Failure;

/// Raw C callback invoked when a plain text row is activated.
///
/// Arguments are the row's item id as returned by `uda_tray_menu_add_text` and
/// the `user_data` pointer registered alongside it.
pub type TextCallback = extern "C" fn(u64, *mut c_void);

/// Raw C callback invoked when a checkbox row is toggled.
///
/// Arguments are the row's item id, the **new** checked state (`0` or `1`) after
/// the toggle has been applied, and the `user_data` pointer.
pub type CheckboxCallback = extern "C" fn(u64, i32, *mut c_void);

/// A `*mut c_void` opaque payload that may cross a thread boundary.
///
/// `TrayAction` requires `FnMut + Send`, but the host's `user_data` pointer is
/// deliberately untyped and therefore not `Send`. Wrapping it in a newtype lets
/// the trampoline capture it honestly instead of pretending the requirement
/// does not exist.
struct SendVoidPtr(*mut c_void);

// SAFETY: this is only ever the host's own opaque `user_data` handle. The host
// hands it to UDA together with its callback and guarantees it stays valid for
// as long as the row lives, so relocating it to the tray worker thread - the
// only thread the backend is allowed to invoke the callback from - is the host's
// declared intent, not something UDA invented. Nothing in UDA dereferences it.
unsafe impl Send for SendVoidPtr {}

impl SendVoidPtr {
    /// Hand the host's pointer back.
    ///
    /// An accessor rather than reading `.0` at the call site on purpose: Rust
    /// 2021's disjoint closure captures would grab only the `*mut c_void` field
    /// and lose the `Send` wrapper, so the trampoline closures must go through a
    /// method to keep the whole struct inside the closure.
    fn raw(&self) -> *mut c_void {
        self.0
    }
}

/// One live record in the registry.
enum TrayEntry {
    /// A registered tray icon.
    Icon(Arc<TrayIcon>),
    /// A context menu, attached to zero or more icons.
    Menu(Arc<TrayMenu>),
}

/// Process-wide handle table for tray icons and menus.
///
/// Kept as a struct rather than free functions so the lookups share one lock
/// discipline and so a reader can see at a glance that handles, entries and the
/// counter are one component.
pub(crate) struct TrayRegistry {
    /// Handles to live records.
    entries: Mutex<HashMap<u64, TrayEntry>>,
    /// Source of handle values. Starts at 1 so `0` means "no handle" in C.
    counter: AtomicU64,
}

impl TrayRegistry {
    /// An empty registry.
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
        }
    }

    /// The process-wide instance.
    fn global() -> &'static Self {
        static REGISTRY: OnceLock<TrayRegistry> = OnceLock::new();
        REGISTRY.get_or_init(TrayRegistry::new)
    }

    /// Lock the table, recovering from a poisoned lock.
    ///
    /// A panic while an entry was being inserted must not make every later call
    /// fail: the remaining records are still valid and still destroyable, which
    /// is strictly better for a host that is shutting down.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<u64, TrayEntry>> {
        match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::debug!("tray registry lock was poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    /// Allocate a fresh handle and store `entry` under it.
    ///
    /// The counter is monotonic, so a live handle can never be reissued. The
    /// `while` guard is belt-and-braces for the (practically unreachable) case
    /// of the counter colliding with a record inserted from elsewhere.
    fn allocate(&self, entry: TrayEntry) -> u64 {
        let mut entries = self.lock();
        let handle = loop {
            let candidate = self.counter.fetch_add(1, Ordering::Relaxed);
            if candidate != 0 && !entries.contains_key(&candidate) {
                break candidate;
            }
        };
        entries.insert(handle, entry);
        handle
    }

    /// Borrow the icon behind `handle`.
    fn icon(&self, handle: u64) -> Result<Arc<TrayIcon>, Failure> {
        let entries = self.lock();
        match entries.get(&handle) {
            Some(TrayEntry::Icon(icon)) => Ok(Arc::clone(icon)),
            Some(TrayEntry::Menu(_)) => Err(Failure::InvalidArgument(format!(
                "tray handle {handle} is a menu, not a tray icon"
            ))),
            None => Err(Failure::InvalidArgument(format!(
                "tray handle {handle} is not live in this process"
            ))),
        }
    }

    /// Borrow the menu behind `handle`.
    fn menu(&self, handle: u64) -> Result<Arc<TrayMenu>, Failure> {
        let entries = self.lock();
        match entries.get(&handle) {
            Some(TrayEntry::Menu(menu)) => Ok(Arc::clone(menu)),
            Some(TrayEntry::Icon(_)) => Err(Failure::InvalidArgument(format!(
                "tray handle {handle} is a tray icon, not a menu"
            ))),
            None => Err(Failure::InvalidArgument(format!(
                "tray menu handle {handle} is not live in this process"
            ))),
        }
    }

    /// Remove the icon behind `handle`, returning its record.
    ///
    /// The entry leaves the table *before* its `Drop` runs, so `NIM_DELETE` and
    /// the worker shutdown never hold the registry lock - a stalled shell cannot
    /// block an unrelated `uda_tray_create` on another thread.
    fn remove_icon(&self, handle: u64) -> Result<Arc<TrayIcon>, Failure> {
        let entry = {
            let mut entries = self.lock();
            entries.remove(&handle)
        };
        match entry {
            Some(TrayEntry::Icon(icon)) => Ok(icon),
            // Put it back: the caller addressed a menu with an icon call, so the
            // record must not be lost to a typo.
            Some(TrayEntry::Menu(menu)) => {
                self.insert(handle, TrayEntry::Menu(menu));
                Err(Failure::InvalidArgument(format!(
                    "tray handle {handle} is a menu, not a tray icon"
                )))
            }
            None => Err(Failure::InvalidArgument(format!(
                "tray handle {handle} is not live in this process"
            ))),
        }
    }

    /// Remove the menu behind `handle`, returning its record.
    fn remove_menu(&self, handle: u64) -> Result<Arc<TrayMenu>, Failure> {
        let entry = {
            let mut entries = self.lock();
            entries.remove(&handle)
        };
        match entry {
            Some(TrayEntry::Menu(menu)) => Ok(menu),
            Some(TrayEntry::Icon(icon)) => {
                self.insert(handle, TrayEntry::Icon(icon));
                Err(Failure::InvalidArgument(format!(
                    "tray handle {handle} is a tray icon, not a menu"
                )))
            }
            None => Err(Failure::InvalidArgument(format!(
                "tray menu handle {handle} is not live in this process"
            ))),
        }
    }

    /// Re-insert a record after a mis-typed lookup was rejected.
    fn insert(&self, handle: u64, entry: TrayEntry) {
        self.lock().insert(handle, entry);
    }

    /// Number of live records, split by kind. Exposed for tests and diagnostics.
    #[cfg(test)]
    fn live_counts(&self) -> (usize, usize) {
        let entries = self.lock();
        let icons = entries
            .values()
            .filter(|entry| matches!(entry, TrayEntry::Icon(_)))
            .count();
        (icons, entries.len() - icons)
    }
}

/// Create a tray icon and return its registry handle.
pub(crate) fn create_icon(name: &str, tooltip: &str) -> Result<u64, Failure> {
    // An empty application name is harmless: `TrayIconBuilder` substitutes the
    // crate default, which is what a C caller passing `""` means.
    let mut builder = TrayIconBuilder::new();
    if !name.trim().is_empty() {
        builder = builder.name(name);
    }
    // The icon image is deliberately absent here; the C ABI exposes the icon as
    // its own setter so a host can start invisible and pick pixels later.
    let icon = dispatch::create_tray(builder.tooltip(tooltip).build())?;
    let handle = TrayRegistry::global().allocate(TrayEntry::Icon(Arc::new(icon)));
    log::debug!("created tray icon handle {handle} ({name:?})");
    Ok(handle)
}

/// Replace the icon's tooltip.
pub(crate) fn set_tooltip(handle: u64, tooltip: &str) -> Result<(), Failure> {
    let icon = TrayRegistry::global().icon(handle)?;
    icon.set_tooltip(tooltip);
    Ok(())
}

/// Replace the icon's image from a filesystem path or icon-theme name.
pub(crate) fn set_icon_path(handle: u64, path: &str) -> Result<(), Failure> {
    let icon = TrayRegistry::global().icon(handle)?;
    icon.set_icon(TrayIconSource::Path(path.to_string()))?;
    Ok(())
}

/// Replace the icon's image from raw RGBA pixels.
///
/// `len` bounds the caller's buffer; only `stride * height` bytes are copied, so
/// a pad at the end of the host's allocation is never read. Both the bound check
/// and the platform-side validation run before the OS sees the pixels, which is
/// what keeps a malformed icon a status code rather than a crash inside FFI.
pub(crate) fn set_icon_rgba(
    handle: u64,
    width: u32,
    height: u32,
    stride: u32,
    data: *const u8,
    len: usize,
) -> Result<(), Failure> {
    if data.is_null() {
        return Err(Failure::InvalidArgument("`data` must not be null".to_string()));
    }
    let needed = stride
        .checked_mul(height)
        .ok_or_else(|| Failure::InvalidArgument("icon stride * height overflows".to_string()))?
        as usize;
    if len < needed {
        return Err(Failure::InvalidArgument(format!(
            "icon data is shorter than stride * height: {len} < {needed} bytes"
        )));
    }

    // SAFETY: `data` is non-null and the caller guarantees `len` readable bytes
    // at that address; only the first `needed` of them are touched.
    let bytes = unsafe { std::slice::from_raw_parts(data, needed) }.to_vec();

    let icon = TrayRegistry::global().icon(handle)?;
    icon.set_icon(TrayIconSource::Rgba {
        width,
        height,
        stride,
        data: bytes,
    })?;
    Ok(())
}

/// Show or hide the icon without unregistering it.
pub(crate) fn set_visible(handle: u64, visible: bool) -> Result<(), Failure> {
    let icon = TrayRegistry::global().icon(handle)?;
    if visible {
        icon.show();
    } else {
        icon.hide();
    }
    Ok(())
}

/// Destroy a tray icon and unregister it from the shell.
pub(crate) fn destroy_icon(handle: u64) -> Result<(), Failure> {
    let icon = TrayRegistry::global().remove_icon(handle)?;
    // The `Arc` leaving scope here runs `TrayIcon::drop`, which is the whole
    // teardown: the backend's worker observes the shutdown flag, issues
    // `NIM_DELETE`, destroys its window and exits.
    drop(icon);
    log::debug!("destroyed tray icon handle {handle}");
    Ok(())
}

/// Create an empty context menu and return its registry handle.
pub(crate) fn create_menu() -> Result<u64, Failure> {
    let handle = TrayRegistry::global().allocate(TrayEntry::Menu(Arc::new(TrayMenu::new())));
    log::debug!("created tray menu handle {handle}");
    Ok(handle)
}

/// Append a plain text row, optionally with a callback.
///
/// Returns the row's item id, which the callback receives as its first argument
/// so a host can tell rows apart without keeping a table of its own.
pub(crate) fn menu_add_text(
    menu_handle: u64,
    label: &str,
    callback: Option<TextCallback>,
    user_data: *mut c_void,
) -> Result<u64, Failure> {
    let menu = TrayRegistry::global().menu(menu_handle)?;
    // Push first so the core model allocates the id: it is the authority on what
    // the shell will see, and duplicating that numbering here would let the two
    // disagree. The callback then needs a second lookup by label-free id, which
    // `entries()` gives back, so the trampoline is attached in a second step.
    menu.push(MenuItem::text(label))?;

    // The id of the row just appended: the menu assigns ids monotonically, so
    // the last entry is the one this call added.
    let item_id = menu
        .entries()
        .last()
        .map(|(id, _)| *id)
        .ok_or_else(|| Failure::InvalidArgument("the menu dropped the new row".to_string()))?;

    if let Some(callback) = callback {
        let payload = SendVoidPtr(user_data);
        // Re-attaching through `set_action` swaps the closure for every holder of
        // the row, which is exactly the documented core behaviour.
        menu.set_action(
            item_id,
            Some(TrayAction::new(move |_event: &TrayEvent| {
                callback(item_id.into_raw(), payload.raw());
            })),
        );
    }

    log::debug!("tray menu {menu_handle}: added text row {}", item_id.into_raw());
    Ok(item_id.into_raw())
}

/// Append a visual separator.
pub(crate) fn menu_add_separator(menu_handle: u64) -> Result<(), Failure> {
    let menu = TrayRegistry::global().menu(menu_handle)?;
    menu.push(MenuItem::separator())?;
    Ok(())
}

/// Append a checkbox row with a callback that receives the new checked state.
///
/// The model's checkbox value is inverted by the trampoline before the callback
/// runs, so the `checked` argument is the state the shell is about to render
/// rather than the one that was just clicked - which is what a host needs to
/// sync its own UI and to keep the menu's own value consistent.
pub(crate) fn menu_add_checkbox(
    menu_handle: u64,
    label: &str,
    checked: bool,
    callback: Option<CheckboxCallback>,
    user_data: *mut c_void,
) -> Result<u64, Failure> {
    let menu = TrayRegistry::global().menu(menu_handle)?;
    let item = if checked {
        MenuItem::checkbox_checked(label)
    } else {
        MenuItem::checkbox(label)
    };
    menu.push(item)?;

    // The id of the row just appended, for the same reason as in `menu_add_text`.
    let item_id = menu
        .entries()
        .last()
        .map(|(id, _)| *id)
        .ok_or_else(|| Failure::InvalidArgument("the menu dropped the new row".to_string()))?;

    let payload = SendVoidPtr(user_data);
    // The menu is cloned into the closure so the trampoline can invert the
    // model itself. Doing it here rather than in the backend keeps the two
    // platforms' behaviour identical: both report the *new* state, and both keep
    // the stored checkbox value in sync with what the shell renders next.
    let toggling = menu.clone();
    menu.set_action(
        item_id,
        Some(TrayAction::new(move |_event: &TrayEvent| {
            toggling.toggle(item_id);
            let next_checked = toggling
                .find(item_id)
                .map(|item| item.state().checked)
                .unwrap_or(false);
            if let Some(callback) = callback {
                callback(item_id.into_raw(), i32::from(next_checked), payload.raw());
            }
        })),
    );

    log::debug!(
        "tray menu {menu_handle}: added checkbox row {}",
        item_id.into_raw()
    );
    Ok(item_id.into_raw())
}

/// Attach a menu to a tray icon, replacing any previous menu.
pub(crate) fn set_menu(tray_handle: u64, menu_handle: u64) -> Result<(), Failure> {
    let icon = TrayRegistry::global().icon(tray_handle)?;
    let menu = TrayRegistry::global().menu(menu_handle)?;
    icon.set_menu(menu);
    Ok(())
}

/// Destroy a menu handle.
///
/// Safe to call after `uda_tray_set_menu`: the icon holds its own `Arc`, so the
/// tray keeps working with the last attached layout until the icon is destroyed.
pub(crate) fn destroy_menu(menu_handle: u64) -> Result<(), Failure> {
    let menu = TrayRegistry::global().remove_menu(menu_handle)?;
    let rows = menu.len();
    drop(menu);
    log::debug!("destroyed tray menu handle {menu_handle} ({rows} rows)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util;
    use std::os::raw::c_void;

    /// Serialises every test that inspects the shared registry.
    static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

    /// Hold the shared registry for the duration of a test.
    fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
        match REGISTRY_LOCK.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    /// Item id captured by a callback.
    static LAST_ITEM_ID: AtomicU64 = AtomicU64::new(0);
    /// How many times the text callback fired.
    static TEXT_FIRES: AtomicU64 = AtomicU64::new(0);
    /// The `user_data` pointer the text callback was handed.
    static TEXT_USER_DATA: AtomicU64 = AtomicU64::new(0);
    /// Checked state last seen by the checkbox callback.
    static CHECKBOX_STATE: AtomicU64 = AtomicU64::new(99);
    /// How many times the checkbox callback fired.
    static CHECKBOX_FIRES: AtomicU64 = AtomicU64::new(0);

    extern "C" fn text_callback(item_id: u64, user_data: *mut c_void) {
        LAST_ITEM_ID.store(item_id, Ordering::SeqCst);
        TEXT_FIRES.fetch_add(1, Ordering::SeqCst);
        TEXT_USER_DATA.store(user_data as usize as u64, Ordering::SeqCst);
    }

    extern "C" fn checkbox_callback(item_id: u64, checked: i32, _user_data: *mut c_void) {
        LAST_ITEM_ID.store(item_id, Ordering::SeqCst);
        CHECKBOX_STATE.store(checked as u64, Ordering::SeqCst);
        CHECKBOX_FIRES.fetch_add(1, Ordering::SeqCst);
    }

    /// The registry-level `menu_add_text` used by tests.
    fn registry_text(
        menu: u64,
        label: &str,
        callback: Option<TextCallback>,
        user_data: *mut c_void,
    ) -> Result<u64, Failure> {
        menu_add_text(menu, label, callback, user_data)
    }

    /// The registry-level `menu_add_checkbox` used by tests.
    fn registry_checkbox(
        menu: u64,
        label: &str,
        checked: bool,
        callback: Option<CheckboxCallback>,
        user_data: *mut c_void,
    ) -> Result<u64, Failure> {
        menu_add_checkbox(menu, label, checked, callback, user_data)
    }

    #[test]
    fn menu_handles_are_unique_and_non_zero() {
        let _guard = registry_guard();
        let first = create_menu().expect("first menu");
        let second = create_menu().expect("second menu");
        let third = create_menu().expect("third menu");

        assert_ne!(first, 0, "0 is the C ABI's invalid handle");
        assert_ne!(second, 0);
        assert_ne!(third, 0);
        assert_ne!(first, second);
        assert_ne!(first, third);
        assert_ne!(second, third);

        assert!(destroy_menu(first).is_ok());
        assert!(destroy_menu(second).is_ok());
        assert!(destroy_menu(third).is_ok());
    }

    #[test]
    fn destroying_an_unknown_icon_handle_is_an_error() {
        let _guard = registry_guard();
        let (icons, _menus) = TrayRegistry::global().live_counts();
        // No tray icon can be created without a shell; this value is therefore
        // always a handle this process never issued.
        let result = destroy_icon(u64::MAX - 1);
        assert!(result.is_err(), "an unknown handle must be rejected");
        let (after, _) = TrayRegistry::global().live_counts();
        assert_eq!(icons, after, "a failed destroy must not drop a record");
    }

    #[test]
    fn a_menu_handle_is_not_accepted_where_an_icon_is_expected() {
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");

        // Addressing a menu with an icon call must fail *and* leave the menu
        // alive, so a host that mixed up its handles can still clean up.
        assert!(set_tooltip(menu, "oops").is_err());
        assert!(set_visible(menu, false).is_err());
        assert!(destroy_icon(menu).is_err());

        let (_, menus) = TrayRegistry::global().live_counts();
        assert!(menus >= 1, "the mis-addressed menu must survive");
        assert!(destroy_menu(menu).is_ok());
    }

    #[test]
    fn set_menu_reports_a_handle_mix_up_without_losing_either_record() {
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");
        // A menu as both arguments is a mix-up: the first must be an icon. The
        // failed lookup must not silently drop the menu.
        assert!(set_menu(menu, menu).is_err());
        let (icons, menus) = TrayRegistry::global().live_counts();
        assert_eq!(icons, 0, "no icon could have been created here");
        assert!(menus >= 1);
        assert!(destroy_menu(menu).is_ok());
    }

    #[test]
    fn a_menu_handle_cannot_be_destroyed_twice() {
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");
        assert!(destroy_menu(menu).is_ok());
        assert!(
            destroy_menu(menu).is_err(),
            "a destroyed menu handle must be single-use"
        );
    }

    #[test]
    fn menu_rows_are_mapped_to_stable_non_zero_ids_in_order() {
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");

        let welcome_id = registry_text(menu, "welcome", None, std::ptr::null_mut())
            .expect("text row");
        let checkbox_id = registry_checkbox(menu, "dark mode", false, None, std::ptr::null_mut())
            .expect("checkbox row");
        assert!(menu_add_separator(menu).is_ok(), "separator");
        let quit_id =
            registry_text(menu, "quit", None, std::ptr::null_mut()).expect("text row");

        assert_ne!(welcome_id, 0);
        assert_ne!(checkbox_id, 0);
        assert_ne!(quit_id, 0);
        assert_ne!(welcome_id, checkbox_id);
        assert_ne!(welcome_id, quit_id);
        assert_ne!(checkbox_id, quit_id);

        let rows = TrayRegistry::global()
            .menu(menu)
            .expect("the menu is still live")
            .entries();
        assert_eq!(rows.len(), 4, "three rows and one separator");
        assert_eq!(rows[0].0.into_raw(), welcome_id);
        assert_eq!(rows[0].1.label(), Some("welcome"));
        assert_eq!(rows[1].0.into_raw(), checkbox_id);
        assert_eq!(rows[1].1.label(), Some("dark mode"));
        assert!(rows[1].1.is_checkbox());
        assert!(!rows[1].1.state().checked, "it starts unchecked");
        assert!(rows[2].1.is_separator());
        assert_eq!(rows[3].0.into_raw(), quit_id);
        assert_eq!(rows[3].1.label(), Some("quit"));

        assert!(destroy_menu(menu).is_ok());
    }

    #[test]
    fn a_text_row_callback_fires_with_the_row_id_and_user_data() {
        let _guard = registry_guard();
        TEXT_FIRES.store(0, Ordering::SeqCst);
        TEXT_USER_DATA.store(0, Ordering::SeqCst);
        let menu = create_menu().expect("menu");

        let mut marker: u64 = 0x5eed;
        let item_id = registry_text(
            menu,
            "ping",
            Some(text_callback),
            std::ptr::from_mut(&mut marker).cast(),
        )
        .expect("text row");

        let rows = TrayRegistry::global()
            .menu(menu)
            .expect("the menu is still live")
            .entries();
        // The trampoline is reachable through the row's action, which is how
        // every backend invokes it, so this exercises the real path.
        rows[0]
            .1
            .action()
            .expect("the row carries a callback")
            .invoke(&TrayEvent::Click);

        assert_eq!(TEXT_FIRES.load(Ordering::SeqCst), 1);
        assert_eq!(LAST_ITEM_ID.load(Ordering::SeqCst), item_id);
        assert_eq!(
            TEXT_USER_DATA.load(Ordering::SeqCst),
            &mut marker as *mut u64 as u64
        );

        assert!(destroy_menu(menu).is_ok());
    }

    #[test]
    fn a_checkbox_callback_reports_the_new_state() {
        let _guard = registry_guard();
        CHECKBOX_FIRES.store(0, Ordering::SeqCst);
        let menu = create_menu().expect("menu");

        let item_id = registry_checkbox(
            menu,
            "toggle me",
            false,
            Some(checkbox_callback),
            std::ptr::null_mut(),
        )
        .expect("checkbox row");

        for expected in [1u64, 0, 1] {
            let rows = TrayRegistry::global()
                .menu(menu)
                .expect("the menu is still live")
                .entries();
            rows[0]
                .1
                .action()
                .expect("the row carries a callback")
                .invoke(&TrayEvent::Click);
            // The model is inverted before the callback runs, so the value the
            // host sees is the state the shell will render next.
            assert_eq!(CHECKBOX_STATE.load(Ordering::SeqCst), expected);
        }
        assert_eq!(CHECKBOX_FIRES.load(Ordering::SeqCst), 3);
        assert_eq!(LAST_ITEM_ID.load(Ordering::SeqCst), item_id);

        // The point of the whole exercise: the menu's own value stayed in sync.
        let rows = TrayRegistry::global()
            .menu(menu)
            .expect("the menu is still live")
            .entries();
        assert!(rows[0].1.state().checked, "three toggles from unchecked");

        assert!(destroy_menu(menu).is_ok());
    }

    #[test]
    fn a_null_callback_yields_a_plain_row_rather_than_an_error() {
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");
        let item_id = registry_text(menu, "no callback", None, std::ptr::null_mut())
            .expect("a row without a callback is legal");
        assert_ne!(item_id, 0);

        let rows = TrayRegistry::global()
            .menu(menu)
            .expect("the menu is still live")
            .entries();
        assert_eq!(rows.len(), 1);
        // No callback means no trampoline is installed, so the row renders and
        // activates silently - which is exactly what a polling host wants.
        assert!(
            rows[0].1.action().is_none(),
            "a null callback must not install a trampoline"
        );
        // The row is still enabled, so it is clickable.
        assert!(!rows[0].1.is_disabled());
        assert!(destroy_menu(menu).is_ok());
    }

    #[test]
    fn a_menu_handle_cannot_receive_rows_after_destruction() {
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");
        assert!(destroy_menu(menu).is_ok());
        assert!(
            menu_add_separator(menu).is_err(),
            "a destroyed menu must reject further rows"
        );
    }

    #[test]
    fn an_empty_label_is_rejected_before_the_row_exists() {
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");
        let result = registry_text(menu, "   ", None, std::ptr::null_mut());
        assert!(result.is_err(), "an invisible row must not be created");
        let rows = TrayRegistry::global()
            .menu(menu)
            .expect("the menu is still live")
            .len();
        assert_eq!(rows, 0, "a rejected push leaves the menu untouched");
        assert!(destroy_menu(menu).is_ok());
    }

    #[test]
    fn a_typed_failure_leaves_a_message_for_the_caller() {
        let _guard = registry_guard();
        // Going through the public export, not the registry directly: it is
        // `catch_boundary` that writes the thread-local last-message slot, so a
        // C caller must observe a diagnosis after a failed call.
        let _ = util::take_last_message();
        let status = crate::uda_tray_menu_destroy(0);
        assert_eq!(status, crate::UDA_ERR_INVALID_ARGUMENT);
        assert!(
            util::take_last_message().is_some(),
            "a typed failure must leave a message for `uda_last_error_message`"
        );
    }

    #[test]
    fn every_rejection_carries_a_readable_diagnosis() {
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");

        // The registry's own error strings are what a host reads through
        // `uda_last_error_message()`. Checked on the `Failure` values rather
        // than on the thread-local slot: the slot is per-thread and these unit
        // tests run in parallel, so asserting on it from a shared helper could
        // observe another test's message instead of this one's.
        let rejections = [
            set_tooltip(menu, "x"),
            set_menu(menu, menu),
            destroy_icon(menu),
            set_visible(menu, false),
            destroy_menu(u64::MAX),
        ];

        for rejection in rejections {
            let failure = rejection.expect_err("a menu is not a tray icon");
            assert_eq!(failure.status(), crate::UDA_ERR_INVALID_ARGUMENT);
            assert!(
                !failure.message().is_empty(),
                "a rejection must explain itself, not just report a code"
            );
        }

        // None of those rejections may have consumed the menu.
        let (_, menus) = TrayRegistry::global().live_counts();
        assert!(menus >= 1, "a mis-addressed menu must survive every rejection");
        assert!(destroy_menu(menu).is_ok());
    }

    #[test]
    fn a_typo_between_handle_kinds_names_the_real_kind() {
        // The message must name which kind the handle actually is, so a host
        // debugging its own bookkeeping can tell "stale handle" from "wrong
        // handle kind" without guessing.
        let _guard = registry_guard();
        let menu = create_menu().expect("menu");

        let message = destroy_icon(menu)
            .expect_err("a menu is not an icon")
            .message();
        assert!(
            message.contains("menu"),
            "the diagnosis must name the real kind: {message}"
        );
        assert!(destroy_menu(menu).is_ok());
    }
}
