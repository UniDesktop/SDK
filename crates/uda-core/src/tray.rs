//! Platform-independent core for the system tray module.
//!
//! This module owns only *data model* and *lifecycle* types. The actual OS
//! integration lives in `uda-platform-linux` (`org.kde.StatusNotifierItem`) and
//! `uda-platform-windows` (`Shell_NotifyIconW`); see
//! `docs/internals/tray_specs.md` for the protocol details both backends must
//! honour.
//!
//! # Design highlights
//!
//! * **Never panic** - every fallible step returns `Result<_, UdaError>`; icon
//!   bytes and menu text supplied by the host are validated before use, and a
//!   poisoned lock is recovered instead of propagated (the menu is pure data,
//!   so continuing is strictly better than failing every later update).
//! * **Capability-driven** - [`TrayIcon::support_level`],
//!   [`TrayIcon::capabilities`] and [`TrayManager::support_level`] let a host
//!   degrade gracefully when a shell lacks a feature.
//! * **Non-blocking** - the platform backend owns a worker thread; [`TrayIcon`]
//!   is `Send + Sync` and every mutation is applied to shared state, so the
//!   host's main thread is never blocked or polled.
//! * **RAII** - dropping a [`TrayIcon`] marks it for unregistration, the same
//!   way [`crate::wakelock::WakeLockGuard`] releases its lock.

use crate::capability::{Capability, SupportLevel};
use crate::error::UdaError;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

/// A callback that fires when a menu row is activated, or on a tray interaction.
///
/// The callback runs on the **tray worker thread**. It must therefore be cheap,
/// must not block, and must not touch host UI state directly - forward the event
/// into the host's own loop instead.
///
/// Shared rather than owned so a [`MenuItem`] stays cheaply `Clone`: rows are
/// snapshotted on every layout export, and a boxed closure cannot be cloned.
/// Replacing a callback goes through `set_action`, which swaps the closure for
/// every holder of the same `Arc`.
///
/// The wrapper exists only to supply `Debug`, which a `dyn FnMut` does not
/// implement; without it `MenuItem` could not derive `Debug` and hosts would
/// lose the ability to log a menu tree.
#[derive(Clone)]
pub struct TrayAction(Arc<Mutex<dyn FnMut(&TrayEvent) + Send + 'static>>);

impl TrayAction {
    /// Wrap a callback.
    #[must_use]
    pub fn new<F>(callback: F) -> Self
    where
        F: FnMut(&TrayEvent) + Send + 'static,
    {
        Self(Arc::new(Mutex::new(callback)))
    }

    /// Invoke the callback.
    ///
    /// A poisoned lock is recovered: the closure owns its own state, so the
    /// worst case is a callback that was interrupted mid-update, which is still
    /// strictly better than failing every later activation.
    pub fn invoke(&self, event: &TrayEvent) {
        let mut callback = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::warn!("tray action lock poisoned; invoking the callback anyway");
                poisoned.into_inner()
            }
        };
        callback(event);
    }
}

impl fmt::Debug for TrayAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TrayAction(..)")
    }
}

/// Callback fired for a tray interaction.
///
/// A plain boxed closure rather than the shared [`TrayAction`] wrapper: there is
/// exactly one owner (the config) and no snapshotting, so there is nothing to
/// clone and no reason to pay for a `Mutex`.
pub type TrayEventHandler = Box<dyn FnMut(&TrayEvent) + Send + 'static>;

// ---------------------------------------------------------------------------
// Tooltips
// ---------------------------------------------------------------------------

/// Hard ceiling for a tooltip, in `char`s.
///
/// Windows `NOTIFYICONDATAW::szTip` holds 128 UTF-16 code units *including* its
/// NUL terminator, so 127 is the safety cap used cross-platform.
pub const TOOLTIP_MAX_CHARS: usize = 127;

/// Preferred tooltip length. Text longer than this triggers a
/// [`SupportLevel::Partial`] report rather than silent clipping.
pub const TOOLTIP_TARGET_CHARS: usize = 80;

/// Truncate `text` to at most `limit` `char`s without splitting a `char`.
///
/// Returns the original borrow when it already fits, keeping the common path
/// allocation-free.
#[must_use]
pub fn truncate_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((byte_index, _)) => &text[..byte_index],
        None => text,
    }
}

/// Clamp `text` to the platform-safe tooltip length.
///
/// No ellipsis is appended on purpose: U+2026 is non-ASCII, and this project
/// keeps its build scripts ASCII-only so a Windows `cp1252` console cannot trip
/// over them.
#[must_use]
pub fn sanitize_tooltip(text: &str) -> &str {
    truncate_chars(text, TOOLTIP_MAX_CHARS)
}

/// Whether `text` exceeds [`TOOLTIP_TARGET_CHARS`] and therefore needs clamping.
#[must_use]
pub fn tooltip_needs_truncation(text: &str) -> bool {
    text.chars().count() > TOOLTIP_TARGET_CHARS
}

// ---------------------------------------------------------------------------
// Menu model
// ---------------------------------------------------------------------------

/// Stable identifier of a row inside a [`TrayMenu`].
///
/// Ids are assigned by the menu and stay valid until the row is removed, which
/// is what lets a host keep a [`TrayMenuHandle`] across unrelated inserts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MenuItemId(u64);

impl MenuItemId {
    /// The raw numeric value.
    #[must_use]
    pub const fn into_raw(self) -> u64 {
        self.0
    }
}

impl fmt::Display for MenuItemId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tray-item-{}", self.0)
    }
}

/// Interaction state shared by every activatable menu variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MenuItemState {
    /// Whether the row accepts activation. `false` renders it greyed out.
    pub enabled: bool,
    /// Checkbox value. Ignored by non-checkbox variants.
    pub checked: bool,
}

impl MenuItemState {
    /// A plain, enabled, unchecked row.
    #[must_use]
    pub const fn enabled() -> Self {
        Self {
            enabled: true,
            checked: false,
        }
    }

    /// Whether this state renders as greyed out.
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        !self.enabled
    }

    /// A copy with `enabled` replaced.
    #[must_use]
    pub const fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// A copy with `checked` replaced.
    #[must_use]
    pub const fn with_checked(mut self, checked: bool) -> Self {
        self.checked = checked;
        self
    }
}

/// A single row of a [`TrayMenu`].
///
/// Note that labels are validated non-empty by [`TrayMenu::push`]; constructors
/// here stay infallible so item construction reads naturally, and the menu
/// enforces the invariant at insertion time.
#[derive(Debug, Clone)]
pub enum MenuItem {
    // `PartialEq` is hand-written below: `TrayAction` has no meaningful equality.
    /// Plain text row; optional callback fires on activation.
    Text {
        /// Displayed label.
        label: String,
        /// Interaction state.
        state: MenuItemState,
        /// Callback invoked when the row is activated.
        action: Option<TrayAction>,
    },
    /// Checkbox row; `state.checked` carries the current value.
    Checkbox {
        /// Displayed label.
        label: String,
        /// Interaction state.
        state: MenuItemState,
        /// Callback invoked when the row is toggled.
        action: Option<TrayAction>,
    },
    /// Visual separator. Never activatable and never labelled.
    Separator,
    /// Nested submenu. `children` is shared so a host can keep mutating the
    /// nested menu after the row was pushed.
    Submenu {
        /// Displayed label.
        label: String,
        /// Interaction state.
        state: MenuItemState,
        /// The nested rows.
        children: Arc<TrayMenu>,
    },
}

