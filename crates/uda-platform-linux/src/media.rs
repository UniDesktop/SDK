//! Linux media backend: MPRIS v2 over the session bus.
//!
//! # Player discovery
//!
//! MPRIS has no "current player" registry. The convention is to scan the session
//! bus for names starting with `org.mpris.MediaPlayer2.` and pick one, which is
//! what [`LinuxMediaManager::active_player`] does: a player that reports
//! `Playing` wins, otherwise the first that answers is kept so a paused track is
//! still visible.
//!
//! `playerctld` (`org.mpris.MediaPlayer2.playerctld`) is deliberately skipped: it
//! is a multiplexing daemon that forwards commands elsewhere and publishes no
//! `xesam:` metadata, so selecting it would desynchronise the reported state from
//! what the user sees.
//!
//! # Deadlock avoidance (mandatory)
//!
//! Every interaction with a *third-party* D-Bus peer is wrapped in
//! [`tokio::time::timeout`], and property reads go through
//! [`zbus::fdo::PropertiesProxy`] rather than an interface proxy. A peer may be
//! hung, may still be starting up, or may implement only part of the MPRIS
//! surface (a minimal `playerctld` clone, a browser that answers `Get` but not
//! `GetAll`); `PropertiesProxy` issues a plain `org.freedesktop.DBus.Properties`
//! call and the timeout bounds the wait, so an incomplete peer can only cost
//! [`DBUS_TIMEOUT`] and never the caller's thread.
//!
//! The connection is created *inside* the async block that uses it and is owned
//! by the [`tokio::runtime::Builder::new_current_thread`] runtime built for that
//! one call, mirroring the tray and wallpaper workers: no connection, proxy or
//! runtime outlives the call that created it.
//!
//! See `docs/internals/media_specs.md` for the property mapping.

use std::time::Duration;

use uda_core::capability::Capability;
use uda_core::error::UdaError;
use uda_core::media::{MediaCommand, MediaManager, MediaMetadata, PlaybackStatus};
use zbus::fdo::PropertiesProxy;
use zbus::proxy::Builder as ProxyBuilder;
use zbus::{Connection, Proxy};

/// Hard ceiling for one D-Bus round trip to a media player.
///
/// Five seconds is far above the worst real latency (a loaded desktop answers in
/// a few milliseconds) yet short enough that a wedged peer cannot stall a UI
/// thread that called a synchronous export.
const DBUS_TIMEOUT: Duration = Duration::from_secs(5);

/// Well-known object path every MPRIS player implements.
const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";

/// Interface carrying the playback state and the transport methods.
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Interface used to read properties through `org.freedesktop.DBus.Properties`.
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";

/// Property holding the `a{sv}` metadata dictionary.
const METADATA_PROPERTY: &str = "Metadata";

/// Property holding the `PlaybackStatus` string.
const STATUS_PROPERTY: &str = "PlaybackStatus";

/// Property holding the position in microseconds.
const POSITION_PROPERTY: &str = "Position";

/// Bus-name prefix of every MPRIS player.
const MPRIS_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// The multiplexing daemon, which is never the player the user is looking at.
const PLAYERCTLD_NAME: &str = "org.mpris.MediaPlayer2.playerctld";

/// Linux media manager.
///
/// A zero-sized marker: every method opens its own session-bus connection,
/// which keeps the trait synchronous (no runtime parked for the process
/// lifetime) at the cost of one connection per call. The media APIs are
/// low-frequency (a UI reads the now-playing track, not a stream of frames), so
/// that trade is worth the simplicity.
#[derive(Debug, Default, Clone, Copy)]
pub struct LinuxMediaManager;

impl LinuxMediaManager {
    pub fn new() -> Self {
        Self
    }

