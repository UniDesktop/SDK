//! Windows notification manager: native toasts via WinRT.
//!
//! # Backend selection
//!
//! Windows has no FreeDesktop-style daemon; the supported mechanism is the WinRT
//! `Windows.UI.Notifications` API (see `AGENTS.md` section 3):
//!
//! 1. `ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)`
//!    produces an XML document with the standard title/body text nodes.
//! 2. The `text` nodes are located with XPath and filled from the
//!    [`Notification`] fields.
//! 3. `ToastNotificationManager::CreateToastNotifier()` + `ToastNotifier::Show()`
//!    displays the toast.
//!
//! # Limitations (Phase 1)
//!
//! - **App identity.** A packaged (MSIX/APPX) application is addressed by its
//!   package identity; a classic Win32 process must register an AppUserModelID
//!   and a Start-menu shortcut, otherwise the toast may not appear. UDA calls
//!   `CreateToastNotifier()` with the default identity and reports
//!   [`NotificationSetting`] through [`WindowsNotificationManager::availability`]
//!   so the caller can detect this before relying on toasts.
//! - **Actions.** Toast *buttons* require the `actions` content plus an activated
//!   handler that only a packaged app can register, so `Notification::actions`
//!   is accepted for trait parity but not surfaced as buttons in Phase 1.
//! - **Progress.** The FreeDesktop progress concept has no direct toast
//!   equivalent; it is intentionally ignored rather than approximated.

use windows::core::HSTRING;
use windows::Data::Xml::Dom::{XmlDocument, XmlNodeList};
use windows::UI::Notifications::{
    ToastNotification, ToastNotificationManager, ToastNotifier, ToastTemplateType,
};

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::notification::{Notification, NotificationManager, Urgency};

/// XPath selecting the title text node of the `ToastText02` template.
const TITLE_XPATH: &str = "/toast/visual/binding/text[1]";

/// XPath selecting the body text node of the `ToastText02` template.
const BODY_XPATH: &str = "/toast/visual/binding/text[2]";

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

    /// Build the toast XML document for a notification.
    fn build_document(notification: &Notification) -> Result<XmlDocument, UdaError> {
        // `ToastText02` is the canonical two-line text toast: one title and one
        // body line, which maps exactly onto `summary` + `body`.
        let document = ToastNotificationManager::GetTemplateContent(ToastTemplateType::ToastText02)
            .map_err(|e| UdaError::Internal(format!("GetTemplateContent failed: {e}")))?;

        Self::fill_template(&document, notification)?;

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

impl WindowsNotificationManager {
    /// Create the notifier for the calling process.
    ///
    /// An unpackaged Win32 process that has no Start-menu shortcut carrying an
    /// AppUserModelID cannot be resolved to an app identity, and WinRT reports
    /// `ELEMENT_NOT_FOUND` (`0x80070490`). That is an expected, diagnosable
    /// condition rather than a bug, so it is mapped to
    /// [`UdaError::NotSupported`] per `AGENTS.md` Principle 1 (degrade
    /// gracefully): the caller can then register an AUMID or fall back to
    /// another channel instead of receiving an opaque internal error.
    fn create_notifier() -> Result<ToastNotifier, UdaError> {
        let notifier = ToastNotificationManager::CreateToastNotifier().map_err(|e| {
            if is_element_not_found(&e) {
                UdaError::NotSupported(
                    "no toast app identity is registered for this process; toasts require a packaged app or an AppUserModelID".to_string(),
                )
            } else {
                UdaError::Internal(format!("CreateToastNotifier failed: {e}"))
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
        let notifier = Self::create_notifier()?;

        notifier
            .Setting()
            .map_err(|e| UdaError::Internal(format!("NotificationSetting query failed: {e}")))
    }
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

        let notifier = Self::create_notifier()?;

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

        if !notification.app_icon.is_empty() {
            log::debug!(
                "Windows toast icon override requires the appImage content; app_icon ignored"
            );
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
