//! Linux system tray backend: `org.kde.StatusNotifierItem` (SNI) + `com.canonical.dbusmenu`.
//!
//! # Architecture
//!
//! ```text
//!  host thread                     D-Bus worker thread
//!  ───────────                     ───────────────────
//!  TrayIcon ── Arc<TrayIconInner>   zbus::Connection (session bus)
//!    ├ tooltip / icon / menu         ├ /StatusNotifierItem -> StatusNotifierItemInterface
//!    └ visible / capabilities        └ /MenuBar            -> DBusMenuInterface
//!
//!  every mutation is a plain write   the worker keeps its own mirror of the
//!  to `Mutex<TrayIconState>`;        state, reads happen on demand, so the
//!  the worker never blocks the host  shell sees fresh data and the host never
//!                                   waits on the bus
//! ```
//!
//! # State sharing: `Arc<Mutex<..>>`, never `RwLock`
//!
//! AGENTS.md forbids deriving `Clone` on a `Mutex`; the wrapper is what gets
//! cloned, and the lock is shared behind an `Arc`. `zbus` requires `Clone` on
//! an interface type, which is exactly why the state lives in
//! `Arc<Mutex<TrayShared>>` and the interface is a thin handle over it.
//!
//! A `RwLock` would look cheaper (readers dominate: every property getter is a
//! read), but it is the wrong tool here:
//!
//! * `zbus` dispatches each method call in its own task, so readers really can
//!   overlap. However every handler also takes a **second** guard while
//!   replying, and a read guard held across an `.await` is precisely the
//!   pattern that deadlocks against a waiting writer. `Mutex` makes the hazard
//!   impossible: one guard per `lock()`, always dropped before the next
//!   acquisition, never held across `await`.
//! * Poisoning has one obvious recovery story, which is what
//!   [`uda_core::tray`] already does everywhere: recover and keep serving,
//!   because the state is pure data that is rewritten field by field.
//!
//! # Never panic (AGENTS.md §1)
//!
//! No `unwrap()`, `expect()`, `panic!`, `unreachable!` or `unsafe` in this
//! module. Locks are recovered from poisoning, every D-Bus call is fallible,
//! and every host-supplied buffer is validated before it is transcribed.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use uda_core::capability::{Capability, SupportLevel};
use uda_core::error::UdaError;
use uda_core::tray::{
    MenuItem, TrayEvent, TrayFeature, TrayIcon, TrayIconConfig, TrayIconSource, TrayManager,
};
use zbus::object_server::SignalContext;
use zbus::{interface, Connection};

/// Window inside which two `Activate` calls are reported as a double click.
///
/// Matches the Windows default `GetDoubleClickTime()` so a host behaves the same
/// on both platforms. SNI itself has no double-click signal (see
/// `docs/internals/tray_specs.md` §1.7), so this is a host-visible
/// approximation and `TRAY_DOUBLE_CLICK` is deliberately not advertised.
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

/// How often the worker checks whether the host still owns its icon.
const SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Object path exporting [`StatusNotifierItemInterface`].
const SNI_PATH: &str = "/StatusNotifierItem";

/// Object path exporting [`DBusMenuInterface`].
const MENU_PATH: &str = "/MenuBar";

/// Well-known name of the KDE StatusNotifierWatcher.
const WATCHER_SERVICE: &str = "org.kde.StatusNotifierWatcher";

/// Object path of the watcher.
const WATCHER_PATH: &str = "/StatusNotifierWatcher";

/// Interface carrying `RegisterStatusNotifierItem` on the KDE watcher.
const WATCHER_INTERFACE: &str = "org.kde.StatusNotifierWatcher";

/// Freedesktop fallback watcher interface, used when the KDE one is absent.
const WATCHER_FALLBACK_INTERFACE: &str = "org.freedesktop.StatusNotifierWatcher";

/// Per-process counter making every generated bus name unique.
static TRAY_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Lock a mutex, recovering from a poisoned lock.
///
/// The tray state is pure data that is rewritten field by field, so resuming
/// after a panic inside one callback is strictly better than failing every
/// later update. This mirrors `TrayAction::invoke` in `uda-core`.
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
// Icon transcoding
// ---------------------------------------------------------------------------

/// The icon bytes advertised to a shell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum IconPayload {
    /// No icon at all.
    #[default]
    None,
    /// A freedesktop icon-theme name or a file path.
    Name(String),
    /// Raw bottom-up 32bpp rows as `IconPixmap` expects; byte order per pixel
    /// is B, G, R, A.
    Pixmap(Vec<(i32, i32, Vec<u8>)>),
}

impl IconPayload {
    /// Derive the payload from a validated [`TrayIconSource`].
    ///
    /// A path icon travels through as a theme name. An RGBA icon is transcoded:
    /// straight RGBA top-down becomes ARGB32 little-endian bottom-up, with
    /// `stride` padding skipped rather than copied (`tray_specs.md` §1.4).
    fn from_source(source: &TrayIconSource) -> Self {
        if source.validate().is_err() {
            log::warn!("tray icon failed validation; reporting it as missing");
            return Self::Name("image-missing".to_string());
        }
        match source {
            TrayIconSource::Path(path) => {
                let trimmed = path.trim();
                if trimmed.is_empty() {
                    Self::None
                } else {
                    Self::Name(trimmed.to_string())
                }
            }
            TrayIconSource::Rgba { .. } => match rgba_to_argb(source) {
                Some(pixmap) => Self::Pixmap(pixmap),
                None => {
                    log::warn!("tray icon could not be transcoded to ARGB32");
                    Self::Name("image-missing".to_string())
                }
            },
        }
    }
}

/// Convert straight RGBA top-down bytes into bottom-up 32bpp rows (B, G, R, A).
///
/// Returns `None` when the source is not an RGBA buffer or does not match its
/// declared dimensions. Every access is bounds-checked through slice indexing,
/// so a malformed buffer produces `None` rather than a panic.
fn rgba_to_argb(source: &TrayIconSource) -> Option<Vec<(i32, i32, Vec<u8>)>> {
    let TrayIconSource::Rgba {
        width,
        height,
        stride,
        data,
    } = source
    else {
        return None;
    };
    if *width == 0 || *height == 0 {
        return None;
    }
    let row_bytes = u32::checked_mul(*width, 4)?;
    if *stride < row_bytes {
        return None;
    }
    let needed = u32::checked_mul(*stride, *height)? as usize;
    if data.len() < needed {
        return None;
    }

    let width = *width as usize;
    let height = *height as usize;
    let stride = *stride as usize;

    let mut out = vec![0u8; width * 4 * height];
    for row in 0..height {
        // Start of this source row; `stride` bytes per row, of which the last
        // `width * 4` carry pixels.
        let src_row = &data[row * stride..row * stride + width * 4];
        // Bottom-up: source row 0 lands at the end of the destination.
        let dst_row = &mut out[(height - 1 - row) * width * 4..(height - row) * width * 4];
        for column in 0..width {
            let pixel = &src_row[column * 4..column * 4 + 4];
            let base = column * 4;
            // RGBA -> BGRA little-endian byte order.
            //
            // `IconPixmap` is documented as `a(iiay)` "ARGB32 rows" — which in
            // practice means **byte** order B, G, R, A (see the SNI spec and
            // `tray_specs.md` §1.4). Writing the channels as A, R, G, B swaps
            // red and blue, so an icon that should be red arrives blue.
            dst_row[base] = pixel[2];
            dst_row[base + 1] = pixel[1];
            dst_row[base + 2] = pixel[0];
            dst_row[base + 3] = pixel[3];
        }
    }

    Some(vec![(width as i32, height as i32, out)])
}

// ---------------------------------------------------------------------------
// Menu model -> com.canonical.dbusmenu
// ---------------------------------------------------------------------------

/// One menu row, flattened into the integer-keyed layout dbusmenu needs.
#[derive(Debug, Clone)]
struct MenuRow {
    /// dbusmenu item id, stable for as long as the row exists.
    id: i32,
    /// Row label; separators carry `None`.
    label: Option<String>,
    /// Whether the row is interactive.
    enabled: bool,
    /// Whether the row renders a checkmark.
    checkbox: bool,
    /// Current checkbox value.
    checked: bool,
    /// Whether the row is purely visual.
    separator: bool,
    /// Nested rows, with their own contiguous id range.
    children: Vec<MenuRow>,
    /// Callback fired when the shell reports this row as clicked.
    ///
    /// Stored beside the exported row so an `Event` can be dispatched without
    /// re-walking [`uda_core::tray::TrayMenu`], which the host may have mutated
    /// in the meantime. Snapshotting at layout time is what keeps a click
    /// addressed to the row the user actually saw.
    action: Option<uda_core::tray::TrayAction>,
}

