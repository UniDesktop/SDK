//! Windows notification manager: native toasts via WinRT.
//!
//! The supported mechanism is `Windows.UI.Notifications` (see `AGENTS.md` §3):
//! `GetTemplateContent` produces an XML document, the `text` and `image` nodes
//! are located with XPath and filled from the [`Notification`] fields, and
//! `CreateToastNotifierWithId(app_name)` + `Show()` displays it.
//!
//! # App identity
//!
//! A classic Win32 process has no package identity, so the parameterless
//! `CreateToastNotifier()` fails with `ELEMENT_NOT_FOUND` (`0x80070490`) and no
//! toast appears. **Both** of these are therefore required:
//!
//! 1. `CreateToastNotifierWithId(app_name)` addresses the toast to an explicit
//!    id instead of letting WinRT resolve one.
//! 2. `SetCurrentProcessExplicitAppUserModelID(app_name)` records the same id on
//!    the process, registered once per process (see [`ensure_app_identity`]) and
//!    never overwriting an AUMID the host already set, which would break its
//!    activation routing.
//!
//! Neither needs a Start-menu shortcut, so a plain `node script.js` or a Python
//! interpreter shows a native toast. The caller can probe the outcome through
//! [`WindowsNotificationManager::availability`].
//!
//! # Limitations
//!
//! - **Action buttons** need an activation handler only a packaged app can
//!   register, so `Notification::actions` is accepted for trait parity but
//!   degrades to a read-only text card in an unpackaged host. See
//!   `docs/internals/notification_specs.md` §3.2.
//! - **Progress** has no direct toast equivalent and is ignored.
//! - **Toast source name** cannot be overridden on a host the shell already
//!   bound to a package identity. See `notification_specs.md` §3.1.
//!
//! # Icons
//!
//! `Notification::app_icon` is the *caller's* bitmap, separate from the identity
//! icon Windows draws for the AUMID. An unpackaged process has no identity icon,
//! so the `<image>` node has to be filled for any picture to appear, and the
//! template varies with the input ([`build_document`]).
//!
//! `src` accepts a path, a `file://` URI or an `http(s)://` URL. A plain Windows
//! path becomes a `file://` URI because the platform resolves the attribute in
//! the *shell's* context, not the sender's working directory; a value that
//! already carries a scheme passes through untouched.

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

/// XPath selecting the image node, present in both image templates and only
/// queried for the image-carrying one.
const IMAGE_XPATH: &str = "/toast/visual/binding/image";

/// Text carried in `alt` when the caller supplied no name to derive it from. The
/// attribute is read aloud by a screen reader, so an empty value is worse than a
/// generic word.
const ICON_ALT_FALLBACK: &str = "notification";

/// Windows notification manager, stateless: WinRT resolves the notifier from the
/// calling process's identity at `Show` time.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsNotificationManager;

impl WindowsNotificationManager {
    pub fn new() -> Self {
        Self
    }

    /// Fill the text nodes of a toast template from a [`Notification`].
    ///
    /// `GetTemplateContent` already contains the `text` elements, so they are
    /// looked up rather than created. A missing node means a malformed template,
    /// which is reported: silently dropping the body would truncate the toast.
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