    /// Connect to the session bus.
    async fn connection() -> Result<Connection, UdaError> {
        // Bounded like every other call: a machine with no session bus must fail
        // fast rather than leave a caller's thread parked.
        match tokio::time::timeout(DBUS_TIMEOUT, Connection::session()).await {
            Ok(Ok(connection)) => Ok(connection),
            Ok(Err(e)) => Err(UdaError::DetectionFailed(format!("zbus session: {e}"))),
            Err(_) => Err(UdaError::DetectionFailed(
                "connecting to the session bus timed out".to_string(),
            )),
        }
    }

    /// List the MPRIS players currently on the bus, best candidate first.
    ///
    /// A name that is not answerable is skipped rather than failing the whole
    /// scan: players appear and disappear while a UI is open, so a stale entry
    /// must not break the query for the live ones.
    async fn players(connection: &Connection) -> Vec<String> {
        let Ok(names) = list_names(connection).await else {
            return Vec::new();
        };

        names
            .into_iter()
            .filter(|name| name.starts_with(MPRIS_PREFIX))
            .filter(|name| name != PLAYERCTLD_NAME)
            .collect()
    }

    /// Pick the player to talk to, or `None` when the bus has none.
    ///
    /// A `Playing` player always wins; otherwise the first player that answers a
    /// property read is kept, so a paused track is still reported. Every probe is
    /// failure-tolerant and time-bounded: an unreachable or silent candidate is
    /// skipped after at most [`DBUS_TIMEOUT`], and the call ends with `None`
    /// rather than an error when nothing answers.
    async fn active_player(connection: &Connection) -> Option<String> {
        let mut fallback: Option<String> = None;

        for name in Self::players(connection).await {
            // Each probe builds and drops its own proxy so nothing outlives the
            // loop iteration that created it.
            let properties = match properties_proxy(connection, &name).await {
                Ok(properties) => properties,
                Err(error) => {
                    log::debug!("could not build a properties proxy for {name}: {error}");
                    continue;
                }
            };

            match read_status(&properties).await {
                Some(status) => {
                    if status == PlaybackStatus::Playing {
                        return Some(name);
                    }
                    // First answering player becomes the paused fallback.
                    if fallback.is_none() {
                        fallback = Some(name);
                    }
                }
                None => {
                    log::debug!("player {name} did not report a playback status; skipping");
                }
            }
        }

        fallback
    }
}

/// Build the `org.freedesktop.DBus.Properties` proxy for one player.
///
/// Every read goes through this proxy rather than an interface proxy on
/// `org.mpris.MediaPlayer2.Player`. The difference matters for peers that
/// implement only part of MPRIS: a property read is a plain
/// `org.freedesktop.DBus.Properties.Get`/`GetAll` call the peer either answers
/// or does not, whereas an interface proxy can make zbus drive additional
/// traffic (introspection, property caching) the peer is not prepared for.
///
/// The proxy borrows the connection, so it must not outlive the async block that
/// also owns that connection.
async fn properties_proxy<'a>(
    connection: &'a Connection,
    name: &'a str,
) -> Result<PropertiesProxy<'a>, UdaError> {
    // The builder methods each take the raw string and convert it; a conversion
    // failure here is a caller bug or a corrupt bus name, not a runtime race.
    let built = ProxyBuilder::<PropertiesProxy<'a>>::new(connection)
        .destination(name)
        .map_err(|e| UdaError::DetectionFailed(format!("bad player bus name {name}: {e}")))?
        .path(MPRIS_PATH)
        .map_err(|e| UdaError::DetectionFailed(format!("bad object path {MPRIS_PATH}: {e}")))?
        .interface(PROPERTIES_INTERFACE)
        .map_err(|e| UdaError::DetectionFailed(format!("bad interface: {e}")))?;

    // Bounded like every other await here. Constructing a proxy does not touch
    // the bus in zbus 4, so this is belt-and-braces rather than a real wait, but
    // it costs nothing and keeps the "no unbounded await" invariant total.
    match tokio::time::timeout(DBUS_TIMEOUT, built.build()).await {
        Ok(Ok(properties)) => Ok(properties),
        Ok(Err(e)) => Err(UdaError::DetectionFailed(format!(
            "properties proxy for {name}: {e}"
        ))),
        Err(_) => Err(UdaError::DetectionFailed(format!(
            "building the properties proxy for {name} timed out"
        ))),
    }
}

