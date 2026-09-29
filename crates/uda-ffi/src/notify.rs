//! Notification exports shared by the C callers.
//!
//! The FreeDesktop spec (and the Windows toast equivalent) carries nine fields,
//! so a C caller cannot pass them positionally without a struct. Rather than
//! inventing an ABI struct that every binding would have to mirror, this module
//! exposes one function with the four fields a notification actually needs
//! (`title`, `body`, `icon`, `actions`) and defaults the rest: `app_name` is
//! `UDA`, `replaces_id` is 0, `expire_timeout` is the server default, and the
//! urgency is normal. Callers that need more control use the Rust trait
//! directly.
//!
//! Actions are transported as a flat, newline-separated list because the C ABI
//! has no way to express a list of pairs. A trailing record without a label is
//! dropped rather than paired with an empty label, so the shell never renders a
//! blank button.
//!
//! `app_name` is the one field that is *not* cosmetic: on Windows it is the
//! AppUserModelID a toast is addressed to. An unpackaged process has none, so
//! the backend registers `app_name` as a fallback AUMID before the first
//! notifier is created; a caller that leaves it empty gets the generic
//! `UniDesktop.Notification` identity instead.

use uda_core::notification::{Notification, NotificationManager, Urgency};

use crate::error::Failure;

/// Build a [`Notification`] from the four C-visible fields.
///
/// Factored out so the action parsing is testable without touching a session
/// bus: the rest of the module only deals with delivering the result.
fn build_notification(
    app_name: &str,
    title: &str,
    body: &str,
    icon: &str,
    actions: &str,
) -> Notification {
    let mut notification = Notification {
        // An empty `app_name` must not reach a backend: on Windows it would
        // become an empty AUMID, which `SetCurrentProcessExplicitAppUserModelID`
        // rejects. Substitute the generic identity here so every backend sees a
        // usable name.
        app_name: if app_name.trim().is_empty() {
            "UniDesktop.Notification".to_string()
        } else {
            app_name.to_string()
        },
        replaces_id: 0,
        app_icon: icon.to_string(),
        summary: title.to_string(),
        body: body.to_string(),
        actions: Vec::new(),
        expire_timeout: 0,
        urgency: Urgency::Normal,
    };

    // A trailing record without a label cannot become a button, so it is
    // dropped instead of paired with an empty string the shell would render
    // as a blank row.
    let mut lines = actions.split('\n').filter(|part| !part.is_empty());
    while let (Some(key), Some(label)) = (lines.next(), lines.next()) {
        notification
            .actions
            .push((key.to_string(), label.to_string()));
    }

    notification
}

/// Send a notification and write the server-assigned id into `out_id`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn notify(
    app_name: &str,
    title: &str,
    body: &str,
    icon: &str,
    actions: &str,
    out_id: &mut u32,
) -> Result<(), Failure> {
    let notification = build_notification(app_name, title, body, icon, actions);

    // The manager is built per call (Linux opens a session-bus connection,
    // Windows resolves the process identity), so a fresh single-threaded
    // runtime per notification keeps the ABI synchronous without parking a
    // worker thread for the lifetime of the process.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            Failure::Uda(uda_core::error::UdaError::Internal(format!(
                "could not start a runtime for the notification: {error}"
            )))
        })?;

    let id = runtime.block_on(send(&notification))?;
    *out_id = id;
    Ok(())
}

/// Deliver `notification` through the platform backend.
async fn send(notification: &Notification) -> Result<u32, Failure> {
    #[cfg(target_os = "linux")]
    {
        let manager = uda_platform_linux::notification::LinuxNotificationManager::new().await?;
        Ok(manager.send(notification).await?)
    }

    #[cfg(target_os = "windows")]
    {
        let manager = uda_platform_windows::notification::WindowsNotificationManager::new();
        Ok(manager.send(notification).await?)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = notification;
        Err(Failure::Uda(uda_core::error::UdaError::NotSupported(
            "no notification backend for this target".to_string(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_are_split_into_key_label_pairs() {
        let notification = build_notification("UDA", "t", "b", "", "open\n查看详情\nclose\n关闭");
        assert_eq!(
            notification.actions,
            vec![
                ("open".to_string(), "查看详情".to_string()),
                ("close".to_string(), "关闭".to_string()),
            ]
        );
    }

    #[test]
    fn a_trailing_record_without_a_label_is_dropped() {
        let notification = build_notification("UDA", "t", "b", "", "open\ndetails\norphan");
        assert_eq!(
            notification.actions,
            vec![("open".to_string(), "details".to_string())]
        );
    }

    #[test]
    fn empty_records_are_skipped_rather_than_paired() {
        let notification = build_notification("UDA", "t", "b", "", "\n\nopen\n查看\n");
        assert_eq!(
            notification.actions,
            vec![("open".to_string(), "查看".to_string())]
        );
    }

    #[test]
    fn the_title_and_body_reach_the_notification_verbatim() {
        let notification = build_notification("UDA", "标题", "正文内容", "", "");
        assert_eq!(notification.summary, "标题");
        assert_eq!(notification.body, "正文内容");
        // Defaults the C surface does not expose must stay predictable.
        assert_eq!(notification.replaces_id, 0);
        assert_eq!(notification.expire_timeout, 0);
        assert_eq!(notification.urgency, Urgency::Normal);
    }

    #[test]
    fn the_app_name_becomes_the_toast_identity() {
        // On Windows this value is the AppUserModelID the toast is addressed to,
        // so it must survive verbatim rather than being overwritten.
        let notification = build_notification("UDA Notification Demo", "t", "b", "", "");
        assert_eq!(notification.app_name, "UDA Notification Demo");
    }

    #[test]
    fn an_empty_app_name_falls_back_to_the_generic_identity() {
        // `SetCurrentProcessExplicitAppUserModelID` rejects an empty string, so
        // the fallback must be substituted before any backend sees the value.
        let notification = build_notification("   ", "t", "b", "", "");
        assert_eq!(notification.app_name, "UniDesktop.Notification");
    }

    #[test]
    fn an_empty_icon_is_passed_through_unchanged() {
        let notification = build_notification("UDA", "t", "b", "", "");
        assert!(notification.app_icon.is_empty());
        assert!(notification.actions.is_empty());
    }

    #[test]
    fn an_icon_path_is_kept_verbatim_for_the_backend() {
        let notification = build_notification("UDA", "t", "b", "/tmp/icon.png", "");
        assert_eq!(notification.app_icon, "/tmp/icon.png");
    }
}