// `PartialEq` is hand-written because `TrayAction` is a shared closure with no
// meaningful equality: two rows that differ only in callback identity are equal
// for every purpose a host cares about, and a derived impl would not compile.
// Submenu children are excluded because `TrayMenu` is interior-mutable by
// design; use `items()` when a deep comparison is genuinely needed.
impl PartialEq for MenuItem {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Text {
                    label: left_label,
                    state: left_state,
                    ..
                },
                Self::Text {
                    label: right_label,
                    state: right_state,
                    ..
                },
            )
            | (
                Self::Checkbox {
                    label: left_label,
                    state: left_state,
                    ..
                },
                Self::Checkbox {
                    label: right_label,
                    state: right_state,
                    ..
                },
            )
            | (
                Self::Submenu {
                    label: left_label,
                    state: left_state,
                    ..
                },
                Self::Submenu {
                    label: right_label,
                    state: right_state,
                    ..
                },
            ) => left_label == right_label && left_state == right_state,
            (Self::Separator, Self::Separator) => true,
            _ => false,
        }
    }
}

impl Eq for MenuItem {}

impl MenuItem {
    /// A separator row.
    #[must_use]
    pub const fn separator() -> Self {
        Self::Separator
    }

    /// An enabled text row without a callback.
    #[must_use]
    pub fn text(label: impl Into<String>) -> Self {
        Self::Text {
            label: label.into(),
            state: MenuItemState::enabled(),
            action: None,
        }
    }

    /// An enabled text row with a callback.
    #[must_use]
    pub fn text_with_action<F>(label: impl Into<String>, action: F) -> Self
    where
        F: FnMut(&TrayEvent) + Send + 'static,
    {
        Self::Text {
            label: label.into(),
            state: MenuItemState::enabled(),
            action: Some(TrayAction::new(action)),
        }
    }

    /// A disabled text row.
    #[must_use]
    pub fn text_disabled(label: impl Into<String>) -> Self {
        Self::Text {
            label: label.into(),
            state: MenuItemState::default().with_enabled(false),
            action: None,
        }
    }

    /// An enabled checkbox that starts unchecked.
    #[must_use]
    pub fn checkbox(label: impl Into<String>) -> Self {
        Self::Checkbox {
            label: label.into(),
            state: MenuItemState::enabled(),
            action: None,
        }
    }

    /// An enabled checkbox that starts checked.
    #[must_use]
    pub fn checkbox_checked(label: impl Into<String>) -> Self {
        Self::Checkbox {
            label: label.into(),
            state: MenuItemState::enabled().with_checked(true),
            action: None,
        }
    }

    /// A submenu with initial children.
    #[must_use]
    pub fn submenu(label: impl Into<String>, children: Arc<TrayMenu>) -> Self {
        Self::Submenu {
            label: label.into(),
            state: MenuItemState::enabled(),
            children,
        }
    }

    /// The label, if this variant shows one; separators return `None`.
    #[must_use]
    pub fn label(&self) -> Option<&str> {
        match self {
            Self::Text { label, .. }
            | Self::Checkbox { label, .. }
            | Self::Submenu { label, .. } => Some(label),
            Self::Separator => None,
        }
    }

    /// The interaction state.
    #[must_use]
    pub const fn state(&self) -> MenuItemState {
        match self {
            Self::Text { state, .. } | Self::Checkbox { state, .. } | Self::Submenu { state, .. } => {
                *state
            }
            // A separator is never interactive; reporting a disabled state keeps
            // `is_disabled()` true without a special case at every call site.
            Self::Separator => MenuItemState {
                enabled: false,
                checked: false,
            },
        }
    }

    /// Whether the row is disabled (or is a separator).
    #[must_use]
    pub const fn is_disabled(&self) -> bool {
        !self.state().enabled
    }

    /// Whether this row is a checkbox.
    #[must_use]
    pub const fn is_checkbox(&self) -> bool {
        matches!(self, Self::Checkbox { .. })
    }

    /// Whether this row is a separator.
    #[must_use]
    pub const fn is_separator(&self) -> bool {
        matches!(self, Self::Separator)
    }

    /// Whether this row is a submenu.
    #[must_use]
    pub const fn is_submenu(&self) -> bool {
        matches!(self, Self::Submenu { .. })
    }

    /// Borrow the row's callback, if it has one.
    ///
    /// Exposed so a host (or the FFI layer's tests) can tell a row that *fires*
    /// from one that merely renders, without depending on the variant's private
    /// field. `Submenu` and `Separator` never carry a callback.
    #[must_use]
    pub fn action(&self) -> Option<&TrayAction> {
        match self {
            Self::Text { action, .. } | Self::Checkbox { action, .. } => action.as_ref(),
            Self::Submenu { .. } | Self::Separator => None,
        }
    }

    /// A copy with the label replaced; separators are unchanged.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        let label = label.into();
        match &mut self {
            Self::Text { label: slot, .. }
            | Self::Checkbox { label: slot, .. }
            | Self::Submenu { label: slot, .. } => *slot = label,
            Self::Separator => {}
        }
        self
    }

    /// A copy with the interaction state replaced; separators are unchanged.
    #[must_use]
    pub fn with_state(mut self, state: MenuItemState) -> Self {
        match &mut self {
            Self::Text { state: slot, .. }
            | Self::Checkbox { state: slot, .. }
            | Self::Submenu { state: slot, .. } => *slot = state,
            Self::Separator => {}
        }
        self
    }

    /// A copy with the callback replaced; separators and submenus are unchanged.
    #[must_use]
    pub fn with_action(mut self, action: Option<TrayAction>) -> Self {
        match &mut self {
            Self::Text { action: slot, .. } | Self::Checkbox { action: slot, .. } => *slot = action,
            Self::Submenu { .. } | Self::Separator => {}
        }
        self
    }

    /// A copy with the callback cleared; separators and submenus are unchanged.
    #[must_use]
    pub fn without_action(mut self) -> Self {
        match &mut self {
            Self::Text { action: slot, .. } | Self::Checkbox { action: slot, .. } => *slot = None,
            Self::Submenu { .. } | Self::Separator => {}
        }
        self
    }

    /// A copy with the checkbox value inverted; other variants are unchanged.
    #[must_use]
    pub fn toggled(self) -> Self {
        // Capture the state first: `with_state` consumes `self`, so reading the
        // old value inside the argument would borrow a moved value.
        let state = self.state();
        self.with_state(state.with_checked(!state.checked))
    }
}

