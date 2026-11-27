//! Audio engine: librespot `Player` for local playback (the app owns the
//! queue), the Spotify Connect device's player when the phone/another
//! client casts to Ferrotune, or a simulated engine for `--mock` mode and
//! tests. Exposed as a command handle plus an event stream; the app wraps
//! the stream in an iced `Subscription`. Now-playing state is driven by
//! these player events only: nothing polls Spotify for playback state.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use tokio::sync::mpsc;

use crate::config::Bitrate;

pub use super::session::SpotifySession;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineKind {
    Mock,
    /// Bitrate, volume normalisation and whether librespot plays locally
    /// (`false` while Spotify Soloist is the local engine). A change
    /// restarts the engine.
    Librespot(BitrateKey, bool, bool),
    /// Native HTTP-based engine (bitrate, normalisation). Uses
    /// storage-resolve v2 / HLS instead of AP audio keys.
    Native(BitrateKey, bool),
}

/// Hashable mirror of [`Bitrate`] for subscription identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitrateKey {
    Low,
    Normal,
    High,
}

impl From<Bitrate> for BitrateKey {
    fn from(b: Bitrate) -> Self {
        match b {
            Bitrate::Low => Self::Low,
            Bitrate::Normal => Self::Normal,
            Bitrate::High => Self::High,
        }
    }
}

#[derive(Debug)]
pub enum PlaybackCommand {
    Attach(SpotifySession),
    Load {
        uri: String,
        duration_ms: u32,
        start: bool,
        position_ms: u32,
    },
    Play,
    Pause,
    Stop,
    Seek(u32),
    /// Start fetching the next track so it can start without a gap.
    Preload {
        uri: String,
    },
    /// 0.0..=1.0
    SetVolume(f32),
    /// Control the Spotify Connect playback (while another device cast to us).
    Remote(RemoteCommand),
    /// Re-read the user's Connect devices (the picker opened).
    RefreshDevices,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RemoteCommand {
    Play,
    Pause,
    Next,
    Previous,
    Seek(u32),
    /// 0.0..=1.0
    Volume(f32),
}

/// Metadata of the track the Connect device is playing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteTrack {
    pub uri: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub cover_url: Option<String>,
    pub duration_ms: u32,
}

/// Events of the Spotify Connect device (remote-controlled playback).
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteEvent {
    Track(RemoteTrack),
    Playing {
        position_ms: u32,
    },
    Paused {
        position_ms: u32,
    },
    Position {
        position_ms: u32,
    },
    /// Playback moved to another device or stopped.
    Stopped,
    /// 0.0..=1.0
    Volume(f32),
}

#[derive(Clone)]
pub struct PlaybackHandle(mpsc::UnboundedSender<PlaybackCommand>);

impl fmt::Debug for PlaybackHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PlaybackHandle")
    }
}

impl PlaybackHandle {
    pub fn send(&self, cmd: PlaybackCommand) {
        if self.0.send(cmd).is_err() {
            tracing::warn!("playback engine is gone");
        }
    }

    /// Detached handle for tests.
    pub fn channel() -> (Self, mpsc::UnboundedReceiver<PlaybackCommand>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self(tx), rx)
    }
}

#[derive(Debug, Clone)]
pub enum PlaybackEvent {
    Ready(PlaybackHandle),
    Loading {
        uri: String,
    },
    Playing {
        uri: String,
        position_ms: u32,
    },
    Paused {
        uri: String,
        position_ms: u32,
    },
    Position {
        uri: String,
        position_ms: u32,
    },
    Stopped,
    /// The player suggests preloading the next track now.
    PreloadNext,
    EndOfTrack {
        uri: String,
    },
    Unavailable {
        uri: String,
        reason: super::audio_diag::UnavailableReason,
    },
    /// The audio session or player died (network drop, suspend/resume,
    /// revoked token). The app must reconnect and re-attach.
    SessionLost,
    /// Spotify Connect: another device is controlling playback here.
    Remote(RemoteEvent),
    /// The user's Connect devices and what the active one plays.
    Cluster(super::connect_ctl::ClusterState),
    Error(String),
}

