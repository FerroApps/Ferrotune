//! Spotify Connect device controller.
//!
//! Connect allows Spotify devices to discover each other and control playback
//! remotely.  The lifecycle is:
//!
//! 1. **Register** the device via PUT to
//!    `/connect-state/v1/devices/hobs_{device_id}` (SpClient).
//! 2. **Subscribe** to `hm://connect-state/v1/cluster` and
//!    `hm://connect-state/v1/connect/volume` on the Dealer WebSocket.
//! 3. **Process** incoming commands (play, pause, skip, volume, transfer).
//!
//! This module provides the types for device description, the registration
//! payload builder, and command parsing from dealer JSON messages.  It is pure
//! (no I/O) so it can be tested without a network.
//!
//! # Key URIs
//!
//! | Purpose | URI |
//! |---------|-----|
//! | Registration | `/connect-state/v1/devices/hobs_{device_id}` |
//! | Cluster updates | `hm://connect-state/v1/cluster` |
//! | Volume | `hm://connect-state/v1/connect/volume` |
//! | Player commands | `hm://connect-state/v1/player/command` |
//! | Transfer | `hm://connect-state/v1/connect/transfer/from/{device_id}` |

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Dealer URI prefix for cluster state updates.
pub const CLUSTER_URI: &str = "hm://connect-state/v1/cluster";

/// Dealer URI prefix for volume commands.
pub const VOLUME_URI: &str = "hm://connect-state/v1/connect/volume";

/// Dealer URI prefix for player commands.
pub const PLAYER_COMMAND_URI: &str = "hm://connect-state/v1/player/command";

/// Dealer URI prefix for transfer commands.
pub const TRANSFER_URI: &str = "hm://connect-state/v1/connect/transfer";

/// SpClient path prefix for device registration.
pub const DEVICES_PATH: &str = "/connect-state/v1/devices";

/// Hidden observer device-id prefix (matches what the web player sends).
pub const OBSERVER_PREFIX: &str = "hobs_";

/// The Spirc protocol version advertised during registration.
const SPIRC_VERSION: &str = "3.2.6";

/// The software version string sent during registration.
const SOFTWARE_VERSION: &str = "Spotify-1.3.8.620";

// ---------------------------------------------------------------------------
// DeviceType
// ---------------------------------------------------------------------------

/// Spotify Connect device type, matching the official protocol values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum DeviceType {
    Computer = 1,
    Tablet = 2,
    Smartphone = 3,
    Speaker = 4,
    Tv = 5,
    Avr = 6,
    Stb = 7,
    AudioDongle = 8,
    GameConsole = 9,
    CastVideo = 10,
    CastAudio = 11,
    Automobile = 12,
    Smartwatch = 14,
    Chromebook = 15,
}

impl DeviceType {
    /// The uppercase string form used in the registration JSON payload.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Computer => "COMPUTER",
            Self::Tablet => "TABLET",
            Self::Smartphone => "SMARTPHONE",
            Self::Speaker => "SPEAKER",
            Self::Tv => "TV",
            Self::Avr => "AVR",
            Self::Stb => "STB",
            Self::AudioDongle => "AUDIO_DONGLE",
            Self::GameConsole => "GAME_CONSOLE",
            Self::CastVideo => "CAST_VIDEO",
            Self::CastAudio => "CAST_AUDIO",
            Self::Automobile => "AUTOMOBILE",
            Self::Smartwatch => "SMARTWATCH",
            Self::Chromebook => "CHROMEBOOK",
        }
    }

    /// Parse from the uppercase string form used in the protocol.
    pub fn from_str_name(s: &str) -> Option<Self> {
        match s {
            "COMPUTER" => Some(Self::Computer),
            "TABLET" => Some(Self::Tablet),
            "SMARTPHONE" => Some(Self::Smartphone),
            "SPEAKER" => Some(Self::Speaker),
            "TV" => Some(Self::Tv),
            "AVR" => Some(Self::Avr),
            "STB" => Some(Self::Stb),
            "AUDIO_DONGLE" => Some(Self::AudioDongle),
            "GAME_CONSOLE" => Some(Self::GameConsole),
            "CAST_VIDEO" => Some(Self::CastVideo),
            "CAST_AUDIO" => Some(Self::CastAudio),
            "AUTOMOBILE" => Some(Self::Automobile),
            "SMARTWATCH" => Some(Self::Smartwatch),
            "CHROMEBOOK" => Some(Self::Chromebook),
            _ => None,
        }
    }
}

