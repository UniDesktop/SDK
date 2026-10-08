//! Linux system tray backend: `org.kde.StatusNotifierItem` (SNI) + `com.canonical.dbusmenu`.
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
//! The state is `Arc<Mutex<TrayShared>>` rather than `RwLock`: a read guard held
//! across an `.await` is what deadlocks against a waiting writer, and `zbus`
//! dispatches each method call in its own task. A `Mutex` makes the hazard
//! impossible - one guard per `lock()`, always dropped before the next
//! acquisition. Locking is always recovered from poisoning, as in
//! [`uda_core::tray`].
//!
//! Spawning the worker is not the same as being registered: `create` blocks on
//! a one-shot readiness channel until the worker has actually put the item on
//! the session bus, so a worker that died at `Connection::session()` surfaces
//! as an error instead of a full-capability icon that never appears.
//!
//! No `unwrap()`, `expect()`, `panic!`, `unreachable!` or `unsafe` in this
//! module; every D-Bus call is fallible and every host-supplied buffer is
//! validated before it is transcribed.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::hash::Hasher;
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
/// Matches the Windows default `GetDoubleClickTime()`. SNI has no double-click
/// signal of its own (`docs/internals/tray_specs.md` §1.7), so this is a
/// host-visible approximation and `TRAY_DOUBLE_CLICK` is deliberately not
/// advertised.
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

/// Lock a mutex, recovering from a poisoned lock: the state is pure data
/// rewritten field by field, so resuming beats failing every later update.
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
    /// Derive the payload from a validated [`TrayIconSource`]: a path travels
    /// through as a theme name, RGBA is transcoded (see `tray_specs.md` §1.4).
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

/// Convert straight RGBA top-down bytes into bottom-up 32bpp rows (B, G, R, A),
/// returning `None` for a source that is not RGBA or does not match its declared
/// dimensions. All access is bounds-checked, so a malformed buffer yields
/// `None` rather than a panic.
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
        // `stride` bytes per source row, of which the first `width * 4` carry
        // pixels; the padding is skipped rather than copied.
        let src_row = &data[row * stride..row * stride + width * 4];
        // Bottom-up: source row 0 lands at the end of the destination.
        let dst_row = &mut out[(height - 1 - row) * width * 4..(height - row) * width * 4];
        for column in 0..width {
            let pixel = &src_row[column * 4..column * 4 + 4];
            let base = column * 4;
            // RGBA -> BGRA little-endian byte order. `IconPixmap` is documented
            // as `a(iiay)` "ARGB32 rows", which in practice means **byte** order
            // B, G, R, A; writing A, R, G, B instead swaps red and blue.
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

/// Persistent mapping from a row's identity to the dbusmenu id a shell sees.
///
/// An `Event` is addressed by dbusmenu id (the `Event` gotcha in
/// `docs/internals/tray_specs.md` §1.5), so the same row must keep the same
/// id for as long as it exists. The map is the bridge between the host's
/// stable [`uda_core::tray::MenuItemId`] and the integer ids the protocol
/// uses: a row is allocated one id on first sight, a removed row's id is
/// retired, and a re-added row gets a fresh one - so a shell that still caches
/// an old id can never trigger a different row.
///
/// The owning menu's identity is part of the key because two submenus are
/// separate `TrayMenu`s whose core id counters both start at 1; keying on the
/// core id alone would alias their rows.
#[derive(Debug)]
struct MenuIdMap {
    /// `(owning menu, core MenuItemId)` -> dbusmenu id.
    entries: HashMap<(usize, u64), i32>,
    /// Next dbusmenu id to hand out. It only ever grows, which is what makes
    /// retired ids unreusable.
    next_id: i32,
}

impl MenuIdMap {
    /// An empty map. Ids start at 1: 0 is the protocol's root sentinel.
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_id: 1,
        }
    }

    /// The dbusmenu id of a row, allocating one on first sight. The counter
    /// saturates rather than wrapping, since a wrapped counter would alias a
    /// live row.
    fn id_for(&mut self, menu: &Arc<uda_core::tray::TrayMenu>, core_id: u64) -> i32 {
        let key = (Arc::as_ptr(menu) as usize, core_id);
        if let Some(&id) = self.entries.get(&key) {
            return id;
        }
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.entries.insert(key, id);
        id
    }

    /// Drop the mappings of rows that no longer exist. Their ids stay retired:
    /// pruning cannot cause reuse because `next_id` never rewinds.
    fn prune_absent(&mut self, live: &HashSet<(usize, u64)>) {
        self.entries.retain(|key, _| live.contains(key));
    }
}

/// A compact summary of the menu the shell should currently see. Two different
/// fingerprints mean "the layout changed, tell the shell"; equal fingerprints
/// mean "nothing to announce this tick".
///
/// The fingerprint is content-based because the host mutates the menu **in
/// place** through a shared `Arc` - a pointer comparison cannot see those
/// mutations at all. The owning menu's identity is folded in so a replacement
/// menu always reads as a change even when its rows repeat the old labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MenuFingerprint(u64);

/// A menu hasher, threaded through the fingerprint walk.
type MenuHasher = std::collections::hash_map::DefaultHasher;

/// Fingerprint a menu: structure, labels, enabled and checked state, plus the
/// core ids. Swapped callbacks are deliberately invisible - actions are not
/// exported over dbusmenu, so they cannot change what the shell renders.
fn menu_fingerprint(menu: Option<&Arc<uda_core::tray::TrayMenu>>) -> MenuFingerprint {
    let mut hasher = MenuHasher::new();
    match menu {
        None => hasher.write_u8(0),
        Some(menu) => {
            hasher.write_u8(1);
            hasher.write_usize(Arc::as_ptr(menu) as usize);
            fingerprint_menu(menu, &mut hasher);
        }
    }
    MenuFingerprint(hasher.finish())
}

/// Walk one menu into the fingerprint hash, rows in export order.
fn fingerprint_menu(menu: &uda_core::tray::TrayMenu, hasher: &mut MenuHasher) {
    let entries = menu.entries();
    hasher.write_usize(entries.len());
    for (core_id, item) in entries {
        hasher.write_u64(core_id.into_raw());
        fingerprint_item(&item, hasher);
    }
}

/// Walk one row into the fingerprint hash: kind, label, interaction state and,
/// for a submenu, its nested rows.
fn fingerprint_item(item: &MenuItem, hasher: &mut MenuHasher) {
    match item {
        MenuItem::Separator => hasher.write_u8(0),
        MenuItem::Text { label, state, .. } => {
            hasher.write_u8(1);
            fingerprint_row(label, state, hasher);
        }
        MenuItem::Checkbox { label, state, .. } => {
            hasher.write_u8(2);
            fingerprint_row(label, state, hasher);
        }
        MenuItem::Submenu {
            label,
            state,
            children,
            ..
        } => {
            hasher.write_u8(3);
            fingerprint_row(label, state, hasher);
            fingerprint_menu(children, hasher);
        }
    }
}

/// Hash the parts of a row that are visible to the shell.
fn fingerprint_row(label: &str, state: &uda_core::tray::MenuItemState, hasher: &mut MenuHasher) {
    hasher.write(label.as_bytes());
    hasher.write_u8(u8::from(state.enabled));
    hasher.write_u8(u8::from(state.checked));
}

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
    /// Stored beside the exported row so an `Event` dispatches without re-walking
    /// [`uda_core::tray::TrayMenu`], which the host may have mutated since the
    /// layout was exported.
    action: Option<uda_core::tray::TrayAction>,
}

impl MenuRow {
    /// Map a [`MenuItem`] onto a row, taking its dbusmenu id from the
    /// persistent `ids` map so a row keeps its id across snapshots.
    ///
    /// `menu` is the `TrayMenu` the row came from (it is part of the map key);
    /// `core_id` is the row's [`uda_core::tray::MenuItemId`]. On a fresh map the
    /// first allocation is contiguous in traversal order; after edits, stability
    /// wins over contiguity by design.
    fn from_item(
        menu: &Arc<uda_core::tray::TrayMenu>,
        core_id: u64,
        item: &MenuItem,
        ids: &mut MenuIdMap,
        live: &mut HashSet<(usize, u64)>,
    ) -> Self {
        let id = ids.id_for(menu, core_id);
        live.insert((Arc::as_ptr(menu) as usize, core_id));
        let state = item.state();

        // Every row starts with the same id and interaction state; only the
        // kind-specific fields are filled in by the match below.
        let mut row = Self {
            id,
            label: None,
            enabled: state.enabled,
            checkbox: false,
            checked: false,
            separator: false,
            children: Vec::new(),
            action: None,
        };

        match item {
            MenuItem::Separator => {
                // Never interactive and never labelled, whatever the state says.
                row.enabled = false;
                row.separator = true;
            }
            MenuItem::Text { label, action, .. } => {
                row.label = Some(label.clone());
                row.action = action.clone();
            }
            MenuItem::Checkbox { label, action, .. } => {
                row.label = Some(label.clone());
                row.checkbox = true;
                row.checked = state.checked;
                row.action = action.clone();
            }
            MenuItem::Submenu {
                label, children, ..
            } => {
                // A submenu is its own `TrayMenu`, whose core ids are
                // independent of the parent's; `from_item` keys the nested rows
                // on the submenu's identity, so they never alias the parent's.
                row.label = Some(label.clone());
                for (child_id, child) in children.entries() {
                    row.children.push(Self::from_item(
                        children,
                        child_id.into_raw(),
                        &child,
                        ids,
                        live,
                    ));
                }
            }
        }

        row
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
            // `checkmark` is what makes a shell draw a real checkbox.
            props.insert("toggle-type", "checkmark".into());
            props.insert("toggle-state", i32::from(self.checked).into());
        }
        if !self.children.is_empty() {
            props.insert("children-display", "submenu".into());
        }
        props
    }

    /// Find a row by dbusmenu id, depth-first.
    fn find(rows: &[MenuRow], id: i32) -> Option<&MenuRow> {
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

/// Children are variants containing complete `(ia{sv}av)` nodes.
type MenuChildren = Vec<zvariant::OwnedValue>;

/// The standard dbusmenu layout node: `(ia{sv}av)`.
type MenuNode = (i32, OwnedProps, MenuChildren);

/// Own the property values so they can travel back over D-Bus.
///
/// `try_to_owned` reports the rare case where a payload cannot be cloned into a
/// static one (an `Fd`, for instance), which cannot happen for our types.
fn owned_props(props: HashMap<&'static str, zvariant::Value<'_>>) -> OwnedProps {
    props
        .into_iter()
        .filter_map(|(key, value)| {
            value
                .try_to_owned()
                .ok()
                .map(|owned| (key.to_string(), owned))
        })
        .collect()
}

/// Keep only the property names the shell asked for; an empty list asks for
/// all of them, which is how both `GetGroupProperties` and `GetProperty`
/// express "no filter".
fn filtered_props<'a>(
    props: HashMap<&'static str, zvariant::Value<'a>>,
    wanted: &[&str],
) -> HashMap<&'static str, zvariant::Value<'a>> {
    if wanted.is_empty() {
        return props;
    }
    props
        .into_iter()
        .filter(|(key, _)| wanted.contains(key))
        .collect()
}