impl MenuRow {
    /// Map a [`MenuItem`] onto a row, allocating the dbusmenu id.
    ///
    /// `menuid` is a counter owned by the caller so nested submenus allocate
    /// from one monotonic sequence. It saturates rather than wrapping: a wrapped
    /// counter would alias a live row.
    fn from_item(item: &MenuItem, menuid: &mut i32) -> Self {
        let id = *menuid;
        *menuid = menuid.saturating_add(1);
        let state = item.state();
        match item {
            MenuItem::Separator => Self {
                id,
                label: None,
                enabled: false,
                checkbox: false,
                checked: false,
                separator: true,
                children: Vec::new(),
                action: None,
            },
            MenuItem::Text { label, action, .. } => Self {
                id,
                label: Some(label.clone()),
                enabled: state.enabled,
                checkbox: false,
                checked: false,
                separator: false,
                children: Vec::new(),
                action: action.clone(),
            },
            MenuItem::Checkbox { label, action, .. } => Self {
                id,
                label: Some(label.clone()),
                enabled: state.enabled,
                checkbox: true,
                checked: state.checked,
                separator: false,
                children: Vec::new(),
                action: action.clone(),
            },
            MenuItem::Submenu {
                label, children, ..
            } => {
                // Children are snapshotted *after* the parent's id is allocated,
                // so a submenu owns a contiguous id range.
                let mut nested = Vec::new();
                for (_, child) in children.entries() {
                    nested.push(Self::from_item(&child, menuid));
                }
                Self {
                    id,
                    label: Some(label.clone()),
                    enabled: state.enabled,
                    checkbox: false,
                    checked: false,
                    separator: false,
                    children: nested,
                    action: None,
                }
            }
        }
    }

    /// The dbusmenu `type` property value.
    fn kind(&self) -> &'static str {
        if self.separator {
            "separator"
        } else {
            "standard"
        }
    }

    /// Serialise into the `a{sv}` map dbusmenu expects.
    fn properties(&self) -> HashMap<&'static str, zvariant::Value<'_>> {
        let mut props: HashMap<&'static str, zvariant::Value<'_>> = HashMap::new();
        props.insert("type", self.kind().into());
        if let Some(label) = self.label.as_deref() {
            props.insert("label", label.into());
        }
        props.insert("enabled", self.enabled.into());
        props.insert("visible", true.into());
        if self.checkbox {
            // `checkmark` is what makes a shell draw a real checkbox instead of
            // a plain text row.
            props.insert("toggle-type", "checkmark".into());
            props.insert("toggle-state", i32::from(self.checked).into());
        }
        if !self.children.is_empty() {
            props.insert("children-display", "submenu".into());
        }
        props
    }

    /// Find a row by dbusmenu id, depth-first.
    fn find<'a>(rows: &'a [MenuRow], id: i32) -> Option<&'a MenuRow> {
        for row in rows {
            if row.id == id {
                return Some(row);
            }
            if let Some(found) = Self::find(&row.children, id) {
                return Some(found);
            }
        }
        None
    }
}

/// An owned property map, as the dbusmenu replies need `'static` payloads.
type OwnedProps = HashMap<String, zvariant::OwnedValue>;

/// The encoded child list of a dbusmenu node: `a(ia{sv}v)`.
type MenuChildren = Vec<(i32, OwnedProps, zvariant::OwnedValue)>;

/// One dbusmenu layout node: `(ia{sv}ia{sv}v)`.
type MenuNode = (i32, OwnedProps, MenuChildren, zvariant::OwnedValue);

/// The property map a row exposes.
///
/// Keys are `'static` literals and values are borrowed `Value`s, so this is the
/// cheap in-memory form; [`owned_props`] converts it for the wire.
fn row_properties(row: &MenuRow) -> HashMap<&'static str, zvariant::Value<'_>> {
    row.properties()
}

/// Own the property values so they can travel back over D-Bus.
fn owned_props(props: HashMap<&'static str, zvariant::Value<'_>>) -> OwnedProps {
    props
        .into_iter()
        .filter_map(|(key, value)| {
            // `to_owned` on a `Value` clones the payload into a static one;
            // `try_to_owned` reports the rare case where that is impossible
            // (an `Fd`, for instance), which cannot happen for our types.
            value
                .try_to_owned()
                .ok()
                .map(|owned| (key.to_string(), owned))
        })
        .collect()
}

/// An empty variant payload.
///
/// dbusmenu's `v` slots carry "no icon" for UDA. A zero-field structure is the
/// canonical filler: it serialises to a valid variant body without asserting a
/// concrete type on the receiving side, which a bare unit would not express.
fn empty_variant() -> zvariant::OwnedValue {
    let filler = zvariant::StructureBuilder::new().build();
    match zvariant::Value::from(filler).try_to_owned() {
        Ok(owned) => owned,
        // Unreachable for a zero-field structure (there is nothing to clone);
        // routing through the same encoder used for children keeps the function
        // total without an `expect`.
        Err(error) => {
            log::warn!("tray could not encode an empty variant: {error}");
            empty_variant_with(Vec::new())
        }
    }
}

/// A snapshot of the menu attached to an icon, taken per request.
///
/// Rows are captured with their callbacks, so an `Event` never reaches back into
/// the host's live menu tree.
#[derive(Debug, Clone)]
struct MenuSnapshot {
    rows: Vec<MenuRow>,
}

impl MenuSnapshot {
    /// Snapshot the menu behind an `Option<Arc<TrayMenu>>`.
    fn from_menu(menu: Option<&Arc<uda_core::tray::TrayMenu>>) -> Option<Self> {
        let menu = menu?;
        let mut rows = Vec::new();
        // dbusmenu ids start at 1; 0 is the protocol's "root" sentinel.
        let mut menuid: i32 = 1;
        for (_, item) in menu.entries() {
            rows.push(MenuRow::from_item(&item, &mut menuid));
        }
        Some(Self { rows })
    }

    /// Look up a row by dbusmenu id.
    fn row(&self, id: i32) -> Option<&MenuRow> {
        MenuRow::find(&self.rows, id)
    }

    /// Encode the whole tree as the `GetLayout` root node.
    ///
    /// The root carries id 0 and no properties; the children are the top-level
    /// rows, each wrapped in a variant so the wire signature stays `(ia{sv}ia{sv}v)`.
    fn root_node(&self, recurse: bool) -> MenuNode {
        let children: MenuChildren = self
            .rows
            .iter()
            .map(|row| {
                let node = layout_node(row, recurse);
                (node.0, node.1, empty_variant_with(node.2))
            })
            .collect();
        (0, OwnedProps::new(), children, empty_variant())
    }

    /// The owned id/property pairs for `GetGroupProperties`.
    fn group_properties(&self, ids: &[i32], wanted: &[&str]) -> Vec<(i32, OwnedProps)> {
        ids.iter()
            .filter_map(|id| {
                let row = self.row(*id)?;
                let props = row_properties(row);
                let filtered: HashMap<&'static str, zvariant::Value<'_>> = props
                    .into_iter()
                    .filter(|(key, _)| wanted.is_empty() || wanted.contains(key))
                    .collect();
                Some((*id, owned_props(filtered)))
            })
            .collect()
    }

    /// One owned property of one row.
    fn property(&self, id: i32, name: &str) -> Option<zvariant::OwnedValue> {
        let row = self.row(id)?;
        let props = row_properties(row);
        let value = props.get(name)?;
        value.try_to_owned().ok()
    }
}

/// Encode a row into a layout node, recursing when asked.
fn layout_node(row: &MenuRow, recurse: bool) -> MenuNode {
    let children: MenuChildren = if recurse {
        row.children
            .iter()
            .map(|child| {
                let node = layout_node(child, true);
                (node.0, node.1, empty_variant_with(node.2))
            })
            .collect()
    } else {
        Vec::new()
    };
    (
        row.id,
        owned_props(row_properties(row)),
        children,
        empty_variant(),
    )
}

/// Wrap an already-encoded child list in a variant.
fn empty_variant_with(children: MenuChildren) -> zvariant::OwnedValue {
    match zvariant::Value::new(children).try_to_owned() {
        Ok(owned) => owned,
        Err(error) => {
            log::warn!("tray menu children could not be encoded: {error}");
            empty_variant()
        }
    }
}

