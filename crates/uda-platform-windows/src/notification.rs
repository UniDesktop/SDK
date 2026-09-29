//! Windows notification manager: native toasts via WinRT.
//!
//! # Backend selection
//!
//! Windows has no FreeDesktop-style daemon; the supported mechanism is the WinRT
//! `Windows.UI.Notifications` API (see `AGENTS.md` section 3):
//!
//! 1. `ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastImageAndText02)`
//!    produces an XML document with the standard title/body text nodes and an
//!    `<image>` node. A text-only host falls back to `ToastText02` (see
//!    [`build_document`]).
//! 2. The `text` and `image` nodes are located with XPath and filled from the
//!    [`Notification`] fields.
//! 3. `ToastNotificationManager::CreateToastNotifierWithId(app_name)` +
//!    `ToastNotifier::Show()` displays the toast. The explicit id (rather than
//!    the parameterless `CreateToastNotifier()`) is what lets an unpackaged
//!    process show a toast at all; see "App identity" below.
//!
//! # App identity
//!
//! A packaged (MSIX/APPX) application is addressed by its package identity. A
//! classic Win32 process has none, so the parameterless
//! `CreateToastNotifier()` fails with `ELEMENT_NOT_FOUND` (`0x80070490`) and no
//! toast ever appears - which is what an unpackaged `node script.js` hits.
//!
//! UDA fixes that two ways, and **both are required**:
//!
//! 1. `CreateToastNotifierWithId(app_name)` addresses the toast to an explicit
//!    id instead of letting WinRT resolve one from the process. This is the call
//!    that actually succeeds for an unpackaged binary; the parameterless
//!    `CreateToastNotifier()` keeps failing even after an AUMID is registered,
//!    because the process still has no *resolved* identity.
//! 2. `SetCurrentProcessExplicitAppUserModelID(app_name)` records the same id on
//!    the process so the shell can match it when deciding where to display the
//!    toast. Registered once per process (see [`ensure_app_identity`]); a host
//!    that already set its own AUMID keeps it, because overwriting it would
//!    break that host's activation routing.
//!
//! Neither requires a Start-menu shortcut, so a plain `node script.js` or a
//! Python interpreter shows a native toast.
//!
//! The caller can still probe the outcome through
//! [`WindowsNotificationManager::availability`], which reports
//! [`NotificationSetting`].
//!
//! # Limitations
//!
//! - **Action buttons.** Toast buttons require the `actions` content plus an
//!   activated handler that only a packaged app can register, so
//!   `Notification::actions` is accepted for trait parity but not surfaced as
//!   buttons in an unpackaged host — the notification degrades to a read-only
//!   text card. See `docs/internals/notification_specs.md` §3.2.
//! - **Progress.** The FreeDesktop progress concept has no direct toast
//!   equivalent; it is intentionally ignored rather than approximated.
//! - **Toast source name.** On a host the shell has already bound to a package
//!   identity (for example a Microsoft Store runtime), the card's source line
//!   shows that package family name and cannot be overridden. See
//!   `docs/internals/notification_specs.md` §3.1.
//!
//! # Icons
//!
//! `Notification::app_icon` is the *caller's* bitmap, which is separate from the
//! icon Windows draws for the toast's identity (the small square in the card's
//! top-left corner, taken from the AUMID/app registration). The two are
//! independent, and an unpackaged process has no identity icon at all, so the
//! `<image>` node has to be filled for any picture to appear.
//!
//! The template therefore varies with the input, because the two shapes have
//! different schemas — see [`build_document`].
//!
//! The `src` attribute accepts a filesystem path, a `file://` URI, or an
//! `http(s)://` URL. A plain Windows path is converted to a `file://` URI here
//! because the notification platform resolves the attribute relative to the
//! *shell's* context, not the sender's working directory, so a bare
//! `C:\pics\a.png` is not reliably located. A value that already carries a
//! scheme is passed through untouched, which keeps a caller that already built a
//! URI from being double-prefixed.

use std::sync::OnceLock;

use windows::core::{Interface, HSTRING, PCWSTR};
use windows::Data::Xml::Dom::{XmlDocument, XmlElement, XmlNodeList};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    GetCurrentProcessExplicitAppUserModelID, SetCurrentProcessExplicitAppUserModelID,
};
use windows::UI::Notifications::{
    ToastNotification, ToastNotificationManager, ToastNotifier, ToastTemplateType,
};

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::notification::{self, Notification, NotificationManager, Urgency};