/// Encoded child variants for `rows` under the dbusmenu recursion depth:
/// `-1` is the whole subtree, `0` excludes the children entirely, and `n`
/// allows exactly `n` more levels below each child.
fn children_variants(rows: &[MenuRow], depth: i32) -> Result<MenuChildren, zvariant::Error> {
    if depth == 0 {
        return Ok(Vec::new());
    }
    let child_depth = if depth < 0 { -1 } else { depth - 1 };
    rows.iter()
        .map(|row| zvariant::Value::new(subtree_node(row, child_depth)?).try_to_owned())
        .collect()
}

/// A snapshot of the menu attached to an icon, taken per request, with callbacks
/// captured so an `Event` never reaches back into the host's live menu tree.
#[derive(Debug, Clone)]
struct MenuSnapshot {
    rows: Vec<MenuRow>,
}

/// Why a `GetLayout` node could not be built: the requested parent is not part
/// of the current snapshot, or the payload failed to encode.
#[derive(Debug)]
enum LayoutRequestError {
    UnknownParent(i32),
    Encode(zvariant::Error),
}

/// The standard properties every dbusmenu root carries, whether or not rows are
/// attached. They come from the com.canonical.dbusmenu protocol itself (the
/// whole-menu side of the interface, not the row property table in
/// `tray_specs.md` §1.5): the protocol version this backend implements, the
/// text direction and the menu status.
fn root_properties() -> HashMap<&'static str, zvariant::Value<'static>> {
    HashMap::from([
        ("Version", 3u32.into()),
        ("TextDirection", "ltr".into()),
        ("Status", "normal".into()),
    ])
}

/// One standard root property, addressed by name. The root is not a row: it
/// answers exactly these properties and nothing else.
fn root_property(name: &str) -> Option<zvariant::OwnedValue> {
    root_properties()
        .remove(name)
        .and_then(|value| value.try_to_owned().ok())
}

/// The root properties for `GetGroupProperties`, filtered like row properties.
fn root_properties_filtered(wanted: &[&str]) -> OwnedProps {
    owned_props(filtered_props(root_properties(), wanted))
}

/// The root node when no menu is attached: standard properties, no rows.
fn empty_root_node() -> MenuNode {
    (0, owned_props(root_properties()), Vec::new())
}

impl MenuSnapshot {
    /// Snapshot the menu behind an `Option<Arc<TrayMenu>>`, assigning dbusmenu
    /// ids from `ids` so they survive menu edits. Mappings of rows that no
    /// longer exist are pruned afterwards; their ids are never handed out again.
    fn from_menu(
        menu: Option<&Arc<uda_core::tray::TrayMenu>>,
        ids: &mut MenuIdMap,
    ) -> Option<Self> {
        let menu = menu?;
        let mut rows = Vec::new();
        let mut live = HashSet::new();
        for (core_id, item) in menu.entries() {
            rows.push(MenuRow::from_item(
                menu,
                core_id.into_raw(),
                &item,
                ids,
                &mut live,
            ));
        }
        ids.prune_absent(&live);
        Some(Self { rows })
    }

    /// Look up a row by dbusmenu id.
    fn row(&self, id: i32) -> Option<&MenuRow> {
        MenuRow::find(&self.rows, id)
    }

    /// The `GetLayout` reply node for `parent_id` at the requested recursion
    /// depth. `parent_id` 0 is the root; any other id addresses a row of the
    /// current snapshot, and an id that is not part of it is an error rather
    /// than a silently wrong subtree.
    fn layout_node(&self, parent_id: i32, depth: i32) -> Result<MenuNode, LayoutRequestError> {
        if parent_id == 0 {
            return self.root_node(depth).map_err(LayoutRequestError::Encode);
        }
        let row = self
            .row(parent_id)
            .ok_or(LayoutRequestError::UnknownParent(parent_id))?;
        subtree_node(row, depth).map_err(LayoutRequestError::Encode)
    }

    /// Encode the root: standard whole-menu properties plus the children the
    /// depth allows (`-1` the whole subtree, `0` the root alone, `n` exactly `n`
    /// child levels).
    fn root_node(&self, depth: i32) -> Result<MenuNode, zvariant::Error> {
        let children = children_variants(&self.rows, depth)?;
        Ok((0, owned_props(root_properties()), children))
    }

    /// The owned id/property pairs for `GetGroupProperties`. Id 0 never
    /// resolves to a row; the interface answers it from [`root_properties`].
    fn group_properties(&self, ids: &[i32], wanted: &[&str]) -> Vec<(i32, OwnedProps)> {
        ids.iter()
            .filter_map(|id| {
                let row = self.row(*id)?;
                Some((*id, owned_props(filtered_props(row.properties(), wanted))))
            })
            .collect()
    }

    /// One owned property of one row.
    fn property(&self, id: i32, name: &str) -> Option<zvariant::OwnedValue> {
        let row = self.row(id)?;
        let props = row.properties();
        let value = props.get(name)?;
        value.try_to_owned().ok()
    }
}

/// Encode a row into a layout node, including children per the dbusmenu
/// recursion depth: `-1` is the whole subtree, `0` the row alone, `n` exactly
/// `n` child levels below this row.
fn subtree_node(row: &MenuRow, depth: i32) -> Result<MenuNode, zvariant::Error> {
    let children = children_variants(&row.children, depth)?;
    Ok((row.id, owned_props(row.properties()), children))
}

// ---------------------------------------------------------------------------
// Shared worker state
// ---------------------------------------------------------------------------

/// Which mirrored parts of [`TrayShared`] moved during a
/// [`TrayShared::sync_from`].
///
/// The menu is absent on purpose: its content is tracked by the worker's
/// [`MenuFingerprint`], because the host mutates the shared `Arc` in place.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct MirrorDelta {
    /// The tooltip text changed.
    tooltip: bool,
    /// The icon source changed and the payload was rebuilt.
    icon: bool,
    /// The visibility changed.
    visible: bool,
}

/// Whether two optional menu handles point at the same live menu.
fn menus_are_the_same(
    left: &Option<Arc<uda_core::tray::TrayMenu>>,
    right: &Option<Arc<uda_core::tray::TrayMenu>>,
) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => Arc::ptr_eq(left, right),
        (None, None) => true,
        _ => false,
    }
}