impl std::fmt::Display for DeviceType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// RepeatMode
// ---------------------------------------------------------------------------

/// Playback repeat mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    Off,
    Context,
    Track,
}

impl RepeatMode {
    /// Parse from the string form used in the Connect protocol.
    fn from_str_name(s: &str) -> Option<Self> {
        match s {
            "off" | "OFF" | "none" | "NONE" => Some(Self::Off),
            "context" | "CONTEXT" => Some(Self::Context),
            "track" | "TRACK" => Some(Self::Track),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// DeviceInfo
// ---------------------------------------------------------------------------

/// Device capabilities sent during registration.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// The unique device identifier (hex string).
    pub device_id: String,
    /// Human-readable device name.
    pub device_name: String,
    /// What kind of device this is.
    pub device_type: DeviceType,
    /// Current volume (0 .. `max_volume`).
    pub volume: u32,
    /// Maximum volume value (typically 65535).
    pub max_volume: u32,
    /// Whether this device is currently the active playback target.
    pub is_active: bool,
}

// ---------------------------------------------------------------------------
// ConnectCommand
// ---------------------------------------------------------------------------

/// Commands received from the Connect service via dealer messages.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectCommand {
    /// Start playback.
    Play {
        context_uri: Option<String>,
        track_uri: Option<String>,
        offset: Option<u32>,
    },
    /// Pause playback.
    Pause,
    /// Resume playback.
    Resume,
    /// Seek to a position.
    SeekTo { position_ms: u32 },
    /// Skip to the next track.
    SkipNext,
    /// Skip to the previous track.
    SkipPrev,
    /// Set the playback volume.
    SetVolume { volume: u32 },
    /// Toggle shuffle mode.
    SetShuffle { shuffle: bool },
    /// Set repeat mode.
    SetRepeat { repeat: RepeatMode },
    /// Transfer playback from another device.
    Transfer { from_device: String },
    /// Remote logout request.
    Logout,
    /// A command we don't recognise yet.
    Unknown(String),
}

// ---------------------------------------------------------------------------
// ConnectState
// ---------------------------------------------------------------------------

/// The Connect state manager.
///
/// Holds the local device description and builds the JSON payloads that the
/// Connect protocol requires.  It is intentionally I/O-free: the caller is
/// responsible for sending them over SpClient / Mercury.
pub struct ConnectState {
    device: DeviceInfo,
}

impl ConnectState {
    /// Create a new Connect state for the given device.
    pub fn new(device: DeviceInfo) -> Self {
        Self { device }
    }

    /// The local device id.
    pub fn device_id(&self) -> &str {
        &self.device.device_id
    }

    /// The local device name.
    pub fn device_name(&self) -> &str {
        &self.device.device_name
    }

    /// The SpClient path for registering this device.
    ///
    /// Returns e.g. `/connect-state/v1/devices/hobs_<device_id>`.
    pub fn registration_path(&self) -> String {
        format!("{DEVICES_PATH}/{OBSERVER_PREFIX}{}", self.device.device_id)
    }

    /// Build the device registration payload (JSON).
    ///
    /// This matches the structure the official desktop client sends when
    /// registering with the connect-state service.
    pub fn registration_payload(&self) -> serde_json::Value {
        let volume_steps = if self.device.max_volume > 0 {
            // Official client typically sends 64 volume steps.
            64
        } else {
            0
        };

        serde_json::json!({
            "member_type": "CONNECT_STATE",
            "device": {
                "device_info": {
                    "can_play": true,
                    "volume": self.device.volume,
                    "name": self.device.device_name,
                    "device_id": self.device.device_id,
                    "device_type": self.device.device_type.as_str(),
                    "device_software_version": SOFTWARE_VERSION,
                    "spirc_version": SPIRC_VERSION,
                    "capabilities": {
                        "can_be_player": true,
                        "gaia_eq_connect_id": true,
                        "supports_logout": true,
                        "is_observable": true,
                        "volume_steps": volume_steps,
                        "supported_types": ["audio/episode", "audio/track"],
                        "command_acks": true,
                        "supports_rename": false,
                        "supports_playlist_v2": true,
                        "supports_transfer_command": true,
                        "supports_command_request": true,
                        "supports_gzip_pushes": true,
                        "needs_full_player_state": true
                    }
                }
            }
        })
    }

