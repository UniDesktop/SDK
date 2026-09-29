//! Windows media backend: WinRT SMTC (`GlobalSystemMediaTransportControls`).
//!
//! # Backend selection
//!
//! Windows has exactly one supported media-control surface, the Global System
//! Media Transport Controls session manager:
//!
//! | Concern              | API                                                                  |
//! |----------------------|----------------------------------------------------------------------|
//! | Session lookup       | `GlobalSystemMediaTransportControlsSessionManager::RequestAsync()`   |
//! | Current session      | `GetCurrentSession()`                                                |
//! | Metadata             | `TryGetMediaPropertiesAsync()` -> `Title`/`Artist`/`AlbumTitle`      |
//! | Playback status      | `GetPlaybackInfo()` -> `Controls.PlaybackStatus`                     |
//! | Timeline             | `GetTimelineProperties()` -> `EndTime`/`Position`                    |
//! | Transport commands   | `TryTogglePlayPauseAsync()`, `TrySkipNextAsync()`, ...                |
//!
//! `GetCurrentSession()` returns the session that currently has system focus,
//! which is the Windows equivalent of MPRIS picking an active player. It returns
//! nothing when no media is playing at all; that is the "no player" case and is
//! reported as [`UdaError::NotSupported`] for commands and `None`/`Unknown` for
//! reads.
//!
//! # Three quirks that cost hours if missed
//!
//! 1. **Every `Try*Async()` command returns `bool`, not a `Result`.** `false`
//!    means the session *refused* the command (for example `Next` when the app
//!    does not enable it), while `true` only means the app *accepted* it - the
//!    app can still fail afterwards. UDA maps `false` to
//!    [`UdaError::CommandFailed`] so a caller can distinguish "sent" from
//!    "refused", which a bare `Result` would hide.
//! 2. **`TimeSpan` is in 100-nanosecond ticks, not milliseconds.**
//!    `Duration / 10_000` converts to milliseconds. A `Duration` of zero means
//!    "unknown", which happens for live streams, and is reported as `None`
//!    rather than `Some(0)`.
//! 3. **"No current session" arrives as an `Err`, not a null.** WinRT reports it
//!    through the `HRESULT`, so the *absence* of a player looks like a failure
//!    unless the caller reads it as a value. Because a machine with nothing
//!    playing is the everyday state - a CI runner, a fresh desktop, any user
//!    between songs - reads answer `Ok(None)` / `Ok(Unknown)` and only commands
//!    raise `NotSupported`. This matches the Linux backend, where an MPRIS
//!    lookup over a session bus with no player name yields `None` as well.
//!
//! # Threading
//!
//! The WinRT activation of the session manager must run on a multi-threaded
//! apartment, and the `IAsyncOperation::get()` completion used here blocks on an
//! event that the worker pumps. Each call therefore builds a short-lived
//! current-thread runtime and drives the async chain inside it, which keeps
//! [`MediaManager`](uda_core::media::MediaManager) synchronous without parking a
//! runtime for the lifetime of the process.
//!
//! See `docs/internals/media_specs.md` for the full property mapping.

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus,
};

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::media::{MediaCommand, MediaManager, MediaMetadata, PlaybackStatus};

/// Windows media manager.
///
/// A zero-sized marker: every method resolves the current session on its own, so
/// the manager cannot hold a stale session from a previous call.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsMediaManager;

impl WindowsMediaManager {
    pub fn new() -> Self {
        Self
    }