/// Lazily spawns the engine when first polled and yields its events.
pub fn events(kind: EngineKind) -> impl Stream<Item = PlaybackEvent> + Send + 'static {
    use futures::StreamExt;
    futures::stream::once(async move {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = mpsc::unbounded_channel();
        match kind {
            EngineKind::Mock => {
                tokio::spawn(run_mock(cmd_rx, ev_tx));
            }
            EngineKind::Librespot(b, norm, local) => {
                tokio::spawn(run_librespot(b, norm, local, cmd_rx, ev_tx));
            }
            EngineKind::Native(b, norm) => {
                tokio::spawn(super::native::run(b, norm, cmd_rx, ev_tx));
            }
        }
        let ready =
            futures::stream::once(async move { PlaybackEvent::Ready(PlaybackHandle(cmd_tx)) });
        let rest =
            futures::stream::unfold(
                ev_rx,
                |mut rx| async move { rx.recv().await.map(|e| (e, rx)) },
            );
        ready.chain(rest)
    })
    .flatten()
}

/// Simulated engine: advances a clock while "playing".
#[derive(Debug, Default)]
pub struct MockEngine {
    pub uri: Option<String>,
    pub duration_ms: u32,
    pub position_ms: u32,
    pub playing: bool,
    pub volume: f32,
}

impl MockEngine {
    /// Apply a command, returning events to emit.
    pub fn command(&mut self, cmd: PlaybackCommand) -> Vec<PlaybackEvent> {
        match cmd {
            PlaybackCommand::Attach(_)
            | PlaybackCommand::Remote(_)
            | PlaybackCommand::RefreshDevices
            | PlaybackCommand::Preload { .. } => vec![],
            PlaybackCommand::Load {
                uri,
                duration_ms,
                start,
                position_ms,
            } => {
                self.uri = Some(uri.clone());
                self.duration_ms = duration_ms;
                self.position_ms = position_ms;
                self.playing = start;
                let mut v = vec![PlaybackEvent::Loading { uri: uri.clone() }];
                v.push(if start {
                    PlaybackEvent::Playing { uri, position_ms }
                } else {
                    PlaybackEvent::Paused { uri, position_ms }
                });
                v
            }
            PlaybackCommand::Play => self.set_playing(true),
            PlaybackCommand::Pause => self.set_playing(false),
            PlaybackCommand::Stop => {
                self.uri = None;
                self.playing = false;
                vec![PlaybackEvent::Stopped]
            }
            PlaybackCommand::Seek(ms) => {
                self.position_ms = ms.min(self.duration_ms);
                self.status_event().into_iter().collect()
            }
            PlaybackCommand::SetVolume(v) => {
                self.volume = v.clamp(0.0, 1.0);
                vec![]
            }
        }
    }

    fn set_playing(&mut self, playing: bool) -> Vec<PlaybackEvent> {
        if self.uri.is_none() {
            return vec![];
        }
        self.playing = playing;
        self.status_event().into_iter().collect()
    }

    fn status_event(&self) -> Option<PlaybackEvent> {
        let uri = self.uri.clone()?;
        Some(if self.playing {
            PlaybackEvent::Playing {
                uri,
                position_ms: self.position_ms,
            }
        } else {
            PlaybackEvent::Paused {
                uri,
                position_ms: self.position_ms,
            }
        })
    }

    /// Advance the clock by `dt_ms`.
    pub fn tick(&mut self, dt_ms: u32) -> Vec<PlaybackEvent> {
        let Some(uri) = self.uri.clone().filter(|_| self.playing) else {
            return vec![];
        };
        self.position_ms = self.position_ms.saturating_add(dt_ms);
        if self.position_ms >= self.duration_ms {
            self.playing = false;
            self.position_ms = self.duration_ms;
            vec![PlaybackEvent::EndOfTrack { uri }]
        } else {
            vec![PlaybackEvent::Position {
                uri,
                position_ms: self.position_ms,
            }]
        }
    }
}

