//! Cross-platform media playback control interface.
//!
//! The model is deliberately small: one snapshot of what is playing, one enum of
//! what a player can be told to do, and three read/act methods. It mirrors what
//! both MPRIS v2 (Linux) and SMTC (Windows) can express; anything richer
//! (seek, shuffle, queue inspection) is left to the platform traits because the
//! two backends cannot agree on it.
//!
//! See `docs/internals/media_specs.md` for the protocol-level mapping.

use crate::capability::Capability;
use crate::error::UdaError;

/// What the active player is doing right now.
///
/// `Unknown` is a first-class state rather than an error: it covers "no session
/// has focus" (Windows), "the player did not publish the property" (MPRIS), and
/// "the bus answered but the value was unparseable". A caller must treat it as
/// "cannot tell", never as `Paused`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlaybackStatus {
    /// Audio or video is actively progressing.
    Playing,
    /// A track is selected and progress is halted.
    Paused,
    /// Nothing is loaded, or playback reached the end.
    Stopped,
    /// The state could not be determined.
    #[default]
    Unknown,
}

impl PlaybackStatus {
    /// The C-ABI code for this status.
    ///
    /// The mapping is part of the ABI: renumbering it breaks every binding.
    pub const fn code(self) -> i32 {
        match self {
            Self::Playing => 0,
            Self::Paused => 1,
            Self::Stopped => 2,
            Self::Unknown => 3,
        }
    }

    /// Recover a status from its C-ABI code.
    ///
    /// An unrecognised code becomes `Unknown` rather than an error, so an older
    /// binding cannot make a newer library refuse to answer.
    pub const fn from_code(code: i32) -> Self {
        match code {
            0 => Self::Playing,
            1 => Self::Paused,
            2 => Self::Stopped,
            _ => Self::Unknown,
        }
    }
}

/// A snapshot of the track the active player is holding.
///
/// Every field is owned and pre-joined for display: an MPRIS `xesam:artist` is a
/// list, but a UI wants one string, so the platform backend does that join
/// (`", "`) and the struct stays free of the wire types.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MediaMetadata {
    /// Track title. Empty when the player publishes none.
    pub title: String,
    /// Artist(s), already joined for display.
    pub artist: String,
    /// Album name.
    pub album: String,
    /// Track length in milliseconds, or `None` when unknown or unbounded
    /// (a live stream).
    pub duration_ms: Option<u64>,
    /// Playback position in milliseconds, or `None` when the backend cannot
    /// report it.
    pub position_ms: Option<u64>,
}

impl MediaMetadata {
    /// Whether this snapshot carries no usable information at all.
    ///
    /// Used to turn "the player answered, but said nothing" into the same
    /// `Ok(None)` a host with no player gets, so a binding never has to
    /// distinguish the two.
    pub fn is_empty(&self) -> bool {
        self.title.is_empty()
            && self.artist.is_empty()
            && self.album.is_empty()
            && self.duration_ms.is_none()
            && self.position_ms.is_none()
    }
}

/// An instruction for the active player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaCommand {
    /// Start playback.
    Play,
    /// Halt playback, keeping the position.
    Pause,
    /// Switch between playing and paused.
    TogglePlayPause,
    /// Advance to the next track.
    Next,
    /// Return to the previous track.
    Previous,
    /// Stop playback and unload.
    Stop,
}

impl MediaCommand {
    /// The C-ABI code for this command.
    pub const fn code(self) -> i32 {
        match self {
            Self::Play => 0,
            Self::Pause => 1,
            Self::TogglePlayPause => 2,
            Self::Next => 3,
            Self::Previous => 4,
            Self::Stop => 5,
        }
    }

    /// Recover a command from its C-ABI code.
    ///
    /// Returns `None` for an unrecognised code so the FFI boundary can report
    /// `UDA_ERR_INVALID_ARGUMENT` instead of sending a bogus instruction to the
    /// user's player.
    pub const fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::Play),
            1 => Some(Self::Pause),
            2 => Some(Self::TogglePlayPause),
            3 => Some(Self::Next),
            4 => Some(Self::Previous),
            5 => Some(Self::Stop),
            _ => None,
        }
    }
}