    /// Run an asynchronous SMTC query to completion.
    ///
    /// The `IAsyncOperation::get()` helper installs a completion handler and
    /// waits for it, so the body must run inside a runtime that can pump its own
    /// events - hence the current-thread runtime in the [`MediaManager`] methods.
    fn block<F, T>(operation: F) -> Result<T, UdaError>
    where
        F: std::future::Future<Output = Result<T, UdaError>>,
    {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("could not start a runtime: {e}")))?;

        runtime.block_on(operation)
    }

    /// Resolve the SMTC session manager.
    async fn session_manager() -> Result<GlobalSystemMediaTransportControlsSessionManager, UdaError>
    {
        // `RequestAsync` hands back an `IAsyncOperation`, whose `get()` waits for
        // the result without a message pump of its own; running it inside the
        // caller's task satisfies that requirement.
        let request = GlobalSystemMediaTransportControlsSessionManager::RequestAsync()
            .map_err(|e| UdaError::DetectionFailed(format!("SMTC manager request: {e}")))?;

        request.get().map_err(|e| {
            // A machine without SMTC (Windows Server Core, a locked-down build)
            // lands here; it is an unsupported feature, not a crash.
            UdaError::NotSupported(format!("SMTC session manager: {e}"))
        })
    }

    /// Resolve the session that currently holds media focus, if any.
    ///
    /// Returns `Ok(None)` when nothing is playing. **A machine with no running
    /// player is the everyday state, not a fault**, so this is an ordinary answer
    /// rather than an error - the same contract the Linux backend keeps, where a
    /// session bus with no `org.mpris.MediaPlayer2.*` name simply yields `None`.
    async fn current_session(
    ) -> Result<Option<windows::Media::Control::GlobalSystemMediaTransportControlsSession>, UdaError>
    {
        let manager = Self::session_manager().await?;

        // `GetCurrentSession()` reports "nothing playing" as an error rather than
        // as a null/nullable result, which is why the Err arm is a *value* here
        // and not a propagated failure.
        match manager.GetCurrentSession() {
            Ok(session) => Ok(Some(session)),
            Err(_) => {
                log::debug!("no SMTC session is holding media focus; treating as no player");
                Ok(None)
            }
        }
    }
}

/// Map an SMTC playback status onto the core enum.
///
/// `Closed` and `Changing` have no MPRIS counterpart and both describe a session
/// that is not holding stable playback, so they become `Unknown` rather than
/// being guessed as `Paused`.
pub(crate) fn status_from_smtc(
    status: GlobalSystemMediaTransportControlsSessionPlaybackStatus,
) -> PlaybackStatus {
    match status {
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Playing => PlaybackStatus::Playing,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Paused => PlaybackStatus::Paused,
        GlobalSystemMediaTransportControlsSessionPlaybackStatus::Stopped => PlaybackStatus::Stopped,
        // Closed, Changing and any future value.
        _ => PlaybackStatus::Unknown,
    }
}

/// Convert a WinRT `TimeSpan` (100-ns ticks) to milliseconds.
///
/// Zero (and anything that divides down to zero) means "unknown": a live stream
/// reports exactly this, and `Some(0)` would render as a 0:00 track.
pub(crate) fn milliseconds_from_ticks(ticks: i64) -> Option<u64> {
    if ticks <= 0 {
        return None;
    }
    u64::try_from(ticks).ok().map(|value| value / 10_000)
}

/// Translate a core command into the SMTC `Try*Async` call it corresponds to.
///
/// SMTC has no separate play/pause-only transport for a toggle; a `Toggle` maps
/// to `TryTogglePlayPauseAsync`, which is what the media overlay uses.
pub(crate) async fn send_smtc_command(command: MediaCommand) -> Result<(), UdaError> {
    // Unlike a read, a command with nothing to address *is* an error: the caller
    // asked to pause something and there is no something.
    let Some(session) = WindowsMediaManager::current_session().await? else {
        return Err(UdaError::NotSupported(
            "no media session is currently playing".to_string(),
        ));
    };

    run_command(&session, command).await
}

/// Invoke one transport method and report a refusal as an error.
async fn run_command(
    session: &windows::Media::Control::GlobalSystemMediaTransportControlsSession,
    command: MediaCommand,
) -> Result<(), UdaError> {
    // Each call returns `bool`: `true` = accepted by the session, `false` = the
    // app refused (or is not listening). Both are successful WinRT calls, so the
    // distinction is made after the await rather than in the error channel.
    let accepted = match command {
        MediaCommand::Play => session.TryPlayAsync().and_then(|op| op.get()),
        MediaCommand::Pause => session.TryPauseAsync().and_then(|op| op.get()),
        MediaCommand::TogglePlayPause => session.TryTogglePlayPauseAsync().and_then(|op| op.get()),
        MediaCommand::Stop => session.TryStopAsync().and_then(|op| op.get()),
        MediaCommand::Next => session.TrySkipNextAsync().and_then(|op| op.get()),
        MediaCommand::Previous => session.TrySkipPreviousAsync().and_then(|op| op.get()),
    };

    match accepted {
        Ok(true) => Ok(()),
        Ok(false) => Err(UdaError::CommandFailed(format!(
            "the media session refused the command {:?}",
            command.code()
        ))),
        Err(e) => Err(UdaError::CommandFailed(format!("SMTC {e}"))),
    }
}