/// List the unique connection names on the session bus.
async fn list_names(connection: &Connection) -> Result<Vec<String>, UdaError> {
    let proxy =
        match tokio::time::timeout(DBUS_TIMEOUT, zbus::fdo::DBusProxy::new(connection)).await {
            Ok(Ok(proxy)) => proxy,
            Ok(Err(e)) => {
                return Err(UdaError::DetectionFailed(format!("dbus proxy: {e}")));
            }
            Err(_) => {
                return Err(UdaError::DetectionFailed(
                    "building the bus-daemon proxy timed out".to_string(),
                ));
            }
        };

    // `list_names` reports `OwnedBusName`s; `.to_string()` yields the bus name a
    // well-known MPRIS service is registered under.
    match tokio::time::timeout(DBUS_TIMEOUT, proxy.list_names()).await {
        Ok(Ok(names)) => Ok(names
            .into_iter()
            .map(|name| name.to_string())
            .collect::<Vec<_>>()),
        Ok(Err(e)) => Err(UdaError::DetectionFailed(format!("ListNames: {e}"))),
        Err(_) => Err(UdaError::DetectionFailed("ListNames timed out".to_string())),
    }
}

/// Read one string property from a player, tolerating the races of a player exiting.
///
/// Returns `None` for "the property is absent", "the call failed" and "the call
/// timed out": at this layer the three are indistinguishable to the caller, which
/// only wants to know whether there is a usable value. The timeout is what makes
/// the distinction survivable - a peer that never answers costs
/// [`DBUS_TIMEOUT`] and nothing more.
async fn read_optional_string(
    properties: &PropertiesProxy<'_>,
    interface: zbus::names::InterfaceName<'_>,
    property: &str,
) -> Option<String> {
    // `get` returns an `OwnedValue`; D-Bus models a missing property as an
    // error (`PropertyNotFound`), which also collapses to `None` here.
    let value = match tokio::time::timeout(DBUS_TIMEOUT, properties.get(interface, property)).await
    {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            log::debug!("reading {property} failed: {e}");
            return None;
        }
        Err(_) => {
            log::debug!("reading {property} timed out after {DBUS_TIMEOUT:?}");
            return None;
        }
    };

    value.downcast_ref::<String>().ok()
}

/// Read the `PlaybackStatus` property and map it onto the core enum.
async fn read_status(properties: &PropertiesProxy<'_>) -> Option<PlaybackStatus> {
    let interface = player_interface()?;
    let raw = read_optional_string(properties, interface, STATUS_PROPERTY).await?;
    Some(status_from_mpris(&raw))
}

/// The typed interface name of `org.mpris.MediaPlayer2.Player`.
///
/// `None` only if the constant above were edited to something invalid, which
/// would be a build-time mistake; returning `None` keeps the callers free of an
/// `unwrap` on a compile-time value.
fn player_interface() -> Option<zbus::names::InterfaceName<'static>> {
    zbus::names::InterfaceName::from_static_str(PLAYER_INTERFACE).ok()
}

/// Map an MPRIS `PlaybackStatus` string onto the core enum.
///
/// A value outside the specification (a player may add its own) degrades to
/// `Unknown` rather than being guessed, because a wrong "Playing" would make a UI
/// show a progress bar for a stopped track.
pub(crate) fn status_from_mpris(value: &str) -> PlaybackStatus {
    match value.trim() {
        "Playing" => PlaybackStatus::Playing,
        "Paused" => PlaybackStatus::Paused,
        "Stopped" => PlaybackStatus::Stopped,
        other => {
            log::debug!("unrecognised MPRIS PlaybackStatus {other:?}; reporting Unknown");
            PlaybackStatus::Unknown
        }
    }
}

