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

// Only the shared blocking bridge below deals in futures; targets without a
// backend never see it.
#[cfg(any(target_os = "linux", target_os = "windows"))]
use std::future::Future;

use uda_core::error::UdaError;
use uda_core::notification::Notification;
// `send` resolves through this trait on the targets that have a backend; the
// static fallback below answers without it, so the import would be unused
// (and warned about) everywhere else.
#[cfg(any(target_os = "linux", target_os = "windows"))]
use uda_core::notification::NotificationManager;
use uda_core::notification::Urgency;

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

/// Run `future` to completion from synchronous code, wherever the caller is.
///
/// The C ABI is synchronous while every platform backend is async, so the FFI
/// layer has to bridge the two. A `block_on` on a runtime built here would panic
/// if the caller already runs inside a tokio runtime (a tray worker thread, for
/// example), because tokio refuses a nested `block_on`. The two situations are
/// therefore split:
///
/// - **No ambient runtime:** a private current-thread runtime is built for this
///   one call and dropped afterwards, so no worker thread stays parked for the
///   lifetime of the process.
/// - **Inside a runtime:** the future is moved onto a dedicated worker thread
///   and polled there on its own runtime. The caller's runtime is never blocked
///   from inside itself and stays free to service the very events (tray clicks,
///   D-Bus signals) the future may be waiting for.
///
/// Every failure path returns [`UdaError::Internal`]; the helper contains no
/// `unwrap`, `expect` or panic of its own. The `'static` bound is what the
/// worker-thread branch needs: the future is moved onto a fresh thread, so it
/// may not borrow from the caller's frame.
///
/// Shared with the wake-lock tier, so every synchronous export in this crate
/// gets the same nesting behaviour.
#[cfg(any(target_os = "linux", target_os = "windows"))]
pub(crate) fn run_sync<T>(future: impl Future<Output = T> + Send + 'static) -> Result<T, UdaError>
where
    T: Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_ok() {
        return run_sync_on_worker_thread(future);
    }
    Ok(blocking_runtime()?.block_on(future))
}

/// Build the private current-thread runtime both branches of [`run_sync`] poll
/// on.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn blocking_runtime() -> Result<tokio::runtime::Runtime, UdaError> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| UdaError::Internal(format!("could not start a tokio runtime: {error}")))
}

/// Execute `future` on a thread of its own for a caller already inside a
/// runtime.
///
/// The worker builds its own current-thread runtime, so the `block_on` there
/// can never nest into the caller's runtime, however long the future takes. The
/// result is sent back on every path; a worker that dies before answering (a
/// panic inside the future, say) closes the channel, which the caller reports
/// as an internal error instead of unwinding through its own runtime.
#[cfg(any(target_os = "linux", target_os = "windows"))]
fn run_sync_on_worker_thread<T>(
    future: impl Future<Output = T> + Send + 'static,
) -> Result<T, UdaError>
where
    T: Send + 'static,
{
    use std::sync::mpsc;

    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::Builder::new()
        .name("uda-ffi-block-on".to_string())
        .spawn(move || {
            // Sent on every path, so the caller never waits on a channel whose
            // sender vanished without leaving an answer.
            let _ = sender.send(blocking_runtime().map(|runtime| runtime.block_on(future)));
        });

    let worker = worker.map_err(|error| {
        UdaError::Internal(format!(
            "could not spawn a worker thread for a nested blocking call: {error}"
        ))
    })?;

    let result = receiver.recv().map_err(|error| {
        UdaError::Internal(format!(
            "the blocking worker thread finished without a result: {error}"
        ))
    })?;

    // The worker has already answered, so joining only reaps the thread that is
    // on its way out; a failure here cannot invalidate the already-delivered
    // result.
    if worker.join().is_err() {
        log::debug!("the blocking worker thread panicked after delivering its result");
    }

    result
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

    // Targets with a backend drive the async `send` through the shared
    // blocking bridge; everywhere else the backend *is* the static answer
    // below, so no runtime is ever constructed. The two `?`s peel the bridge's
    // own runtime failure first, then the send failure. The notification is
    // moved into `send` because the worker-thread branch of the bridge needs
    // an owning, `'static` future.
    #[cfg(any(target_os = "linux", target_os = "windows"))]
    let id = run_sync(send(notification))??;

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    let id = send(notification)?;

    *out_id = id;
    Ok(())
}

/// Deliver `notification` through the platform backend.
#[cfg(any(target_os = "linux", target_os = "windows"))]
async fn send(notification: Notification) -> Result<u32, Failure> {
    #[cfg(target_os = "linux")]
    {
        let manager = uda_platform_linux::notification::LinuxNotificationManager::new().await?;
        Ok(manager.send(&notification).await?)
    }

    #[cfg(target_os = "windows")]
    {
        let manager = uda_platform_windows::notification::WindowsNotificationManager::new();
        Ok(manager.send(&notification).await?)
    }
}

/// Deliver `notification` on a target that has no backend at all.
///
/// Kept synchronous: there is nothing to await, and the C ABI sees the same
/// `UDA_ERR_NOT_SUPPORTED` the runtime-driven targets report for a missing
/// backend.
#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn send(notification: Notification) -> Result<u32, Failure> {
    let _ = notification;
    Err(Failure::Uda(UdaError::NotSupported(
        "no notification backend for this target".to_string(),
    )))
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

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn run_sync_completes_a_standalone_future() {
        // No ambient runtime here: the helper must build (and drop) its own.
        let value = run_sync(async { 40 + 2 }).expect("a standalone runtime always works");
        assert_eq!(value, 42);
    }

    #[cfg(any(target_os = "linux", target_os = "windows"))]
    #[test]
    fn run_sync_survives_being_called_inside_a_runtime() {
        // The regression this guards against: `block_on` from within a runtime
        // used to panic with "Cannot start a runtime from within a runtime".
        // A tray worker thread runs on exactly such a runtime, so the helper
        // must take the worker-thread branch and answer normally.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the test runtime builds");

        let outcome = runtime.block_on(async {
            // `spawn_local`-style context checks aside, the mere fact that
            // `try_current()` is `Ok` here proves the ambient runtime is the
            // context `run_sync` runs in.
            assert!(tokio::runtime::Handle::try_current().is_ok());
            run_sync(async { "answered from the nested bridge" })
        });

        assert_eq!(
            outcome.expect("a nested call must not panic"),
            "answered from the nested bridge"
        );
    }
}