async fn run_mock(
    mut cmd_rx: mpsc::UnboundedReceiver<PlaybackCommand>,
    ev_tx: mpsc::UnboundedSender<PlaybackEvent>,
) {
    let mut engine = MockEngine::default();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        let events = tokio::select! {
            cmd = cmd_rx.recv() => match cmd {
                Some(c) => engine.command(c),
                None => return,
            },
            _ = tick.tick() => engine.tick(1000),
        };
        for e in events {
            if ev_tx.send(e).is_err() {
                return;
            }
        }
    }
}

fn uri_string(u: &librespot_core::SpotifyUri) -> String {
    u.to_uri().unwrap_or_default()
}

fn map_player_event(e: librespot_playback::player::PlayerEvent) -> Option<PlaybackEvent> {
    use librespot_playback::player::PlayerEvent as P;
    Some(match e {
        P::Loading { track_id, .. } => PlaybackEvent::Loading {
            uri: uri_string(&track_id),
        },
        P::Playing {
            track_id,
            position_ms,
            ..
        } => PlaybackEvent::Playing {
            uri: uri_string(&track_id),
            position_ms,
        },
        P::Paused {
            track_id,
            position_ms,
            ..
        } => PlaybackEvent::Paused {
            uri: uri_string(&track_id),
            position_ms,
        },
        P::PositionChanged {
            track_id,
            position_ms,
            ..
        }
        | P::PositionCorrection {
            track_id,
            position_ms,
            ..
        }
        | P::Seeked {
            track_id,
            position_ms,
            ..
        } => PlaybackEvent::Position {
            uri: uri_string(&track_id),
            position_ms,
        },
        P::EndOfTrack { track_id, .. } => PlaybackEvent::EndOfTrack {
            uri: uri_string(&track_id),
        },
        P::Unavailable { track_id, .. } => PlaybackEvent::Unavailable {
            uri: uri_string(&track_id),
            // Read what librespot logged while loading (it does not put the
            // reason into the event).
            reason: super::audio_diag::RECORDER.take(std::time::Instant::now()),
        },
        P::Stopped { .. } => PlaybackEvent::Stopped,
        P::TimeToPreloadNextTrack { .. } => PlaybackEvent::PreloadNext,
        _ => return None,
    })
}

/// Player configuration shared by the local and the Connect player.
pub fn player_config(
    bitrate: BitrateKey,
    normalisation: bool,
) -> librespot_playback::config::PlayerConfig {
    use librespot_playback::config::{Bitrate as LBitrate, PlayerConfig};
    PlayerConfig {
        bitrate: match bitrate {
            BitrateKey::Low => LBitrate::Bitrate96,
            BitrateKey::Normal => LBitrate::Bitrate160,
            BitrateKey::High => LBitrate::Bitrate320,
        },
        position_update_interval: Some(Duration::from_secs(1)),
        normalisation,
        ..PlayerConfig::default()
    }
}

/// Does this Connect-player event mean another device is casting to us?
pub fn starts_cast(e: &librespot_playback::player::PlayerEvent) -> bool {
    use librespot_playback::player::PlayerEvent as P;
    matches!(
        e,
        P::TrackChanged { .. } | P::Loading { .. } | P::Playing { .. }
    )
}