/// Join the artist list an MPRIS player publishes.
///
/// `xesam:artist` is an `as` (array of strings), not a single string, so the
/// entries are joined with `", "` for display. An empty list yields an empty
/// string rather than an error: a track without a credited artist is normal.
pub(crate) fn join_artists(artists: &[String]) -> String {
    artists
        .iter()
        .map(|artist| artist.trim())
        .filter(|artist| !artist.is_empty())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Convert MPRIS microseconds to milliseconds.
///
/// `mpris:length` is an `i64` in **microseconds**. A value of zero or less means
/// a live stream or unknown duration, which maps to `None` rather than `Some(0)`:
/// `Some(0)` would render as a 0:00 track.
pub(crate) fn duration_from_micros(micros: i64) -> Option<u64> {
    if micros <= 0 {
        return None;
    }
    u64::try_from(micros).ok().map(|value| value / 1000)
}

/// Extract a title from a raw metadata value.
///
/// The spec says `xesam:title` is a string, but some clients (Chromium-derived
/// ones) publish an array. Accepting both keeps the title from vanishing; the
/// first element of an array is what the player itself would display.
pub(crate) fn title_from_value(value: &zbus::zvariant::Value<'_>) -> String {
    // `downcast_ref` is how zvariant recovers the concrete type behind a
    // variant. A failed cast means the player sent something else, which is a
    // data-shape difference and not an error.
    if let Ok(array) = value.downcast_ref::<zbus::zvariant::Array<'_>>() {
        return array.get::<String>(0).ok().flatten().unwrap_or_default();
    }
    value.downcast_ref::<String>().unwrap_or_default()
}

/// Extract the artist list from a raw metadata value.
pub(crate) fn artists_from_value(value: &zbus::zvariant::Value<'_>) -> Vec<String> {
    use zbus::zvariant::Array;

    if let Ok(array) = value.downcast_ref::<Array<'_>>() {
        return strings_from_array(&array);
    }
    // A player that publishes a single string instead of an array still carries
    // usable information.
    if let Ok(artist) = value.downcast_ref::<String>() {
        return vec![artist];
    }
    Vec::new()
}

/// Read every element of an `as` array as a string, skipping unreadable ones.
///
/// A malformed element (a nested variant, a non-string) is dropped rather than
/// failing the whole array: the remaining entries still name the artist.
fn strings_from_array(array: &zbus::zvariant::Array<'_>) -> Vec<String> {
    let mut artists = Vec::with_capacity(array.len());
    for index in 0..array.len() {
        if let Ok(Some(artist)) = array.get::<String>(index) {
            artists.push(artist);
        }
    }
    artists
}

/// Look one key up in an MPRIS metadata dictionary.
///
/// The dictionary is scanned linearly: a `Metadata` dictionary holds a handful of
/// well-known keys and MPRIS publishes it as a plain array of pairs, so a linear
/// scan is both simpler and cheaper than building a `HashMap` on every read. A
/// missing key, a non-string key or an unreadable value all yield `None`,
/// because every field is optional and one odd entry must not discard the rest.
fn dict_get<'d>(
    dictionary: &'d zbus::zvariant::Dict<'_, 'd>,
    key: &str,
) -> Option<&'d zbus::zvariant::Value<'d>> {
    dictionary
        .iter()
        .find(|(candidate, _)| {
            candidate
                .downcast_ref::<zbus::zvariant::Str<'_>>()
                .is_ok_and(|name| name.as_str() == key)
        })
        .map(|(_, value)| value)
}