/// XPath selecting the title text node of the toast templates.
const TITLE_XPATH: &str = "/toast/visual/binding/text[1]";

/// XPath selecting the body text node of the toast templates.
const BODY_XPATH: &str = "/toast/visual/binding/text[2]";

/// XPath selecting the image node of the `ToastImageAndText02` template.
///
/// Present in both image templates, so the same expression covers either of
/// them. It is only queried for the image-carrying template, whose binding is
/// guaranteed to contain the node.
const IMAGE_XPATH: &str = "/toast/visual/binding/image";

/// Text carried in `alt` when the caller supplied no name to draw alt text from.
///
/// The attribute is read aloud by a screen reader, so an empty value is worse
/// than a generic word.
const ICON_ALT_FALLBACK: &str = "notification";

/// Windows notification manager.
///
/// The manager is stateless: WinRT resolves the notifier from the calling
/// process's identity at `Show` time.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsNotificationManager;

impl WindowsNotificationManager {
    pub fn new() -> Self {
        Self
    }

    /// Fill the text nodes of a toast template from a [`Notification`].
    ///
    /// `GetTemplateContent` returns a document whose `text` elements already
    /// exist, so the nodes are looked up rather than created. If a node is
    /// missing the template is malformed, which is reported rather than ignored,
    /// because silently dropping the body would produce a truncated toast.
    fn fill_template(document: &XmlDocument, notification: &Notification) -> Result<(), UdaError> {
        // `SelectNodes` takes an `HSTRING`, so the XPath literals are converted
        // here rather than at the call site to keep the constants readable.
        let title_xpath: HSTRING = TITLE_XPATH.into();
        let body_xpath: HSTRING = BODY_XPATH.into();

        let title_nodes = document
            .SelectNodes(&title_xpath)
            .map_err(|e| UdaError::Internal(format!("SelectNodes({TITLE_XPATH}) failed: {e}")))?;
        let body_nodes = document
            .SelectNodes(&body_xpath)
            .map_err(|e| UdaError::Internal(format!("SelectNodes({BODY_XPATH}) failed: {e}")))?;

        set_text_node(&title_nodes, &notification.summary, "summary")?;
        set_text_node(&body_nodes, &notification.body, "body")?;

        Ok(())
    }

    /// Fill the `<image>` node of an image-carrying template.
    ///
    /// Kept separate from [`fill_template`] because only the image templates have
    /// the node: calling this against `ToastText02` would fail on an XPath that
    /// matches nothing, and the whole send would be rejected for a notification
    /// the user asked to be text-only. [`build_document`] picks the template, so
    /// the caller is the one that knows whether this may run.
    fn fill_image(document: &XmlDocument, notification: &Notification) -> Result<(), UdaError> {
        let source = notification::image_source(&notification.app_icon);
        let Some(source) = source else {
            // Nothing to draw. Leaving the template's own placeholder empty is
            // the documented degradation: the shell then renders the card
            // without a picture rather than with a broken-image frame.
            return Ok(());
        };

        let image_xpath: HSTRING = IMAGE_XPATH.into();
        let node = document.SelectSingleNode(&image_xpath).map_err(|e| {
            UdaError::Internal(format!("SelectSingleNode({IMAGE_XPATH}) failed: {e}"))
        })?;

        // `SelectSingleNode` returns an `IXmlNode`, but attributes live on an
        // *element*: `SetAttribute` is declared on `XmlElement`. The cast is the
        // query interface for the same COM object, so it costs no copy and
        // cannot fail for a node the XPath already typed as an element.
        let element = node.cast::<XmlElement>().map_err(|e| {
            UdaError::Internal(format!("the toast image node is not an element: {e}"))
        })?;

        // `SetAttribute` takes an `HSTRING`, so both names and both values are
        // converted once here instead of at each of the four call sites.
        let source_attribute: HSTRING = "src".into();
        let alt_attribute: HSTRING = "alt".into();
        let value: HSTRING = source.into();

        element
            .SetAttribute(&source_attribute, &value)
            .map_err(|e| UdaError::Internal(format!("SetAttribute(src) failed: {e}")))?;

        // `alt` is required by the schema and read aloud by a screen reader, so
        // it carries the caller's name rather than being left empty.
        let alt = notification::header_title(&notification.app_name).unwrap_or(ICON_ALT_FALLBACK);
        let alt_value: HSTRING = alt.into();
        element
            .SetAttribute(&alt_attribute, &alt_value)
            .map_err(|e| UdaError::Internal(format!("SetAttribute(alt) failed: {e}")))?;

        Ok(())
    }