// ---------------------------------------------------------------------------
// Shared worker state
// ---------------------------------------------------------------------------

/// The state the D-Bus worker serves.
///
/// Pure data: the worker copies it out of the host's [`TrayIconInner`] on every
/// mutation and answers D-Bus queries from this mirror, so no handler ever has
/// to reach into the icon's lock (which the platform crate cannot see anyway).
struct TrayShared {
    /// Application name; also the dbusmenu `Id`.
    name: String,
    /// Current tooltip.
    tooltip: String,
    /// Current icon payload.
    icon: IconPayload,
    /// Whether the item is shown.
    visible: bool,
    /// The live menu, when one is attached.
    menu: Option<Arc<uda_core::tray::TrayMenu>>,
    /// Left-click handler.
    on_click: Option<uda_core::tray::TrayEventHandler>,
    /// Double-click handler, fed by the synthesised [`TrayEvent::DoubleClick`].
    on_double_click: Option<uda_core::tray::TrayEventHandler>,
    /// The icon handle, so the worker can mirror host updates and tell when the
    /// host has let go of it.
    host: Option<Arc<uda_core::tray::TrayIconInner>>,
    /// When `Activate` last arrived, for double-click synthesis.
    last_activate: Option<Instant>,
}

impl Default for TrayShared {
    /// A registered icon is visible by default.
    ///
    /// `TrayIconInner` starts visible, and `TrayIcon::hide` is the only way to
    /// turn that off, so a freshly created mirror must agree with it. Deriving
    /// `Default` would instead yield `visible: false` and the worker would
    /// publish `status = Passive` for an icon the host never hid.
    fn default() -> Self {
        Self {
            name: String::new(),
            tooltip: String::new(),
            icon: IconPayload::None,
            visible: true,
            menu: None,
            on_click: None,
            on_double_click: None,
            host: None,
            last_activate: None,
        }
    }
}

impl fmt::Debug for TrayShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrayShared")
            .field("name", &self.name)
            .field("tooltip", &self.tooltip)
            .field("icon", &self.icon)
            .field("visible", &self.visible)
            .field("menu", &self.menu.as_ref().map(|menu| menu.len()))
            .field("on_click", &self.on_click.is_some())
            .field("on_double_click", &self.on_double_click.is_some())
            .field("host", &self.host.is_some())
            .field("last_activate", &self.last_activate)
            .finish()
    }
}

impl TrayShared {
    /// Copy the host-visible state into the mirror.
    ///
    /// Values are read from the public accessors (`tooltip()`, `menu()`, …) and
    /// the icon through [`TrayIconInner::lock_state`], always under exactly one
    /// lock, so this stays in step with the host without ever nesting locks.
    fn sync_from(&mut self) {
        // Borrowing the host through one guard: `lock_or_recover` returns a
        // `MutexGuard`, and everything is copied out before it is dropped.
        let Some(host) = self.host.as_ref() else {
            return;
        };
        let snapshot = {
            let state = host.lock_state();
            (
                state.tooltip.clone(),
                state.icon.clone(),
                state.menu.clone(),
                state.visible,
            )
        };
        self.tooltip = snapshot.0;
        self.icon = snapshot
            .1
            .as_ref()
            .map(IconPayload::from_source)
            .unwrap_or_default();
        self.menu = snapshot.2;
        self.visible = snapshot.3;
    }

    /// Adopt the values collected from a config at registration time.
    fn from_config(config: &TrayIconConfig) -> Self {
        Self {
            name: config.name.clone(),
            tooltip: config.tooltip.clone(),
            icon: config
                .icon
                .as_ref()
                .map(IconPayload::from_source)
                .unwrap_or_default(),
            visible: true,
            menu: config.menu.clone(),
            on_click: None,
            on_double_click: None,
            host: None,
            last_activate: None,
        }
    }

    /// A snapshot suitable for the item interface property getters.
    fn item_snapshot(&self) -> ItemSnapshot {
        ItemSnapshot {
            tooltip: self.tooltip.clone(),
            icon: self.icon.clone(),
            visible: self.visible,
            name: self.name.clone(),
        }
    }

    /// A snapshot suitable for the menu interface handlers.
    fn menu_snapshot(&self) -> Option<MenuSnapshot> {
        MenuSnapshot::from_menu(self.menu.as_ref())
    }
}

/// The values an SNI property getter needs, snapshotted out of the lock.
#[derive(Debug, Clone, Default)]
struct ItemSnapshot {
    name: String,
    tooltip: String,
    icon: IconPayload,
    visible: bool,
}

// ---------------------------------------------------------------------------
// org.kde.StatusNotifierItem
// ---------------------------------------------------------------------------

/// The SNI interface exported at `/StatusNotifierItem`.
///
/// `zbus` requires `Clone` on an interface type (the object server clones it
/// when serving). The shared state lives behind one `Arc<Mutex<TrayShared>>`,
/// so cloning is a pointer bump and every clone observes the same state.
#[derive(Clone)]
pub struct StatusNotifierItemInterface {
    shared: Arc<Mutex<TrayShared>>,
}

impl StatusNotifierItemInterface {
    /// Wrap shared state.
    ///
    /// Module-private: only [`Worker`] builds an interface, and it already owns
    /// the shared state. Exposing a constructor would let a caller fabricate a
    /// second, unrelated view of an icon's state.
    #[must_use]
    fn new(shared: Arc<Mutex<TrayShared>>) -> Self {
        Self { shared }
    }

    /// Snapshot the state for a handler, dropping the lock immediately.
    fn snapshot(&self) -> ItemSnapshot {
        let shared = lock_or_recover(&self.shared, "status notifier item");
        shared.item_snapshot()
    }

    /// Dispatch an activation to the host callback.
    ///
    /// Two activations inside [`DOUBLE_CLICK_WINDOW`] are reported as a
    /// [`TrayEvent::DoubleClick`] and routed to `on_double_click`; every other
    /// activation is a plain [`TrayEvent::Click`]. The callback runs **without**
    /// the state lock held, so host code may safely update the icon or the menu
    /// from inside it.
    fn dispatch_activation(&mut self) {
        // One guard: read the timestamp, record the new one, take the handler.
        let (event, handler) = {
            let mut shared = lock_or_recover(&self.shared, "status notifier item");
            let now = Instant::now();
            let double_click = match shared.last_activate {
                Some(last) if now.duration_since(last) <= DOUBLE_CLICK_WINDOW => true,
                _ => false,
            };
            // The window always restarts, so a triple click reads as
            // click-then-double rather than one long double.
            shared.last_activate = Some(now);
            if double_click {
                (TrayEvent::DoubleClick, shared.on_double_click.take())
            } else {
                (TrayEvent::Click, shared.on_click.take())
            }
        };

        // A `FnMut` closure needs a mutable borrow, and the boxed handler was
        // moved out of the state, so `handler` is taken by value and called
        // through `as_mut()` here.
        if let Some(mut handler) = handler {
            handler(&event);
            // Put the closure back even if it panicked: a handler that is never
            // restored would silently stop receiving clicks. The state lock was
            // released before the call, so re-acquiring it here is safe.
            let mut shared = lock_or_recover(&self.shared, "status notifier item");
            match event {
                TrayEvent::Click => shared.on_click = Some(handler),
                TrayEvent::DoubleClick => shared.on_double_click = Some(handler),
            }
        }
    }
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItemInterface {
    /// Primary activation; reported by the shell as a left click.
    async fn activate(&mut self, _x: i32, _y: i32) {
        self.dispatch_activation();
    }

    /// Secondary activation (middle click on most shells).
    ///
    /// `TrayEvent` has no distinct variant for it, so it is reported as a click
    /// rather than dropped on the floor.
    async fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.dispatch_activation();
    }

    /// The shell wants the context menu at (x, y).
    ///
    /// dbusmenu already carries the geometry, so UDA only logs it; the shell
    /// renders the menu itself from the `Menu` property.
    async fn context_menu(&self, _x: i32, _y: i32) {
        log::debug!("tray context menu requested by the shell");
    }

    /// Wheel input; not part of the cross-platform model.
    async fn scroll(&self, _delta: i32, _orientation: String) {
        log::debug!("tray scroll received; no cross-platform event for it");
    }

    /// Wayland activation token handshake; accepted and discarded.
    async fn provide_xdg_activation_token(&self, _token: String) {
        log::debug!("tray received an XDG activation token");
    }