    /// Build a "set active" state update payload.
    ///
    /// Used to tell the Connect service whether this device is the active
    /// playback target.
    pub fn set_active_payload(&self, is_active: bool) -> serde_json::Value {
        serde_json::json!({
            "member_type": "CONNECT_STATE",
            "device": {
                "device_info": {
                    "device_id": self.device.device_id,
                    "name": self.device.device_name,
                    "device_type": self.device.device_type.as_str(),
                    "can_play": true,
                    "volume": self.device.volume,
                    "is_active": is_active,
                    "device_software_version": SOFTWARE_VERSION,
                    "spirc_version": SPIRC_VERSION,
                }
            }
        })
    }

    /// Update the stored volume.
    pub fn set_volume(&mut self, volume: u32) {
        self.device.volume = volume;
        debug!(volume, "connect: volume updated");
    }

    /// Parse a dealer message into a [`ConnectCommand`].
    ///
    /// The `uri` is the dealer message URI and `payload` its decoded JSON
    /// body.  Returns `None` if the URI is not a recognised Connect command.
    pub fn parse_command(uri: &str, payload: &serde_json::Value) -> Option<ConnectCommand> {
        // Transfer: hm://connect-state/v1/connect/transfer/from/{device_id}
        if let Some(rest) = uri.strip_prefix("hm://connect-state/v1/connect/transfer/from/") {
            let from_device = rest.split('/').next().unwrap_or(rest).to_owned();
            return Some(ConnectCommand::Transfer { from_device });
        }

        // Volume: hm://connect-state/v1/connect/volume
        if uri.starts_with(VOLUME_URI) {
            let volume = payload.get("volume").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            return Some(ConnectCommand::SetVolume { volume });
        }

        // Player command: hm://connect-state/v1/player/command
        if uri.starts_with(PLAYER_COMMAND_URI) {
            return Some(parse_player_command(payload));
        }

        // Cluster update: not a command to us, return None so the caller can
        // handle it as a state update.
        if uri.starts_with(CLUSTER_URI) {
            return None;
        }

        warn!(uri, "connect: unrecognised dealer URI");
        None
    }
}

impl std::fmt::Debug for ConnectState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConnectState")
            .field("device_id", &self.device.device_id)
            .field("device_name", &self.device.device_name)
            .field("device_type", &self.device.device_type)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Command parsing helpers
// ---------------------------------------------------------------------------

/// Parse the `command` / `endpoint` field inside a player-command payload.
fn parse_player_command(payload: &serde_json::Value) -> ConnectCommand {
    // The payload can carry the command name in several places depending on
    // the Connect protocol version.  We try `command.endpoint` first, then
    // fall back to a top-level `endpoint` string.
    let endpoint = payload
        .get("command")
        .and_then(|c| c.get("endpoint"))
        .and_then(|e| e.as_str())
        .or_else(|| payload.get("endpoint").and_then(|e| e.as_str()))
        .unwrap_or("");

    // The command-specific options live under "command.options" or directly in
    // the payload, depending on the wire format.
    let options = payload
        .get("command")
        .and_then(|c| c.get("options"))
        .unwrap_or(payload);

    match endpoint {
        "play" => {
            let context_uri = options
                .get("context_uri")
                .or_else(|| {
                    payload
                        .get("command")
                        .and_then(|c| c.get("context").and_then(|ctx| ctx.get("uri")))
                })
                .and_then(|v| v.as_str())
                .map(str::to_owned);

            let track_uri = options
                .get("track_uri")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    options
                        .get("skip_to")
                        .and_then(|s| s.get("track_uri").and_then(|v| v.as_str()))
                })
                .map(str::to_owned);

            let offset = options
                .get("skip_to")
                .and_then(|s| s.get("track_index").and_then(|v| v.as_u64()))
                .map(|v| v as u32);

            ConnectCommand::Play {
                context_uri,
                track_uri,
                offset,
            }
        }
        "pause" => ConnectCommand::Pause,
        "resume" => ConnectCommand::Resume,
        "seek_to" => {
            let position_ms = options
                .get("value")
                .or_else(|| options.get("position"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            ConnectCommand::SeekTo { position_ms }
        }
        "skip_next" => ConnectCommand::SkipNext,
        "skip_prev" => ConnectCommand::SkipPrev,
        "set_volume" => {
            let volume = options
                .get("value")
                .or_else(|| options.get("volume"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            ConnectCommand::SetVolume { volume }
        }
        "set_shuffling_context" | "set_shuffle" => {
            let shuffle = options
                .get("value")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            ConnectCommand::SetShuffle { shuffle }
        }
        "set_repeating_context" | "set_repeating_track" | "set_repeat" => {
            let repeat = options
                .get("value")
                .and_then(|v| v.as_str())
                .and_then(RepeatMode::from_str_name);

            // set_repeating_context / set_repeating_track are toggle-style:
            // when used with a bool value, map to the appropriate mode.
            let repeat = repeat.unwrap_or_else(|| {
                let on = options
                    .get("value")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                match endpoint {
                    "set_repeating_track" if on => RepeatMode::Track,
                    "set_repeating_context" if on => RepeatMode::Context,
                    _ => RepeatMode::Off,
                }
            });
            ConnectCommand::SetRepeat { repeat }
        }
        "logout" => ConnectCommand::Logout,
        other => {
            debug!(endpoint = other, "connect: unknown player command");
            ConnectCommand::Unknown(other.to_owned())
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version;

    fn test_device() -> DeviceInfo {
        DeviceInfo {
            device_id: "abc123def456".to_owned(),
            device_name: "FerroTune".to_owned(),
            device_type: DeviceType::Computer,
            volume: 32768,
            max_volume: 65535,
            is_active: false,
        }
    }

    // -- DeviceType ----------------------------------------------------------

    #[test]
    fn device_type_as_str_round_trip() {
        let types = [
            (DeviceType::Computer, "COMPUTER"),
            (DeviceType::Tablet, "TABLET"),
            (DeviceType::Smartphone, "SMARTPHONE"),
            (DeviceType::Speaker, "SPEAKER"),
            (DeviceType::Tv, "TV"),
            (DeviceType::Avr, "AVR"),
            (DeviceType::Stb, "STB"),
            (DeviceType::AudioDongle, "AUDIO_DONGLE"),
            (DeviceType::GameConsole, "GAME_CONSOLE"),
            (DeviceType::CastVideo, "CAST_VIDEO"),
            (DeviceType::CastAudio, "CAST_AUDIO"),
            (DeviceType::Automobile, "AUTOMOBILE"),
            (DeviceType::Smartwatch, "SMARTWATCH"),
            (DeviceType::Chromebook, "CHROMEBOOK"),
        ];

        for (dt, name) in types {
            assert_eq!(dt.as_str(), name, "as_str mismatch for {dt:?}");
            assert_eq!(
                DeviceType::from_str_name(name),
                Some(dt),
                "from_str_name mismatch for {name}"
            );
        }
    }

    #[test]
    fn device_type_from_unknown_returns_none() {
        assert_eq!(DeviceType::from_str_name("FRIDGE"), None);
    }

    #[test]
    fn device_type_display() {
        assert_eq!(DeviceType::Computer.to_string(), "COMPUTER");
        assert_eq!(DeviceType::Automobile.to_string(), "AUTOMOBILE");
    }

    #[test]
    fn device_type_repr_values() {
        assert_eq!(DeviceType::Computer as u8, 1);
        assert_eq!(DeviceType::Smartwatch as u8, 14);
        assert_eq!(DeviceType::Chromebook as u8, 15);
    }

    // -- Registration payload ------------------------------------------------

    #[test]
    fn registration_payload_structure() {
        let state = ConnectState::new(test_device());
        let payload = state.registration_payload();

        assert_eq!(payload["member_type"], "CONNECT_STATE");

        let info = &payload["device"]["device_info"];
        assert_eq!(info["name"], "FerroTune");
        assert_eq!(info["device_id"], "abc123def456");
        assert_eq!(info["device_type"], "COMPUTER");
        assert_eq!(info["can_play"], true);
        assert_eq!(info["volume"], 32768);
        assert_eq!(info["device_software_version"], SOFTWARE_VERSION);
        assert_eq!(info["spirc_version"], SPIRC_VERSION);

        let caps = &info["capabilities"];
        assert_eq!(caps["can_be_player"], true);
        assert_eq!(caps["supports_logout"], true);
        assert_eq!(caps["supports_transfer_command"], true);
        assert_eq!(caps["supports_gzip_pushes"], true);
        assert_eq!(caps["needs_full_player_state"], true);
        assert_eq!(caps["volume_steps"], 64);
        assert_eq!(caps["command_acks"], true);

        let types = caps["supported_types"]
            .as_array()
            .expect("supported_types array");
        assert!(types.iter().any(|v| v == "audio/track"));
        assert!(types.iter().any(|v| v == "audio/episode"));
    }

    #[test]
    fn registration_path_uses_hobs_prefix() {
        let state = ConnectState::new(test_device());
        assert_eq!(
            state.registration_path(),
            "/connect-state/v1/devices/hobs_abc123def456"
        );
    }

    // -- set_active_payload --------------------------------------------------

    #[test]
    fn set_active_payload_active() {
        let state = ConnectState::new(test_device());
        let payload = state.set_active_payload(true);

        let info = &payload["device"]["device_info"];
        assert_eq!(info["is_active"], true);
        assert_eq!(info["device_id"], "abc123def456");
    }

    #[test]
    fn set_active_payload_inactive() {
        let state = ConnectState::new(test_device());
        let payload = state.set_active_payload(false);
        assert_eq!(payload["device"]["device_info"]["is_active"], false);
    }

    // -- set_volume ----------------------------------------------------------

    #[test]
    fn set_volume_updates_state() {
        let mut state = ConnectState::new(test_device());
        assert_eq!(
            state.registration_payload()["device"]["device_info"]["volume"],
            32768
        );

        state.set_volume(50000);

        assert_eq!(
            state.registration_payload()["device"]["device_info"]["volume"],
            50000
        );
    }

    // -- Command parsing: transfer -------------------------------------------

    #[test]
    fn parse_transfer_command() {
        let cmd = ConnectState::parse_command(
            "hm://connect-state/v1/connect/transfer/from/other_device_id",
            &serde_json::Value::Null,
        );

        assert_eq!(
            cmd,
            Some(ConnectCommand::Transfer {
                from_device: "other_device_id".to_owned(),
            })
        );
    }

    #[test]
    fn parse_transfer_with_trailing_path() {
        let cmd = ConnectState::parse_command(
            "hm://connect-state/v1/connect/transfer/from/dev123/extra",
            &serde_json::Value::Null,
        );

        assert_eq!(
            cmd,
            Some(ConnectCommand::Transfer {
                from_device: "dev123".to_owned(),
            })
        );
    }

    // -- Command parsing: volume (from URI) ----------------------------------

    #[test]
    fn parse_volume_command_from_uri() {
        let payload = serde_json::json!({ "volume": 42000 });
        let cmd = ConnectState::parse_command("hm://connect-state/v1/connect/volume", &payload);

        assert_eq!(cmd, Some(ConnectCommand::SetVolume { volume: 42000 }));
    }

    // -- Command parsing: player commands ------------------------------------

    #[test]
    fn parse_play_command() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "play",
                "options": {
                    "context_uri": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M",
                    "skip_to": {
                        "track_uri": "spotify:track:4iV5W9uYEdYUVa79Axb7Rh",
                        "track_index": 3
                    }
                }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(
            cmd,
            Some(ConnectCommand::Play {
                context_uri: Some("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M".to_owned()),
                track_uri: Some("spotify:track:4iV5W9uYEdYUVa79Axb7Rh".to_owned()),
                offset: Some(3),
            })
        );
    }

    #[test]
    fn parse_play_command_with_context_uri_in_context() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "play",
                "context": {
                    "uri": "spotify:album:1DFixLWuPkv3KT3TnV35m3"
                },
                "options": {}
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(
            cmd,
            Some(ConnectCommand::Play {
                context_uri: Some("spotify:album:1DFixLWuPkv3KT3TnV35m3".to_owned()),
                track_uri: None,
                offset: None,
            })
        );
    }

    #[test]
    fn parse_pause_command() {
        let payload = serde_json::json!({
            "command": { "endpoint": "pause" }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::Pause));
    }

    #[test]
    fn parse_resume_command() {
        let payload = serde_json::json!({
            "command": { "endpoint": "resume" }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::Resume));
    }

    #[test]
    fn parse_seek_command() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "seek_to",
                "options": { "value": 120000 }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(
            cmd,
            Some(ConnectCommand::SeekTo {
                position_ms: 120000,
            })
        );
    }

    #[test]
    fn parse_skip_next() {
        let payload = serde_json::json!({
            "command": { "endpoint": "skip_next" }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::SkipNext));
    }

    #[test]
    fn parse_skip_prev() {
        let payload = serde_json::json!({
            "command": { "endpoint": "skip_prev" }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::SkipPrev));
    }

    #[test]
    fn parse_set_volume_command() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "set_volume",
                "options": { "value": 55000 }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::SetVolume { volume: 55000 }));
    }

    #[test]
    fn parse_set_shuffle_command() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "set_shuffling_context",
                "options": { "value": true }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::SetShuffle { shuffle: true }));
    }

    #[test]
    fn parse_set_shuffle_off() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "set_shuffle",
                "options": { "value": false }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::SetShuffle { shuffle: false }));
    }

    #[test]
    fn parse_set_repeat_context() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "set_repeat",
                "options": { "value": "context" }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(
            cmd,
            Some(ConnectCommand::SetRepeat {
                repeat: RepeatMode::Context,
            })
        );
    }

    #[test]
    fn parse_set_repeat_track() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "set_repeating_track",
                "options": { "value": true }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(
            cmd,
            Some(ConnectCommand::SetRepeat {
                repeat: RepeatMode::Track,
            })
        );
    }

    #[test]
    fn parse_set_repeat_off() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "set_repeat",
                "options": { "value": "off" }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(
            cmd,
            Some(ConnectCommand::SetRepeat {
                repeat: RepeatMode::Off,
            })
        );
    }

    #[test]
    fn parse_logout_command() {
        let payload = serde_json::json!({
            "command": { "endpoint": "logout" }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::Logout));
    }

    #[test]
    fn parse_unknown_command() {
        let payload = serde_json::json!({
            "command": { "endpoint": "some_new_thing" }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(
            cmd,
            Some(ConnectCommand::Unknown("some_new_thing".to_owned()))
        );
    }

    #[test]
    fn parse_cluster_update_returns_none() {
        let cmd = ConnectState::parse_command(
            "hm://connect-state/v1/cluster",
            &serde_json::json!({"cluster": {}}),
        );

        assert_eq!(cmd, None);
    }

    #[test]
    fn parse_unrecognised_uri_returns_none() {
        let cmd =
            ConnectState::parse_command("hm://something-else/v1/foo", &serde_json::Value::Null);

        assert_eq!(cmd, None);
    }

    // -- Command parsing: top-level endpoint format --------------------------

    #[test]
    fn parse_top_level_endpoint_format() {
        // Some versions of the protocol put `endpoint` at the top level.
        let payload = serde_json::json!({
            "endpoint": "pause"
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::Pause));
    }

    // -- ConnectState accessors ----------------------------------------------

    #[test]
    fn connect_state_accessors() {
        let state = ConnectState::new(test_device());
        assert_eq!(state.device_id(), "abc123def456");
        assert_eq!(state.device_name(), "FerroTune");
    }

    #[test]
    fn connect_state_debug_format() {
        let state = ConnectState::new(test_device());
        let debug = format!("{state:?}");
        assert!(debug.contains("abc123def456"));
        assert!(debug.contains("FerroTune"));
        assert!(debug.contains("Computer"));
    }

    // -- Version constants used in payloads ----------------------------------

    #[test]
    fn software_version_matches_crate_version() {
        // The registration software version must track version::SEMANTIC_VERSION.
        assert!(
            SOFTWARE_VERSION.contains(version::SEMANTIC_VERSION),
            "SOFTWARE_VERSION ({SOFTWARE_VERSION}) should contain {}",
            version::SEMANTIC_VERSION
        );
    }

    // -- Seek with position field -------------------------------------------

    #[test]
    fn parse_seek_with_position_field() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "seek_to",
                "options": { "position": 60000 }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::SeekTo { position_ms: 60000 }));
    }

    // -- Volume with volume field -------------------------------------------

    #[test]
    fn parse_set_volume_with_volume_field() {
        let payload = serde_json::json!({
            "command": {
                "endpoint": "set_volume",
                "options": { "volume": 12345 }
            }
        });

        let cmd = ConnectState::parse_command("hm://connect-state/v1/player/command", &payload);

        assert_eq!(cmd, Some(ConnectCommand::SetVolume { volume: 12345 }));
    }
}