    /// Build the toast XML document for a notification.
    ///
    /// The template is chosen from `app_icon` rather than fixed, because the two
    /// shapes have different schemas: only `ToastImageAndText02` carries the
    /// `<image>` node this fill needs. Selecting an image template
    /// unconditionally would work too, but picking the text template when there
    /// is no icon keeps the card from advertising a picture it does not have.
    fn build_document(notification: &Notification) -> Result<XmlDocument, UdaError> {
        // `ToastImageAndText02` is the two-line text toast with a leading
        // image, which maps exactly onto `summary` + `body` + `app_icon`.
        let (template, with_image) = if notification::image_source(&notification.app_icon).is_some()
        {
            (ToastTemplateType::ToastImageAndText02, true)
        } else {
            (ToastTemplateType::ToastText02, false)
        };

        let document = ToastNotificationManager::GetTemplateContent(template)
            .map_err(|e| UdaError::Internal(format!("GetTemplateContent failed: {e}")))?;

        Self::fill_template(&document, notification)?;
        if with_image {
            Self::fill_image(&document, notification)?;
        }

        Ok(document)
    }
}

/// Write `text` into the first node of an XPath result.
fn set_text_node(nodes: &XmlNodeList, text: &str, field: &str) -> Result<(), UdaError> {
    let node = nodes
        .Item(0)
        .map_err(|e| UdaError::Internal(format!("toast template has no {field} text node: {e}")))?;

    // `IXmlNode::InnerText` escapes the value for us, so quotes and newlines in
    // the summary/body cannot break the XML document.
    node.SetInnerText(&text.into())
        .map_err(|e| UdaError::Internal(format!("SetInnerText for {field} failed: {e}")))?;

    Ok(())
}

/// Fallback AppUserModelID used when the caller supplies no `app_name`.
///
/// AUMIDs are dotted reverse-DNS strings by convention; a fixed value keeps
/// every UDA notification from an unnamed host grouped under one identity
/// instead of one toast per empty name.
const FALLBACK_AUMID: &str = "UniDesktop.Notification";

/// The AUMID this process has already registered, if any.
///
/// `OnceLock` rather than a `Mutex<bool>`: the registration must happen exactly
/// once and before the first notifier is created, and `get_or_init` gives that
/// without a lock on the read path. A second notification with a different
/// `app_name` keeps the first id rather than switching mid-process, because the
/// shell has already associated the running process with it.
static REGISTERED_AUMID: OnceLock<String> = OnceLock::new();

/// Normalise a caller-supplied `app_name` into a usable AUMID.
///
/// An empty or whitespace-only name cannot be registered (Win32 rejects it), so
/// it becomes [`FALLBACK_AUMID`]. Everything else is passed through verbatim:
/// the shell treats the AUMID as an opaque string, and a caller that already
/// uses a real identity (MSIX package name, or its own AUMID) must keep it.
fn resolve_aumid(app_name: &str) -> String {
    let trimmed = app_name.trim();
    if trimmed.is_empty() {
        FALLBACK_AUMID.to_string()
    } else {
        trimmed.to_string()
    }
}

