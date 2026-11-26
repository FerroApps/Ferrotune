//! Spotify Connect *controller*: see the user's other devices (phone,
//! official desktop app, speakers), move playback to one of them and
//! control it. Uses the same connect-state service the official clients use:
//!
//! * the device list ("cluster") comes from a `PUT
//!   /connect-state/v1/devices/hobs_<id>` as a hidden observer (device type
//!   `OBSERVER`, reason `PICKER_OPENED`) and then from the dealer pushes on
//!   `hm://connect-state/v1/cluster`;
//! * commands go to `POST /connect-state/v1/player/command/from/<us>/to/<it>`,
//!   transfers to `POST /connect-state/v1/connect/transfer/from/<us>/to/<it>`
//!   and volume to `PUT /connect-state/v1/connect/volume/from/<us>/to/<it>`.
//!
//! Parsing and request bodies are pure and unit-tested; the HTTP calls go
//! through [`InternalClient`] (wiremock-tested).

use std::sync::Arc;

use serde_json::{Value, json};
use spotify_proto::connect::Cluster;

use super::client::{InternalClient, JSON, Request, Target};
use crate::error::Result;

/// Kind of a Connect device, for the picker icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeviceKind {
    Computer,
    Smartphone,
    Tablet,
    Speaker,
    Tv,
    Car,
    #[default]
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RemoteDevice {
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    /// 0.0..=1.0; `None` when the device has fixed volume.
    pub volume: Option<f32>,
    pub is_active: bool,
}

/// Track playing on the active device.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ClusterTrack {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub cover_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClusterPlayer {
    pub track: Option<ClusterTrack>,
    pub context_uri: String,
    pub is_playing: bool,
    pub is_paused: bool,
    /// Position when the cluster was produced.
    pub position_ms: u32,
    pub duration_ms: u32,
    pub shuffle: bool,
    pub repeat_context: bool,
    pub repeat_track: bool,
}

/// Parsed connect-state cluster.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ClusterState {
    pub active_device_id: Option<String>,
    /// Other devices, sorted by name (ours and observers are left out).
    pub devices: Vec<RemoteDevice>,
    pub player: Option<ClusterPlayer>,
}

impl ClusterState {
    pub fn device(&self, id: &str) -> Option<&RemoteDevice> {
        self.devices.iter().find(|d| d.id == id)
    }

    /// The active device when it is not this computer.
    pub fn active_remote(&self) -> Option<&RemoteDevice> {
        self.active_device_id
            .as_deref()
            .and_then(|id| self.device(id))
    }
}

/// Prefix of the hidden observer device id (as the web player uses).
pub const OBSERVER_PREFIX: &str = "hobs_";

pub fn observer_id(device_id: &str) -> String {
    format!("{OBSERVER_PREFIX}{device_id}")
}

fn kind_of(t: spotify_proto::devices::DeviceType) -> DeviceKind {
    use spotify_proto::devices::DeviceType as T;
    match t {
        T::COMPUTER | T::CHROMEBOOK => DeviceKind::Computer,
        T::SMARTPHONE | T::SMARTWATCH => DeviceKind::Smartphone,
        T::TABLET => DeviceKind::Tablet,
        T::SPEAKER | T::AVR | T::AUDIO_DONGLE | T::CAST_AUDIO | T::HOME_THING => {
            DeviceKind::Speaker
        }
        T::TV | T::STB | T::CAST_VIDEO | T::GAME_CONSOLE => DeviceKind::Tv,
        T::AUTOMOBILE | T::CAR_THING => DeviceKind::Car,
        _ => DeviceKind::Other,
    }
}

/// `spotify:image:<hex>` (or an https URL) -> CDN URL.
pub fn image_url(v: &str) -> Option<String> {
    if let Some(id) = v.strip_prefix("spotify:image:") {
        let ok = !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit());
        return ok.then(|| format!("{}{id}", super::types::IMAGE_CDN));
    }
    v.starts_with("https://").then(|| v.to_owned())
}

fn clamp_ms(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}