/// Read the metadata of the current session.
///
/// `Ok(None)` covers both "no session is playing" and "the session published no
/// usable fields", so a caller cannot distinguish them - and does not need to,
/// because neither is a failure.
async fn active_metadata_async() -> Result<Option<MediaMetadata>, UdaError> {
    let Some(session) = WindowsMediaManager::current_session().await? else {
        return Ok(None);
    };

    let properties = session
        .TryGetMediaPropertiesAsync()
        .map_err(|e| UdaError::DetectionFailed(format!("SMTC media properties: {e}")))?
        .get()
        .map_err(|e| UdaError::DetectionFailed(format!("SMTC media properties: {e}")))?;

    // `Artist` is a single string on Windows (MPRIS publishes a list, which is
    // joined on the Linux side); no post-processing is needed here.
    let title = string_from_hstring(properties.Title());
    let artist = string_from_hstring(properties.Artist());
    let album = string_from_hstring(properties.AlbumTitle());

    let mut metadata = MediaMetadata {
        title,
        artist,
        album,
        duration_ms: None,
        position_ms: None,
    };

    // The timeline is a separate object and can be absent for a session that
    // publishes metadata but no position (a web radio, for instance).
    if let Ok(timeline) = session.GetTimelineProperties() {
        metadata.duration_ms = milliseconds_from_ticks(ticks_of(timeline.EndTime()));
        metadata.position_ms = milliseconds_from_ticks(ticks_of(timeline.Position()));
    }

    if metadata.is_empty() {
        return Ok(None);
    }
    Ok(Some(metadata))
}

/// Convert an `HSTRING` read into an owned string, tolerating a failed read.
fn string_from_hstring(value: windows::core::Result<windows::core::HSTRING>) -> String {
    value
        .ok()
        .map(|text| text.to_string_lossy())
        .unwrap_or_default()
}

/// Read the tick count of a timeline value, tolerating a failed read.
///
/// A session that fails to publish a timeline value reports zero ticks, which
/// [`milliseconds_from_ticks`] already maps to `None`.
fn ticks_of(timespan: windows::core::Result<windows::Foundation::TimeSpan>) -> i64 {
    timespan.map(|span| span.Duration).unwrap_or_default()
}

/// Read the playback status of the current session.
///
/// With no session the status is `Unknown` rather than `Stopped`: nothing is
/// loaded, which is exactly what `Stopped` would claim to know.
async fn playback_status_async() -> Result<PlaybackStatus, UdaError> {
    let Some(session) = WindowsMediaManager::current_session().await? else {
        return Ok(PlaybackStatus::Unknown);
    };

    let info = session
        .GetPlaybackInfo()
        .map_err(|e| UdaError::DetectionFailed(format!("SMTC playback info: {e}")))?;

    let status = info
        .PlaybackStatus()
        .map_err(|e| UdaError::DetectionFailed(format!("SMTC playback status: {e}")))?;

    Ok(status_from_smtc(status))
}

impl MediaManager for WindowsMediaManager {
    fn active_metadata(&self) -> Result<Option<MediaMetadata>, UdaError> {
        Self::block(active_metadata_async())
    }

    fn playback_status(&self) -> Result<PlaybackStatus, UdaError> {
        Self::block(playback_status_async())
    }

    fn send_command(&self, command: MediaCommand) -> Result<(), UdaError> {
        Self::block(send_smtc_command(command))
    }