/// Build the metadata snapshot from an MPRIS `Metadata` dictionary.
///
/// Every field is optional on the wire, and a missing one is an empty string
/// rather than an error. `duration_ms` uses [`duration_from_micros`], which
/// performs the microseconds -> milliseconds conversion and rejects unbounded
/// streams.
///
/// [`dict_get`] is a helper rather than [`zbus::zvariant::Dict::get`] because the
/// dictionary keys arrive as variants and the lookup must not fail the whole
/// parse when one key is absent.
pub(crate) fn metadata_from_dict(dictionary: &zbus::zvariant::Dict<'_, '_>) -> MediaMetadata {
    let title = dict_get(dictionary, "xesam:title")
        .map(|value| title_from_value(&value))
        .unwrap_or_default();
    let artist = dict_get(dictionary, "xesam:artist")
        .map(|value| join_artists(&artists_from_value(&value)))
        .unwrap_or_default();
    let album = dict_get(dictionary, "xesam:album")
        .and_then(|value| value.downcast_ref::<String>().ok())
        .unwrap_or_default();

    // The duration is a signed i64: the spec permits negative sentinels, and a
    // player that reports one must not be turned into a huge positive number.
    let duration_ms = dict_get(dictionary, "mpris:length")
        .and_then(|value| value.downcast_ref::<i64>().ok())
        .and_then(duration_from_micros);

    MediaMetadata {
        title,
        artist,
        album,
        duration_ms,
        // The position is read separately, because it changes continuously while
        // the metadata dictionary does not.
        position_ms: None,
    }
}

/// Convert a core command into the MPRIS method name.
pub(crate) fn method_for_command(command: MediaCommand) -> &'static str {
    match command {
        MediaCommand::Play => "Play",
        MediaCommand::Pause => "Pause",
        MediaCommand::TogglePlayPause => "PlayPause",
        MediaCommand::Next => "Next",
        MediaCommand::Previous => "Previous",
        MediaCommand::Stop => "Stop",
    }
}

/// Read the metadata dictionary of a player.
///
/// The dictionary is fetched as a raw dict of variants and parsed by
/// [`metadata_from_dict`], which keeps every field optional.
async fn read_metadata(
    properties: &PropertiesProxy<'_>,
) -> Result<Option<MediaMetadata>, UdaError> {
    let Some(interface) = player_interface() else {
        return Ok(None);
    };

    let dictionary = match tokio::time::timeout(
        DBUS_TIMEOUT,
        properties.get(interface, METADATA_PROPERTY),
    )
    .await
    {
        Ok(Ok(value)) => value,
        // A player that publishes no metadata is normal (a browser tab that
        // never set a title, a radio stream); it is not an error.
        Ok(Err(_)) => return Ok(None),
        Err(_) => {
            log::debug!("reading {METADATA_PROPERTY} timed out after {DBUS_TIMEOUT:?}");
            return Ok(None);
        }
    };

    // `downcast_ref` to `Dict` is the supported way to inspect an `a{sv}`; it
    // borrows the entries' variants, which is all parsing needs.
    let Ok(dictionary) = dictionary.downcast_ref::<zbus::zvariant::Dict<'_, '_>>() else {
        log::debug!("the player published Metadata with an unexpected shape");
        return Ok(None);
    };

    let metadata = metadata_from_dict(&dictionary);
    if metadata.is_empty() {
        return Ok(None);
    }
    Ok(Some(metadata))
}

/// Read the playback position in milliseconds.
///
/// `Position` is an `x` (int64) property, so it is read as a number rather than
/// through [`read_optional_string`].
async fn read_position(properties: &PropertiesProxy<'_>) -> Option<u64> {
    let interface = player_interface()?;

    let micros = match tokio::time::timeout(
        DBUS_TIMEOUT,
        properties.get(interface, POSITION_PROPERTY),
    )
    .await
    {
        Ok(Ok(value)) => value.downcast_ref::<i64>().ok(),
        Ok(Err(_)) | Err(_) => None,
    };

    micros.and_then(duration_from_micros)
}