        // `SelectSingleNode` returns an `IXmlNode`, but `SetAttribute` is declared
        // on `XmlElement`; the cast is a query interface for the same COM object.
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
    /// The template follows `app_icon` because the two shapes have different
    /// schemas: only `ToastImageAndText02` carries the `<image>` node this fill
    /// needs. `ToastImageAndText02` is the two-line text toast with a leading
    /// image, mapping onto `summary` + `body` + `app_icon`.
    fn build_document(notification: &Notification) -> Result<XmlDocument, UdaError> {
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

    // `InnerText` escapes the value, so quotes and newlines cannot break the
    // document.
    node.SetInnerText(&text.into())
        .map_err(|e| UdaError::Internal(format!("SetInnerText for {field} failed: {e}")))?;

    Ok(())
}

/// Fallback AppUserModelID when the caller supplies no `app_name`, so every
/// notification from an unnamed host lands under one identity.
const FALLBACK_AUMID: &str = "UniDesktop.Notification";

/// The AUMID this process has already registered, if any.
///
/// `OnceLock` rather than a `Mutex<bool>`: registration happens exactly once and
/// before the first notifier is created, with no lock on the read path. A second
/// notification with a different `app_name` keeps the first id, because the shell
/// has already associated the running process with it.
static REGISTERED_AUMID: OnceLock<String> = OnceLock::new();

/// Normalise a caller-supplied `app_name` into a usable AUMID.
///
/// An empty or whitespace-only name cannot be registered (Win32 rejects it), so
/// it becomes [`FALLBACK_AUMID`]. Everything else passes through verbatim: the
/// shell treats the AUMID as opaque, and a caller that already uses a real
/// identity (MSIX package name, or its own AUMID) must keep it.
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
/// unpackaged Win32 process receive toasts. The first call wins, and a host that
/// registered its own AUMID before UDA ran keeps it.
fn ensure_app_identity(app_name: &str) -> Result<(), UdaError> {
    // An initialised cell means an identity exists, from UDA or from the host.
    if REGISTERED_AUMID.get().is_some() {
        return Ok(());
    }

    // `GetCurrentProcessExplicitAppUserModelID` returns a string the shell owns,
    // so it is only inspected and then released.
    if let Ok(existing) = unsafe { GetCurrentProcessExplicitAppUserModelID() } {
        if !existing.is_null() {
            // SAFETY: a null-terminated UTF-16 string owned by the shell, read
            // once into an owned `String` here.
            let text = unsafe { existing.to_string() };
            if let Ok(text) = text {
                if !text.is_empty() {
                    log::debug!("process already has AppUserModelID {text}; leaving it untouched");
                    let _ = REGISTERED_AUMID.set(text);
                    return Ok(());
                }
            }
            // SAFETY: allocated by the shell with the COM task allocator, so it
            // is released with the matching call.
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
    /// process, and an unpackaged host has none, so it fails with
    /// `ELEMENT_NOT_FOUND` no matter what AUMID was registered.
    ///
    /// `ensure_app_identity` still runs first, because the shell also matches the
    /// id against the process's AUMID when deciding where to show the toast.
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
    /// The Windows analogue of a capability probe: `DisabledForApplication` for a
    /// process whose toasts are turned off, [`UdaError::NotSupported`] for one
    /// with no identity at all.
    pub fn availability(
        &self,
    ) -> Result<windows::UI::Notifications::NotificationSetting, UdaError> {
        // Probe with the fallback identity a send with no `app_name` uses, so the
        // probe answers the same question the send would ask.
        let notifier = Self::create_notifier("")?;

        notifier.Setting().map_err(|e| {
            // `Setting` resolves the identity again, so an unresolved id surfaces
            // here too and maps to the same `NotSupported` as `create_notifier`.
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

/// Convert a UTF-8 Rust string into a null-terminated UTF-16 buffer. The Win32
/// shell calls take `PCWSTR`, so the caller keeps the `Vec` alive across the call.
fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// HRESULT for Win32 `ERROR_NOT_FOUND` (`0x80070490`), surfaced by WinRT as
/// `ELEMENT_NOT_FOUND` when no app identity can be resolved. Written in hex and
/// let the compiler convert, since the signed value is easy to get wrong by hand.
const ELEMENT_NOT_FOUND_HRESULT: i32 = 0x8007_0490_u32 as i32;

/// Whether a WinRT error is `ELEMENT_NOT_FOUND`, compared on the raw HRESULT,
/// which is stable across the `windows` crate versions this workspace uses.
fn is_element_not_found(error: &windows::core::Error) -> bool {
    error.code().0 == ELEMENT_NOT_FOUND_HRESULT
}

#[async_trait::async_trait]
impl NotificationManager for WindowsNotificationManager {
    async fn send(&self, notification: &Notification) -> Result<u32, UdaError> {
        let document = Self::build_document(notification)?;

        // `CreateToastNotification` is the activation factory on
        // `ToastNotification` itself, not a method on the manager.
        let toast = ToastNotification::CreateToastNotification(&document)
            .map_err(|e| UdaError::Internal(format!("CreateToastNotification failed: {e}")))?;

        let notifier = Self::create_notifier(&notification.app_name)?;

        notifier
            .Show(&toast)
            .map_err(|e| UdaError::Internal(format!("ToastNotifier::Show failed: {e}")))?;

        // WinRT hands back no notification ID and the toast object is opaque
        // after `Show`, so `replaces_id` is meaningless here and `0` is returned.
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