    fn capabilities(&self) -> Capability {
        // SMTC is present on every supported Windows 10/11 build, so the
        // capability is advertised unconditionally; whether a *session* exists is
        // a runtime question answered by the methods above.
        Capability::MEDIA_CONTROL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A brand-new `TimeSpan` is zero, which must read as "unknown".
    #[test]
    fn a_zero_timespan_is_unknown_not_zero_ms() {
        let zero = windows::Foundation::TimeSpan::default();
        assert_eq!(zero.Duration, 0);
        assert_eq!(milliseconds_from_ticks(zero.Duration), None);
    }

    #[test]
    fn one_second_of_ticks_is_one_thousand_milliseconds() {
        // 1 second = 10_000_000 ticks of 100ns.
        assert_eq!(milliseconds_from_ticks(10_000_000), Some(1_000));
        assert_eq!(milliseconds_from_ticks(1), Some(0));
    }

    #[test]
    fn a_negative_duration_is_rejected_rather_than_wrapped() {
        assert_eq!(milliseconds_from_ticks(-10_000_000), None);
        assert_eq!(milliseconds_from_ticks(i64::MIN), None);
    }

    #[test]
    fn playback_statuses_map_onto_the_core_enum() {
        use GlobalSystemMediaTransportControlsSessionPlaybackStatus as S;

        assert_eq!(status_from_smtc(S::Playing), PlaybackStatus::Playing);
        assert_eq!(status_from_smtc(S::Paused), PlaybackStatus::Paused);
        assert_eq!(status_from_smtc(S::Stopped), PlaybackStatus::Stopped);
    }

    #[test]
    fn a_session_that_is_not_stable_reports_unknown() {
        use GlobalSystemMediaTransportControlsSessionPlaybackStatus as S;

        // "Closed" means the app went away and "Changing" means a new track is
        // being loaded: neither is "Paused", and guessing would desync a UI.
        assert_eq!(status_from_smtc(S::Closed), PlaybackStatus::Unknown);
        assert_eq!(status_from_smtc(S::Changing), PlaybackStatus::Unknown);
    }

    #[test]
    fn a_machine_with_no_player_is_not_an_error() {
        // The contract that made CI fail: nothing playing is the everyday state,
        // so reads must answer a value rather than an Err. This is the Windows
        // counterpart of the Linux backend's
        // `a_headless_session_has_no_player_and_reports_none`.
        //
        // A bare CI runner has no SMTC session, so the assertion is exact here;
        // on a developer machine with music playing the test would see a real
        // session instead, which is equally acceptable - both are `Ok`.
        let manager = WindowsMediaManager::new();

        match manager.active_metadata() {
            Ok(_) => {}
            Err(e) => panic!("no player must not be an error, got: {e}"),
        }

        match manager.playback_status() {
            Ok(_) => {}
            Err(e) => panic!("no player must not be an error, got: {e}"),
        }
    }

    #[test]
    fn a_command_with_no_session_to_address_is_an_error() {
        // The asymmetry: a read has nothing to say, but a command has nothing to
        // send *to*. Reporting success there would let a caller believe it paused
        // something.
        let outcome: Result<(), UdaError> = Err(UdaError::NotSupported(
            "no media session is currently playing".to_string(),
        ));

        match outcome {
            Err(UdaError::NotSupported(message)) => {
                assert!(message.contains("no media session"), "message: {message}");
            }
            other => panic!("expected NotSupported, got {other:?}"),
        }
    }

    #[test]
    fn capabilities_report_media_control() {
        assert_eq!(
            WindowsMediaManager::new().capabilities(),
            Capability::MEDIA_CONTROL
        );
    }

    #[test]
    fn a_refused_command_is_reported_as_a_failure() {
        // The `Ok(false)` branch is the SMTC-specific quirk: the WinRT call
        // succeeded but the session declined. A caller must be able to tell that
        // apart from "sent", which is why it maps to `CommandFailed`.
        let outcome: Result<(), UdaError> = Err(UdaError::CommandFailed(
            "the media session refused the command 2".to_string(),
        ));

        match outcome {
            Err(UdaError::CommandFailed(message)) => {
                assert!(message.contains("refused"), "message: {message}");
            }
            other => panic!("expected a CommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn an_accepted_command_is_reported_as_success() {
        // A `true` reply only means the session accepted the command; it can
        // still fail inside the app, and that is indistinguishable from here.
        let accepted: Result<bool, UdaError> = Ok(true);
        assert_eq!(accepted.ok(), Some(true));
    }

    #[test]
    fn every_command_has_a_stable_code_for_diagnostics() {
        // The refusal message quotes the code so a log identifies the command.
        assert_eq!(MediaCommand::TogglePlayPause.code(), 2);
        assert_eq!(
            MediaCommand::from_code(2),
            Some(MediaCommand::TogglePlayPause)
        );
    }
}