/// Map an event of the Connect device's player.
pub fn map_remote_event(e: librespot_playback::player::PlayerEvent) -> Option<RemoteEvent> {
    use librespot_metadata::audio::UniqueFields;
    use librespot_playback::player::PlayerEvent as P;
    Some(match e {
        P::TrackChanged { audio_item } => {
            let (artists, album) = match &audio_item.unique_fields {
                UniqueFields::Track { artists, album, .. } => (
                    artists.0.iter().map(|a| a.name.clone()).collect(),
                    album.clone(),
                ),
                UniqueFields::Episode { show_name, .. } => (vec![show_name.clone()], String::new()),
                UniqueFields::Local { artists, album, .. } => (
                    artists.iter().cloned().collect(),
                    album.clone().unwrap_or_default(),
                ),
            };
            RemoteEvent::Track(RemoteTrack {
                uri: audio_item.uri.clone(),
                name: audio_item.name.clone(),
                artists,
                album,
                cover_url: audio_item.covers.first().map(|c| c.url.clone()),
                duration_ms: audio_item.duration_ms,
            })
        }
        P::Playing { position_ms, .. } => RemoteEvent::Playing { position_ms },
        P::Paused { position_ms, .. } => RemoteEvent::Paused { position_ms },
        P::PositionChanged { position_ms, .. }
        | P::PositionCorrection { position_ms, .. }
        | P::Seeked { position_ms, .. } => RemoteEvent::Position { position_ms },
        P::Stopped { .. } | P::SessionDisconnected { .. } => RemoteEvent::Stopped,
        P::VolumeChanged { volume } => RemoteEvent::Volume(f32::from(volume) / f32::from(u16::MAX)),
        _ => return None,
    })
}