/// Read and drive the system's active media session.
pub trait MediaManager {
    /// Describe what the active player is holding.
    ///
    /// `Ok(None)` is the normal answer when nothing is playing *or* when no
    /// player responds; it is not a failure. A platform that cannot answer at
    /// all returns [`UdaError::NotSupported`].
    fn active_metadata(&self) -> Result<Option<MediaMetadata>, UdaError>;

    /// Report what the active player is doing.
    ///
    /// Returns [`PlaybackStatus::Unknown`] when there is no session or the state
    /// cannot be read, rather than an error.
    fn playback_status(&self) -> Result<PlaybackStatus, UdaError>;

    /// Send `command` to the active player.
    ///
    /// A player that refuses the command (pausing an already-paused stream, for
    /// instance) is a successful call: the platform cannot distinguish "declined"
    /// from "done" through these APIs, and reporting an error would make a
    /// perfectly normal toggle look like a failure.
    fn send_command(&self, command: MediaCommand) -> Result<(), UdaError>;

    /// The capabilities this platform's media backend can honour.
    fn capabilities(&self) -> Capability;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_status_codes_are_stable() {
        // These numbers are part of the C ABI (see include/uda.h), so a change
        // here silently breaks every binding.
        assert_eq!(PlaybackStatus::Playing.code(), 0);
        assert_eq!(PlaybackStatus::Paused.code(), 1);
        assert_eq!(PlaybackStatus::Stopped.code(), 2);
        assert_eq!(PlaybackStatus::Unknown.code(), 3);
    }

    #[test]
    fn playback_status_round_trips_every_code() {
        for status in [
            PlaybackStatus::Playing,
            PlaybackStatus::Paused,
            PlaybackStatus::Stopped,
            PlaybackStatus::Unknown,
        ] {
            assert_eq!(PlaybackStatus::from_code(status.code()), status);
        }
    }

    #[test]
    fn an_unknown_status_code_degrades_to_unknown() {
        // An older binding must be able to talk to a newer library that added a
        // state; guessing one of the known states would be worse.
        assert_eq!(PlaybackStatus::from_code(99), PlaybackStatus::Unknown);
        assert_eq!(PlaybackStatus::from_code(-1), PlaybackStatus::Unknown);
    }

    #[test]
    fn command_codes_are_stable() {
        assert_eq!(MediaCommand::Play.code(), 0);
        assert_eq!(MediaCommand::Pause.code(), 1);
        assert_eq!(MediaCommand::TogglePlayPause.code(), 2);
        assert_eq!(MediaCommand::Next.code(), 3);
        assert_eq!(MediaCommand::Previous.code(), 4);
        assert_eq!(MediaCommand::Stop.code(), 5);
    }

    #[test]
    fn command_round_trips_every_code() {
        for command in [
            MediaCommand::Play,
            MediaCommand::Pause,
            MediaCommand::TogglePlayPause,
            MediaCommand::Next,
            MediaCommand::Previous,
            MediaCommand::Stop,
        ] {
            assert_eq!(MediaCommand::from_code(command.code()), Some(command));
        }
    }

    #[test]
    fn an_unknown_command_code_is_rejected_rather_than_guessed() {
        // Sending a bogus instruction to the user's player is worse than
        // refusing the call, so this must stay `None`.
        assert_eq!(MediaCommand::from_code(6), None);
        assert_eq!(MediaCommand::from_code(-1), None);
    }

    #[test]
    fn a_default_metadata_is_empty() {
        // `is_empty` is what turns "the player said nothing" into the same
        // `Ok(None)` a host with no player produces.
        assert!(MediaMetadata::default().is_empty());
    }

    #[test]
    fn a_title_alone_is_enough_to_be_non_empty() {
        let metadata = MediaMetadata {
            title: "Song".to_string(),
            ..MediaMetadata::default()
        };
        assert!(!metadata.is_empty());
    }

    #[test]
    fn a_duration_alone_counts_as_information() {
        // A live-stream player often publishes nothing but a length.
        let metadata = MediaMetadata {
            duration_ms: Some(1),
            ..MediaMetadata::default()
        };
        assert!(!metadata.is_empty());
    }
}