/// A row plus its stable id, as stored inside a [`TrayMenu`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct Entry {
    id: MenuItemId,
    item: MenuItem,
}

/// A context menu attached to a tray icon.
///
/// Rows live behind a `Mutex`, which is what makes updates thread-safe: any
/// thread may lock, mutate the tree, and the backend re-exports the new layout
/// to the shell. The lock is never held while calling into the OS, so a slow
/// shell cannot block a host update indefinitely.
#[derive(Debug, Default)]
pub struct TrayMenu {
    entries: Mutex<Vec<Entry>>,
    next_id: Mutex<u64>,
}

impl TrayMenu {
    /// An empty menu.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a [`TrayMenuBuilder`].
    #[must_use]
    pub fn builder() -> TrayMenuBuilder {
        TrayMenuBuilder::default()
    }

    /// Lock the row list, recovering from a poisoned lock.
    fn lock_entries(&self) -> MutexGuard<'_, Vec<Entry>> {
        match self.entries.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::warn!("tray menu lock poisoned; recovering the row list");
                poisoned.into_inner()
            }
        }
    }

    /// Allocate the next monotonic id.
    fn allocate_id(&self) -> MenuItemId {
        let mut next_id = match self.next_id.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::warn!("tray menu id counter poisoned; recovering");
                poisoned.into_inner()
            }
        };
        // Saturating so the counter can never wrap into a duplicate id, even on
        // a pathological menu that is rebuilt billions of times.
        *next_id = next_id.saturating_add(1);
        MenuItemId(*next_id)
    }

    /// Reject rows that would be invisible yet still occupy a slot.
    fn validate(item: &MenuItem) -> Result<(), UdaError> {
        if let Some(label) = item.label() {
            if label.trim().is_empty() {
                return Err(UdaError::NotSupported(
                    "tray menu items must have a non-empty label".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Append a row and return its stable id.
    pub fn push(&self, item: MenuItem) -> Result<MenuItemId, UdaError> {
        Self::validate(&item)?;
        let id = self.allocate_id();
        self.lock_entries().push(Entry { id, item });
        Ok(id)
    }

    /// Insert a row at `index`, clamped to the end, and return its stable id.
    pub fn insert(&self, index: usize, item: MenuItem) -> Result<MenuItemId, UdaError> {
        Self::validate(&item)?;
        let id = self.allocate_id();
        let mut entries = self.lock_entries();
        let index = index.min(entries.len());
        entries.insert(index, Entry { id, item });
        Ok(id)
    }

    /// Remove the row addressed by `id`. Returns whether a row was removed.
    pub fn remove(&self, id: MenuItemId) -> bool {
        let mut entries = self.lock_entries();
        let before = entries.len();
        entries.retain(|entry| entry.id != id);
        entries.len() != before
    }

    /// Remove every row. The id counter keeps counting upward so live handles
    /// never alias a freshly inserted row.
    pub fn clear(&self) {
        self.lock_entries().clear();
    }

    /// Number of direct rows.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock_entries().len()
    }

    /// Whether the menu has no direct rows.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock_entries().is_empty()
    }

    /// Snapshot the rows for a backend to export.
    #[must_use]
    pub fn items(&self) -> Vec<MenuItem> {
        self.lock_entries()
            .iter()
            .map(|entry| entry.item.clone())
            .collect()
    }

    /// Snapshot the rows together with their ids.
    #[must_use]
    pub fn entries(&self) -> Vec<(MenuItemId, MenuItem)> {
        self.lock_entries()
            .iter()
            .map(|entry| (entry.id, entry.item.clone()))
            .collect()
    }

    /// Find a row by id.
    #[must_use]
    pub fn find(&self, id: MenuItemId) -> Option<MenuItem> {
        self.lock_entries()
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.item.clone())
    }

    /// Look up a row by label, returning the first match.
    #[must_use]
    pub fn find_by_label(&self, label: &str) -> Option<(MenuItemId, MenuItem)> {
        self.lock_entries()
            .iter()
            .find(|entry| entry.item.label() == Some(label))
            .map(|entry| (entry.id, entry.item.clone()))
    }

    /// Borrow a live handle to a row.
    ///
    /// Takes `self: &Arc<Self>` because a handle must hold a strong reference so
    /// the menu stays alive independently of the tray icon; a plain `&self`
    /// cannot express that lifetime.
    #[must_use]
    pub fn handle(self: &Arc<Self>, id: MenuItemId) -> Option<TrayMenuHandle> {
        if self.find(id).is_none() {
            return None;
        }
        Some(TrayMenuHandle::new(Arc::clone(self), id))
    }

    /// Replace a row in place. Returns `false` when the id is gone.
    fn replace(&self, id: MenuItemId, item: MenuItem) -> bool {
        let mut entries = self.lock_entries();
        match entries.iter_mut().find(|entry| entry.id == id) {
            Some(entry) => {
                entry.item = item;
                true
            }
            None => false,
        }
    }

    /// A copy with the row's label replaced; no-op when the id is gone.
    pub fn set_label(&self, id: MenuItemId, label: impl Into<String>) -> bool {
        match self.find(id) {
            Some(item) => self.replace(id, item.with_label(label)),
            None => false,
        }
    }

    /// A copy with the row's state replaced; no-op when the id is gone.
    pub fn set_state(&self, id: MenuItemId, state: MenuItemState) -> bool {
        match self.find(id) {
            Some(item) => self.replace(id, item.with_state(state)),
            None => false,
        }
    }

    /// Enable or disable a row; no-op when the id is gone.
    pub fn set_enabled(&self, id: MenuItemId, enabled: bool) -> bool {
        match self.find(id) {
            // Capture the state before `with_state` consumes the row.
            Some(item) => {
                let state = item.state().with_enabled(enabled);
                self.replace(id, item.with_state(state))
            }
            None => false,
        }
    }

    /// Set a checkbox's value; no-op for other variants or a gone id.
    pub fn set_checked(&self, id: MenuItemId, checked: bool) -> bool {
        match self.find(id) {
            Some(item) if item.is_checkbox() => {
                let state = item.state().with_checked(checked);
                self.replace(id, item.with_state(state))
            }
            _ => false,
        }
    }

    /// Replace a row's callback; no-op when the id is gone.
    pub fn set_action(&self, id: MenuItemId, action: Option<TrayAction>) -> bool {
        match self.find(id) {
            Some(item) => self.replace(id, item.with_action(action)),
            None => false,
        }
    }

    /// Invert a checkbox's value; no-op for other variants or a gone id.
    pub fn toggle(&self, id: MenuItemId) -> bool {
        match self.find(id) {
            Some(item) if item.is_checkbox() => self.replace(id, item.toggled()),
            _ => false,
        }
    }
}

/// Ergonomic builder for [`TrayMenu`].
#[derive(Debug, Default)]
pub struct TrayMenuBuilder {
    menu: TrayMenu,
}

impl TrayMenuBuilder {
    /// Start an empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a row.
    pub fn item(self, item: MenuItem) -> Result<Self, UdaError> {
        self.menu.push(item)?;
        Ok(self)
    }

    /// Append a text row.
    pub fn text(self, label: impl Into<String>) -> Result<Self, UdaError> {
        self.item(MenuItem::text(label))
    }

    /// Append a text row with a callback.
    pub fn text_with_action<F>(
        self,
        label: impl Into<String>,
        action: F,
    ) -> Result<Self, UdaError>
    where
        F: FnMut(&TrayEvent) + Send + 'static,
    {
        self.item(MenuItem::text_with_action(label, action))
    }

    /// Append a disabled text row.
    pub fn text_disabled(self, label: impl Into<String>) -> Result<Self, UdaError> {
        self.item(MenuItem::text_disabled(label))
    }

    /// Append a separator.
    pub fn separator(self) -> Result<Self, UdaError> {
        self.item(MenuItem::separator())
    }

    /// Append a checkbox.
    pub fn checkbox(self, label: impl Into<String>) -> Result<Self, UdaError> {
        self.item(MenuItem::checkbox(label))
    }

    /// Append a checkbox that starts checked.
    pub fn checkbox_checked(self, label: impl Into<String>) -> Result<Self, UdaError> {
        self.item(MenuItem::checkbox_checked(label))
    }

    /// Append a submenu.
    pub fn submenu(
        self,
        label: impl Into<String>,
        children: Arc<TrayMenu>,
    ) -> Result<Self, UdaError> {
        self.item(MenuItem::submenu(label, children))
    }

    /// Finish the menu.
    #[must_use]
    pub fn build(self) -> TrayMenu {
        self.menu
    }
}

/// A live reference to one row of a [`TrayMenu`].
///
/// The handle addresses the row by its stable [`MenuItemId`], so inserting a row
/// above it does not invalidate it.
#[derive(Debug, Clone)]
pub struct TrayMenuHandle {
    menu: Arc<TrayMenu>,
    id: MenuItemId,
}

impl TrayMenuHandle {
    /// Wrap an existing `Arc<TrayMenu>` and id. Backend and host use.
    #[must_use]
    pub(crate) const fn new(menu: Arc<TrayMenu>, id: MenuItemId) -> Self {
        Self { menu, id }
    }

    /// The stable id this handle addresses.
    #[must_use]
    pub const fn id(&self) -> MenuItemId {
        self.id
    }

    /// The current row, if it still exists.
    #[must_use]
    pub fn item(&self) -> Option<MenuItem> {
        self.menu.find(self.id)
    }

    /// Replace the row's label; no-op when the id is gone.
    pub fn set_label(&self, label: impl Into<String>) -> bool {
        self.menu.set_label(self.id, label)
    }

    /// Replace the row's interaction state; no-op when the id is gone.
    pub fn set_state(&self, state: MenuItemState) -> bool {
        self.menu.set_state(self.id, state)
    }

    /// Enable or disable the row; no-op when the id is gone.
    pub fn set_enabled(&self, enabled: bool) -> bool {
        self.menu.set_enabled(self.id, enabled)
    }

    /// Set a checkbox's value; no-op for other variants or a gone id.
    pub fn set_checked(&self, checked: bool) -> bool {
        self.menu.set_checked(self.id, checked)
    }

    /// Replace the row's callback; no-op when the id is gone.
    pub fn set_action(&self, action: Option<TrayAction>) -> bool {
        self.menu.set_action(self.id, action)
    }

    /// Invert a checkbox's value; no-op for other variants or a gone id.
    pub fn toggle(&self) -> bool {
        self.menu.toggle(self.id)
    }

    /// Remove the row; no-op when the id is gone.
    pub fn remove(&self) -> bool {
        self.menu.remove(self.id)
    }
}

// ---------------------------------------------------------------------------
// Icon
// ---------------------------------------------------------------------------

/// Source image for a tray icon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrayIconSource {
    /// A file path or freedesktop icon-theme name, resolved by the backend.
    Path(String),
    /// Raw RGBA bytes, top-down; `stride` may exceed `width * 4` to describe
    /// padded rows.
    Rgba {
        /// Pixel width.
        width: u32,
        /// Pixel height.
        height: u32,
        /// Bytes per row; must be at least `width * 4`.
        stride: u32,
        /// At least `stride * height` bytes.
        data: Vec<u8>,
    },
}