/// The state the D-Bus worker serves, copied out of the host's
/// [`TrayIconInner`] on every mutation and answered from this mirror.
struct TrayShared {
    /// Application name; also the dbusmenu `Id`.
    name: String,
    /// Current tooltip.
    tooltip: String,
    /// Current icon payload.
    icon: IconPayload,
    /// The icon source `icon` was built from. Kept beside the payload so a
    /// sync only re-transcodes when the source actually changed: an idle tick
    /// must not re-run the whole RGBA -> ARGB conversion every 200 ms.
    icon_source: Option<TrayIconSource>,
    /// Whether the item is shown.
    visible: bool,
    /// The live menu, when one is attached.
    menu: Option<Arc<uda_core::tray::TrayMenu>>,
    /// dbusmenu ids for the mirrored menu's rows, stable across snapshots.
    menu_ids: MenuIdMap,
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
    /// A registered icon is visible by default: `TrayIconInner` starts visible
    /// and `TrayIcon::hide` is the only way to turn that off, so a fresh mirror
    /// must agree, otherwise the worker would publish `status = Passive` for an
    /// icon the host never hid.
    fn default() -> Self {
        Self {
            name: String::new(),
            tooltip: String::new(),
            icon: IconPayload::None,
            icon_source: None,
            visible: true,
            menu: None,
            menu_ids: MenuIdMap::new(),
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
    /// Copy the host-visible state into the mirror under exactly one lock, so
    /// this stays in step with the host without ever nesting locks.
    ///
    /// Returns which parts of the mirror moved. The icon source is compared
    /// byte-for-byte *before* any transcode runs, so an idle tick neither
    /// re-runs the RGBA -> ARGB conversion nor allocates a fresh payload; the
    /// previous ARGB rows are reused untouched. The menu is re-pointed by
    /// identity only; its *content* is tracked by the worker's fingerprint.
    fn sync_from(&mut self) -> MirrorDelta {
        let Some(host) = self.host.clone() else {
            return MirrorDelta::default();
        };
        let mut delta = MirrorDelta::default();
        {
            let state = host.lock_state();
            if state.tooltip != self.tooltip {
                self.tooltip = state.tooltip.clone();
                delta.tooltip = true;
            }
            if state.icon != self.icon_source {
                self.icon_source = state.icon.clone();
                self.icon = self
                    .icon_source
                    .as_ref()
                    .map(IconPayload::from_source)
                    .unwrap_or_default();
                delta.icon = true;
            }
            if state.visible != self.visible {
                self.visible = state.visible;
                delta.visible = true;
            }
            if !menus_are_the_same(&self.menu, &state.menu) {
                self.menu = state.menu.clone();
            }
        }
        delta
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
            icon_source: config.icon.clone(),
            visible: true,
            menu: config.menu.clone(),
            menu_ids: MenuIdMap::new(),
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
    ///
    /// Takes `&mut self`: snapshotting allocates dbusmenu ids for rows the map
    /// has not seen yet, which is what keeps those ids stable across snapshots.
    fn menu_snapshot(&mut self) -> Option<MenuSnapshot> {
        MenuSnapshot::from_menu(self.menu.as_ref(), &mut self.menu_ids)
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
/// `zbus` requires `Clone` on an interface type; the state behind one
/// `Arc<Mutex<TrayShared>>` makes cloning a pointer bump.
#[derive(Clone)]
pub struct StatusNotifierItemInterface {
    shared: Arc<Mutex<TrayShared>>,
}

impl StatusNotifierItemInterface {
    /// Wrap shared state. Module-private: only [`Worker`] builds an interface,
    /// and a public constructor would let a caller fabricate a second,
    /// unrelated view of an icon's state.
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
    /// activation is a plain [`TrayEvent::Click`]. When no double-click handler
    /// is registered - the C ABI cannot register one at all - the second
    /// activation falls back to `on_click` instead of being swallowed. The
    /// callback runs **without** the state lock held, so host code may update
    /// the icon or the menu from inside it.
    fn dispatch_activation(&mut self) {
        // One guard: read the timestamp, record the new one, take the handler.
        let (event, handler) = {
            let mut shared = lock_or_recover(&self.shared, "status notifier item");
            let now = Instant::now();
            let double_click = match shared.last_activate {
                Some(last) => now.duration_since(last) <= DOUBLE_CLICK_WINDOW,
                None => false,
            };
            // The window restarts, so a triple click reads as click-then-double.
            shared.last_activate = Some(now);
            if double_click {
                match shared.on_double_click.take() {
                    Some(handler) => (TrayEvent::DoubleClick, Some(handler)),
                    None => (TrayEvent::Click, shared.on_click.take()),
                }
            } else {
                (TrayEvent::Click, shared.on_click.take())
            }
        };

        if let Some(handler) = handler {
            // The guard owns the handler for the duration of the call and puts
            // it back on drop, so the callback survives a panic *and* a normal
            // return: a handler that is never restored would silently stop
            // receiving clicks. The panic is caught so one misbehaving host
            // callback cannot kill the zbus task that dispatched it.
            let mut guard = HandlerGuard {
                shared: Arc::clone(&self.shared),
                event,
                handler: Some(handler),
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if let Some(handler) = guard.handler.as_mut() {
                    handler(&guard.event);
                }
            }));
            if outcome.is_err() {
                log::warn!("tray activation handler panicked; it stays registered");
            }
            // Restores the handler under the state lock, with no callback running.
            drop(guard);
        }
    }
}

/// Owns a taken activation handler while it runs and always puts it back on
/// drop, whether the call returned normally or unwound.
struct HandlerGuard {
    shared: Arc<Mutex<TrayShared>>,
    event: TrayEvent,
    handler: Option<uda_core::tray::TrayEventHandler>,
}

impl Drop for HandlerGuard {
    fn drop(&mut self) {
        if let Some(handler) = self.handler.take() {
            // The lock was released before the callback ran, so re-acquiring it
            // here is safe; poisoning is recovered like everywhere else.
            let mut shared = lock_or_recover(&self.shared, "status notifier item");
            match self.event {
                TrayEvent::Click => shared.on_click = Some(handler),
                TrayEvent::DoubleClick => shared.on_double_click = Some(handler),
            }
        }
    }
}

/// The four fields of the SNI `ToolTip` property, in specification order:
/// `(icon name, icon pixmap, title, description)`.
type ToolTipShape = (String, Vec<(i32, i32, Vec<u8>)>, String, String);

#[interface(name = "org.kde.StatusNotifierItem")]
impl StatusNotifierItemInterface {
    /// Primary activation; reported by the shell as a left click.
    async fn activate(&mut self, _x: i32, _y: i32) {
        self.dispatch_activation();
    }

    /// Secondary activation (middle click on most shells). `TrayEvent` has no
    /// distinct variant for it, so it is reported as a click rather than dropped.
    async fn secondary_activate(&mut self, _x: i32, _y: i32) {
        self.dispatch_activation();
    }

    /// The shell wants the context menu at (x, y). dbusmenu carries the geometry,
    /// so UDA only logs it; the shell renders the menu from `Menu`.
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
    fn tool_tip(&self) -> ToolTipShape {
        let snapshot = self.snapshot();
        match snapshot.icon {
            IconPayload::Name(name) => (name, Vec::new(), snapshot.tooltip, String::new()),
            IconPayload::Pixmap(pixmap) => (String::new(), pixmap, snapshot.tooltip, String::new()),
            IconPayload::None => (String::new(), Vec::new(), snapshot.tooltip, String::new()),
        }
    }

    /// Object path of the `com.canonical.dbusmenu` implementation.
    #[zbus(property)]
    fn menu(&self) -> zvariant::ObjectPath<'static> {
        // MENU_PATH is a fixed, valid object path, not application input.
        zvariant::ObjectPath::from_static_str_unchecked(MENU_PATH)
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

    /// Emitted when the tooltip changed.
    ///
    /// Per the SNI spec `NewToolTip()` carries no payload: a host re-reads the
    /// `ToolTip` property, which is served as a `v` - a variant over the
    /// `(icon name, icon pixmap, title, description)` struct that `tool_tip`
    /// above fills. zbus derives the signal member from this method's name, so
    /// the name must stay `new_tool_tip` for the wire member to remain
    /// `NewToolTip`.
    #[zbus(signal)]
    async fn new_tool_tip(signal_context: &SignalContext<'_>) -> zbus::Result<()>;

    /// Emitted when the status changed.
    #[zbus(signal)]
    async fn new_status(signal_context: &SignalContext<'_>, status: &str) -> zbus::Result<()>;
}

// ---------------------------------------------------------------------------
// com.canonical.dbusmenu
// ---------------------------------------------------------------------------

/// The dbusmenu interface exported at `/MenuBar`, sharing the icon's state plus
/// a layout revision so the shell is told to re-read when the menu changes.
#[derive(Clone)]
pub struct DBusMenuInterface {
    shared: Arc<Mutex<TrayShared>>,
    revision: Arc<Mutex<u32>>,
}

impl DBusMenuInterface {
    /// Wrap shared state. Module-private for the same reason as
    /// [`StatusNotifierItemInterface::new`]: one worker owns the state, and a
    /// second view could only serve stale data.
    #[must_use]
    fn new(shared: Arc<Mutex<TrayShared>>, revision: Arc<Mutex<u32>>) -> Self {
        Self { shared, revision }
    }

    /// Snapshot the menu, dropping the state lock immediately. Snapshotting
    /// needs `&mut` because it may allocate dbusmenu ids for fresh rows.
    fn menu_snapshot(&self) -> Option<MenuSnapshot> {
        let mut shared = lock_or_recover(&self.shared, "dbus menu");
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
    /// `parent_id` addresses the subtree to export (0 is the root); an id that
    /// is not part of the current layout is an `InvalidArgs` error rather than
    /// a silently wrong reply. `recursion_depth` follows the dbusmenu
    /// specification: `-1` is the whole subtree, `0` exports the requested node
    /// alone, and `n` exports exactly `n` child levels below it.
    async fn get_layout(
        &self,
        parent_id: i32,
        recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, MenuNode)> {
        let revision = self.revision();
        let node = match self.menu_snapshot() {
            Some(snapshot) => match snapshot.layout_node(parent_id, recursion_depth) {
                Ok(node) => node,
                Err(LayoutRequestError::UnknownParent(id)) => {
                    return Err(zbus::fdo::Error::InvalidArgs(format!(
                        "unknown menu item id {id}"
                    )));
                }
                Err(LayoutRequestError::Encode(error)) => {
                    return Err(zbus::fdo::Error::Failed(error.to_string()));
                }
            },
            // No menu attached: the root still exists, it just has no rows.
            None if parent_id == 0 => empty_root_node(),
            None => {
                return Err(zbus::fdo::Error::InvalidArgs(format!(
                    "unknown menu item id {parent_id}"
                )))
            }
        };
        Ok((revision, node))
    }

    /// Incremental property read for a set of ids. The root (id 0) is answered
    /// with the standard whole-menu properties even when no rows are attached.
    async fn get_group_properties(
        &self,
        ids: Vec<i32>,
        property_names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<(i32, OwnedProps)>> {
        let wanted: Vec<&str> = property_names.iter().map(String::as_str).collect();
        let mut results = Vec::new();
        if ids.contains(&0) {
            results.push((0, root_properties_filtered(&wanted)));
        }
        if let Some(snapshot) = self.menu_snapshot() {
            results.extend(snapshot.group_properties(&ids, &wanted));
        }
        Ok(results)
    }

    /// Single property read. The root (id 0) carries the standard dbusmenu
    /// properties and is answerable whether or not rows are attached.
    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<zvariant::OwnedValue> {
        if id == 0 {
            return root_property(&name).ok_or_else(|| {
                zbus::fdo::Error::InvalidArgs(format!("the menu root has no property '{name}'"))
            });
        }
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
        snapshot.property(id, &name).ok_or_else(|| {
            zbus::fdo::Error::InvalidArgs(format!("item {id} has no property '{name}'"))
        })
    }

    /// A shell-reported interaction with a row, addressed by dbusmenu id. The
    /// callback is invoked **after** the state lock is dropped: holding a D-Bus
    /// dispatch guard across host code would stall every other request.
    async fn event(&self, id: i32, event_id: String, _data: zvariant::Value<'_>, _timestamp: u32) {
        if event_id != "clicked" {
            // `hovered` carries no host-visible meaning in UDA's model.
            log::debug!("tray menu event '{event_id}' ignored");
            return;
        }

        let action = {
            let Some(snapshot) = self.menu_snapshot() else {
                return;
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

    /// Whether the shell should re-read a submenu before showing it. Always
    /// `true`: rows are snapshotted per call, so a re-read is cheap and the shell
    /// can never show a stale submenu.
    async fn about_to_show(&self, _id: i32) -> zbus::fdo::Result<bool> {
        Ok(true)
    }

    /// Signal: some properties changed.
    ///
    /// TODO(tray): emit this for label/enabled/checked-only changes instead of
    /// the full `LayoutUpdated`; a whole-layout announce is always correct for
    /// the shells, just more talkative than the protocol requires.
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

/// What the worker tells [`LinuxTrayManager::create`] once the bus work is
/// done: the item is live, live with a stated degradation, or dead.
enum WorkerReady {
    /// The item and its menu are exported and the bus name is held.
    Ready,
    /// The item is exported, but a non-fatal part of the registration failed;
    /// the string is the human-readable reason.
    Degraded(String),
    /// Initialisation failed; the worker has exited or is on its way out.
    Failed(String),
}

/// How long [`LinuxTrayManager::create`] waits for the worker's readiness
/// report before giving up on the icon.
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Everything the worker thread needs.
///
/// There is deliberately no command enum. Shutdown has two triggers and
/// neither carries a payload: the *primary* one is the worker polling
/// [`Self::host_released`], which reads the `shutdown` flag that
/// `TrayIcon::drop` sets when the host releases its handle (the success path
/// of `create` deliberately leaks the command sender, so the channel below
/// never disconnects for a live icon); the *secondary* one is that same
/// channel disconnecting, which happens only when `create` fails and its
/// shutdown sender is dropped while the worker is still coming up. Both are
/// observed in the poll loop of [`Self::run_async`].
struct Worker {
    /// The unique bus name this item owns.
    bus_name: String,
    /// Shared state served by both interfaces.
    shared: Arc<Mutex<TrayShared>>,
    /// Layout revision of the menu.
    revision: Arc<Mutex<u32>>,
    /// Closed when the host drops the icon; the disconnect ends the worker.
    commands: std::sync::mpsc::Receiver<()>,
    /// One-shot readiness report back to `create`, consumed on first send.
    ready: Option<std::sync::mpsc::Sender<WorkerReady>>,
    /// Fingerprint of the menu the shell was last told about.
    last_menu: Option<MenuFingerprint>,
}

impl Worker {
    /// Run the worker until the host is gone or a shutdown is requested.
    fn run(mut self) {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                log::error!("tray worker could not start a runtime: {error}");
                self.report(WorkerReady::Failed(format!(
                    "could not start the worker runtime: {error}"
                )));
                return;
            }
        };
        runtime.block_on(self.run_async());
    }

    /// Hand the readiness report to `create`. A gone receiver only means
    /// `create` timed out and gave up; the worker learns about that through the
    /// disconnected command channel instead. The sender is one-shot: it is
    /// consumed on the first report, so later failures cannot overwrite the
    /// verdict `create` already acted on.
    fn report(&mut self, outcome: WorkerReady) {
        if let Some(sender) = self.ready.take() {
            let _ = sender.send(outcome);
        }
    }

    async fn run_async(mut self) {
        let connection = match Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                log::warn!("tray worker found no session bus: {error}");
                self.report(WorkerReady::Failed(format!("no session bus: {error}")));
                return;
            }
        };

        // A unique bus name keeps several icons in one process from colliding.
        if let Err(error) = connection.request_name(self.bus_name.as_str()).await {
            log::warn!("tray bus name '{}' was refused: {error}", self.bus_name);
            self.report(WorkerReady::Failed(format!(
                "the bus name {} was refused: {error}",
                self.bus_name
            )));
            return;
        }

        let item = StatusNotifierItemInterface::new(Arc::clone(&self.shared));
        let menu = DBusMenuInterface::new(Arc::clone(&self.shared), Arc::clone(&self.revision));

        let server = connection.object_server();
        if let Err(error) = server.at(SNI_PATH, item).await {
            log::warn!("tray could not export {SNI_PATH}: {error}");
            self.report(WorkerReady::Failed(format!(
                "could not export {SNI_PATH}: {error}"
            )));
            return;
        }
        if let Err(error) = server.at(MENU_PATH, menu).await {
            log::warn!("tray could not export {MENU_PATH}: {error}");
            self.report(WorkerReady::Failed(format!(
                "could not export {MENU_PATH}: {error}"
            )));
            return;
        }

        // Both paths are compile-time constants, so these contexts cannot fail
        // to build; `SignalContext::new` would only re-parse the same strings.
        let sni_context = SignalContext::from_parts(
            connection.clone(),
            zvariant::ObjectPath::from_static_str_unchecked(SNI_PATH),
        );
        let menu_context = SignalContext::from_parts(
            connection.clone(),
            zvariant::ObjectPath::from_static_str_unchecked(MENU_PATH),
        );

        // A missing watcher is not fatal: the item stays exported, so a watcher
        // that starts later can still find it. It is a degradation, though, and
        // it is reported as one instead of being logged into silence.
        if let Err(error) = register_with_watcher(&connection, &self.bus_name).await {
            log::warn!("tray registered without a watcher ({error}); the shell may not show it");
            self.report(WorkerReady::Degraded(error.to_string()));
        } else {
            self.report(WorkerReady::Ready);
        }

        log::info!("tray item '{}' is live", self.bus_name);

        loop {
            // A disconnected sender means nothing owns this icon any more; an
            // empty channel is the normal case and falls through.
            if let Err(std::sync::mpsc::TryRecvError::Disconnected) = self.commands.try_recv() {
                break;
            }
            if self.host_released() {
                break;
            }
            self.refresh(&sni_context, &menu_context).await;
            tokio::time::sleep(SHUTDOWN_POLL_INTERVAL).await;
        }

        self.shutdown(&connection).await;
    }

    /// Mirror the host state and announce whatever the shell must re-read.
    ///
    /// Icon, tooltip and visibility changes come from the mirror delta; menu
    /// changes are detected by fingerprinting the live menu, because the host
    /// mutates it in place through a shared `Arc` and a pointer compare is
    /// blind to that. A detected change re-keys the snapshot, advances the
    /// revision and emits a real `LayoutUpdated(revision, 0)` - the signal the
    /// shells re-read on. Every signal is emitted without any state lock held:
    /// holding it across a signal could deadlock against a shell calling back
    /// into the item.
    async fn refresh(
        &mut self,
        sni_context: &SignalContext<'static>,
        menu_context: &SignalContext<'static>,
    ) {
        let (delta, visible, menu) = {
            let mut shared = lock_or_recover(&self.shared, "tray worker state");
            let delta = shared.sync_from();
            (delta, shared.visible, shared.menu.clone())
        };

        let menu_changed = self.note_menu_fingerprint(menu_fingerprint(menu.as_ref()));
        if menu_changed {
            // Re-key the snapshot now, before anything is announced: this
            // allocates ids for the new rows and retires the ids of removed
            // ones (a row re-added before the next tick must get a fresh id),
            // so the snapshot a shell reads after the signal is already in
            // step and no lock is held across the signal itself.
            let mut shared = lock_or_recover(&self.shared, "tray worker state");
            let _ = shared.menu_snapshot();
        }

        if delta.tooltip {
            // One mirrored field backs two properties: `title` serves the
            // tooltip text as the item's Title, and `tool_tip` serves it as
            // the third slot of the ToolTip struct. Both properties therefore
            // change together, so both signals go out and a host listening to
            // either one re-reads in step.
            let _ = StatusNotifierItemInterface::new_title(sni_context).await;
            let _ = StatusNotifierItemInterface::new_tool_tip(sni_context).await;
        }
        if delta.icon {
            let _ = StatusNotifierItemInterface::new_icon(sni_context).await;
        }
        if delta.visible {
            // A hidden icon is `"Passive"`, a visible one `"Active"`.
            let status = if visible { "Active" } else { "Passive" };
            let _ = StatusNotifierItemInterface::new_status(sni_context, status).await;
        }
        if menu_changed {
            self.bump_revision_and_announce(menu_context).await;
        }
    }

    /// Record the fingerprint of the mirrored menu and report whether the
    /// layout the shell should see changed. The first observation counts as a
    /// change so the initial layout is announced like any other.
    fn note_menu_fingerprint(&mut self, fingerprint: MenuFingerprint) -> bool {
        let changed = self.last_menu != Some(fingerprint);
        self.last_menu = Some(fingerprint);
        changed
    }

    /// Advance the menu revision and return the new value. It saturates rather
    /// than wrapping: a wrapped revision would re-announce a value the shell has
    /// already seen.
    fn advance_revision(&self) -> u32 {
        let mut revision = lock_or_recover(&self.revision, "dbus menu revision");
        *revision = revision.saturating_add(1);
        *revision
    }

    /// Advance the menu revision and announce the new layout on the bus:
    /// `LayoutUpdated(revision, 0)` - the whole menu changed, parent 0.
    async fn bump_revision_and_announce(&self, menu_context: &SignalContext<'_>) {
        let revision = self.advance_revision();
        log::debug!("tray menu layout is now at revision {revision}");
        if let Err(error) = DBusMenuInterface::layout_updated(menu_context, revision, 0).await {
            log::debug!("tray could not announce the new menu layout: {error}");
        }
    }

    /// Whether the host has dropped its icon: `TrayIcon::drop` sets `shutdown`
    /// exactly once, the one unambiguous "unregister and exit" signal.
    fn host_released(&self) -> bool {
        let shared = lock_or_recover(&self.shared, "tray worker state");
        match shared.host.as_ref() {
            Some(host) => host.lock_state().shutdown,
            // No host registered: nothing can own this icon, so serving it would
            // leak a permanent ghost entry in the shell's tray.
            None => true,
        }
    }

    /// Unregister everything: interfaces are removed before the bus name is
    /// released, so the shell never observes a name with no objects behind it.
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

/// Register the item with a StatusNotifierWatcher: the KDE interface first, then
/// the freedesktop one (`tray_specs.md` §1.6), erroring when neither is
/// reachable.
async fn register_with_watcher(connection: &Connection, service: &str) -> Result<(), UdaError> {
    for interface_name in [WATCHER_INTERFACE, WATCHER_FALLBACK_INTERFACE] {
        let proxy =
            match zbus::Proxy::new(connection, WATCHER_SERVICE, WATCHER_PATH, interface_name).await
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

/// The Linux tray manager. Cheap to construct: no connection is opened until
/// [`TrayManager::create`] runs, so probing capabilities never touches the bus.
///
/// A manager remembers - weakly - every icon it created, so
/// [`TrayManager::support_level`] can relay a degradation those icons recorded
/// (a session without a `StatusNotifierWatcher`, for one). The references are
/// `Weak`, so remembering never keeps an icon alive, and a dropped icon leaves
/// the answer on its own; a manager that created nothing answers statically.
#[derive(Debug, Default, Clone)]
pub struct LinuxTrayManager {
    /// Weak handles to the icons this manager created, pruned on access.
    icons: Arc<Mutex<Vec<std::sync::Weak<uda_core::tray::TrayIconInner>>>>,
}

impl LinuxTrayManager {
    /// A new manager.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Remember an icon this manager created, dropping the entries whose icon
    /// is already gone. The lock mirrors [`lock_or_recover`]: the vector is
    /// pure data, so a poisoned lock is recovered rather than propagated.
    fn remember(&self, inner: &Arc<uda_core::tray::TrayIconInner>) {
        let mut icons = lock_or_recover(&self.icons, "tray manager icons");
        icons.retain(|weak| weak.strong_count() > 0);
        icons.push(Arc::downgrade(inner));
    }

    /// The degradation reason of the first live icon that recorded one, if any.
    ///
    /// Lock order note: this takes the registry lock and, per candidate, the
    /// icon's own state lock. No path ever takes the registry lock while
    /// holding an icon state lock, so the order cannot invert.
    fn recorded_degradation(&self) -> Option<String> {
        let mut icons = lock_or_recover(&self.icons, "tray manager icons");
        icons.retain(|weak| weak.strong_count() > 0);
        icons
            .iter()
            .find_map(|weak| weak.upgrade()?.lock_state().degraded.clone())
    }

    /// The capability set this backend publishes for a registered item.
    ///
    /// `TRAY_DOUBLE_CLICK` is deliberately absent: SNI has no double-click
    /// signal, so claiming it would break the "honest capability" contract
    /// (`tray_specs.md` §1.7). A host synthesises it from two `Click`s.
    fn advertised_capabilities() -> Capability {
        Capability::SYSTEM_TRAY
            | Capability::TRAY_ICON
            | Capability::TRAY_TOOLTIP
            | Capability::TRAY_CLICK
            | Capability::TRAY_CONTEXT_MENU
            | Capability::TRAY_CHECKBOX
            | Capability::TRAY_DYNAMIC_MENU
    }

    /// Allocate a unique bus name for a new item: the PID makes it unique per
    /// process, the counter disambiguates a second icon in the same process, and
    /// the application name is sanitised into dotted components.
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

    /// Start the worker thread that owns the D-Bus connection, returning the
    /// shutdown sender whose drop tells the worker to tear down.
    ///
    /// This blocks until the worker reports readiness: spawned is not
    /// registered, and handing back a fully capable icon whose worker died at
    /// `Connection::session()` is exactly the dishonest success the readiness
    /// handshake exists to prevent.
    ///
    /// The readiness verdict travels back with the sender: `Ok(None)` - live
    /// and healthy; `Ok(Some(reason))` - live but degraded, with the reason to
    /// record on the icon before it reaches the host; `Err` - no icon to hand
    /// over at all.
    fn spawn_worker(
        shared: Arc<Mutex<TrayShared>>,
        bus_name: String,
    ) -> Result<(std::sync::mpsc::Sender<()>, Option<String>), UdaError> {
        let revision = Arc::new(Mutex::new(0u32));
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = Worker {
            bus_name,
            shared,
            revision,
            // The receiver moves into the thread, so the sender is the only other
            // endpoint: dropping it closes the channel, and `try_recv` then
            // reports a disconnect, which the worker treats as "shut down".
            commands: receiver,
            ready: Some(ready_sender),
            last_menu: None,
        };

        std::thread::Builder::new()
            .name("uda-tray-worker".to_string())
            .spawn(move || worker.run())
            .map_err(|error| {
                UdaError::Internal(format!("could not spawn the tray worker: {error}"))
            })?;

        // Returning `Err` here drops the shutdown sender, which disconnects the
        // command channel and stops a worker that is still coming up; the icon
        // handle the caller built is dropped with the failed `create`.
        let degraded = readiness_outcome(ready_receiver.recv_timeout(READY_TIMEOUT))?;
        Ok((sender, degraded))
    }
}

/// Translate a readiness report into what [`LinuxTrayManager::create`] must
/// act on: `Ok(None)` - ready; `Ok(Some(reason))` - live but degraded with the
/// reason to surface to the host; `Err` - the item never made it onto the bus
/// and there is no icon to hand over.
///
/// A degraded worker is not an error: the item answers on the bus, only its
/// discoverability is reduced. The reason is therefore *returned* rather than
/// dropped, so `create` can record it on the icon and `support_level` can
/// relay it - a degradation that only ever reaches `log::warn!` is invisible
/// to a host that embeds this library.
fn readiness_outcome(
    report: Result<WorkerReady, std::sync::mpsc::RecvTimeoutError>,
) -> Result<Option<String>, UdaError> {
    match report {
        Ok(WorkerReady::Ready) => Ok(None),
        Ok(WorkerReady::Degraded(reason)) => {
            log::warn!("tray item is live but degraded: {reason}");
            Ok(Some(reason))
        }
        // The worker could not put the item on the bus. There is no fallback
        // tier for a tray, so this is the graceful tier-4 error, carrying the
        // reason verbatim.
        Ok(WorkerReady::Failed(reason)) => Err(UdaError::NotSupported(format!(
            "system tray is unavailable: {reason}"
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(UdaError::Internal(format!(
            "the tray worker did not report readiness within {READY_TIMEOUT:?}"
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err(UdaError::Internal(
            "the tray worker exited before reporting readiness".to_string(),
        )),
    }
}

/// Copy the registration-time values into the host-visible state, which both
/// halves read: the host through its accessors, the worker through
/// [`TrayShared::sync_from`]. Seeding it in one place keeps a freshly created
/// icon self-consistent.
fn seed_host_state(inner: &uda_core::tray::TrayIconInner, config: &TrayIconConfig) {
    let mut state = inner.lock_state();
    state.tooltip = config.tooltip.clone();
    state.icon = config.icon.clone();
    state.menu = config.menu.clone();
    state.visible = true;
}

impl TrayManager for LinuxTrayManager {
    fn create(&self, config: TrayIconConfig) -> Result<TrayIcon, UdaError> {
        let inner = Arc::new(uda_core::tray::TrayIconInner::new(config.name.clone()));
        let icon = TrayIcon::from_inner(Arc::clone(&inner));

        seed_host_state(&inner, &config);

        // A `Box<dyn FnMut>` cannot be cloned, so this is the only chance to
        // move the callbacks into the worker state.
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

        // Blocks until the worker has exported the item on the session bus, so
        // the capabilities published below describe a live icon, never a dead
        // one. On failure this returns before any capability is published. A
        // degradation travels back with the readiness verdict.
        let (shutdown, degraded) = Self::spawn_worker(Arc::clone(&shared), bus_name)?;

        inner.set_capabilities(Self::advertised_capabilities());
        // The handshake has completed by now, so the degradation - if any - is
        // already decided: a host can never observe the icon claiming `Full`
        // while the worker's verdict is still in flight.
        inner.set_degraded(degraded);
        // Remember the icon weakly, so this manager's `support_level` can
        // relay the recorded degradation for as long as the icon lives.
        self.remember(&inner);

        // The sender is deliberately leaked: dropping it would shut the worker
        // down immediately. The worker exits on its own once the host drops the
        // icon, because `TrayIconInner::state.shutdown` is then set.
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
        // `TrayFeature` has no separate "will the shell display it" question:
        // `Icon` is where shell-side visibility is answered. A watcher-less
        // registration does not change what the code can do - the item stays
        // exported and serviceable (`tray_specs.md` §1.6) - it degrades
        // whether the shell will ever show the icon, and that is session state
        // the static capability set cannot know. The manager therefore relays
        // the degradation an icon it created has recorded; a manager that
        // created no icon answers statically, exactly as before.
        if feature == TrayFeature::Icon {
            if let Some(reason) = self.recorded_degradation() {
                return SupportLevel::Partial(reason);
            }
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
        // An unclaimed flag is a plain `None`, with one exception below: a
        // backend either answers for a feature or does not, and `Partial` is
        // reserved for a degraded answer stated with its reason.
        if capabilities.contains(flag) {
            SupportLevel::Full
        } else if feature == TrayFeature::DoubleClick {
            // SNI has no native double-click signal, but two `Activate` calls
            // inside the window *are* synthesised into one event. That is a
            // real, degraded answer, so it is reported with its reason instead
            // of as a plain `None`; the bit stays unadvertised because it is
            // not a native capability. The window is read from the constant the
            // synthesis actually uses, so the reason cannot drift from it.
            SupportLevel::Partial(format!(
                "double click is synthesised from two Activations inside a \
                 {} ms window; SNI has no native double click",
                DOUBLE_CLICK_WINDOW.as_millis()
            ))
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

        // `a(iiay)` "ARGB32" in **byte** order means B, G, R, A. The output is
        // bottom-up, so the destination rows are the source rows reversed while
        // the column order inside each row is preserved.
        assert_eq!(&bytes[0..4], &[31, 21, 11, 255]);
        assert_eq!(&bytes[4..8], &[61, 51, 41, 255]);
        assert_eq!(&bytes[8..12], &[30, 20, 10, 255]);
        assert_eq!(&bytes[12..16], &[60, 50, 40, 255]);
    }

    #[test]
    fn red_and_blue_are_not_swapped() {
        // A pure red pixel must come out with red in byte 2 and blue in byte 0;
        // writing A, R, G, B instead swaps them and turns every icon's reds blue.
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
        // Premultiplication is *not* applied by the shell, so a fully transparent
        // pixel must retain its RGB.
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
        assert!(menu
            .push(MenuItem::submenu("more", Arc::clone(&child)))
            .is_ok());
        menu
    }

    /// Snapshot a menu with a throw-away id map, for tests that do not care
    /// about id stability across snapshots.
    fn snapshot_of(menu: &Arc<uda_core::tray::TrayMenu>) -> MenuSnapshot {
        MenuSnapshot::from_menu(Some(menu), &mut MenuIdMap::new()).expect("a menu must snapshot")
    }

    #[test]
    fn menu_rows_carry_the_documented_properties() {
        let menu = sample_menu();
        let snapshot = snapshot_of(&menu);
        assert_eq!(snapshot.rows.len(), 5);

        let text = &snapshot.rows[0];
        assert_eq!(text.id, 1);
        assert_eq!(text.label.as_deref(), Some("open"));
        assert!(text.enabled);
        assert!(!text.checkbox);
        let props = text.properties();
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
        let props = checkbox.properties();
        assert_eq!(
            props
                .get("toggle-type")
                .and_then(|v| <&str>::try_from(v).ok()),
            Some("checkmark")
        );
        assert_eq!(
            props
                .get("toggle-state")
                .and_then(|v| <i32>::try_from(v).ok()),
            Some(1)
        );

        let locked = &snapshot.rows[3];
        assert!(!locked.enabled);
        assert_eq!(
            locked
                .properties()
                .get("enabled")
                .and_then(|v| <bool>::try_from(v).ok()),
            Some(false)
        );

        let submenu = &snapshot.rows[4];
        assert_eq!(submenu.children.len(), 1);
        // A submenu is encoded through `children-display`, not `toggle-type`.
        assert_eq!(
            submenu
                .properties()
                .get("children-display")
                .and_then(|v| <&str>::try_from(v).ok()),
            Some("submenu")
        );
    }

    #[test]
    fn fresh_ids_are_allocated_in_traversal_order() {
        let menu = sample_menu();
        let snapshot = snapshot_of(&menu);
        // Top-level ids come first, then the nested child.
        let submenu = &snapshot.rows[4];
        assert_eq!(submenu.id, 5);
        assert_eq!(submenu.children[0].id, 6);
        assert_eq!(submenu.children[0].label.as_deref(), Some("inner"));

        // On a fresh map the first allocation is contiguous from 1 and every id
        // in the tree is unique, which is what lets an `Event` address exactly
        // one row. After edits, stability wins over contiguity (see the
        // stability test below).
        let mut ids = Vec::new();
        collect_ids(&snapshot.rows, &mut ids);
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (1..=6).collect::<Vec<i32>>());
        let before = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), before, "duplicate dbusmenu id allocated");
    }

    #[test]
    fn dbusmenu_ids_are_stable_across_menu_mutations() {
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        let separator = menu.push(MenuItem::separator()).expect("push");
        assert!(menu.push(MenuItem::text("open")).is_ok());
        assert!(menu.push(MenuItem::text("quit")).is_ok());

        let mut ids = MenuIdMap::new();
        let first = MenuSnapshot::from_menu(Some(&menu), &mut ids).expect("a menu must snapshot");
        let open_id = first.rows[1].id;
        let quit_id = first.rows[2].id;
        let separator_id = first.rows[0].id;

        // Insert a row above "open" and drop the separator: neither "open" nor
        // "quit" may be reallocated to a different dbusmenu id, or the shell's
        // cached ids would fire the wrong rows (P1-18).
        assert!(menu.insert(0, MenuItem::text("new")).is_ok());
        assert!(menu.remove(separator));
        let second = MenuSnapshot::from_menu(Some(&menu), &mut ids).expect("a menu must snapshot");

        let relocated_open = second
            .rows
            .iter()
            .find(|row| row.label.as_deref() == Some("open"))
            .expect("open survives");
        let relocated_quit = second
            .rows
            .iter()
            .find(|row| row.label.as_deref() == Some("quit"))
            .expect("quit survives");
        assert_eq!(relocated_open.id, open_id);
        assert_eq!(relocated_quit.id, quit_id);

        // The fresh row got a fresh id: the retired separator id is never
        // reused, so a shell that still caches it cannot trigger this row.
        let fresh = &second.rows[0];
        assert_eq!(fresh.label.as_deref(), Some("new"));
        assert_ne!(fresh.id, separator_id);
        assert!(fresh.id > open_id && fresh.id > quit_id && fresh.id > separator_id);

        // The pruned map holds no stale rows and still has no duplicates.
        let mut all = Vec::new();
        collect_ids(&second.rows, &mut all);
        all.sort_unstable();
        let unique = all.len();
        all.dedup();
        assert_eq!(all.len(), unique);
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

        let snapshot = snapshot_of(&menu);
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
    fn layout_matches_the_dbusmenu_wire_signature() {
        use zvariant::Type;
        assert_eq!(MenuNode::signature().as_str(), "(ia{sv}av)");
        let menu = sample_menu();
        let snapshot = snapshot_of(&menu);
        let root = snapshot.root_node(-1).expect("root");
        for child in &root.2 {
            assert_eq!(child.value_signature().as_str(), "(ia{sv}av)");
        }
        let request = zbus::Message::method("/MenuBar", "GetLayout")
            .expect("method")
            .build(&(0i32, -1i32, Vec::<String>::new()))
            .expect("request");
        let reply = zbus::Message::method_reply(&request)
            .expect("reply")
            .build(&(1u32, root))
            .expect("valid D-Bus message");
        let (_revision, decoded): (u32, MenuNode) = reply.body().deserialize().expect("decode");
        assert_eq!(decoded.2.len(), 5);
    }

    /// Run inside `dbus-run-session`. A local message round trip alone does
    /// not catch invalid payloads that the bus rejects by disconnecting us.
    #[tokio::test]
    async fn get_layout_survives_session_bus_transport() -> Result<(), Box<dyn std::error::Error>> {
        // Use the protocol type independently of the implementation alias.
        type WireNode = (
            i32,
            HashMap<String, zvariant::OwnedValue>,
            Vec<zvariant::OwnedValue>,
        );

        let shared = Arc::new(Mutex::new(TrayShared::from_config(&TrayIconConfig::new(
            "LayoutTransportTest",
        ))));
        // The guard is the connection attempt itself, not an env-var guess, so
        // it can only skip when no bus can be reached at all; every assertion
        // below runs strictly after transport is proven to work and therefore
        // still fails loudly when it should.
        let server = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                println!("skipping: no session bus on this host ({error})");
                return Ok(());
            }
        };
        server
            .object_server()
            .at(
                SNI_PATH,
                StatusNotifierItemInterface::new(Arc::clone(&shared)),
            )
            .await?;
        server
            .object_server()
            .at(
                MENU_PATH,
                DBusMenuInterface::new(Arc::clone(&shared), Arc::new(Mutex::new(1))),
            )
            .await?;
        let destination = server
            .unique_name()
            .ok_or("session connection has no unique name")?;
        let client = zbus::Connection::session().await?;

        // Hosts discover the menu through SNI. A string with the same text
        // is not an object path and makes Plasma fall back to ContextMenu().
        let reply = tokio::time::timeout(
            Duration::from_secs(3),
            client.call_method(
                Some(destination.as_str()),
                SNI_PATH,
                Some("org.freedesktop.DBus.Properties"),
                "Get",
                &("org.kde.StatusNotifierItem", "Menu"),
            ),
        )
        .await??;
        let value: zvariant::OwnedValue = reply.body().deserialize()?;
        assert_eq!(value.value_signature().as_str(), "o");
        let menu_path = zvariant::ObjectPath::try_from(value)?;
        assert_eq!(menu_path.as_str(), MENU_PATH);

        for (menu, count) in [
            (None, 0),
            (Some(Arc::new(uda_core::tray::TrayMenu::new())), 0),
            (Some(sample_menu()), 5),
        ] {
            lock_or_recover(&shared, "test menu").menu = menu;
            let reply = tokio::time::timeout(
                Duration::from_secs(3),
                client.call_method(
                    Some(destination.as_str()),
                    menu_path.as_str(),
                    Some("com.canonical.dbusmenu"),
                    "GetLayout",
                    &(0i32, -1i32, Vec::<String>::new()),
                ),
            )
            .await??;
            assert_eq!(
                reply
                    .body()
                    .signature()
                    .ok_or("missing reply signature")?
                    .as_str(),
                "u(ia{sv}av)"
            );
            let (revision, root): (u32, WireNode) = reply.body().deserialize()?;
            assert_eq!(revision, 1);
            assert_eq!(root.0, 0);
            assert_eq!(root.2.len(), count);
            if count != 0 {
                let submenu = WireNode::try_from(root.2[4].try_clone()?)?;
                assert_eq!(submenu.2.len(), 1);
                let leaf = WireNode::try_from(submenu.2[0].try_clone()?)?;
                assert_eq!(<&str>::try_from(&leaf.1["label"])?, "inner");
                assert!(leaf.2.is_empty());
            }
        }

        // A menu event must still reach the application after layout reads.
        let fired = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&fired);
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        menu.push(MenuItem::text_with_action("open", move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }))?;
        lock_or_recover(&shared, "test menu").menu = Some(menu);

        // The swapped-in menu has never been seen by the stable id map, so the
        // id from the *previous* layout stays retired and must not fire: a
        // click on the stale id 1 lands on no row at all.
        tokio::time::timeout(
            Duration::from_secs(3),
            client.call_method(
                Some(destination.as_str()),
                menu_path.as_str(),
                Some("com.canonical.dbusmenu"),
                "Event",
                &(1i32, "clicked", zvariant::Value::new(0i32), 0u32),
            ),
        )
        .await??;
        assert_eq!(fired.load(Ordering::SeqCst), 0);

        // A shell re-reads the layout after the change and clicks the id that
        // read hands out - reading is also what re-keys the id map to the new
        // menu, so this is exactly the id a real client would use.
        let reply = tokio::time::timeout(
            Duration::from_secs(3),
            client.call_method(
                Some(destination.as_str()),
                menu_path.as_str(),
                Some("com.canonical.dbusmenu"),
                "GetLayout",
                &(0i32, 1i32, Vec::<String>::new()),
            ),
        )
        .await??;
        let (_revision, root): (u32, WireNode) = reply.body().deserialize()?;
        assert_eq!(root.2.len(), 1);
        let open_id = WireNode::try_from(root.2[0].try_clone()?)?.0;

        tokio::time::timeout(
            Duration::from_secs(3),
            client.call_method(
                Some(destination.as_str()),
                menu_path.as_str(),
                Some("com.canonical.dbusmenu"),
                "Event",
                &(open_id, "clicked", zvariant::Value::new(0i32), 0u32),
            ),
        )
        .await??;
        assert_eq!(fired.load(Ordering::SeqCst), 1);
        Ok(())
    }

    /// Run inside `dbus-run-session`; skipped on hosts without a session bus.
    ///
    /// Pins the wire shape of the tooltip announcement: zbus derives the signal
    /// member from the Rust method name, so the SNI-mandated `NewToolTip` only
    /// exists for as long as the method is named `new_tool_tip`, and the spec's
    /// payload-free form - a host re-reads the `ToolTip` property, a `v` over
    /// the `(icon name, icon pixmap, title, description)` struct - must reach
    /// the bus with an empty body.
    #[tokio::test]
    async fn the_tool_tip_announcement_reaches_the_bus_as_new_tool_tip(
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Same skip rule as `get_layout_survives_session_bus_transport` above:
        // a missing bus is an environment fact, not a regression, and the
        // assertions only run once a connection actually exists.
        let server = match zbus::Connection::session().await {
            Ok(connection) => connection,
            Err(error) => {
                println!("skipping: no session bus on this host ({error})");
                return Ok(());
            }
        };
        let unique_name = server
            .unique_name()
            .ok_or("session connection has no unique name")?
            .to_string();

        // Subscribe before emitting, or the broadcast could land between the
        // emit and the match rule taking effect. A second connection plays the
        // shell: the daemon forwards broadcast signals to every connection
        // holding a matching rule, which is exactly how a host observes them.
        let client = zbus::Connection::session().await?;
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::MessageType::Signal)
            .sender(unique_name.as_str())?
            .interface("org.kde.StatusNotifierItem")?
            .member("NewToolTip")?
            .path(SNI_PATH)?
            .build();
        let mut stream = zbus::MessageStream::for_match_rule(rule, &client, None).await?;

        let context = SignalContext::from_parts(
            server.clone(),
            zvariant::ObjectPath::from_static_str_unchecked(SNI_PATH),
        );
        StatusNotifierItemInterface::new_tool_tip(&context).await?;

        // `MessageStream` is only a `Stream`; zbus re-exports the extension
        // traits it itself uses, so no extra dependency is pulled in here.
        use zbus::export::futures_util::StreamExt;
        let received = tokio::time::timeout(Duration::from_secs(3), stream.next())
            .await
            .map_err(|_| "the NewToolTip signal did not arrive in time")?
            .ok_or("the signal stream ended unexpectedly")??;

        let header = received.header();
        assert_eq!(header.message_type(), zbus::MessageType::Signal);
        assert_eq!(
            header.member().map(|member| member.as_str()),
            Some("NewToolTip"),
            "the wire member must keep the SNI spelling"
        );
        assert_eq!(
            header.interface().map(|interface| interface.as_str()),
            Some("org.kde.StatusNotifierItem")
        );
        assert_eq!(header.path().map(|path| path.as_str()), Some(SNI_PATH));
        // No payload on the wire: the `v` over the tooltip struct is the
        // `ToolTip` property a host re-reads, not the signal body.
        assert_eq!(received.body().len(), 0);
        Ok(())
    }

    #[test]
    fn the_layout_root_carries_id_zero() {
        let menu = sample_menu();
        let snapshot = snapshot_of(&menu);
        let root = snapshot.root_node(-1).expect("layout encoding");
        assert_eq!(root.0, 0, "dbusmenu's root id is 0");
        assert_eq!(root.2.len(), 5);
        // The root carries the standard whole-menu properties, not row props.
        assert!(root.1.contains_key("Version"));
        assert!(root.1.contains_key("TextDirection"));
        assert!(root.1.contains_key("Status"));
    }

    #[test]
    fn a_menu_without_rows_still_encodes() {
        let snapshot = snapshot_of(&Arc::new(uda_core::tray::TrayMenu::new()));
        assert!(snapshot.rows.is_empty());
        let root = snapshot.root_node(-1).expect("layout encoding");
        assert_eq!(root.2.len(), 0);
        // Even a bare root answers the standard properties.
        assert!(root.1.contains_key("Version"));
    }

    #[test]
    fn recursion_depth_follows_the_dbusmenu_semantics() {
        let menu = sample_menu();
        let snapshot = snapshot_of(&menu);
        let submenu_node = |parent: &MenuNode| -> MenuNode {
            MenuNode::try_from(parent.2[4].try_clone().expect("clone variant")).expect("node")
        };

        // -1: the whole subtree, including the nested row.
        let deep = snapshot.root_node(-1).expect("layout encoding");
        assert_eq!(deep.2.len(), 5);
        assert_eq!(submenu_node(&deep).2.len(), 1, "depth -1 recurses fully");

        // 0: the requested node alone, no children anywhere.
        let shallow = snapshot.root_node(0).expect("layout encoding");
        assert!(shallow.2.is_empty(), "depth 0 excludes children");

        // 1: exactly one child level, no grandchildren.
        let one = snapshot.root_node(1).expect("layout encoding");
        assert_eq!(one.2.len(), 5);
        assert!(
            submenu_node(&one).2.is_empty(),
            "depth 1 stops after the first level"
        );

        // 2: two child levels.
        let two = snapshot.root_node(2).expect("layout encoding");
        let nested = submenu_node(&two);
        assert_eq!(nested.2.len(), 1);
        let leaf =
            MenuNode::try_from(nested.2[0].try_clone().expect("clone variant")).expect("node");
        assert!(leaf.2.is_empty(), "depth 2 stops after the second level");
    }

    #[test]
    fn a_layout_is_addressed_by_parent_id() {
        let menu = sample_menu();
        let snapshot = snapshot_of(&menu);
        let submenu_id = snapshot.rows[4].id;
        let inner_id = snapshot.rows[4].children[0].id;

        // A submenu id exports that subtree, not the root.
        let subtree = match snapshot.layout_node(submenu_id, -1) {
            Ok(node) => node,
            Err(error) => panic!("a live row must resolve as a parent: {error:?}"),
        };
        assert_eq!(subtree.0, submenu_id);
        assert_eq!(subtree.2.len(), 1);
        let inner =
            MenuNode::try_from(subtree.2[0].try_clone().expect("clone variant")).expect("node");
        assert_eq!(inner.0, inner_id);

        // A leaf with depth 0 exports itself alone.
        let alone = match snapshot.layout_node(inner_id, 0) {
            Ok(node) => node,
            Err(error) => panic!("a live row must resolve as a parent: {error:?}"),
        };
        assert_eq!(alone.0, inner_id);
        assert!(alone.2.is_empty());

        // An unknown parent is a request error, not a silently wrong root.
        match snapshot.layout_node(9_999, -1) {
            Err(LayoutRequestError::UnknownParent(id)) => assert_eq!(id, 9_999),
            other => panic!("expected UnknownParent, got {other:?}"),
        }
    }

    #[test]
    fn the_root_answers_the_standard_dbusmenu_properties() {
        let version = root_property("Version").and_then(|value| u32::try_from(value).ok());
        assert_eq!(version, Some(3), "the dbusmenu protocol version is 3");
        let direction =
            root_property("TextDirection").and_then(|value| String::try_from(value).ok());
        assert_eq!(direction.as_deref(), Some("ltr"));
        let status = root_property("Status").and_then(|value| String::try_from(value).ok());
        assert_eq!(status.as_deref(), Some("normal"));

        // The root is not a row: it answers no row property at all.
        assert!(root_property("label").is_none());
        assert!(root_property("toggle-state").is_none());

        // GetGroupProperties filters the root like any other entry.
        assert_eq!(root_properties_filtered(&["Version"]).len(), 1);
        let all = root_properties_filtered(&[]);
        assert!(all.contains_key("Version"));
        assert!(all.contains_key("TextDirection"));
        assert!(all.contains_key("Status"));
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
        assert_eq!(
            manager.support_level(TrayFeature::Tooltip),
            SupportLevel::Full
        );
        assert_eq!(
            manager.support_level(TrayFeature::Click),
            SupportLevel::Full
        );
        // SNI has no native double click, but the synthesis is a real degraded
        // answer, so it is reported as Partial with a reason, not as None. The
        // reason names the window the synthesis actually uses.
        let double_click = manager.support_level(TrayFeature::DoubleClick);
        assert_eq!(
            double_click,
            SupportLevel::Partial(format!(
                "double click is synthesised from two Activations inside a \
                 {} ms window; SNI has no native double click",
                DOUBLE_CLICK_WINDOW.as_millis()
            ))
        );
        assert!(
            double_click.reason().is_some(),
            "a Partial answer must carry its reason"
        );
        assert_eq!(
            manager.capabilities(),
            LinuxTrayManager::advertised_capabilities()
        );
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
    fn the_menu_fingerprint_tracks_in_place_mutation() {
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        let untouched = menu_fingerprint(Some(&menu));

        // Different menus with identical content are still different menus.
        assert_ne!(untouched, menu_fingerprint(None));
        assert_ne!(
            untouched,
            menu_fingerprint(Some(&Arc::new(uda_core::tray::TrayMenu::new())))
        );

        // A pushed row is a change even though the Arc pointer did not move -
        // exactly the mutation a pointer comparison is blind to (P1-16).
        assert!(menu.push(MenuItem::checkbox("pin")).is_ok());
        let pushed = menu_fingerprint(Some(&menu));
        assert_ne!(pushed, untouched);

        // An unchanged menu must fingerprint identically, tick after tick.
        assert_eq!(pushed, menu_fingerprint(Some(&menu)));

        // Toggling the checkbox changes wire-visible state.
        let (id, _) = menu.find_by_label("pin").expect("row exists");
        assert!(menu.toggle(id));
        let toggled = menu_fingerprint(Some(&menu));
        assert_ne!(toggled, pushed);

        // So does a relabel.
        assert!(menu.set_label(id, "pinned"));
        assert_ne!(menu_fingerprint(Some(&menu)), toggled);
    }

    #[test]
    fn the_menu_fingerprint_tracks_nested_submenu_mutation() {
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        let child = Arc::new(uda_core::tray::TrayMenu::new());
        assert!(menu
            .push(MenuItem::submenu("more", Arc::clone(&child)))
            .is_ok());
        let before = menu_fingerprint(Some(&menu));

        // The host mutates the nested menu through its own shared Arc; the
        // top-level pointer never moves, yet the layout must read as changed.
        assert!(child.push(MenuItem::text("inner")).is_ok());
        assert_ne!(menu_fingerprint(Some(&menu)), before);
    }

    /// A worker wired to throw-away endpoints, for unit-testing the pieces of
    /// the loop that need no bus.
    fn test_worker(shared: Arc<Mutex<TrayShared>>) -> Worker {
        Worker {
            bus_name: "org.kde.StatusNotifierItem-test".to_string(),
            shared,
            revision: Arc::new(Mutex::new(0)),
            commands: std::sync::mpsc::channel().1,
            ready: None,
            last_menu: None,
        }
    }

    #[test]
    fn a_worker_sees_only_real_menu_changes() {
        let shared = Arc::new(Mutex::new(TrayShared::default()));
        let menu = Arc::new(uda_core::tray::TrayMenu::new());
        lock_or_recover(&shared, "test").menu = Some(Arc::clone(&menu));
        let mut worker = test_worker(Arc::clone(&shared));

        // The first observation announces the layout's existence...
        assert!(worker.note_menu_fingerprint(menu_fingerprint(Some(&menu))));
        // ...and an idle tick announces nothing.
        assert!(!worker.note_menu_fingerprint(menu_fingerprint(Some(&menu))));

        // An in-place mutation through the shared Arc must read as a change.
        assert!(menu.push(MenuItem::text("row")).is_ok());
        assert!(worker.note_menu_fingerprint(menu_fingerprint(Some(&menu))));
    }

    #[test]
    fn the_revision_advances_monotonically() {
        let worker = test_worker(Arc::new(Mutex::new(TrayShared::default())));
        assert_eq!(worker.advance_revision(), 1);
        assert_eq!(worker.advance_revision(), 2);
    }

    #[test]
    fn sync_from_reuses_the_transcode_for_an_unchanged_icon_source() {
        let inner = Arc::new(uda_core::tray::TrayIconInner::new("test".to_string()));
        let icon = TrayIcon::from_inner(Arc::clone(&inner));
        let config = TrayIconConfig {
            name: "test".to_string(),
            icon: Some(padded_rgba()),
            tooltip: "tip".to_string(),
            menu: None,
            on_click: None,
            on_double_click: None,
        };
        seed_host_state(&inner, &config);
        let mut state = TrayShared::from_config(&config);
        state.host = Some(Arc::clone(&inner));

        // `from_config` already adopted the configured icon, so the first sync
        // is a no-op, exactly like the worker's idle ticks.
        assert!(!state.sync_from().icon);

        // The adopted payload is a real transcoded pixmap, not a placeholder.
        let payload = state.icon.clone();
        assert!(matches!(payload, IconPayload::Pixmap(_)));

        // An idle tick: the source is unchanged, so nothing is retranscoded and
        // the delta reports no icon change (P2-38).
        assert!(!state.sync_from().icon);
        assert_eq!(state.icon, payload);

        // Re-setting a byte-identical source is also a no-op...
        icon.set_icon(padded_rgba()).expect("a valid icon");
        assert!(!state.sync_from().icon);

        // ...while a genuinely different source retranscodes exactly once.
        icon.set_icon(TrayIconSource::Path("app.png".to_string()))
            .expect("a valid icon");
        let delta = state.sync_from();
        assert!(delta.icon);
        assert_eq!(state.icon, IconPayload::Name("app.png".to_string()));

        // Tooltip and visibility ride the same delta.
        icon.set_tooltip("updated");
        let delta = state.sync_from();
        assert!(delta.tooltip);
        assert!(!delta.icon);
        icon.hide();
        let delta = state.sync_from();
        assert!(delta.visible);
        assert!(!delta.tooltip);
    }

    #[test]
    fn a_panicking_click_handler_keeps_receiving_clicks() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let shared = Arc::new(Mutex::new(TrayShared {
            on_click: Some(Box::new(|_| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                if CALLS.load(Ordering::SeqCst) == 1 {
                    panic!("the first click handler explodes on purpose");
                }
            })),
            ..TrayShared::default()
        }));
        let mut item = StatusNotifierItemInterface::new(Arc::clone(&shared));

        item.dispatch_activation();
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        // The handler was put back after the panic: the second click still
        // reaches host code instead of vanishing (P2-37).
        item.dispatch_activation();
        assert_eq!(CALLS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_second_click_without_a_double_click_handler_still_reaches_on_click() {
        static CLICKS: AtomicUsize = AtomicUsize::new(0);
        let shared = Arc::new(Mutex::new(TrayShared {
            on_click: Some(Box::new(|_| {
                CLICKS.fetch_add(1, Ordering::SeqCst);
            })),
            ..TrayShared::default()
        }));
        let mut item = StatusNotifierItemInterface::new(Arc::clone(&shared));

        item.dispatch_activation();
        item.dispatch_activation();
        // The C ABI cannot register on_double_click, so the second activation
        // inside the window must fall back to the click handler (P1-22).
        assert_eq!(CLICKS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_failed_readiness_report_surfaces_as_an_error() {
        // A worker-reported failure means there is no icon to hand over; it
        // surfaces as the graceful tier-4 error with the reason attached.
        assert!(matches!(
            readiness_outcome(Ok(WorkerReady::Failed(
                "no session bus: refused".to_string()
            ))),
            Err(UdaError::NotSupported(_))
        ));
        // Timing out is a failure too: the worker never confirmed anything.
        assert!(readiness_outcome(Err(std::sync::mpsc::RecvTimeoutError::Timeout)).is_err());
        assert!(
            readiness_outcome(Err(std::sync::mpsc::RecvTimeoutError::Disconnected)).is_err(),
            "a worker that died before reporting is an error, not a success"
        );
        // Live workers are handed over, degraded or not - but a degraded one
        // must carry its reason out, or it would be invisible to the host.
        assert!(
            readiness_outcome(Ok(WorkerReady::Ready)).is_ok_and(|degraded| degraded.is_none()),
            "a healthy worker reports no degradation"
        );
        assert_eq!(
            readiness_outcome(Ok(WorkerReady::Degraded("no watcher".to_string())))
                .expect("a degraded worker is not an error"),
            Some("no watcher".to_string())
        );
    }

    #[test]
    fn a_recorded_degradation_flips_manager_support_level_to_partial() {
        let manager = LinuxTrayManager::new();
        // No icon, no degradation: the static answer stands.
        assert_eq!(manager.support_level(TrayFeature::Icon), SupportLevel::Full);

        // Inject the state without touching any bus: the manager only reads
        // what the registration path would have recorded.
        let inner = Arc::new(uda_core::tray::TrayIconInner::new("degraded".to_string()));
        manager.remember(&inner);
        assert_eq!(
            manager.support_level(TrayFeature::Icon),
            SupportLevel::Full,
            "a remembered icon without a degradation answers statically"
        );

        inner.set_degraded(Some(
            "no StatusNotifierWatcher is reachable on the session bus".to_string(),
        ));
        assert_eq!(
            manager.support_level(TrayFeature::Icon),
            SupportLevel::Partial(
                "no StatusNotifierWatcher is reachable on the session bus".to_string()
            )
        );
        // The degradation is about shell-side visibility only: the other
        // features keep their static answers.
        assert_eq!(
            manager.support_level(TrayFeature::Tooltip),
            SupportLevel::Full
        );
        assert!(
            manager
                .support_level(TrayFeature::DoubleClick)
                .reason()
                .is_some(),
            "the synthesised double click keeps its own Partial reason"
        );

        // Dropping the icon must return the answer to the static one: the
        // manager holds the icon weakly, so a gone icon cannot haunt the query.
        drop(inner);
        assert_eq!(manager.support_level(TrayFeature::Icon), SupportLevel::Full);
    }

    #[test]
    fn a_watcherless_session_records_the_degradation_on_the_created_icon() {
        // This is the transport-level half of the story: on the mock harness's
        // isolated bus (test-linux-mock.sh runs the suite inside
        // dbus-run-session) no StatusNotifierWatcher exists, so `create` takes
        // the degraded path deterministically. A real desktop session usually
        // owns the watcher name - and a host with no session bus at all fails
        // `create` for a different reason entirely - so the test stands down
        // in both cases instead of failing for the wrong environment.
        if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
            log::debug!("no session bus; the watcher-less transport test is skipped");
            return;
        }
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => {
                log::debug!("no test runtime: {error}");
                return;
            }
        };
        let watcher_absent = runtime.block_on(async {
            let Ok(connection) = Connection::session().await else {
                return None;
            };
            let Ok(proxy) = zbus::Proxy::new(
                &connection,
                "org.freedesktop.DBus",
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
            )
            .await
            else {
                return None;
            };
            let owned: Result<bool, _> = proxy.call("NameHasOwner", &WATCHER_SERVICE).await;
            owned.ok().map(|owned| !owned)
        });
        if watcher_absent != Some(true) {
            log::debug!("a watcher owns the tray name; the transport test is skipped");
            return;
        }

        let manager = LinuxTrayManager::new();
        let icon = manager
            .create(TrayIconConfig::new("uda-tray-degraded-probe"))
            .expect("the item must register even without a watcher");
        match icon.support_level(TrayFeature::Icon) {
            SupportLevel::Partial(reason) => {
                assert!(
                    !reason.trim().is_empty(),
                    "a degradation must carry a readable reason"
                );
                assert!(
                    reason.contains("StatusNotifierWatcher"),
                    "the reason must name what is missing: {reason}"
                );
            }
            other => panic!("a watcher-less session must degrade the icon, got {other:?}"),
        }
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
        // Only the tooltip is preset; everything else stays at its default, so
        // the fields are set in the initialiser rather than reassigned after.
        let mut state = TrayShared {
            tooltip: "tip".to_string(),
            ..TrayShared::default()
        };
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
        let snapshot = snapshot_of(&menu);
        let props = owned_props(snapshot.rows[0].properties());
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
        let snapshot = snapshot_of(&menu);
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
        let snapshot = snapshot_of(&menu);
        let label = snapshot
            .property(1, "label")
            .and_then(|v| String::try_from(v).ok());
        assert_eq!(label.as_deref(), Some("open"));
        assert!(snapshot.property(1, "no-such-property").is_none());
        assert!(snapshot.property(9_999, "label").is_none());
    }

    #[test]
    fn a_snapshot_without_a_menu_is_none() {
        assert!(MenuSnapshot::from_menu(None, &mut MenuIdMap::new()).is_none());
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
        assert_eq!(
            manager.capabilities(),
            LinuxTrayManager::advertised_capabilities()
        );
    }
}