/// Turn a cluster into what the UI needs. `own_id` is our Connect device id
/// (left out of the list: "This computer" is shown separately).
pub fn parse_cluster(c: &Cluster, own_id: &str) -> ClusterState {
    let active = Some(c.active_device_id.clone()).filter(|s| !s.is_empty());
    let mut devices: Vec<RemoteDevice> = c
        .device
        .iter()
        .filter(|(id, d)| {
            id.as_str() != own_id
                && !id.starts_with(OBSERVER_PREFIX)
                && !d.capabilities.as_ref().is_some_and(|c| c.hidden)
                && !d.is_offline
        })
        .map(|(id, d)| {
            let fixed = d.capabilities.as_ref().is_some_and(|c| c.disable_volume);
            RemoteDevice {
                id: id.clone(),
                name: if d.name.trim().is_empty() {
                    "Unknown device".into()
                } else {
                    d.name.clone()
                },
                kind: kind_of(d.device_type.enum_value_or_default()),
                volume: (!fixed).then(|| f32::from(d.volume.min(65535) as u16) / 65535.0),
                is_active: active.as_deref() == Some(id.as_str()),
            }
        })
        .collect();
    devices.sort_by(|a, b| {
        b.is_active
            .cmp(&a.is_active)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    let player = c.player_state.as_ref().map(|p| {
        let track = p.track.as_ref().filter(|t| !t.uri.is_empty()).map(|t| {
            let m = &t.metadata;
            let get = |k: &str| m.get(k).cloned().unwrap_or_default();
            ClusterTrack {
                uri: t.uri.clone(),
                name: get("title"),
                artists: m
                    .get("artist_name")
                    .map(|a| vec![a.clone()])
                    .unwrap_or_default(),
                album: get("album_title"),
                cover_url: ["image_url", "image_large_url", "image_small_url"]
                    .iter()
                    .find_map(|k| m.get(*k).and_then(|v| image_url(v))),
            }
        });
        // The position is "as of" the player-state timestamp; extrapolate
        // to the server time of this cluster while playing.
        let mut pos = p.position_as_of_timestamp;
        if p.is_playing && !p.is_paused && c.server_timestamp_ms > p.timestamp && p.timestamp > 0 {
            pos = pos.saturating_add(c.server_timestamp_ms - p.timestamp);
        }
        let duration = clamp_ms(p.duration);
        let opts = p.options.as_ref();
        ClusterPlayer {
            track,
            context_uri: p.context_uri.clone(),
            is_playing: p.is_playing,
            is_paused: p.is_paused,
            position_ms: clamp_ms(pos).min(if duration > 0 { duration } else { u32::MAX }),
            duration_ms: duration,
            shuffle: opts.is_some_and(|o| o.shuffling_context),
            repeat_context: opts.is_some_and(|o| o.repeating_context),
            repeat_track: opts.is_some_and(|o| o.repeating_track),
        }
    });
    ClusterState {
        active_device_id: active,
        devices,
        player,
    }
}

/// The observer registration body (protobuf `PutStateRequest`).
pub fn observer_request(observer_id: &str, name: &str) -> spotify_proto::connect::PutStateRequest {
    use spotify_proto::connect::{
        Capabilities, Device, DeviceInfo, MemberType, PutStateReason, PutStateRequest,
    };
    use spotify_proto::devices::DeviceType;
    let caps = Capabilities {
        hidden: true,
        is_observable: true,
        can_be_player: false,
        ..Default::default()
    };
    let info = DeviceInfo {
        can_play: false,
        name: name.to_owned(),
        device_id: observer_id.to_owned(),
        device_type: DeviceType::OBSERVER.into(),
        capabilities: Some(caps).into(),
        device_software_version: spotify_protocol::version::SEMANTIC_VERSION.to_owned(),
        ..Default::default()
    };
    PutStateRequest {
        member_type: MemberType::CONNECT_STATE.into(),
        put_state_reason: PutStateReason::PICKER_OPENED.into(),
        device: Some(Device {
            device_info: Some(info).into(),
            ..Default::default()
        })
        .into(),
        client_side_timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        ..Default::default()
    }
}

/// What to start on a remote device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRequest {
    /// `spotify:playlist:…`, `spotify:album:…`, `spotify:user:<u>:collection`,
    /// or a single `spotify:track:…`.
    pub context_uri: String,
    pub track_uri: Option<String>,
    pub track_index: Option<usize>,
    /// Start position in the first track.
    pub position_ms: Option<u32>,
}

/// A command for the device that plays.
#[derive(Debug, Clone, PartialEq)]
pub enum ControlCommand {
    Pause,
    Resume,
    Next,
    Previous,
    Seek(u32),
    Shuffle(bool),
    Repeat { context: bool, track: bool },
    Play(PlayRequest),
}

/// One controller request.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectOp {
    /// `(device id, command)`
    Command(String, ControlCommand),
    Transfer(String),
    /// `(device id, 0.0..=1.0)`
    Volume(String, f32),
}