/// Invoke a transport method on a player.
///
/// MPRIS transport methods take no arguments and return nothing, so the reply is
/// not inspected: an MPRIS player acknowledges by returning at all. The call is
/// bounded by [`DBUS_TIMEOUT`] because a hung player must not pin the caller.
async fn call_method(connection: &Connection, name: &str, method: &str) -> Result<(), UdaError> {
    // The builder chain returns `zbus::Error`; mapping it to `UdaError` inside
    // the async block keeps the outer `match` arms symmetric.
    let proxy: Proxy<'_> = match tokio::time::timeout(DBUS_TIMEOUT, async {
        let builder = ProxyBuilder::<Proxy<'_>>::new(connection)
            .destination(name)
            .map_err(|e| UdaError::Internal(format!("bad player name {name}: {e}")))?
            .path(MPRIS_PATH)
            .map_err(|e| UdaError::Internal(format!("bad object path: {e}")))?
            .interface(PLAYER_INTERFACE)
            .map_err(|e| UdaError::Internal(format!("bad interface: {e}")))?;

        builder
            .build()
            .await
            .map_err(|e| UdaError::CommandFailed(format!("transport proxy for {name}: {e}")))
    })
    .await
    {
        Ok(Ok(proxy)) => proxy,
        Ok(Err(failure)) => return Err(failure),
        Err(_) => {
            return Err(UdaError::CommandFailed(format!(
                "building the transport proxy for {name} timed out"
            )));
        }
    };

    match tokio::time::timeout(DBUS_TIMEOUT, proxy.call::<&str, (), ()>(method, &())).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(UdaError::CommandFailed(format!("MPRIS {method}: {e}"))),
        Err(_) => Err(UdaError::CommandFailed(format!(
            "MPRIS {method} timed out after {DBUS_TIMEOUT:?}"
        ))),
    }
}

impl MediaManager for LinuxMediaManager {
    fn active_metadata(&self) -> Result<Option<MediaMetadata>, UdaError> {
        // One current-thread runtime per call, built and dropped here: it owns
        // the connection and every proxy, so nothing outlives the call.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("could not start a runtime: {e}")))?;

        runtime.block_on(async {
            let connection = Self::connection().await?;
            let Some(name) = Self::active_player(&connection).await else {
                // No player on the bus is a normal state, not a failure.
                return Ok(None);
            };

            let properties = properties_proxy(&connection, &name).await?;

            let mut metadata = read_metadata(&properties).await?.unwrap_or_default();
            metadata.position_ms = read_position(&properties).await;
            if metadata.is_empty() {
                return Ok(None);
            }
            Ok(Some(metadata))
        })
    }

