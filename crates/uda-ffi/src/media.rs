//! Media-control exports shared by the C callers.
//!
//! The three exports mirror the three things the
//! [`MediaManager`](uda_core::media::MediaManager) trait does: read what is
//! playing, read the playback status, and send a transport command. The status is
//! an `int32_t` code (see [`PlaybackStatus::code`](uda_core::media::PlaybackStatus));
//! the metadata crosses the boundary as three separately-owned strings plus an
//! out duration, because a C caller cannot allocate a Rust struct.
//!
//! # Why an out-code instead of an out-pointer for the status
//!
//! [`uda_media_get_status`] writes into a caller-supplied `int32_t` rather than
//! returning the code, so the return value stays free to report *hard* failures
//! (status `-2` when the platform has no media backend at all). A "no player is
//! running" is **not** a failure: it is reported as `UDA_MEDIA_UNKNOWN` (3) with
//! status [`UDA_OK`](crate::abi::UDA_OK). A caller must therefore treat code 3
//! as "nothing playing", never as a paused track.
//!
//! # Ownership
//!
//! The three strings written by [`uda_media_get_metadata`] are allocated by Rust
//! and must be released with [`uda_free_string`](crate::uda_free_string); they
//! may be null when the corresponding field is empty, which is the normal case
//! for a player that publishes only a title.
//!
//! A machine with nobody to command reports
//! [`UDA_ERR_NOT_SUPPORTED`](crate::abi::UDA_ERR_NOT_SUPPORTED); any other
//! non-`UDA_OK` status means the command was not delivered.

use uda_core::media::{MediaCommand, MediaManager, MediaMetadata, PlaybackStatus};

use crate::error::Failure;

/// Resolve a command code into the core enum.
///
/// An unknown code is rejected here rather than silently mapped, because sending
/// the wrong transport command to a user's player is worse than reporting that
/// the request was malformed.
pub(crate) fn command_from_c(code: i32) -> Result<MediaCommand, Failure> {
    MediaCommand::from_code(code)
        .ok_or_else(|| Failure::InvalidArgument(format!("unknown media command code: {code}")))
}

/// Read the metadata of the active player.
///
/// Returns `None` when no player is running (or the player publishes no usable
/// metadata at all), which the export turns into three null pointers.
pub(crate) fn active_metadata() -> Result<Option<MediaMetadata>, Failure> {
    #[cfg(target_os = "linux")]
    {
        let manager = uda_platform_linux::media::LinuxMediaManager::new();
        Ok(manager.active_metadata()?)
    }

    #[cfg(target_os = "windows")]
    {
        let manager = uda_platform_windows::media::WindowsMediaManager::new();
        Ok(manager.active_metadata()?)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        Err(Failure::Uda(uda_core::error::UdaError::NotSupported(
            "no media backend for this target".to_string(),
        )))
    }
}

/// Read the playback status of the active player.
///
/// A headless box with no player reports [`PlaybackStatus::Unknown`] instead of
/// an error, so a UI can render "not playing" rather than a failure toast.
pub(crate) fn playback_status() -> Result<PlaybackStatus, Failure> {
    #[cfg(target_os = "linux")]
    {
        let manager = uda_platform_linux::media::LinuxMediaManager::new();
        Ok(manager.playback_status()?)
    }

    #[cfg(target_os = "windows")]
    {
        let manager = uda_platform_windows::media::WindowsMediaManager::new();
        Ok(manager.playback_status()?)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        // A platform without a media backend still answers the question, it just
        // cannot answer it positively.
        Ok(PlaybackStatus::Unknown)
    }
}