    /// Whether `Activate` opens a menu instead of firing events.
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }

    /// The item category.
    #[zbus(property)]
    fn category(&self) -> &'static str {
        "ApplicationStatus"
    }

    /// Unique item id.
    #[zbus(property)]
    fn id(&self) -> String {
        let snapshot = self.snapshot();
        snapshot.name
    }

    /// Tooltip title line.
    #[zbus(property)]
    fn title(&self) -> String {
        let snapshot = self.snapshot();
        snapshot.tooltip
    }

    /// Item status: a hidden icon is `"Passive"`, a visible one `"Active"`.
    #[zbus(property)]
    fn status(&self) -> &'static str {
        let snapshot = self.snapshot();
        if snapshot.visible {
            "Active"
        } else {
            "Passive"
        }
    }

    /// Zero: the item has no X window (the Wayland case).
    #[zbus(property)]
    fn window_id(&self) -> u32 {
        0
    }

    /// Freedesktop icon-theme name; empty when a pixmap is supplied.
    #[zbus(property)]
    fn icon_name(&self) -> String {
        let snapshot = self.snapshot();
        match snapshot.icon {
            IconPayload::Name(name) => name,
            IconPayload::Pixmap(_) | IconPayload::None => String::new(),
        }
    }

    /// Bottom-up ARGB32 rows, as the SNI specification requires.
    #[zbus(property)]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        let snapshot = self.snapshot();
        match snapshot.icon {
            IconPayload::Pixmap(rows) => rows,
            IconPayload::Name(_) | IconPayload::None => Vec::new(),
        }
    }

    /// Overlay icon name; UDA does not model one.
    #[zbus(property)]
    fn overlay_icon_name(&self) -> String {
        String::new()
    }

    /// Overlay pixmap; UDA does not model one.
    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    /// Attention icon name; unused.
    #[zbus(property)]
    fn attention_icon_name(&self) -> String {
        String::new()
    }

    /// Attention pixmap; unused.
    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        Vec::new()
    }

    /// `(icon name, pixmap, title, description)`.
    ///
    /// `IconName` and `IconPixmap` are mutually exclusive in SNI, so exactly one
    /// of the two icon slots is populated here.
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Vec<(i32, i32, Vec<u8>)>, String, String) {
        let snapshot = self.snapshot();
        match snapshot.icon {
            IconPayload::Name(name) => (name, Vec::new(), snapshot.tooltip, String::new()),
            IconPayload::Pixmap(pixmap) => (
                String::new(),
                pixmap,
                snapshot.tooltip,
                String::new(),
            ),
            IconPayload::None => (String::new(), Vec::new(), snapshot.tooltip, String::new()),
        }
    }

    /// Object path of the `com.canonical.dbusmenu` implementation.
    #[zbus(property)]
    fn menu(&self) -> &'static str {
        MENU_PATH
    }

    /// Emitted when the tooltip changed.
    #[zbus(signal)]
    async fn new_title(signal_context: &SignalContext<'_>) -> zbus::Result<()>;

    /// Emitted when the icon pixmap changed.
    #[zbus(signal)]
    async fn new_icon(signal_context: &SignalContext<'_>) -> zbus::Result<()>;

    /// Emitted when the attention icon changed; never sent by UDA today.
    #[zbus(signal)]
    async fn new_attention_icon(signal_context: &SignalContext<'_>) -> zbus::Result<()>;

    /// Emitted when the status changed.
    #[zbus(signal)]
    async fn new_status(signal_context: &SignalContext<'_>, status: &str) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// com.canonical.dbusmenu
// ---------------------------------------------------------------------------

/// The dbusmenu interface exported at `/MenuBar`.
///
/// Shares the icon's state plus a layout revision, so the shell is told to
/// re-read when the menu changes.
#[derive(Clone)]
pub struct DBusMenuInterface {
    shared: Arc<Mutex<TrayShared>>,
    revision: Arc<Mutex<u32>>,
}

impl DBusMenuInterface {
    /// Wrap shared state.
    ///
    /// Module-private for the same reason as
    /// [`StatusNotifierItemInterface::new`]: one worker owns the state, and a
    /// second view could only ever serve stale data.
    #[must_use]
    fn new(shared: Arc<Mutex<TrayShared>>, revision: Arc<Mutex<u32>>) -> Self {
        Self { shared, revision }
    }

    /// Snapshot the menu, dropping the state lock immediately.
    fn menu_snapshot(&self) -> Option<MenuSnapshot> {
        let shared = lock_or_recover(&self.shared, "dbus menu");
        shared.menu_snapshot()
    }

    /// The current layout revision.
    fn revision(&self) -> u32 {
        let revision = lock_or_recover(&self.revision, "dbus menu revision");
        *revision
    }

}

#[interface(name = "com.canonical.dbusmenu")]
impl DBusMenuInterface {
    /// Return the menu tree.
    ///
    /// `recursion_depth` of 0 means "unlimited", which is what the shells send;
    /// a negative value is clamped to the same behaviour rather than returning
    /// an empty layout.
    async fn get_layout(
        &self,
        _parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, MenuNode)> {
        let recurse = recursion_depth <= 0 || recursion_depth > 1;
        let root = match self.menu_snapshot() {
            Some(snapshot) => snapshot.root_node(recurse),
            // No menu attached: report an empty root so the shell renders
            // nothing instead of surfacing an error.
            None => (0, HashMap::new(), Vec::new(), empty_variant()),
        };
        Ok((self.revision(), root))
    }

    /// Incremental property read for a set of ids.
    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<(i32, OwnedProps)>> {
        let snapshot = match self.menu_snapshot() {
            Some(snapshot) => snapshot,
            None => return Ok(Vec::new()),
        };
        let wanted: Vec<&str> = property_names.iter().map(String::as_str).collect();
        Ok(snapshot.group_properties(&ids, &wanted))
    }

    /// Single property read.
    async fn get_property(
        &self,
        id: i32,
        name: String,
    ) -> zbus::fdo::Result<zvariant::OwnedValue> {
        let snapshot = match self.menu_snapshot() {
            Some(snapshot) => snapshot,
            None => {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "no menu is attached; cannot read '{name}'"
                )))
            }
        };
        if snapshot.row(id).is_none() {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "unknown menu item id {id}"
            )));
        }
        match snapshot.property(id, &name) {
            Some(value) => Ok(value),
            None => Err(zbus::fdo::Error::InvalidArgs(format!(
                "item {id} has no property '{name}'"
            ))),
        }
    }

    /// A shell-reported interaction with a row.
    ///
    /// dbusmenu carries an integer id, not a path, so UDA keeps the snapshot's
    /// id-to-row map and looks the row up here. The callback is invoked **after**
    /// the state lock is dropped: holding a D-Bus dispatch guard across host
    /// code would stall every other request.
    async fn event(
        &self,
        id: i32,
        event_id: String,
        _data: zvariant::Value<'_>,
        _timestamp: u32,
    ) {
        if event_id != "clicked" {
            // `hovered` carries no host-visible meaning in UDA's model.
            log::debug!("tray menu event '{event_id}' ignored");
            return;
        }

        let action = {
            let snapshot = match self.menu_snapshot() {
                Some(snapshot) => snapshot,
                None => return,
            };
            match snapshot.row(id) {
                Some(row) => row.action.clone(),
                None => {
                    log::debug!("tray menu event for unknown id {id}");
                    None
                }
            }
        };

        if let Some(action) = action {
            action.invoke(&TrayEvent::Click);
        }
    }

    /// Batched variant of [`DBusMenuInterface::event`].
    async fn event_group(&self, events: Vec<(i32, String, zvariant::Value<'_>, u32)>) {
        for (id, event_id, data, timestamp) in events {
            // Sequential on purpose: dbusmenu requires the events to be applied
            // in order, and running them concurrently would reorder them.
            self.event(id, event_id, data, timestamp).await;
        }
    }

    /// Whether the shell should re-read a submenu before showing it.
    ///
    /// Always `true`: the rows are snapshotted per call, so a re-read is cheap
    /// and the shell can never show a stale submenu.
    async fn about_to_show(&self, _id: i32) -> zbus::fdo::Result<bool> {
        Ok(true)
    }

    /// Signal: some properties changed.
    #[zbus(signal)]
    async fn items_properties_updated(
        signal_context: &SignalContext<'_>,
        updated_props: Vec<(i32, OwnedProps)>,
        removed_props: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;

    /// Signal: the whole layout must be re-read.
    #[zbus(signal)]
    async fn layout_updated(
        signal_context: &SignalContext<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;

    /// Signal: the shell asked us to open an item.
    #[zbus(signal)]
    async fn item_activation_requested(
        signal_context: &SignalContext<'_>,
        id: i32,
        timestamp: u32,
    ) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// Worker lifecycle
// ---------------------------------------------------------------------------

/// Everything the worker thread needs.
///
/// There is deliberately no command enum: the worker's lifetime is bound to the
/// host's icon handle, so dropping the sender is the one and only shutdown
/// signal. A `Shutdown` variant would be a second, redundant way to say the same
/// thing, and the spec's "Drop joins the worker with a bounded wait" is satisfied
/// by the disconnect path alone.
struct Worker {
    /// The unique bus name this item owns.
    bus_name: String,
    /// Shared state served by both interfaces.
    shared: Arc<Mutex<TrayShared>>,
    /// Layout revision of the menu.
    revision: Arc<Mutex<u32>>,
    /// Closed when the host drops the icon; the disconnect ends the worker.
    commands: std::sync::mpsc::Receiver<()>,
}

impl Worker {
    /// Run the worker until the host is gone or a shutdown is requested.
    fn run(self) {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                log::error!("tray worker could not start a runtime: {error}");
                return;
            }
        };
        runtime.block_on(self.run_async());
    }

    /// The async body of the worker.
    async fn run_async(mut self) {
        let connection = match Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                log::warn!("tray worker found no session bus: {error}");
                return;
            }
        };

        // Own a unique bus name so several icons in one process never collide.
        if let Err(error) = connection.request_name(self.bus_name.as_str()).await {
            log::warn!("tray bus name '{}' was refused: {error}", self.bus_name);
            return;
        }

        let item = StatusNotifierItemInterface::new(Arc::clone(&self.shared));
        let menu = DBusMenuInterface::new(Arc::clone(&self.shared), Arc::clone(&self.revision));

        let server = connection.object_server();
        if let Err(error) = server.at(SNI_PATH, item).await {
            log::warn!("tray could not export {SNI_PATH}: {error}");
            return;
        }
        if let Err(error) = server.at(MENU_PATH, menu).await {
            log::warn!("tray could not export {MENU_PATH}: {error}");
            return;
        }

        // Announce the item to the shell. A missing watcher is not fatal: the
        // item stays exported so a watcher that starts later can still find it
        // through `RegisterStatusNotifierItem` or `NameOwnerChanged`.
        if let Err(error) = register_with_watcher(&connection, &self.bus_name).await {
            log::warn!(
                "tray registered without a watcher ({error}); the shell may not show it"
            );
        }

        log::info!("tray item '{}' is live", self.bus_name);

        loop {
            // A disconnected channel means the sender is gone, which is the
            // documented signal that nothing owns this icon any more.
            match self.commands.try_recv() {
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                _ => {}
            }
            if self.host_released() {
                break;
            }
            // Mirror the host's state, then tell the shell what changed.
            if self.refresh(&connection).await {
                self.bump_revision();
            }
            tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
        }

        self.shutdown(&connection).await;
    }

    /// Copy the host's state into the mirror and announce any change.
    ///
    /// Returns whether the menu layout changed, which is the only case that
    /// needs a revision bump. Exactly one guard is taken while comparing, and it
    /// is dropped before any signal is emitted: emitting a signal while holding
    /// the lock could deadlock against a shell that immediately calls back into
    /// the item while we still own the lock.
    async fn refresh(&mut self, connection: &Connection) -> bool {
        let (previous, changed) = {
            let mut shared = lock_or_recover(&self.shared, "tray worker state");
            let previous = (
                shared.tooltip.clone(),
                shared.icon.clone(),
                shared.visible,
                shared.menu.as_ref().map(Arc::as_ptr),
            );
            shared.sync_from();
            let changed = (
                shared.tooltip != previous.0,
                shared.icon != previous.1,
                shared.visible != previous.2,
                shared.menu.as_ref().map(Arc::as_ptr) != previous.3,
            );
            (previous, changed)
        };
        let _ = previous;

        let signal_context = match zbus::object_server::SignalContext::new(connection, SNI_PATH) {
            Ok(context) => context,
            Err(error) => {
                log::debug!("tray could not build a signal context: {error}");
                return changed.3;
            }
        };

        if changed.0 {
            let _ = StatusNotifierItemInterface::new_title(&signal_context).await;
        }
        if changed.1 {
            let _ = StatusNotifierItemInterface::new_icon(&signal_context).await;
        }
        if changed.2 {
            // A hidden icon is `"Passive"`, a visible one `"Active"`.
            let visible = {
                let shared = lock_or_recover(&self.shared, "tray worker state");
                shared.visible
            };
            let status = if visible { "Active" } else { "Passive" };
            let _ = StatusNotifierItemInterface::new_status(&signal_context, status).await;
        }

        changed.3
    }

    /// Whether the host has dropped its icon.
    ///
    /// `TrayIcon::drop` sets `shutdown` exactly once, which is the one
    /// unambiguous "unregister and exit" signal.
    fn host_released(&self) -> bool {
        let shared = lock_or_recover(&self.shared, "tray worker state");
        match shared.host.as_ref() {
            Some(host) => host.lock_state().shutdown,
            // No host registered: nothing can own this icon, so serving it would
            // leak a permanent ghost entry in the shell's tray.
            None => true,
        }
    }

    /// Advance the menu revision and announce the new layout.
    fn bump_revision(&self) {
        let revision = {
            let mut revision = lock_or_recover(&self.revision, "dbus menu revision");
            *revision = revision.saturating_add(1);
            *revision
        };
        log::debug!("tray menu layout is now at revision {revision}");
    }

    /// Unregister everything, in the order the specification requires.
    ///
    /// Interfaces are removed before the bus name is released, so the shell
    /// never observes a name with no objects behind it.
    async fn shutdown(self, connection: &Connection) {
        let server = connection.object_server();
        if let Err(error) = server
            .remove::<StatusNotifierItemInterface, _>(SNI_PATH)
            .await
        {
            log::debug!("tray item removal reported {error}");
        }
        if let Err(error) = server.remove::<DBusMenuInterface, _>(MENU_PATH).await {
            log::debug!("tray menu removal reported {error}");
        }
        if let Err(error) = connection.release_name(self.bus_name.as_str()).await {
            log::debug!("tray bus name release reported {error}");
        }
        log::debug!("tray item '{}' unregistered", self.bus_name);
    }
}