async fn run_librespot(
    bitrate: BitrateKey,
    normalisation: bool,
    local_player: bool,
    mut cmd_rx: mpsc::UnboundedReceiver<PlaybackCommand>,
    ev_tx: mpsc::UnboundedSender<PlaybackEvent>,
) {
    use librespot_playback::audio_backend;
    use librespot_playback::config::AudioFormat;
    use librespot_playback::mixer::softmixer::SoftMixer;
    use librespot_playback::mixer::{Mixer, MixerConfig};
    use librespot_playback::player::Player;

    let mixer: Arc<SoftMixer> = match SoftMixer::open(MixerConfig::default()) {
        Ok(m) => Arc::new(m),
        Err(e) => {
            let _ = ev_tx.send(PlaybackEvent::Error(format!("mixer: {e}")));
            return;
        }
    };
    let mut pending_volume: f32 = 0.7;
    let mut player: Option<Arc<Player>> = None;
    let mut player_events: Option<librespot_playback::player::PlayerEventChannel> = None;
    let mut session: Option<librespot_core::Session> = None;
    let mut connect: Option<Arc<super::session::ConnectDevice>> = None;
    let mut remote_events: Option<librespot_playback::player::PlayerEventChannel> = None;
    // True while another device plays through our Connect device.
    let mut remote_active = false;
    // Connect controller: the user's devices (net::connect_ctl).
    let refresh = Arc::new(tokio::sync::Notify::new());
    let mut observer: Option<tokio::task::JoinHandle<()>> = None;
    let (cluster_tx, mut cluster_rx) = mpsc::unbounded_channel();
    let mut health = tokio::time::interval(Duration::from_secs(2));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Report a dead session once and forget it until a new one is attached.
    let lost = |session: &mut Option<librespot_core::Session>| {
        if session.take().is_some() {
            tracing::warn!("audio session lost");
            let _ = ev_tx.send(PlaybackEvent::SessionLost);
        }
    };

    loop {
        let cmd = tokio::select! {
            cmd = cmd_rx.recv() => match cmd { Some(c) => c, None => return },
            _ = health.tick() => {
                let dead = session.as_ref().is_some_and(librespot_core::Session::is_invalid)
                    || player.as_ref().is_some_and(|p| p.is_invalid());
                if dead {
                    lost(&mut session);
                }
                continue;
            }
            Some(c) = cluster_rx.recv() => {
                if ev_tx.send(PlaybackEvent::Cluster(c)).is_err() {
                    return;
                }
                continue;
            }
            ev = async {
                match remote_events.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                let Some(ev) = ev else {
                    remote_events = None;
                    continue;
                };
                if starts_cast(&ev) {
                    // A cast is starting: from the first TrackChanged /
                    // Loading / Playing the Connect stream owns the output,
                    // so the local queue yields at once.
                    if !remote_active && let Some(p) = &player {
                        p.pause();
                    }
                    remote_active = true;
                }
                if let Some(r) = map_remote_event(ev) {
                    if r == RemoteEvent::Stopped {
                        remote_active = false;
                    }
                    if ev_tx.send(PlaybackEvent::Remote(r)).is_err() {
                        return;
                    }
                }
                continue;
            }
            ev = async {
                match player_events.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                match ev {
                    Some(ev) => {
                        let mapped = map_player_event(ev);
                        // "Unavailable" from a dead session is a connection
                        // problem, not a missing track.
                        if matches!(mapped, Some(PlaybackEvent::Unavailable { .. }))
                            && session.as_ref().is_some_and(librespot_core::Session::is_invalid)
                        {
                            lost(&mut session);
                            continue;
                        }
                        if let Some(PlaybackEvent::Unavailable { reason, .. }) = &mapped {
                            tracing::debug!(?reason, "player reported the track unavailable");
                        }
                        if let Some(mapped) = mapped
                            && ev_tx.send(mapped).is_err()
                        {
                            return;
                        }
                    }
                    None => {
                        // The player thread is gone; drop it so the next
                        // Attach builds a fresh one.
                        player_events = None;
                        player = None;
                        lost(&mut session);
                    }
                }
                continue;
            }
        };
        match cmd {
            PlaybackCommand::Attach(new_session) => {
                if let Some(o) = observer.take() {
                    o.abort();
                }
                if new_session.dealer {
                    let s = new_session.session.clone();
                    let own = s.device_id().to_owned();
                    observer = Some(tokio::spawn(super::connect_ctl::observe(
                        s,
                        own,
                        crate::config::APP_NAME.to_owned(),
                        refresh.clone(),
                        cluster_tx.clone(),
                    )));
                }
                session = Some(new_session.session.clone());
                connect = new_session.connect.clone();
                remote_events = connect.as_ref().and_then(|c| c.take_events());
                remote_active = false;
                if !local_player {
                    // Spotify Soloist plays: no local librespot player.
                    continue;
                }
                if let Some(p) = player.as_ref().filter(|p| !p.is_invalid()) {
                    p.set_session(new_session.session);
                    continue;
                }
                let config = player_config(bitrate, normalisation);
                let Some(backend) = audio_backend::find(None) else {
                    let _ = ev_tx.send(PlaybackEvent::Error("no audio backend".into()));
                    continue;
                };
                mixer.set_volume(volume_to_u16(pending_volume));
                let p = Player::new(
                    config,
                    new_session.session,
                    mixer.get_soft_volume(),
                    move || backend(None, AudioFormat::default()),
                );
                player_events = Some(p.get_player_event_channel());
                player = Some(p);
            }
            PlaybackCommand::SetVolume(v) => {
                pending_volume = v.clamp(0.0, 1.0);
                mixer.set_volume(volume_to_u16(pending_volume));
            }
            PlaybackCommand::RefreshDevices => refresh.notify_one(),
            PlaybackCommand::Remote(rc) => {
                let Some(c) = &connect else { continue };
                let r = match rc {
                    RemoteCommand::Play => c.spirc.play(),
                    RemoteCommand::Pause => c.spirc.pause(),
                    RemoteCommand::Next => c.spirc.next(),
                    RemoteCommand::Previous => c.spirc.prev(),
                    RemoteCommand::Seek(ms) => c.spirc.set_position_ms(ms),
                    RemoteCommand::Volume(v) => c.spirc.set_volume(volume_to_u16(v)),
                };
                if let Err(e) = r {
                    tracing::warn!(error = %e, "connect command failed");
                }
            }
            other => {
                if remote_active
                    && matches!(other, PlaybackCommand::Load { .. } | PlaybackCommand::Play)
                    && let Some(c) = &connect
                {
                    // Local playback takes over again: release the Connect
                    // session (pauses the remote stream, device stays listed).
                    let _ = c.spirc.disconnect(true);
                    remote_active = false;
                }
                let Some(p) = &player else {
                    let _ = ev_tx.send(PlaybackEvent::Error(
                        "audio session is not connected yet".into(),
                    ));
                    continue;
                };
                match other {
                    PlaybackCommand::Load {
                        uri,
                        start,
                        position_ms,
                        ..
                    } => match librespot_core::SpotifyUri::from_uri(&uri) {
                        Ok(u) => {
                            tracing::debug!(
                                kind = u.item_type(),
                                start,
                                position_ms,
                                "loading track"
                            );
                            p.load(u, start, position_ms);
                        }
                        Err(e) => {
                            tracing::debug!(error = %e, "not a playable Spotify URI");
                            let _ = ev_tx.send(PlaybackEvent::Unavailable {
                                uri,
                                reason: super::audio_diag::UnavailableReason::Fetch,
                            });
                        }
                    },
                    PlaybackCommand::Play => p.play(),
                    PlaybackCommand::Pause => p.pause(),
                    PlaybackCommand::Stop => p.stop(),
                    PlaybackCommand::Seek(ms) => p.seek(ms),
                    PlaybackCommand::Preload { uri } => {
                        if let Ok(u) = librespot_core::SpotifyUri::from_uri(&uri) {
                            p.preload(u);
                        }
                    }
                    PlaybackCommand::Attach(_)
                    | PlaybackCommand::SetVolume(_)
                    | PlaybackCommand::RefreshDevices
                    | PlaybackCommand::Remote(_) => {}
                }
            }
        }
    }
}