/// Reason a [`TrayIconSource`] was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconError {
    /// Zero width or height.
    EmptyDimensions,
    /// `stride` is smaller than `width * 4`.
    StrideTooSmall,
    /// `data.len()` is smaller than `stride * height`.
    DataTooShort,
    /// An empty path or icon-theme name.
    EmptyPath,
}

impl fmt::Display for IconError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyDimensions => write!(f, "icon width and height must both be non-zero"),
            Self::StrideTooSmall => write!(f, "icon stride must be at least width * 4"),
            Self::DataTooShort => write!(f, "icon data is shorter than stride * height"),
            Self::EmptyPath => write!(f, "icon path must not be empty"),
        }
    }
}

impl std::error::Error for IconError {}

impl TrayIconSource {
    /// Validate the source.
    ///
    /// Both backends call this before touching the OS, so a host-supplied icon
    /// is rejected with a typed error instead of crashing inside FFI.
    pub fn validate(&self) -> Result<(), IconError> {
        match self {
            Self::Path(path) => {
                if path.trim().is_empty() {
                    return Err(IconError::EmptyPath);
                }
                Ok(())
            }
            Self::Rgba {
                width,
                height,
                stride,
                data,
            } => {
                if *width == 0 || *height == 0 {
                    return Err(IconError::EmptyDimensions);
                }
                let row_bytes = width.checked_mul(4).ok_or(IconError::StrideTooSmall)?;
                if *stride < row_bytes {
                    return Err(IconError::StrideTooSmall);
                }
                let needed = stride
                    .checked_mul(*height)
                    .ok_or(IconError::DataTooShort)? as usize;
                if data.len() < needed {
                    return Err(IconError::DataTooShort);
                }
                Ok(())
            }
        }
    }
}

/// An interaction reported by the platform tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Primary (left) click.
    Click,
    /// Double click. Without a native event (Linux SNI) this is synthesised from
    /// two [`TrayEvent::Click`]s inside a short window.
    DoubleClick,
}

/// Configuration collected by [`TrayIconBuilder`].
#[derive(Default)]
pub(crate) struct TrayConfig {
    name: Option<String>,
    icon: Option<TrayIconSource>,
    tooltip: String,
    menu: Option<Arc<TrayMenu>>,
    on_click: Option<TrayEventHandler>,
    on_double_click: Option<TrayEventHandler>,
}

/// Builder for [`TrayIconConfig`].
///
/// Construction never touches the OS: `build()` only packages the
/// configuration. Registration happens when the config is handed to
/// [`TrayManager::create`].
#[derive(Default)]
pub struct TrayIconBuilder {
    config: TrayConfig,
}