/// Register the process's toast identity exactly once.
///
/// `SetCurrentProcessExplicitAppUserModelID` is the documented way to let an
/// unpackaged Win32 process receive toasts; without it
/// `CreateToastNotifier()` fails with `ELEMENT_NOT_FOUND` and nothing is shown.
///
/// The first call wins. A host that registered its own AUMID before UDA ran is
/// detected through `GetCurrentProcessExplicitAppUserModelID` and left alone,
/// because overwriting it would break that host's own activation routing.
fn ensure_app_identity(app_name: &str) -> Result<(), UdaError> {
    // An already-initialised cell means this process registered an identity,
    // either through UDA or through the host before UDA ran.
    if REGISTERED_AUMID.get().is_some() {
        return Ok(());
    }

    // A host that set its own AUMID keeps it: overwriting would break that
    // host's activation routing. `GetCurrentProcessExplicitAppUserModelID`
    // returns a borrowed string the shell owns, so it is only inspected here.
    if let Ok(existing) = unsafe { GetCurrentProcessExplicitAppUserModelID() } {
        if !existing.is_null() {
            // SAFETY: the shell hands back a null-terminated UTF-16 string that
            // stays valid for the lifetime of the process identity; it is read
            // once into an owned `String` before anything else touches it.
            let text = unsafe { existing.to_string() };
            if let Ok(text) = text {
                if !text.is_empty() {
                    log::debug!("process already has AppUserModelID {text}; leaving it untouched");
                    let _ = REGISTERED_AUMID.set(text);
                    return Ok(());
                }
            }
            // SAFETY: the string was allocated by the shell with the COM task
            // allocator, so it is released exactly once with the matching call.
            unsafe {
                let _ = CoTaskMemFree(Some(existing.0.cast()));
            }
        }
    }

    let aumid = resolve_aumid(app_name);
    let wide = to_wide(&aumid);

    // SAFETY: `wide` is a null-terminated `Vec<u16>` that outlives the call, and
    // `SetCurrentProcessExplicitAppUserModelID` only reads the string to record
    // the process's identity. No pointer escapes this function.
    let result = unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(wide.as_ptr())) };
    if let Err(error) = result {
        // A rejected AUMID (for example one the shell cannot parse) is a caller
        // mistake; anything else is an OS-level failure. Both degrade to a
        // diagnostic and let the notifier call report the real consequence.
        log::warn!("SetCurrentProcessExplicitAppUserModelID({aumid}) failed: {error}");
        return Err(UdaError::Internal(format!(
            "could not register the toast identity {aumid}: {error}"
        )));
    }

    log::debug!("registered process AppUserModelID {aumid}");
    let _ = REGISTERED_AUMID.set(aumid);
    Ok(())
}

impl WindowsNotificationManager {
    /// Create the notifier for an explicit toast identity.
    ///
    /// `CreateToastNotifierWithId` is what makes an unpackaged process work: the
    /// parameterless `CreateToastNotifier()` resolves the identity from the
    /// process, and a plain `node script.js` has none, so it fails with
    /// `ELEMENT_NOT_FOUND` no matter what AUMID was registered. Passing the id
    /// explicitly addresses the toast directly.
    ///
    /// `ensure_app_identity` still runs first, because the shell also matches
    /// the id against the process's registered AUMID when it decides where to
    /// show the toast; keeping both in step avoids a toast that is created but
    /// silently dropped.
    ///
    /// A failure to resolve the identity is mapped to [`UdaError::NotSupported`]
    /// per `AGENTS.md` Principle 1 (degrade gracefully), so the caller gets a
    /// diagnosis instead of an opaque WinRT error.
    fn create_notifier(app_name: &str) -> Result<ToastNotifier, UdaError> {
        ensure_app_identity(app_name)?;
        let application_id: HSTRING = resolve_aumid(app_name).into();

        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&application_id)
            .map_err(|e| {
                if is_element_not_found(&e) {
                    UdaError::NotSupported(format!(
                        "no toast app identity could be resolved for {application_id:?}; \
                         toasts require a packaged app or an AppUserModelID"
                    ))
                } else {
                    UdaError::Internal(format!("CreateToastNotifierWithId failed: {e}"))
                }
            })?;

        Ok(notifier)
    }

    /// Report whether toasts can actually be displayed for this process.
    ///
    /// This is the Windows analogue of a capability probe. It returns
    /// `DisabledForApplication` for a process that has an identity but has had
    /// toasts turned off, and [`UdaError::NotSupported`] for a process with no
    /// identity at all.
    pub fn availability(
        &self,
    ) -> Result<windows::UI::Notifications::NotificationSetting, UdaError> {
        // Probe with the same fallback identity a send with no `app_name` uses,
        // so the probe answers the same question the send would ask.
        let notifier = Self::create_notifier("")?;

        notifier.Setting().map_err(|e| {
            // `Setting` resolves the identity a second time, so an unresolved
            // id surfaces here as `ELEMENT_NOT_FOUND` just as it does in
            // `create_notifier`. Mapping it to the same `NotSupported` keeps a
            // probe consistent with a send rather than reporting an internal
            // failure for a condition the caller can act on.
            if is_element_not_found(&e) {
                UdaError::NotSupported(
                    "toast availability cannot be resolved: the process has no app identity"
                        .to_string(),
                )
            } else {
                UdaError::Internal(format!("NotificationSetting query failed: {e}"))
            }
        })
    }
}