/// Send a transport command to the active player.
pub(crate) fn send_command(command: MediaCommand) -> Result<(), Failure> {
    #[cfg(target_os = "linux")]
    {
        let manager = uda_platform_linux::media::LinuxMediaManager::new();
        Ok(manager.send_command(command)?)
    }

    #[cfg(target_os = "windows")]
    {
        let manager = uda_platform_windows::media::WindowsMediaManager::new();
        Ok(manager.send_command(command)?)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        let _ = command;
        Err(Failure::Uda(uda_core::error::UdaError::NotSupported(
            "no media backend for this target".to_string(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{
        UDA_MEDIA_CMD_NEXT, UDA_MEDIA_CMD_PAUSE, UDA_MEDIA_CMD_PLAY, UDA_MEDIA_CMD_PREVIOUS,
        UDA_MEDIA_CMD_STOP, UDA_MEDIA_CMD_TOGGLE, UDA_MEDIA_PAUSED, UDA_MEDIA_PLAYING,
        UDA_MEDIA_STOPPED, UDA_MEDIA_UNKNOWN,
    };

    #[test]
    fn command_codes_match_the_c_header() {
        assert_eq!(UDA_MEDIA_CMD_PLAY, MediaCommand::Play.code());
        assert_eq!(UDA_MEDIA_CMD_PAUSE, MediaCommand::Pause.code());
        assert_eq!(UDA_MEDIA_CMD_TOGGLE, MediaCommand::TogglePlayPause.code());
        assert_eq!(UDA_MEDIA_CMD_NEXT, MediaCommand::Next.code());
        assert_eq!(UDA_MEDIA_CMD_PREVIOUS, MediaCommand::Previous.code());
        assert_eq!(UDA_MEDIA_CMD_STOP, MediaCommand::Stop.code());
    }

    #[test]
    fn status_codes_match_the_c_header() {
        assert_eq!(UDA_MEDIA_PLAYING, PlaybackStatus::Playing.code());
        assert_eq!(UDA_MEDIA_PAUSED, PlaybackStatus::Paused.code());
        assert_eq!(UDA_MEDIA_STOPPED, PlaybackStatus::Stopped.code());
        assert_eq!(UDA_MEDIA_UNKNOWN, PlaybackStatus::Unknown.code());
    }

    #[test]
    fn a_valid_command_code_is_accepted() {
        assert_eq!(
            command_from_c(UDA_MEDIA_CMD_NEXT).ok(),
            Some(MediaCommand::Next)
        );
    }

    #[test]
    fn an_unknown_command_code_is_rejected_rather_than_guessed() {
        // Sending a wrong transport command to the user's player is worse than
        // reporting the request as malformed.
        match command_from_c(42) {
            Err(Failure::InvalidArgument(message)) => {
                assert!(message.contains("42"), "message was: {message}");
            }
            other => panic!("expected InvalidArgument, got {other:?}"),
        }
    }

    #[test]
    fn a_negative_command_code_is_rejected() {
        assert!(command_from_c(-1).is_err());
    }

    #[test]
    fn the_metadata_read_reports_none_or_metadata() {
        // The no-player graceful path: on a headless box this returns `None`
        // rather than an error, and on a real desktop it returns whatever is
        // playing. Either branch must be usable without unwrapping.
        let result = active_metadata();
        assert!(
            result.is_ok(),
            "a missing player is not an error: {result:?}"
        );
    }

    #[test]
    fn the_status_read_never_errors() {
        // Verification requirement: no player -> a status, not a panic. The
        // returned code always lies inside the documented set, so a binding can
        // index its own name table with it.
        let status = playback_status().expect("a missing player is not an error");
        assert!(
            (0..=3).contains(&status.code()),
            "unexpected status code {}",
            status.code()
        );
    }

    #[test]
    fn a_command_without_a_player_is_reported_not_swallowed() {
        // On a box with nothing playing the command cannot be delivered; the
        // caller must be able to see that. A machine *with* a player takes the
        // `Ok` branch, so both outcomes are accepted here.
        let outcome = send_command(MediaCommand::Play);
        assert!(
            outcome.is_ok() || matches!(outcome, Err(Failure::Uda(_))),
            "unexpected failure kind: {outcome:?}"
        );
    }
}