impl TrayIconBuilder {
    /// An empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Application name used for registration (D-Bus bus name / window class).
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = Some(name.into());
        self
    }

    /// Icon from a file path or freedesktop icon-theme name.
    #[must_use]
    pub fn icon_path(mut self, path: impl Into<String>) -> Self {
        self.config.icon = Some(TrayIconSource::Path(path.into()));
        self
    }

    /// Icon from raw RGBA bytes.
    #[must_use]
    pub fn icon_rgba(mut self, width: u32, height: u32, stride: u32, data: Vec<u8>) -> Self {
        self.config.icon = Some(TrayIconSource::Rgba {
            width,
            height,
            stride,
            data,
        });
        self
    }

    /// Tooltip text. Over-long values are clamped by the backend, which then
    /// reports [`SupportLevel::Partial`].
    #[must_use]
    pub fn tooltip(mut self, tooltip: impl Into<String>) -> Self {
        self.config.tooltip = tooltip.into();
        self
    }

    /// Attach a context menu.
    #[must_use]
    pub fn menu(mut self, menu: Arc<TrayMenu>) -> Self {
        self.config.menu = Some(menu);
        self
    }

    /// Register the left-click callback.
    #[must_use]
    pub fn on_click(mut self, handler: TrayEventHandler) -> Self {
        self.config.on_click = Some(handler);
        self
    }

    /// Register the double-click callback.
    #[must_use]
    pub fn on_double_click(mut self, handler: TrayEventHandler) -> Self {
        self.config.on_double_click = Some(handler);
        self
    }

    /// Finish the configuration.
    #[must_use]
    pub fn build(self) -> TrayIconConfig {
        TrayIconConfig {
            name: self
                .config
                .name
                .unwrap_or_else(|| crate::DEFAULT_APP_NAME.to_string()),
            icon: self.config.icon,
            tooltip: self.config.tooltip,
            menu: self.config.menu,
            on_click: self.config.on_click,
            on_double_click: self.config.on_double_click,
        }
    }
}

/// A fully specified, OS-independent tray icon description.
///
/// Handed to a platform backend by [`TrayManager::create`].
pub struct TrayIconConfig {
    /// Application name for registration.
    pub name: String,
    /// Icon image, if any.
    pub icon: Option<TrayIconSource>,
    /// Tooltip text; may be empty.
    pub tooltip: String,
    /// Context menu, if any.
    pub menu: Option<Arc<TrayMenu>>,
    /// Left-click handler.
    pub on_click: Option<TrayEventHandler>,
    /// Double-click handler.
    pub on_double_click: Option<TrayEventHandler>,
}

impl TrayIconConfig {
    /// A config with only the application name set.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            icon: None,
            tooltip: String::new(),
            menu: None,
            on_click: None,
            on_double_click: None,
        }
    }
}

impl Default for TrayIconConfig {
    fn default() -> Self {
        Self::new(crate::DEFAULT_APP_NAME)
    }
}

impl fmt::Debug for TrayIconConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrayIconConfig")
            .field("name", &self.name)
            .field("icon", &self.icon)
            .field("tooltip", &self.tooltip)
            .field("menu", &self.menu.as_ref().map(|menu| menu.len()))
            .field("on_click", &self.on_click.is_some())
            .field("on_double_click", &self.on_double_click.is_some())
            .finish()
    }
}

/// Which individual tray feature a capability query refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrayFeature {
    /// Showing and hiding the icon.
    Icon,
    /// Tooltip / hover text.
    Tooltip,
    /// Single left-click event.
    Click,
    /// Double-click event.
    DoubleClick,
    /// Right-click context menu.
    ContextMenu,
    /// Checkbox rows inside the menu.
    Checkbox,
    /// Runtime menu mutation.
    DynamicMenu,
}

impl fmt::Display for TrayFeature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Icon => write!(f, "tray icon"),
            Self::Tooltip => write!(f, "tray tooltip"),
            Self::Click => write!(f, "tray click event"),
            Self::DoubleClick => write!(f, "tray double-click event"),
            Self::ContextMenu => write!(f, "tray context menu"),
            Self::Checkbox => write!(f, "tray checkbox items"),
            Self::DynamicMenu => write!(f, "tray menu updates"),
        }
    }
}

/// The mutable part of a live icon, shared with the platform worker thread.
///
/// `Default` is hand-written because `Capability` has no `Default` impl, and
/// deriving would silently require one. Starting empty is the deliberate
/// choice: nothing is claimed until a backend proves it.
///
/// Public since the platform backends took ownership of it (Phase 2, Step 2): a
/// backend must read the very state the host writes, otherwise its worker thread
/// could only ever serve a stale copy.
pub struct TrayIconState {
    /// Current tooltip text.
    pub tooltip: String,
    /// Current icon, if one is set.
    pub icon: Option<TrayIconSource>,
    /// Current context menu, if one is attached.
    pub menu: Option<Arc<TrayMenu>>,
    /// Whether the icon is shown.
    pub visible: bool,
    /// Set once and never cleared: makes `Drop` idempotent even under races.
    pub shutdown: bool,
    /// Feature support reported by the backend that registered this icon.
    ///
    /// Empty until the backend publishes it, which is why the accessors below
    /// fall back to a conservative answer instead of claiming a feature.
    pub capabilities: Capability,
}

/// Shared state of a [`TrayIcon`]. Opaque to hosts except through the accessors
/// on [`TrayIcon`]; the platform backends read it directly to mirror host
/// updates into their own worker thread.
pub struct TrayIconInner {
    /// Application name used at registration.
    pub name: String,
    /// The state every holder observes.
    pub state: Mutex<TrayIconState>,
}

impl Default for TrayIconState {
    fn default() -> Self {
        Self {
            tooltip: String::new(),
            icon: None,
            menu: None,
            visible: true,
            shutdown: false,
            capabilities: Capability::empty(),
        }
    }
}

impl TrayIconInner {
    /// Wrap shared state. Backend use only.
    ///
    /// A backend calls this once, hands the `Arc` to its worker thread, and
    /// returns the matching [`TrayIcon`] to the host. The two halves then stay
    /// in sync through this state.
    #[must_use]
    pub fn new(name: String) -> Self {
        Self {
            name,
            state: Mutex::new(TrayIconState {
                tooltip: String::new(),
                icon: None,
                menu: None,
                visible: true,
                shutdown: false,
                capabilities: Capability::empty(),
            }),
        }
    }

    /// Publish what the active backend actually supports. Backend use only.
    ///
    /// Called once registration succeeds, so a host that queries a freshly
    /// created icon never sees a stale or optimistic answer.
    pub fn set_capabilities(&self, capabilities: Capability) {
        self.lock_state().capabilities = capabilities;
    }

    /// Lock the shared state, recovering from a poisoned lock.
    ///
    /// The icon is pure data plus a "please unregister" flag; every field is
    /// independently overwritable, so resuming after a panic in one callback is
    /// safe and far better than failing all later updates.
    pub fn lock_state(&self) -> MutexGuard<'_, TrayIconState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                log::warn!("tray '{}' state lock poisoned; recovering", self.name);
                poisoned.into_inner()
            }
        }
    }
}