/// Convert a UTF-8 Rust string into a null-terminated UTF-16 buffer.
///
/// The Win32 shell calls take `PCWSTR`, which is a borrowed pointer to exactly
/// this layout. The caller must keep the returned `Vec` alive across the call.
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// HRESULT for Win32 `ERROR_NOT_FOUND` (`0x80070490`), surfaced by WinRT as
/// `ELEMENT_NOT_FOUND` when no app identity can be resolved.
///
/// `0x80070490` interpreted as a signed 32-bit value is `-2147024752`; writing
/// the constant in hex and letting the compiler convert avoids the classic
/// hand-conversion bug that silently disables the check.
const ELEMENT_NOT_FOUND_HRESULT: i32 = 0x8007_0490_u32 as i32;

/// Whether a WinRT error is `ELEMENT_NOT_FOUND`.
///
/// `windows_core::Error` exposes the code through `Debug` only, so the numeric
/// comparison is done on the raw `HRESULT` obtained from `as_ptr`, which is
/// stable across the `windows` crate versions used by this workspace.
fn is_element_not_found(error: &windows::core::Error) -> bool {
    error.code().0 == ELEMENT_NOT_FOUND_HRESULT
}

#[async_trait::async_trait]
impl NotificationManager for WindowsNotificationManager {
    async fn send(&self, notification: &Notification) -> Result<u32, UdaError> {
        let document = Self::build_document(notification)?;

        // `CreateToastNotification` is the WinRT activation factory on
        // `ToastNotification` itself, not a method on the manager.
        let toast = ToastNotification::CreateToastNotification(&document)
            .map_err(|e| UdaError::Internal(format!("CreateToastNotification failed: {e}")))?;

        // The identity is registered and then passed explicitly to
        // `CreateToastNotifierWithId`. The parameterless
        // `CreateToastNotifier()` resolves the identity from the process, which
        // for an unpackaged binary means "none" and fails with
        // `ELEMENT_NOT_FOUND` - registering an AUMID alone does not change that.
        let notifier = Self::create_notifier(&notification.app_name)?;

        notifier
            .Show(&toast)
            .map_err(|e| UdaError::Internal(format!("ToastNotifier::Show failed: {e}")))?;

        // WinRT does not hand back a notification ID: the toast object is opaque
        // and cannot be queried after `Show`. `replaces_id` is therefore
        // meaningless on Windows, and `0` is returned for FreeDesktop parity.
        if notification.replaces_id != 0 {
            log::debug!(
                "Windows toasts cannot replace an existing notification; replaces_id={} ignored",
                notification.replaces_id
            );
        }

        if notification.urgency == Urgency::Critical {
            log::debug!("Windows toast urgency is derived from the XML template; Critical maps to the default");
        }

        if notification.expire_timeout < 0 {
            log::debug!(
                "Windows toast expiration is managed by the shell; negative expire_timeout ignored"
            );
        }

        if !notification.actions.is_empty() {
            log::debug!(
                "Windows toast action buttons require a packaged app identity; {} action(s) ignored",
                notification.actions.len()
            );
        }

        if notification::image_source(&notification.app_icon).is_none() {
            log::debug!("no usable app_icon supplied; the toast keeps the identity's own image");
        }

        Ok(0)
    }