/// Map 0.0..=1.0 to librespot's 0..=65535 volume scale.
pub fn volume_to_u16(v: f32) -> u16 {
    (v.clamp(0.0, 1.0) * f32::from(u16::MAX)).round() as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(start: bool) -> PlaybackCommand {
        PlaybackCommand::Load {
            uri: "spotify:track:a".into(),
            duration_ms: 2500,
            start,
            position_ms: 0,
        }
    }

    #[test]
    fn mock_engine_plays_to_end() {
        let mut e = MockEngine::default();
        let ev = e.command(load(true));
        assert!(matches!(ev.last(), Some(PlaybackEvent::Playing { .. })));
        assert!(matches!(
            e.tick(1000)[0],
            PlaybackEvent::Position {
                position_ms: 1000,
                ..
            }
        ));
        e.command(PlaybackCommand::Pause);
        assert!(e.tick(1000).is_empty(), "paused clock must not advance");
        e.command(PlaybackCommand::Play);
        e.tick(1000);
        assert!(matches!(e.tick(1000)[0], PlaybackEvent::EndOfTrack { .. }));
    }

    #[test]
    fn mock_engine_seek_and_stop() {
        let mut e = MockEngine::default();
        assert!(
            e.command(PlaybackCommand::Play).is_empty(),
            "nothing loaded"
        );
        e.command(load(false));
        let ev = e.command(PlaybackCommand::Seek(99_999));
        assert!(matches!(
            ev[0],
            PlaybackEvent::Paused {
                position_ms: 2500,
                ..
            }
        ));
        assert!(matches!(
            e.command(PlaybackCommand::Stop)[0],
            PlaybackEvent::Stopped
        ));
    }

    #[test]
    fn volume_scale() {
        assert_eq!(volume_to_u16(0.0), 0);
        assert_eq!(volume_to_u16(1.0), 65535);
        assert_eq!(volume_to_u16(2.0), 65535);
        assert_eq!(volume_to_u16(0.5), 32768);
    }
}