/// A live tray icon, owned by the host.
///
/// All handles are `Send + Sync`, so any thread may update the tooltip, swap the
/// icon, or replace the menu without synchronising with the host's UI thread.
pub struct TrayIcon {
    inner: Arc<TrayIconInner>,
}

impl fmt::Debug for TrayIcon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrayIcon")
            .field("name", &self.inner.name)
            .field("visible", &self.is_visible())
            .finish_non_exhaustive()
    }
}

impl TrayIcon {
    /// Wrap backend-owned shared state. Backend use only.
    ///
    /// The backend creates the state, keeps an `Arc` for its worker thread, and
    /// returns the handle built here to the host.
    #[must_use]
    pub fn from_inner(inner: Arc<TrayIconInner>) -> Self {
        Self { inner }
    }

    /// Access the shared state. Backend use only.
    ///
    /// Read-only on purpose: a backend observes the host's updates through it
    /// and must not mutate the state behind the host's back.
    #[must_use]
    pub const fn inner(&self) -> &Arc<TrayIconInner> {
        &self.inner
    }

    /// The application name used at registration.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// Replace the tooltip, clamped to [`TOOLTIP_MAX_CHARS`].
    ///
    /// Never fails: clamping is the documented degradation path. Use
    /// [`tooltip_needs_truncation`] first if the host wants to warn the user.
    pub fn set_tooltip(&self, tooltip: impl Into<String>) {
        let tooltip = sanitize_tooltip(&tooltip.into()).to_string();
        self.inner.lock_state().tooltip = tooltip;
        log::debug!("tray '{}' tooltip updated", self.inner.name);
    }

    /// The current tooltip text.
    #[must_use]
    pub fn tooltip(&self) -> String {
        self.inner.lock_state().tooltip.clone()
    }

    /// Replace the icon.
    ///
    /// The new source is validated first, so an invalid value leaves the
    /// previous icon in place and returns a typed error.
    pub fn set_icon(&self, icon: TrayIconSource) -> Result<(), UdaError> {
        icon.validate().map_err(|error| {
            log::warn!("tray '{}' rejected an icon: {error}", self.inner.name);
            UdaError::NotSupported(format!("invalid tray icon: {error}"))
        })?;
        self.inner.lock_state().icon = Some(icon);
        log::debug!("tray '{}' icon updated", self.inner.name);
        Ok(())
    }

    /// Replace the context menu.
    pub fn set_menu(&self, menu: Arc<TrayMenu>) {
        self.inner.lock_state().menu = Some(menu);
        log::debug!("tray '{}' menu replaced", self.inner.name);
    }

    /// Drop the context menu, leaving a bare icon.
    pub fn clear_menu(&self) {
        self.inner.lock_state().menu = None;
        log::debug!("tray '{}' menu cleared", self.inner.name);
    }

    /// The current context menu, if one is attached.
    ///
    /// The backend reads this to re-export a layout whenever the host mutates
    /// the menu, so it must observe the live `Arc` rather than a snapshot.
    #[must_use]
    pub fn menu(&self) -> Option<Arc<TrayMenu>> {
        self.inner.lock_state().menu.clone()
    }

    /// Hide the icon without unregistering it.
    pub fn hide(&self) {
        self.inner.lock_state().visible = false;
        log::debug!("tray '{}' hidden", self.inner.name);
    }

    /// Show a previously hidden icon.
    pub fn show(&self) {
        self.inner.lock_state().visible = true;
        log::debug!("tray '{}' shown", self.inner.name);
    }

    /// Whether the icon is currently shown.
    #[must_use]
    pub fn is_visible(&self) -> bool {
        self.inner.lock_state().visible
    }

    /// Support level for one feature on the active backend.
    ///
    /// Answers from the capability set the backend published at registration, so
    /// the value is always a statement about *this* icon's environment. Before a
    /// backend publishes, the answer stays [`SupportLevel::None`]: the honest
    /// default when no tray mechanism could be reached at all.
    #[must_use]
    pub fn support_level(&self, feature: TrayFeature) -> SupportLevel {
        let capabilities = self.inner.lock_state().capabilities;
        if !capabilities.contains(Capability::SYSTEM_TRAY) {
            return SupportLevel::None;
        }
        // `Icon` is the one feature every tray backend guarantees once an icon
        // exists; `Tooltip` rides along because every backend exposes hover text.
        match feature {
            TrayFeature::Icon | TrayFeature::Tooltip => SupportLevel::Full,
            other if capabilities.contains(feature_flag(other)) => SupportLevel::Full,
            // An absent flag is a plain `None`: a backend either answers for a
            // feature or does not. `Partial` stays reserved for backends that
            // publish a degraded answer explicitly (e.g. Linux double-click
            // synthesis), which is expressed through their own flag set.
            other => {
                log::debug!(
                    "tray feature '{other}' is not reported by this backend; treating it as unavailable"
                );
                SupportLevel::None
            }
        }
    }

    /// The capability set of the active backend.
    ///
    /// Backends that cannot reach any tray mechanism report an empty set, which
    /// is the caller's cue to hide its tray UI entirely.
    #[must_use]
    pub fn capabilities(&self) -> Capability {
        self.inner.lock_state().capabilities
    }
}

/// The capability flag a feature maps onto.
///
/// Features with no flag of their own reuse `SYSTEM_TRAY`, because a backend
/// that can host an icon can always show it.
#[must_use]
fn feature_flag(feature: TrayFeature) -> Capability {
    match feature {
        TrayFeature::Icon => Capability::TRAY_ICON,
        TrayFeature::Tooltip => Capability::TRAY_TOOLTIP,
        TrayFeature::Click => Capability::TRAY_CLICK,
        TrayFeature::DoubleClick => Capability::TRAY_DOUBLE_CLICK,
        TrayFeature::ContextMenu => Capability::TRAY_CONTEXT_MENU,
        TrayFeature::Checkbox => Capability::TRAY_CHECKBOX,
        TrayFeature::DynamicMenu => Capability::TRAY_DYNAMIC_MENU,
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        // Idempotent by construction: only the first caller observes
        // `shutdown == false`, so a double drop, a drop during teardown, or a
        // drop racing a menu open can never unregister twice.
        let first_shutdown = {
            let mut state = self.inner.lock_state();
            let first = !state.shutdown;
            state.shutdown = true;
            first
        };
        if first_shutdown {
            log::debug!(
                "tray '{}' dropping; the backend unregisters it from the taskbar",
                self.inner.name
            );
        }
    }
}

/// Cross-platform tray management interface.
///
/// Implemented by `uda-platform-linux` (StatusNotifierItem, with an
/// AppIndicator fallback) and `uda-platform-windows` (`Shell_NotifyIconW` on a
/// dedicated worker thread).
pub trait TrayManager {
    /// Register a tray icon and return its handle.
    ///
    /// The backend starts its worker thread on first use and keeps it alive for
    /// as long as any handle exists.
    fn create(&self, config: TrayIconConfig) -> Result<TrayIcon, UdaError>;