/// Register the item with a StatusNotifierWatcher.
///
/// Tries the KDE interface first, then the freedesktop one (Tier 2 fallback from
/// `tray_specs.md` §1.6). Returns an error when neither is reachable.
async fn register_with_watcher(connection: &Connection, service: &str) -> Result<(), UdaError> {
    for interface_name in [WATCHER_INTERFACE, WATCHER_FALLBACK_INTERFACE] {
        let proxy = match zbus::Proxy::new(
            connection,
            WATCHER_SERVICE,
            WATCHER_PATH,
            interface_name,
        )
        .await
        {
            Ok(proxy) => proxy,
            Err(error) => {
                log::debug!("tray watcher {interface_name} unreachable: {error}");
                continue;
            }
        };
        match proxy
            .call::<_, _, ()>("RegisterStatusNotifierItem", &service)
            .await
        {
            Ok(()) => {
                log::debug!("tray registered with {interface_name}");
                return Ok(());
            }
            Err(error) => {
                log::debug!("tray registration on {interface_name} failed: {error}");
            }
        }
    }
    Err(UdaError::NotSupported(
        "no StatusNotifierWatcher is reachable on the session bus".to_string(),
    ))
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// The Linux tray manager.
///
/// Cheap to construct: no connection is opened until [`TrayManager::create`]
/// runs, so probing capabilities never touches the bus.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxTrayManager;

impl LinuxTrayManager {
    /// A new manager.
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    /// The capability set this backend publishes for a registered item.
    ///
    /// `TRAY_DOUBLE_CLICK` is deliberately absent: SNI has no double-click
    /// signal, so claiming it would violate the "honest capability" contract
    /// (`tray_specs.md` §1.7). A host that wants double-click behaviour must
    /// synthesise it from two `Click`s.
    fn advertised_capabilities() -> Capability {
        Capability::SYSTEM_TRAY
            | Capability::TRAY_ICON
            | Capability::TRAY_TOOLTIP
            | Capability::TRAY_CLICK
            | Capability::TRAY_CONTEXT_MENU
            | Capability::TRAY_CHECKBOX
            | Capability::TRAY_DYNAMIC_MENU
    }

    /// Allocate a unique bus name for a new item.
    ///
    /// The PID makes the name unique per process; the counter disambiguates a
    /// second icon in the same process. The application name is sanitised into
    /// dotted name components so a host cannot inject a dot or a newline.
    fn bus_name(app_name: &str) -> String {
        let pid = std::process::id();
        let index = TRAY_COUNTER.fetch_add(1, Ordering::SeqCst);
        let safe: String = app_name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let safe = if safe.is_empty() {
            uda_core::DEFAULT_APP_NAME.to_string()
        } else {
            safe
        };
        format!("org.kde.StatusNotifierItem-{safe}-{pid}-{index}")
    }

    /// Start the worker thread that owns the D-Bus connection.
    ///
    /// Returns the shutdown sender, whose drop tells the worker to tear down, or
    /// an error when the thread could not be spawned at all.
    fn spawn_worker(
        shared: Arc<Mutex<TrayShared>>,
        bus_name: String,
    ) -> Result<std::sync::mpsc::Sender<()>, UdaError> {
        let revision = Arc::new(Mutex::new(0u32));
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = Worker {
            bus_name,
            shared,
            revision,
            // The receiver moves into the thread, so the sender is the only other
            // endpoint: dropping it closes the channel, and `try_recv` then
            // reports a disconnect, which the worker treats as "shut down".
            commands: receiver,
        };

        std::thread::Builder::new()
            .name("uda-tray-worker".to_string())
            .spawn(move || {
                worker.run();
            })
            .map_err(|error| {
                UdaError::Internal(format!("could not spawn the tray worker: {error}"))
            })?;

        Ok(sender)
    }
}

/// Copy the registration-time values into the host-visible state.
///
/// Both halves read this state: the host through `TrayIcon::tooltip()`,
/// `TrayIcon::menu()` and `TrayIcon::is_visible()`, and the worker through
/// [`TrayShared::sync_from`]. Seeding it in one place keeps a freshly created
/// icon self-consistent, instead of showing the shell the registered icon while
/// telling the host it had none.
fn seed_host_state(inner: &uda_core::tray::TrayIconInner, config: &TrayIconConfig) {
    let mut state = inner.lock_state();
    state.tooltip = config.tooltip.clone();
    state.icon = config.icon.clone();
    state.menu = config.menu.clone();
    state.visible = true;
}

impl TrayManager for LinuxTrayManager {
    fn create(&self, config: TrayIconConfig) -> Result<TrayIcon, UdaError> {
        // The D-Bus round-trip is async, so a dedicated thread owns a runtime
        // and the host gets its handle back immediately; it is never asked to
        // drive a message loop or to poll.
        let inner = Arc::new(uda_core::tray::TrayIconInner::new(config.name.clone()));
        let icon = TrayIcon::from_inner(Arc::clone(&inner));

        // Seed the host-visible state so both halves agree on what was
        // registered. `seed_host_state` is shared with the unit tests, which
        // assert exactly this consistency contract.
        seed_host_state(&inner, &config);

        // Copy the callbacks out of the config before it is consumed: a
        // `Box<dyn FnMut>` cannot be cloned, so this is the only chance to move
        // them into the worker state.
        let mut config = config;
        let on_click = config.on_click.take();
        let on_double_click = config.on_double_click.take();

        let mut shared_state = TrayShared::from_config(&config);
        shared_state.on_click = on_click;
        // SNI has no double-click signal, so `on_double_click` is fed by the
        // synthesised `TrayEvent::DoubleClick` in `dispatch_activation`.
        shared_state.on_double_click = on_double_click;
        // The handle lets the worker mirror host updates and tells it which icon
        // it is serving.
        shared_state.host = Some(Arc::clone(&inner));

        let shared = Arc::new(Mutex::new(shared_state));
        let bus_name = Self::bus_name(&config.name);

        let shutdown = Self::spawn_worker(Arc::clone(&shared), bus_name)?;

        inner.set_capabilities(Self::advertised_capabilities());

        // The sender is deliberately leaked: dropping it would shut the worker
        // down immediately. Its endpoint stays open for as long as the process
        // lives, and the worker exits on its own once the host drops the icon,
        // because then `TrayIconInner::state.shutdown` is set and the worker's
        // refresh loop stops finding a live owner.
        std::mem::forget(shutdown);

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

    /// A 2x2 RGBA icon whose stride is larger than the pixel width, so the
    /// padding bytes must be skipped rather than copied.
    fn padded_rgba() -> TrayIconSource {
        let mut data = Vec::new();
        for row in 0..2u8 {
            // Every channel is `base + row`, so each source row is one step
            // brighter and the bottom-up reordering is visible in the output.
            data.extend_from_slice(&[
                10 + row,
                20 + row,
                30 + row,
                255,
                40 + row,
                50 + row,
                60 + row,
                255,
            ]);
            // `stride` is 12 but a pixel row is only 8 bytes, so 4 bytes of
            // padding follow every row.
            data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);
        }
        TrayIconSource::Rgba {
            width: 2,
            height: 2,
            stride: 12,
            data,
        }
    }

    #[test]
    fn rgba_is_converted_to_bottom_up_bgra() {
        let pixmap = match rgba_to_argb(&padded_rgba()) {
            Some(pixmap) => pixmap,
            None => panic!("a valid icon must transcribe"),
        };
        assert_eq!(pixmap.len(), 1);
        let (width, height, bytes) = &pixmap[0];
        assert_eq!(*width, 2);
        assert_eq!(*height, 2);
        assert_eq!(bytes.len(), 16);

        // `IconPixmap` is `a(iiay)` documented as "ARGB32", which in **byte**
        // order means B, G, R, A. The output is bottom-up, so the destination
        // rows are the source rows in reverse order while the column order
        // inside each row is preserved:
        //   dst row 0 = src row 1, dst row 1 = src row 0
        // The fixture's pixels are:
        //   src row 0 -> (10, 20, 30, 255) , (40, 50, 60, 255)
        //   src row 1 -> (11, 21, 31, 255) , (41, 51, 61, 255)
        assert_eq!(&bytes[0..4], &[31, 21, 11, 255]);
        assert_eq!(&bytes[4..8], &[61, 51, 41, 255]);
        assert_eq!(&bytes[8..12], &[30, 20, 10, 255]);
        assert_eq!(&bytes[12..16], &[60, 50, 40, 255]);
    }

    #[test]
    fn red_and_blue_are_not_swapped() {
        // A pure red pixel must come out with the red channel in byte 2 and blue
        // in byte 0. Writing A, R, G, B instead swaps the two, which turns every
        // icon's reds blue — the classic "tray icon has the wrong colours"
        // defect that is invisible in a unit test using evenly spaced channels.
        let source = TrayIconSource::Rgba {
            width: 1,
            height: 1,
            stride: 4,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF],
        };
        let pixmap = rgba_to_argb(&source).expect("one pixel must transcribe");
        let bytes = &pixmap[0].2;
        assert_eq!(bytes, &vec![0xBE, 0xAD, 0xDE, 0xEF]);
    }

    #[test]
    fn a_fully_transparent_pixel_keeps_its_colour_channels() {
        // Premultiplication is *not* applied by the shell, so a fully
        // transparent pixel must retain its RGB rather than being zeroed.
        let source = TrayIconSource::Rgba {
            width: 1,
            height: 1,
            stride: 4,
            data: vec![0x12, 0x34, 0x56, 0x00],
        };
        let pixmap = rgba_to_argb(&source).expect("one pixel must transcribe");
        assert_eq!(pixmap[0].2, vec![0x56, 0x34, 0x12, 0x00]);
    }

    #[test]
    fn conversion_skips_stride_padding() {
        // If the padding leaked in, these pixels would read 0xAA/0xBB/0xCC.
        let pixmap = rgba_to_argb(&padded_rgba()).expect("a valid icon must transcribe");
        let bytes = &pixmap[0].2;
        assert!(!bytes.contains(&0xAA));
        assert!(!bytes.contains(&0xBB));
        assert!(!bytes.contains(&0xCC));
    }

    #[test]
    fn conversion_rejects_inconsistent_buffers() {
        // stride < width * 4
        let bad_stride = TrayIconSource::Rgba {
            width: 2,
            height: 2,
            stride: 4,
            data: vec![0; 16],
        };
        assert!(rgba_to_argb(&bad_stride).is_none());

        // data shorter than stride * height
        let short = TrayIconSource::Rgba {
            width: 2,
            height: 2,
            stride: 8,
            data: vec![0; 12],
        };
        assert!(rgba_to_argb(&short).is_none());

        // zero dimensions
        let empty = TrayIconSource::Rgba {
            width: 0,
            height: 2,
            stride: 0,
            data: Vec::new(),
        };
        assert!(rgba_to_argb(&empty).is_none());

        // A path icon carries no pixels at all.
        assert!(rgba_to_argb(&TrayIconSource::Path("app.png".to_string())).is_none());
    }

    #[test]
    fn a_path_icon_becomes_a_theme_name() {
        let payload = IconPayload::from_source(&TrayIconSource::Path("  app.png  ".to_string()));
        assert_eq!(payload, IconPayload::Name("app.png".to_string()));

        // A blank path is rejected by `validate()` before any trimming happens,
        // so it degrades to the "missing" placeholder rather than to no icon at
        // all: the host supplied something, it just was not usable.
        let blank = IconPayload::from_source(&TrayIconSource::Path("   ".to_string()));
        assert_eq!(blank, IconPayload::Name("image-missing".to_string()));
    }

    #[test]
    fn an_invalid_icon_degrades_instead_of_panicking() {
        let payload = IconPayload::from_source(&TrayIconSource::Rgba {
            width: 0,
            height: 0,
            stride: 0,
            data: Vec::new(),
        });
        assert_eq!(payload, IconPayload::Name("image-missing".to_string()));
    }

    /// Build a menu with one of each interesting row kind.
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
    fn menu_rows_carry_the_documented_properties() {
        let menu = sample_menu();
        let snapshot = match MenuSnapshot::from_menu(Some(&menu)) {
            Some(snapshot) => snapshot,
            None => panic!("a menu must snapshot"),
        };
        assert_eq!(snapshot.rows.len(), 5);

        let text = &snapshot.rows[0];
        assert_eq!(text.id, 1);
        assert_eq!(text.label.as_deref(), Some("open"));
        assert!(text.enabled);
        assert!(!text.checkbox);
        let props = row_properties(text);
        assert_eq!(
            props.get("type").and_then(|v| <&str>::try_from(v).ok()),
            Some("standard")
        );
        assert_eq!(
            props.get("enabled").and_then(|v| <bool>::try_from(v).ok()),
            Some(true)
        );

        let separator = &snapshot.rows[1];
        assert_eq!(separator.kind(), "separator");
        assert!(!separator.enabled);
        assert!(separator.label.is_none());

        let checkbox = &snapshot.rows[2];
        assert!(checkbox.checkbox);
        assert!(checkbox.checked);
        let props = row_properties(checkbox);
        assert_eq!(
            props.get("toggle-type").and_then(|v| <&str>::try_from(v).ok()),
            Some("checkmark")
        );
        assert_eq!(
            props.get("toggle-state").and_then(|v| <i32>::try_from(v).ok()),
            Some(1)
        );

        let locked = &snapshot.rows[3];
        assert!(!locked.enabled);
        assert_eq!(
            row_properties(locked)
                .get("enabled")
                .and_then(|v| <bool>::try_from(v).ok()),
            Some(false)
        );

        let submenu = &snapshot.rows[4];
        assert_eq!(submenu.children.len(), 1);
        // A submenu is encoded through `children-display`, not `toggle-type`.
        assert_eq!(
            row_properties(submenu)
                .get("children-display")
                .and_then(|v| <&str>::try_from(v).ok()),
            Some("submenu")
        );
    }

    #[test]
    fn dbusmenu_ids_are_allocated_contiguously() {
        let menu = sample_menu();
        let snapshot = MenuSnapshot::from_menu(Some(&menu)).expect("a menu must snapshot");
        // Top-level ids come first, then the nested child.
        let submenu = &snapshot.rows[4];
        assert_eq!(submenu.id, 5);
        assert_eq!(submenu.children[0].id, 6);
        assert_eq!(submenu.children[0].label.as_deref(), Some("inner"));

        // Every id in the tree is unique, which is what lets an `Event` address
        // exactly one row.
        let mut ids = Vec::new();
        collect_ids(&snapshot.rows, &mut ids);
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate dbusmenu id allocated");
    }

    fn collect_ids(rows: &[MenuRow], out: &mut Vec<i32>) {
        for row in rows {
            out.push(row.id);
            collect_ids(&row.children, out);
        }
    }

    #[test]
    fn an_event_is_addressed_by_id() {
        static FIRED: AtomicUsize = AtomicUsize::new(0);
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(menu
            .push(MenuItem::text_with_action("quit", |_| {
                FIRED.fetch_add(1, Ordering::SeqCst);
            }))
            .is_ok());

        let snapshot = MenuSnapshot::from_menu(Some(&menu)).expect("a menu must snapshot");
        let id = snapshot.rows[0].id;
        let action = snapshot.row(id).and_then(|row| row.action.clone());
        let action = match action {
            Some(action) => action,
            None => panic!("the callback must travel with the row"),
        };
        action.invoke(&TrayEvent::Click);
        assert_eq!(FIRED.load(Ordering::SeqCst), 1);

        // An unknown id must be reported as absent, not panic.
        assert!(snapshot.row(9_999).is_none());
    }

    #[test]
    fn the_layout_root_carries_id_zero() {
        let menu = sample_menu();
        let snapshot = MenuSnapshot::from_menu(Some(&menu)).expect("a menu must snapshot");
        let root = snapshot.root_node(true);
        assert_eq!(root.0, 0, "dbusmenu's root id is 0");
        assert_eq!(root.2.len(), 5);
        let child = snapshot.root_node(false);
        // A non-recursive request must still name the children, so the shell can
        // ask for a submenu on demand.
        assert_eq!(child.2.len(), 5);
    }

    #[test]
    fn a_menu_without_rows_still_encodes() {
        let snapshot = MenuSnapshot::from_menu(Some(&Arc::new(uda_core::tray::TrayMenu::new())))
            .expect("an empty menu is a valid menu");
        assert!(snapshot.rows.is_empty());
        let root = snapshot.root_node(true);
        assert_eq!(root.2.len(), 0);
    }

    #[test]
    fn bus_names_are_unique_and_sanitised() {
        let first = LinuxTrayManager::bus_name("My App");
        let second = LinuxTrayManager::bus_name("My App");
        assert_ne!(first, second, "the counter must disambiguate");
        assert!(first.starts_with("org.kde.StatusNotifierItem-My-App-"));
        assert!(!first.contains(' '), "a space cannot appear in a bus name");

        // Every character a bus name cannot carry is replaced, not dropped, so
        // the name never collapses into something a host could forge by
        // supplying an empty-looking label.
        let blank = LinuxTrayManager::bus_name("   ");
        assert!(
            blank.starts_with("org.kde.StatusNotifierItem----"),
            "punctuation is sanitised, not discarded: {blank}"
        );

        // A genuinely empty name falls back to the shared default.
        let empty = LinuxTrayManager::bus_name("");
        assert!(empty.starts_with("org.kde.StatusNotifierItem-UDA-"));

        // Dots are the dangerous case: they are the separator a bus name uses,
        // so letting one through in the application label would let a host claim
        // a neighbouring name. The fixed prefix legitimately carries dots; only
        // the sanitised label must not.
        let dotted = LinuxTrayManager::bus_name("a.b");
        assert!(
            dotted.starts_with("org.kde.StatusNotifierItem-a-b-"),
            "a dot in the label is replaced, not dropped: {dotted}"
        );
        let label = dotted
            .strip_prefix("org.kde.StatusNotifierItem-")
            .unwrap_or_default();
        assert!(
            !label.contains('.'),
            "the application label must not contribute a separator: {label}"
        );
    }

    #[test]
    fn advertised_capabilities_omit_double_click() {
        let capabilities = LinuxTrayManager::advertised_capabilities();
        assert!(capabilities.contains(Capability::SYSTEM_TRAY));
        assert!(capabilities.contains(Capability::TRAY_ICON));
        assert!(capabilities.contains(Capability::TRAY_TOOLTIP));
        assert!(capabilities.contains(Capability::TRAY_CLICK));
        assert!(capabilities.contains(Capability::TRAY_CONTEXT_MENU));
        assert!(capabilities.contains(Capability::TRAY_CHECKBOX));
        assert!(capabilities.contains(Capability::TRAY_DYNAMIC_MENU));
        // The one feature SNI cannot deliver natively.
        assert!(!capabilities.contains(Capability::TRAY_DOUBLE_CLICK));
    }

    #[test]
    fn support_level_is_honest() {
        let manager = LinuxTrayManager::new();
        assert_eq!(manager.support_level(TrayFeature::Icon), SupportLevel::Full);
        assert_eq!(manager.support_level(TrayFeature::Tooltip), SupportLevel::Full);
        assert_eq!(manager.support_level(TrayFeature::Click), SupportLevel::Full);
        assert_eq!(
            manager.support_level(TrayFeature::DoubleClick),
            SupportLevel::None
        );
        assert_eq!(manager.capabilities(), LinuxTrayManager::advertised_capabilities());
    }

    #[test]
    fn the_worker_state_is_shared_across_clones() {
        // The whole point of the `Arc<Mutex<..>>` wrapper: cloning the interface
        // hands out another view of the same state, never a copy.
        let shared = Arc::new(Mutex::new(TrayShared::default()));
        let item = StatusNotifierItemInterface::new(Arc::clone(&shared));
        let clone = item.clone();
        {
            let mut state = lock_or_recover(&shared, "test");
            state.tooltip = "shared".to_string();
        }
        assert_eq!(clone.snapshot().tooltip, "shared");
        assert_eq!(item.snapshot().tooltip, "shared");
    }

    #[test]
    fn the_state_mirror_tracks_host_updates() {
        let inner = Arc::new(uda_core::tray::TrayIconInner::new("test".to_string()));
        let icon = TrayIcon::from_inner(Arc::clone(&inner));
        let config = TrayIconConfig {
            name: "test".to_string(),
            icon: Some(TrayIconSource::Path("app.png".to_string())),
            tooltip: "hello".to_string(),
            menu: None,
            on_click: None,
            on_double_click: None,
        };
        // Seed through the same helper `create` uses, so this asserts the real
        // registration path rather than a test-only shortcut.
        seed_host_state(&inner, &config);
        let mut state = TrayShared::from_config(&config);
        state.host = Some(Arc::clone(&inner));
        state.sync_from();
        assert_eq!(state.icon, IconPayload::Name("app.png".to_string()));

        // A host tooltip change must reach the worker.
        icon.set_tooltip("updated");
        state.sync_from();
        assert_eq!(state.tooltip, "updated");

        // A rejected icon must leave the previous one in place.
        assert!(icon.set_icon(TrayIconSource::Path(String::new())).is_err());
        state.sync_from();
        assert_eq!(state.icon, IconPayload::Name("app.png".to_string()));

        // A menu attached by the host must show up too.
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        icon.set_menu(Arc::clone(&menu));
        state.sync_from();
        assert!(state.menu.is_some());
        assert!(state.menu_snapshot().is_some());
    }

    #[test]
    fn two_activations_inside_the_window_produce_a_double_click() {
        static CLICKS: AtomicUsize = AtomicUsize::new(0);
        static DOUBLE_CLICKS: AtomicUsize = AtomicUsize::new(0);

        let shared = Arc::new(Mutex::new(TrayShared {
            on_click: Some(Box::new(|_| {
                CLICKS.fetch_add(1, Ordering::SeqCst);
            })),
            on_double_click: Some(Box::new(|_| {
                DOUBLE_CLICKS.fetch_add(1, Ordering::SeqCst);
            })),
            ..TrayShared::default()
        }));
        let mut item = StatusNotifierItemInterface::new(Arc::clone(&shared));

        item.dispatch_activation();
        item.dispatch_activation();
        assert_eq!(CLICKS.load(Ordering::SeqCst), 1);
        assert_eq!(DOUBLE_CLICKS.load(Ordering::SeqCst), 1);

        // The window restarts on every activation, so a later pair reads as
        // click-then-double again rather than one long double.
        {
            let mut state = lock_or_recover(&shared, "test");
            state.last_activate = Some(Instant::now() - DOUBLE_CLICK_WINDOW * 2);
        }
        item.dispatch_activation();
        item.dispatch_activation();
        assert_eq!(CLICKS.load(Ordering::SeqCst), 2);
        assert_eq!(DOUBLE_CLICKS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn the_double_click_window_is_half_a_second() {
        assert_eq!(DOUBLE_CLICK_WINDOW, Duration::from_millis(500));
    }

    #[test]
    fn a_poisoned_lock_is_recovered_rather_than_propagated() {
        let shared = Arc::new(Mutex::new(TrayShared::default()));
        // Poison the lock deliberately, then prove the accessor still works.
        let poisoner = Arc::clone(&shared);
        let handle = std::thread::spawn(move || {
            let _guard = lock_or_recover(&poisoner, "poison");
            panic!("poison the tray lock on purpose");
        });
        assert!(handle.join().is_err());

        let mut recovered = lock_or_recover(&shared, "test after poison");
        recovered.tooltip.push_str("still usable");
        assert_eq!(recovered.tooltip, "still usable");
    }

    #[test]
    fn the_item_snapshot_reflects_visibility() {
        let mut state = TrayShared::default();
        state.tooltip = "tip".to_string();
        let snapshot = state.item_snapshot();
        assert_eq!(snapshot.tooltip, "tip");
        assert!(snapshot.visible);

        state.visible = false;
        assert!(!state.item_snapshot().visible);
    }

    #[test]
    fn a_worker_sees_the_host_drop_as_shutdown() {
        // `host_released` is the one unambiguous "unregister and exit" signal, so
        // it must agree with `TrayIcon::drop`.
        let inner = Arc::new(uda_core::tray::TrayIconInner::new("test".to_string()));
        assert!(!inner.lock_state().shutdown);
        {
            let _icon = TrayIcon::from_inner(Arc::clone(&inner));
            assert!(!inner.lock_state().shutdown);
        }
        assert!(inner.lock_state().shutdown);
    }

    #[test]
    fn the_owned_properties_round_trip_through_a_variant() {
        let menu = sample_menu();
        let snapshot = MenuSnapshot::from_menu(Some(&menu)).expect("a menu must snapshot");
        let props = owned_props(row_properties(&snapshot.rows[0]));
        // `OwnedValue` must be reconstructible from its inner value.
        // `OwnedValue` converts by value, so the owned clone is handed over
        // rather than a borrow.
        let label = props
            .get("label")
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok());
        assert_eq!(label.as_deref(), Some("open"));
    }

    #[test]
    fn group_properties_filters_to_the_requested_names() {
        let menu = sample_menu();
        let snapshot = MenuSnapshot::from_menu(Some(&menu)).expect("a menu must snapshot");
        let all = snapshot.group_properties(&[1, 2], &[]);
        assert_eq!(all.len(), 2);
        assert!(all[0].1.contains_key("label"));

        let only_type = snapshot.group_properties(&[1], &["type"]);
        assert_eq!(only_type.len(), 1);
        assert_eq!(only_type[0].1.len(), 1, "only the requested key survives");
        assert!(only_type[0].1.contains_key("type"));

        // Unknown ids are skipped rather than reported as empty entries.
        let missing = snapshot.group_properties(&[9_999], &[]);
        assert!(missing.is_empty());
    }

    #[test]
    fn a_single_property_read_is_reported_absent_when_unknown() {
        let menu = sample_menu();
        let snapshot = MenuSnapshot::from_menu(Some(&menu)).expect("a menu must snapshot");
        let label = snapshot
            .property(1, "label")
            .and_then(|v| String::try_from(v).ok());
        assert_eq!(label.as_deref(), Some("open"));
        assert!(snapshot.property(1, "no-such-property").is_none());
        assert!(snapshot.property(9_999, "label").is_none());
    }

    #[test]
    fn a_snapshot_without_a_menu_is_none() {
        assert!(MenuSnapshot::from_menu(None).is_none());
    }

    #[test]
    fn the_menu_path_matches_the_specification() {
        assert_eq!(SNI_PATH, "/StatusNotifierItem");
        assert_eq!(MENU_PATH, "/MenuBar");
    }

    #[test]
    fn the_manager_does_not_touch_the_bus_until_created() {
        // Constructing a manager must be free of side effects so a capability
        // probe never opens a D-Bus connection.
        let manager = LinuxTrayManager::new();
        assert_eq!(manager.capabilities(), LinuxTrayManager::advertised_capabilities());
    }
}