/// JSON body of `player/command`.
pub fn command_body(cmd: &ControlCommand) -> Value {
    let c = match cmd {
        ControlCommand::Pause => json!({"endpoint": "pause"}),
        ControlCommand::Resume => json!({"endpoint": "resume"}),
        ControlCommand::Next => json!({"endpoint": "skip_next"}),
        ControlCommand::Previous => json!({"endpoint": "skip_prev"}),
        ControlCommand::Seek(ms) => json!({"endpoint": "seek_to", "value": ms}),
        ControlCommand::Shuffle(on) => json!({"endpoint": "set_shuffling_context", "value": on}),
        ControlCommand::Repeat { context, track } => json!({
            "endpoint": "set_options",
            "repeating_context": context,
            "repeating_track": track,
        }),
        ControlCommand::Play(p) => {
            let mut skip = serde_json::Map::new();
            if let Some(u) = &p.track_uri {
                skip.insert("track_uri".into(), json!(u));
            }
            if let Some(i) = p.track_index {
                skip.insert("track_index".into(), json!(i));
            }
            let mut options = json!({"skip_to": skip, "player_options_override": {}});
            if let Some(ms) = p.position_ms {
                options["seek_to"] = json!(ms);
            }
            json!({
                "endpoint": "play",
                "context": {"uri": p.context_uri, "url": format!("context://{}", p.context_uri)},
                "options": options,
            })
        }
    };
    json!({ "command": c })
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

/// Send a command to device `to`.
pub async fn send_command(
    client: &Arc<InternalClient>,
    from: &str,
    to: &str,
    cmd: &ControlCommand,
) -> Result<()> {
    let path = format!(
        "/connect-state/v1/player/command/from/{}/to/{}",
        enc(from),
        enc(to)
    );
    let body = serde_json::to_vec(&command_body(cmd))?;
    client
        .send(Request::post(Target::Spclient(path), JSON, body))
        .await
        .map(|_| ())
}

/// Move playback to device `to` (keeps the paused/playing state).
pub async fn transfer(client: &Arc<InternalClient>, from: &str, to: &str) -> Result<()> {
    let path = format!(
        "/connect-state/v1/connect/transfer/from/{}/to/{}",
        enc(from),
        enc(to)
    );
    let body = serde_json::to_vec(&json!({"transfer_options": {"restore_paused": "restore"}}))?;
    client
        .send(Request::post(Target::Spclient(path), JSON, body))
        .await
        .map(|_| ())
}

/// Set the volume (0.0..=1.0) of device `to`.
pub async fn set_volume(client: &Arc<InternalClient>, from: &str, to: &str, v: f32) -> Result<()> {
    let path = format!(
        "/connect-state/v1/connect/volume/from/{}/to/{}",
        enc(from),
        enc(to)
    );
    let vol = (v.clamp(0.0, 1.0) * 65535.0).round() as u32;
    let body = serde_json::to_vec(&json!({ "volume": vol }))?;
    let mut req = Request::post(Target::Spclient(path), JSON, body);
    req.method = reqwest::Method::PUT;
    client.send(req).await.map(|_| ())
}

/// Register as a hidden observer and return the current cluster.
async fn fetch_cluster(
    session: &librespot_core::Session,
    observer: &str,
    name: &str,
) -> std::result::Result<Cluster, librespot_core::Error> {
    use protobuf::Message as _;
    let conn = session.connection_id();
    if conn.is_empty() {
        return Err(librespot_core::Error::unavailable(
            "no dealer connection yet",
        ));
    }
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(v) = reqwest::header::HeaderValue::from_str(&conn) {
        headers.insert("x-spotify-connection-id", v);
    }
    let endpoint = format!("/connect-state/v1/devices/{observer}");
    let body = session
        .spclient()
        .request_with_protobuf(
            &reqwest::Method::PUT,
            &endpoint,
            Some(headers),
            &observer_request(observer, name),
        )
        .await?;
    Cluster::parse_from_bytes(&body).map_err(librespot_core::Error::failed_precondition)
}

/// Watch the user's Connect devices: one observer registration (repeated
/// on every `refresh` notification, e.g. when the picker opens), then the
/// dealer's cluster pushes. Ends when `tx` is closed.
pub async fn observe(
    session: librespot_core::Session,
    own_id: String,
    name: String,
    refresh: Arc<tokio::sync::Notify>,
    tx: tokio::sync::mpsc::UnboundedSender<ClusterState>,
) {
    use futures::StreamExt;
    use librespot_core::dealer::protocol::Message;
    use spotify_proto::connect::ClusterUpdate;

    let observer = observer_id(&own_id);
    let mut pushes = match session.dealer().listen_for(
        "hm://connect-state/v1/cluster",
        Message::from_raw::<ClusterUpdate>,
    ) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::debug!(error = %e, "no connect-state pushes");
            None
        }
    };
    // The dealer connection id appears once Spotify Connect has connected.
    let mut ready = false;
    for _ in 0..40 {
        if !session.connection_id().is_empty() {
            ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    if !ready {
        tracing::info!("Spotify Connect is off: the device picker is unavailable");
        return;
    }
    let mut fetch = true;
    loop {
        if fetch {
            fetch = false;
            match fetch_cluster(&session, &observer, &name).await {
                Ok(c) => {
                    let state = parse_cluster(&c, &own_id);
                    tracing::debug!(devices = state.devices.len(), "connect devices");
                    if tx.send(state).is_err() {
                        return;
                    }
                }
                Err(e) => tracing::debug!(error = %e, "could not read the connect devices"),
            }
        }
        tokio::select! {
            () = refresh.notified() => fetch = true,
            () = tx.closed() => return,
            up = async {
                match pushes.as_mut() {
                    Some(p) => p.next().await,
                    None => std::future::pending().await,
                }
            } => match up {
                Some(Ok(u)) => {
                    if let Some(c) = u.cluster.as_ref()
                        && tx.send(parse_cluster(c, &own_id)).is_err()
                    {
                        return;
                    }
                }
                Some(Err(e)) => tracing::debug!(error = %e, "bad cluster push"),
                None => pushes = None,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spotify_proto::connect::{Capabilities, DeviceInfo};
    use spotify_proto::devices::DeviceType;
    use spotify_proto::player::{ContextPlayerOptions, PlayerState, ProvidedTrack};

    fn dev(name: &str, t: DeviceType, volume: u32) -> DeviceInfo {
        DeviceInfo {
            name: name.into(),
            device_type: t.into(),
            volume,
            ..Default::default()
        }
    }

    pub fn sample_cluster() -> Cluster {
        let mut c = Cluster {
            active_device_id: "phone".into(),
            server_timestamp_ms: 10_500,
            ..Default::default()
        };
        c.device
            .insert("phone".into(), dev("Pixel", DeviceType::SMARTPHONE, 32768));
        c.device
            .insert("desk".into(), dev("Desktop", DeviceType::COMPUTER, 65535));
        c.device
            .insert("me".into(), dev("Ferrotune", DeviceType::COMPUTER, 0));
        c.device
            .insert("hobs_me".into(), dev("", DeviceType::OBSERVER, 0));
        let mut hidden = dev("Hidden", DeviceType::SPEAKER, 0);
        hidden.capabilities = Some(Capabilities {
            hidden: true,
            ..Default::default()
        })
        .into();
        c.device.insert("hid".into(), hidden);
        let mut t = ProvidedTrack {
            uri: "spotify:track:abc".into(),
            ..Default::default()
        };
        t.metadata.insert("title".into(), "Song".into());
        t.metadata.insert("artist_name".into(), "Band".into());
        t.metadata.insert("album_title".into(), "Album".into());
        t.metadata
            .insert("image_url".into(), "spotify:image:ab67616d0000b273".into());
        c.player_state = Some(PlayerState {
            track: Some(t).into(),
            context_uri: "spotify:playlist:p1".into(),
            is_playing: true,
            timestamp: 10_000,
            position_as_of_timestamp: 2_000,
            duration: 180_000,
            options: Some(ContextPlayerOptions {
                shuffling_context: true,
                repeating_track: true,
                ..Default::default()
            })
            .into(),
            ..Default::default()
        })
        .into();
        c
    }

    #[test]
    fn cluster_lists_other_visible_devices_active_first() {
        let s = parse_cluster(&sample_cluster(), "me");
        let names: Vec<_> = s.devices.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["Pixel", "Desktop"]);
        assert_eq!(
            s.active_remote().map(|d| d.kind),
            Some(DeviceKind::Smartphone)
        );
        assert!((s.devices[0].volume.unwrap() - 0.5).abs() < 0.01);
        let p = s.player.unwrap();
        assert_eq!(p.position_ms, 2_500, "extrapolated to the server time");
        assert_eq!(p.duration_ms, 180_000);
        assert!(p.shuffle && p.repeat_track && !p.repeat_context);
        let t = p.track.unwrap();
        assert_eq!(t.name, "Song");
        assert_eq!(t.artists, ["Band"]);
        assert_eq!(
            t.cover_url.as_deref(),
            Some("https://i.scdn.co/image/ab67616d0000b273")
        );
    }

    #[test]
    fn cluster_with_us_active_has_no_active_remote() {
        let mut c = sample_cluster();
        c.active_device_id = "me".into();
        let s = parse_cluster(&c, "me");
        assert!(s.active_remote().is_none());
        assert!(s.devices.iter().all(|d| !d.is_active));
        let empty = parse_cluster(&Cluster::default(), "me");
        assert_eq!(empty, ClusterState::default());
    }

    #[test]
    fn hostile_values_do_not_panic() {
        let mut c = sample_cluster();
        if let Some(p) = c.player_state.as_mut() {
            p.position_as_of_timestamp = i64::MAX;
            p.duration = -5;
            p.timestamp = i64::MIN;
        }
        c.server_timestamp_ms = i64::MAX;
        c.device
            .insert("big".into(), dev("Loud", DeviceType::SPEAKER, u32::MAX));
        let s = parse_cluster(&c, "me");
        assert!(s.device("big").unwrap().volume.unwrap() <= 1.0);
        assert_eq!(s.player.unwrap().duration_ms, 0);
        assert_eq!(image_url("spotify:image:../x"), None);
        assert_eq!(image_url("http://x"), None);
    }

    #[test]
    fn command_bodies_match_the_connect_state_shape() {
        assert_eq!(
            command_body(&ControlCommand::Pause),
            json!({"command": {"endpoint": "pause"}})
        );
        assert_eq!(
            command_body(&ControlCommand::Seek(1234))["command"]["value"],
            1234
        );
        let play = command_body(&ControlCommand::Play(PlayRequest {
            context_uri: "spotify:album:a".into(),
            track_uri: Some("spotify:track:t".into()),
            track_index: Some(3),
            position_ms: Some(5000),
        }));
        assert_eq!(play["command"]["options"]["seek_to"], 5000);
        assert_eq!(play["command"]["endpoint"], "play");
        assert_eq!(
            play["command"]["context"]["url"],
            "context://spotify:album:a"
        );
        assert_eq!(play["command"]["options"]["skip_to"]["track_index"], 3);
        let rep = command_body(&ControlCommand::Repeat {
            context: true,
            track: false,
        });
        assert_eq!(rep["command"]["repeating_context"], true);
    }

    #[test]
    fn observer_request_is_hidden_observer() {
        use protobuf::Message as _;
        let r = observer_request("hobs_x", "Ferrotune");
        let bytes = r.write_to_bytes().unwrap();
        let back = spotify_proto::connect::PutStateRequest::parse_from_bytes(&bytes).unwrap();
        let info = back.device.device_info.clone().unwrap();
        assert_eq!(info.device_id, "hobs_x");
        assert_eq!(
            info.device_type.enum_value_or_default(),
            DeviceType::OBSERVER
        );
        assert!(info.capabilities.hidden && !info.capabilities.can_be_player);
    }
}