    /// The capability set of the active backend.
    fn capabilities(&self) -> Capability;

    /// Detailed support level for one feature.
    fn support_level(&self, feature: TrayFeature) -> SupportLevel;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn truncate_chars_never_splits_a_character() {
        // "aé€" is 3 chars but 6 bytes; cutting at 2 must land on a boundary.
        let text = "a\u{00e9}\u{20ac}";
        assert_eq!(truncate_chars(text, 2), "a\u{00e9}");
        assert_eq!(truncate_chars(text, 3), text);
        assert_eq!(truncate_chars(text, 99), text);
        assert_eq!(truncate_chars("", 1), "");
    }

    #[test]
    fn sanitize_tooltip_respects_the_hard_cap() {
        let long: String = "x".repeat(TOOLTIP_MAX_CHARS + 40);
        assert_eq!(sanitize_tooltip(&long).chars().count(), TOOLTIP_MAX_CHARS);
        assert!(tooltip_needs_truncation(&long));
        assert!(!tooltip_needs_truncation("short"));
    }

    #[test]
    fn push_rejects_an_blank_label() {
        let menu = TrayMenu::new();
        match menu.push(MenuItem::text("   ")) {
            Err(UdaError::NotSupported(_)) => {}
            other => panic!("expected NotSupported, got {other:?}"),
        }
        assert!(menu.is_empty());
        // A separator has no label, so it must pass validation.
        assert!(menu.push(MenuItem::separator()).is_ok());
    }

    #[test]
    fn ids_are_stable_across_inserts_and_removals() {
        let menu = TrayMenu::new();
        let first = match menu.push(MenuItem::text("first")) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        let second = match menu.push(MenuItem::text("second")) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        assert_ne!(first, second);

        // Inserting above must not shift the ids of existing rows.
        assert!(menu.insert(0, MenuItem::text("inserted")).is_ok());
        assert_eq!(menu.len(), 3);
        assert_eq!(menu.find(first).and_then(|item| item.label().map(str::to_string)), Some("first".to_string()));
        assert_eq!(menu.find(second).expect("row survives").label(), Some("second"));

        assert!(menu.remove(first));
        assert!(!menu.remove(first));
        assert_eq!(menu.find(first), None);
        assert!(menu.find(second).is_some());
        assert_eq!(menu.len(), 2);
    }

    #[test]
    fn clear_resets_rows_but_not_the_id_counter() {
        let menu = TrayMenu::new();
        let before = match menu.push(MenuItem::text("a")) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        menu.clear();
        assert!(menu.is_empty());
        let after = match menu.push(MenuItem::text("b")) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        // Reusing an id would make a stale handle alias the new row.
        assert_ne!(before, after);
    }

    #[test]
    fn checkbox_semantics_are_enforced() {
        assert!(MenuItem::checkbox_checked("on").state().checked);
        assert!(!MenuItem::checkbox("off").state().checked);

        let menu = TrayMenu::new();
        let checkbox = match menu.push(MenuItem::checkbox("toggle")) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        assert!(menu.toggle(checkbox));
        assert!(menu.find(checkbox).expect("row survives").state().checked);

        let text = match menu.push(MenuItem::text("plain")) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        // Non-checkbox rows ignore checkbox operations instead of corrupting.
        assert!(!menu.toggle(text));
        assert!(!menu.set_checked(text, true));
        assert!(!menu.find(text).expect("row survives").state().checked);
    }

    #[test]
    fn separators_are_never_activatable() {
        let item = MenuItem::separator();
        assert!(item.is_separator());
        assert!(item.is_disabled());
        assert_eq!(item.label(), None);
        assert_eq!(item.state(), MenuItemState::default().with_enabled(false));
    }

    #[test]
    fn dynamic_updates_rewrite_labels_and_state() {
        let menu = TrayMenu::new();
        let id = match menu.push(MenuItem::text_disabled("off")) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        assert!(menu.set_enabled(id, true));
        assert!(!menu.find(id).expect("row survives").is_disabled());
        assert!(menu.set_label(id, "renamed"));
        assert_eq!(menu.find(id).and_then(|item| item.label().map(str::to_string)), Some("renamed".to_string()));
        // Unknown ids are reported as no-ops rather than panicking.
        let unknown = MenuItemId(999_999);
        assert!(!menu.set_label(unknown, "ghost"));
        assert!(!menu.set_enabled(unknown, false));
        assert!(!menu.toggle(unknown));
    }

    #[test]
    fn menu_items_compare_without_touching_callbacks() {
        // Equality ignores callback identity; a derived impl would not compile.
        assert_eq!(MenuItem::text("same"), MenuItem::text("same"));
        assert_ne!(MenuItem::text("a"), MenuItem::text("b"));
        assert_ne!(MenuItem::text("a"), MenuItem::checkbox("a"));
        assert_eq!(MenuItem::separator(), MenuItem::separator());
        assert_ne!(MenuItem::text("a"), MenuItem::separator());
    }

    #[test]
    fn a_handle_survives_unrelated_mutation() {
        let menu = Arc::new(TrayMenu::new());
        let id = match menu.push(MenuItem::checkbox("watched")) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        let handle = menu.handle(id).expect("handle is created for a live row");
        // An insert above must not invalidate the handle.
        assert!(menu.insert(0, MenuItem::text("above")).is_ok());
        assert!(handle.set_checked(true));
        assert!(menu.find(id).expect("row survives").state().checked);

        let missing = menu.handle(MenuItemId(4_042));
        assert!(missing.is_none());
    }