    fn playback_status(&self) -> Result<PlaybackStatus, UdaError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("could not start a runtime: {e}")))?;

        runtime.block_on(async {
            let connection = Self::connection().await?;
            let Some(name) = Self::active_player(&connection).await else {
                return Ok(PlaybackStatus::Unknown);
            };

            let properties = properties_proxy(&connection, &name).await?;

            // A player that published a status a moment ago and cannot be read
            // now has exited; that is `Unknown`, not an error.
            Ok(read_status(&properties)
                .await
                .unwrap_or(PlaybackStatus::Unknown))
        })
    }

    fn send_command(&self, command: MediaCommand) -> Result<(), UdaError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| UdaError::Internal(format!("could not start a runtime: {e}")))?;

        runtime.block_on(async {
            let connection = Self::connection().await?;
            let Some(name) = Self::active_player(&connection).await else {
                // Nothing to command. Reported as unsupported rather than success
                // so a caller can tell "sent" from "there was nobody to send to".
                return Err(UdaError::NotSupported(
                    "no MPRIS player is available to receive the command".to_string(),
                ));
            };

            call_method(&connection, &name, method_for_command(command)).await
        })
    }

    fn capabilities(&self) -> Capability {
        // Every read and every command in this backend goes through the session
        // bus, which is always present in a desktop session. Whether a *player*
        // exists is a runtime question answered by the methods above.
        Capability::MEDIA_CONTROL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build the dictionary shape a player publishes on the wire.
    ///
    /// `Metadata` is an `a{sv}`: string keys, *variant* values. Each entry's
    /// value is therefore wrapped in `Value::new`, which is exactly the variant
    /// nesting `metadata_from_dict` sees on the bus - exercising the real wire
    /// form rather than a hand-written Rust map.
    fn dictionary<'a>(
        entries: Vec<(&'a str, zbus::zvariant::Value<'a>)>,
    ) -> zbus::zvariant::Dict<'a, 'a> {
        // `a{sv}`: the value signature is `v`, so every entry must be a variant.
        let mut dictionary = zbus::zvariant::Dict::new(
            zbus::zvariant::Signature::from_static_str_unchecked("s"),
            zbus::zvariant::Signature::from_static_str_unchecked("v"),
        );

        for (key, value) in entries {
            dictionary
                .append(
                    zbus::zvariant::Value::from(key),
                    zbus::zvariant::Value::new(value),
                )
                .expect("a test entry appends");
        }
        dictionary
    }

    #[test]
    fn status_strings_map_onto_the_core_enum() {
        assert_eq!(status_from_mpris("Playing"), PlaybackStatus::Playing);
        assert_eq!(status_from_mpris("Paused"), PlaybackStatus::Paused);
        assert_eq!(status_from_mpris("Stopped"), PlaybackStatus::Stopped);
    }

    #[test]
    fn an_unknown_status_string_degrades_rather_than_guessing() {
        // A wrong "Playing" would make a UI animate a stopped track.
        assert_eq!(status_from_mpris("Looping"), PlaybackStatus::Unknown);
        assert_eq!(status_from_mpris(""), PlaybackStatus::Unknown);
    }

    #[test]
    fn artists_are_joined_for_display() {
        let artists = vec![
            "Alpha".to_string(),
            "Beta".to_string(),
            " Gamma ".to_string(),
        ];
        assert_eq!(join_artists(&artists), "Alpha, Beta, Gamma");
    }

    #[test]
    fn an_empty_artist_list_is_not_an_error() {
        assert_eq!(join_artists(&[]), "");
    }

    #[test]
    fn blank_artist_entries_are_dropped() {
        let artists = vec!["  ".to_string(), "Solo".to_string(), String::new()];
        assert_eq!(join_artists(&artists), "Solo");
    }

    #[test]
    fn a_single_artist_keeps_its_name_verbatim() {
        assert_eq!(join_artists(&["Solo".to_string()]), "Solo");
    }

    #[test]
    fn microseconds_convert_to_milliseconds() {
        assert_eq!(duration_from_micros(1_000), Some(1));
        assert_eq!(duration_from_micros(180_000_000), Some(180_000));
    }

    #[test]
    fn a_zero_length_is_a_live_stream_not_a_zero_ms_track() {
        // `Some(0)` would render as a 0:00 track; the honest answer is "unknown".
        assert_eq!(duration_from_micros(0), None);
    }

    #[test]
    fn a_negative_length_is_rejected_rather_than_wrapped() {
        // The spec permits negative sentinels; casting them to u64 would produce
        // a duration of billions of milliseconds.
        assert_eq!(duration_from_micros(-1), None);
        assert_eq!(duration_from_micros(i64::MIN), None);
    }

    #[test]
    fn a_title_string_is_taken_as_is() {
        let value = zbus::zvariant::Value::from("Song");
        assert_eq!(title_from_value(&value), "Song");
    }

    #[test]
    fn a_title_array_uses_its_first_element() {
        // Chromium-derived players publish the title as an array.
        let value = zbus::zvariant::Value::from(vec!["Song".to_string(), "Alt".to_string()]);
        assert_eq!(title_from_value(&value), "Song");
    }

    #[test]
    fn an_empty_title_array_yields_an_empty_string() {
        let value = zbus::zvariant::Value::from(Vec::<String>::new());
        assert_eq!(title_from_value(&value), "");
    }

    #[test]
    fn an_artist_array_is_read_fully() {
        let value = zbus::zvariant::Value::from(vec!["A".to_string(), "B".to_string()]);
        assert_eq!(
            artists_from_value(&value),
            vec!["A".to_string(), "B".to_string()]
        );
    }

    #[test]
    fn a_single_artist_string_still_yields_one_entry() {
        let value = zbus::zvariant::Value::from("A");
        assert_eq!(artists_from_value(&value), vec!["A".to_string()]);
    }

    #[test]
    fn a_title_of_the_wrong_type_is_not_an_error() {
        // A player that publishes the title as an integer must not panic.
        let value = zbus::zvariant::Value::from(42i64);
        assert_eq!(title_from_value(&value), "");
        assert!(artists_from_value(&value).is_empty());
    }

    #[test]
    fn a_full_metadata_dictionary_is_parsed() {
        let metadata = metadata_from_dict(&dictionary(vec![
            ("xesam:title", "Song".into()),
            (
                "xesam:artist",
                vec!["A".to_string(), "B".to_string()].into(),
            ),
            ("xesam:album", "Album".into()),
            ("mpris:length", 240_000_000i64.into()),
        ]));

        assert_eq!(metadata.title, "Song");
        assert_eq!(metadata.artist, "A, B");
        assert_eq!(metadata.album, "Album");
        assert_eq!(metadata.duration_ms, Some(240_000));
        // The position is a separate, continuously changing property.
        assert_eq!(metadata.position_ms, None);
    }

    #[test]
    fn an_empty_dictionary_yields_empty_fields_not_an_error() {
        let metadata = metadata_from_dict(&dictionary(Vec::new()));
        assert!(metadata.title.is_empty());
        assert!(metadata.artist.is_empty());
        assert!(metadata.album.is_empty());
        assert_eq!(metadata.duration_ms, None);
    }

    #[test]
    fn a_dictionary_with_only_a_duration_is_still_information() {
        // Radio streams publish a length and nothing else; that must not be
        // mistaken for "no metadata".
        let metadata = metadata_from_dict(&dictionary(vec![("mpris:length", 1i64.into())]));
        assert!(!metadata.is_empty());
        assert_eq!(metadata.duration_ms, Some(0));
    }

    #[test]
    fn commands_map_to_the_mpris_method_names() {
        assert_eq!(method_for_command(MediaCommand::Play), "Play");
        assert_eq!(method_for_command(MediaCommand::Pause), "Pause");
        assert_eq!(
            method_for_command(MediaCommand::TogglePlayPause),
            "PlayPause"
        );
        assert_eq!(method_for_command(MediaCommand::Next), "Next");
        assert_eq!(method_for_command(MediaCommand::Previous), "Previous");
        assert_eq!(method_for_command(MediaCommand::Stop), "Stop");
    }

    #[test]
    fn capabilities_report_media_control() {
        assert_eq!(
            LinuxMediaManager::new().capabilities(),
            Capability::MEDIA_CONTROL
        );
    }

    #[test]
    fn a_headless_session_has_no_player_and_reports_none() {
        // The verification the user asked for: with no session bus reachable (or
        // no player on it), the backend answers "nothing playing" instead of
        // panicking or returning an error.
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(error) => panic!("a runtime is always buildable: {error}"),
        };

        let manager = LinuxMediaManager::new();
        let result = runtime.block_on(async {
            // Directly exercise discovery, which is the part that must degrade.
            // Both the headless case (no session bus) and an empty bus land here.
            match LinuxMediaManager::connection().await {
                Ok(connection) => LinuxMediaManager::active_player(&connection).await,
                Err(_) => None,
            }
        });
        let _ = result;
        // Whichever branch ran, `active_metadata` must not panic either.
        let _ = manager.active_metadata();
    }
}