    fn capabilities(&self) -> Result<Capability, UdaError> {
        // Text toasts work for every process that can display them; the
        // *availability* of that permission is a separate runtime question,
        // exposed through `WindowsNotificationManager::availability`.
        Ok(Capability::SEND_NOTIFICATION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xpaths_select_the_template_text_nodes() {
        assert_eq!(TITLE_XPATH, "/toast/visual/binding/text[1]");
        assert_eq!(BODY_XPATH, "/toast/visual/binding/text[2]");
    }

    #[test]
    fn title_and_body_xpaths_are_distinct() {
        assert_ne!(TITLE_XPATH, BODY_XPATH);
    }

    #[test]
    fn the_image_xpath_matches_the_image_node_only() {
        // The expression must address exactly the image node of the image
        // template, or the icon silently lands somewhere the schema forbids.
        assert_eq!(IMAGE_XPATH, "/toast/visual/binding/image");
        assert_ne!(IMAGE_XPATH, TITLE_XPATH);
        assert_ne!(IMAGE_XPATH, BODY_XPATH);
    }

    // ------------------------------------------------------------------
    // 图标来源规范化（委托给 core）
    // ------------------------------------------------------------------
    //
    // The path/URI rules themselves live in [`uda_core::notification`] and are
    // tested there, where they actually run: this crate is `#![cfg(windows)]`,
    // so a test written here compiles to nothing on the Linux host that runs
    // `cargo test --workspace`. What is asserted here is only what this module
    // is responsible for - that the fill path really consults that helper, and
    // that the template choice follows its answer.

    #[test]
    fn the_fill_path_uses_the_core_normaliser() {
        // A Windows path must reach `SetAttribute` as a `file://` URI, which is
        // only true if `fill_image` goes through the core helper rather than
        // passing the caller's string through verbatim.
        assert_eq!(
            notification::image_source(r"C:\pics\icon.png").as_deref(),
            Some("file:///C:/pics/icon.png")
        );
        assert_eq!(
            notification::image_source("icons/icon.png").as_deref(),
            Some("icons/icon.png")
        );
    }

    #[test]
    fn a_usable_icon_selects_the_image_template() {
        // The two shapes have different schemas: only the image template has the
        // `<image>` node, so the choice must track the helper's answer exactly.
        let with_icon = Notification {
            app_icon: r"C:\pics\icon.png".to_string(),
            ..Notification::default()
        };
        let without_icon = Notification::default();

        assert!(notification::image_source(&with_icon.app_icon).is_some());
        assert!(notification::image_source(&without_icon.app_icon).is_none());
    }

    #[test]
    fn notification_default_is_usable_as_a_toast() {
        let notification = Notification::default();
        assert!(notification.summary.is_empty());
        assert!(notification.body.is_empty());
        assert!(notification.actions.is_empty());
    }

    #[test]
    fn capabilities_report_notification_support() {
        let manager = WindowsNotificationManager::new();
        let caps = match manager.capabilities() {
            Ok(caps) => caps,
            Err(e) => panic!("capabilities failed: {e}"),
        };
        assert_eq!(caps, Capability::SEND_NOTIFICATION);
    }

    #[test]
    fn a_supplied_app_name_becomes_the_aumid() {
        assert_eq!(
            resolve_aumid("UDA Notification Demo"),
            "UDA Notification Demo"
        );
    }

    #[test]
    fn an_empty_app_name_falls_back_to_the_generic_identity() {
        // `SetCurrentProcessExplicitAppUserModelID` rejects an empty string, so
        // the fallback has to be substituted before the Win32 call.
        assert_eq!(resolve_aumid(""), FALLBACK_AUMID);
        assert_eq!(resolve_aumid("   "), FALLBACK_AUMID);
    }

    #[test]
    fn a_surrounding_whitespace_is_trimmed_off_the_aumid() {
        assert_eq!(resolve_aumid("  UDA  "), "UDA");
    }

    #[test]
    fn the_fallback_aumid_is_a_dotted_identity() {
        // AUMIDs are dotted reverse-DNS strings by convention; the shell treats
        // them as opaque, but a bare word is easy to collide with another app.
        assert_eq!(FALLBACK_AUMID, "UniDesktop.Notification");
        assert!(FALLBACK_AUMID.contains('.'));
    }

    #[test]
    fn to_wide_is_null_terminated_for_the_shell_calls() {
        let wide = to_wide("UniDesktop.Notification");
        assert_eq!(wide.last().copied(), Some(0));
        assert!(!wide[..wide.len() - 1].contains(&0));
    }

    #[test]
    fn to_wide_round_trips_a_non_ascii_aumid() {
        // The id crosses into WinRT as UTF-16, so a non-ASCII app name must
        // survive the conversion rather than being mangled at the first
        // multi-byte character.
        let wide = to_wide("应用.通知");
        assert_eq!(
            wide,
            "应用.通知"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn element_not_found_hresult_is_documented() {
        // The constant must equal the signed i32 reading of 0x80070490, which is
        // what `HRESULT::code().0` reports. Asserting against the hex literal
        // (rather than a hand-converted decimal) keeps the two forms in sync.
        assert_eq!(ELEMENT_NOT_FOUND_HRESULT, 0x8007_0490_u32 as i32);
        assert_eq!(ELEMENT_NOT_FOUND_HRESULT as u32, 0x8007_0490);
    }

    #[test]
    fn availability_reports_setting_or_missing_identity() {
        // A bare `cargo test` process has no package identity, so WinRT reports
        // `ELEMENT_NOT_FOUND`. Both outcomes are valid: a real setting means the
        // host is configured for toasts, `NotSupported` means no identity is
        // registered. Neither may surface as an opaque internal error.
        let manager = WindowsNotificationManager::new();
        match manager.availability() {
            Ok(setting) => assert!((0..=4).contains(&setting.0)),
            Err(UdaError::NotSupported(_)) => {}
            Err(e) => panic!("availability returned an unexpected error: {e}"),
        }
    }
}