    #[test]
    fn the_builder_collects_rows_in_order() {
        static FIRED: AtomicUsize = AtomicUsize::new(0);
        let built = TrayMenu::builder()
            .text("open")
            .and_then(|builder| builder.separator())
            .and_then(|builder| builder.checkbox_checked("pin"))
            .and_then(|builder| {
                builder.text_with_action("quit", move |_event: &TrayEvent| {
                    FIRED.fetch_add(1, Ordering::SeqCst);
                })
            })
            .and_then(|builder| Ok(builder.build()));
        let menu = match built {
            Ok(menu) => menu,
            Err(error) => panic!("builder failed: {error}"),
        };
        let labels: Vec<String> = menu
            .items()
            .iter()
            .map(|item| item.label().unwrap_or("|").to_string())
            .collect();
        assert_eq!(
            labels,
            vec![
                "open".to_string(),
                "|".to_string(),
                "pin".to_string(),
                "quit".to_string()
            ]
        );

        // The action compiled into the menu is invocable.
        let quit = menu
            .find_by_label("quit")
            .map(|(id, _)| id)
            .expect("row exists");
        match menu.find(quit) {
            Some(MenuItem::Text { action: Some(action), .. }) => action.invoke(&TrayEvent::Click),
            other => panic!("expected a text row carrying an action, got {other:?}"),
        }
        assert_eq!(FIRED.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn nested_submenus_keep_their_children() {
        let child = Arc::new(TrayMenu::new());
        assert!(child.push(MenuItem::text("inner")).is_ok());
        let menu = TrayMenu::new();
        let parent = match menu.push(MenuItem::submenu("outer", Arc::clone(&child))) {
            Ok(id) => id,
            Err(error) => panic!("push failed: {error}"),
        };
        match menu.find(parent) {
            // The row borrows the same menu the host holds, so mutating the
            // child afterwards is visible through the submenu.
            Some(MenuItem::Submenu { children, .. }) => {
                assert!(Arc::ptr_eq(&children, &child));
                assert_eq!(children.len(), 1);
            }
            other => panic!("expected a submenu, got {other:?}"),
        }
    }

    #[test]
    fn rgba_icons_are_validated_before_reaching_the_backend() {
        assert_eq!(
            TrayIconSource::Rgba {
                width: 2,
                height: 2,
                stride: 8,
                data: vec![0; 16],
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            TrayIconSource::Rgba {
                width: 0,
                height: 2,
                stride: 0,
                data: Vec::new(),
            }
            .validate(),
            Err(IconError::EmptyDimensions)
        );
        assert_eq!(
            TrayIconSource::Rgba {
                width: 2,
                height: 2,
                stride: 7,
                data: vec![0; 14],
            }
            .validate(),
            Err(IconError::StrideTooSmall)
        );
        assert_eq!(
            TrayIconSource::Rgba {
                width: 2,
                height: 2,
                stride: 8,
                data: vec![0; 15],
            }
            .validate(),
            Err(IconError::DataTooShort)
        );
        // Saturating dimensions must be rejected by the checked arithmetic, not
        // wrap into a small buffer and pass. `u32::MAX * 4` overflows, so the
        // stride comparison catches it before the buffer size is ever computed.
        assert_eq!(
            TrayIconSource::Rgba {
                width: u32::MAX,
                height: u32::MAX,
                stride: u32::MAX,
                data: vec![0; 4],
            }
            .validate(),
            Err(IconError::StrideTooSmall)
        );
        // A stride that fits in `u32` but whose `stride * height` overflows is
        // caught by the second checked multiplication instead.
        assert_eq!(
            TrayIconSource::Rgba {
                width: 4,
                height: u32::MAX,
                stride: 0x8000_0000,
                data: vec![0; 4],
            }
            .validate(),
            Err(IconError::DataTooShort)
        );
        assert_eq!(
            TrayIconSource::Path("   ".to_string()).validate(),
            Err(IconError::EmptyPath)
        );
        assert_eq!(
            TrayIconSource::Path("app.png".to_string()).validate(),
            Ok(())
        );
    }

    #[test]
    fn a_valid_icon_is_applied_and_an_invalid_one_is_kept_out() {
        let icon = TrayIcon::from_inner(Arc::new(TrayIconInner::new("test".to_string())));
        let good = TrayIconSource::Path("good.png".to_string());
        assert!(icon.set_icon(good).is_ok());

        let bad = TrayIconSource::Path(String::new());
        match icon.set_icon(bad) {
            Err(UdaError::NotSupported(_)) => {}
            other => panic!("expected NotSupported, got {other:?}"),
        }
        // The previous icon must survive a rejected update.
        let kept = icon.inner().lock_state().icon.clone();
        assert_eq!(kept, Some(TrayIconSource::Path("good.png".to_string())));
    }

    #[test]
    fn tooltip_updates_are_clamped_and_readable() {
        let icon = TrayIcon::from_inner(Arc::new(TrayIconInner::new("test".to_string())));
        icon.set_tooltip("hi");
        assert_eq!(icon.tooltip(), "hi");
        let long: String = "y".repeat(TOOLTIP_MAX_CHARS * 2);
        icon.set_tooltip(long);
        assert_eq!(icon.tooltip().chars().count(), TOOLTIP_MAX_CHARS);
    }

    #[test]
    fn menu_and_visibility_round_trip() {
        let icon = TrayIcon::from_inner(Arc::new(TrayIconInner::new("test".to_string())));
        assert!(icon.is_visible());
        let menu = Arc::new(TrayMenu::new());
        icon.set_menu(Arc::clone(&menu));
        icon.clear_menu();
        icon.hide();
        assert!(!icon.is_visible());
        icon.show();
        assert!(icon.is_visible());
        assert_eq!(menu.len(), 0);
    }

    #[test]
    fn capabilities_stay_empty_until_a_backend_publishes() {
        let inner = Arc::new(TrayIconInner::new("test".to_string()));
        let icon = TrayIcon::from_inner(Arc::clone(&inner));
        // Nothing published yet: the honest answer is "no tray".
        assert_eq!(icon.capabilities(), Capability::empty());
        assert_eq!(icon.support_level(TrayFeature::Icon), SupportLevel::None);
        assert_eq!(icon.support_level(TrayFeature::DoubleClick), SupportLevel::None);

        inner.set_capabilities(Capability::SYSTEM_TRAY);
        assert_eq!(icon.capabilities(), Capability::SYSTEM_TRAY);
        assert_eq!(icon.support_level(TrayFeature::Icon), SupportLevel::Full);
        // Unclaimed features are `None`, never an optimistic `Partial`.
        assert_eq!(icon.support_level(TrayFeature::DoubleClick), SupportLevel::None);
    }

    #[test]
    fn feature_display_names_are_stable() {
        assert_eq!(TrayFeature::Icon.to_string(), "tray icon");
        assert_eq!(TrayFeature::DoubleClick.to_string(), "tray double-click event");
        assert_eq!(MenuItemId(7).to_string(), "tray-item-7");
    }

    #[test]
    fn dropping_an_icon_marks_it_shut_down_once() {
        let inner = Arc::new(TrayIconInner::new("test".to_string()));
        {
            let _icon = TrayIcon::from_inner(Arc::clone(&inner));
            assert!(!inner.lock_state().shutdown);
        }
        assert!(inner.lock_state().shutdown);
        // A second guard over the same inner state observes the flag already set,
        // which is what keeps unregistration idempotent under races.
        let guard = TrayIcon::from_inner(Arc::clone(&inner));
        drop(guard);
        assert!(inner.lock_state().shutdown);
    }

    #[test]
    fn tray_handles_are_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        // Live handles must be shareable across threads: the host may update the
        // tooltip from a worker while the tray thread renders.
        assert_send_sync::<TrayIcon>();
        assert_send_sync::<TrayMenu>();
        assert_send_sync::<TrayMenuHandle>();
        assert_send_sync::<MenuItem>();

        // A config only travels once, from the host to the backend that takes
        // ownership of its callbacks, so `Send` alone is the right bound. A
        // `Box<dyn FnMut>` is deliberately not `Sync`.
        fn assert_send<T: Send>() {}
        assert_send::<TrayIconConfig>();
        assert_send::<TrayAction>();
        assert_send::<TrayEventHandler>();
    }

}
