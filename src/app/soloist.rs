//! Spotify Soloist as the local audio engine: setup flow, status, and
//! routing playback to it.
//!
//! When the user turns it on (Settings > Playback, or the setup card that
//! appears when Spotify refuses librespot's audio key), Ferrotune keeps its
//! librespot session for login, metadata, browse and the Connect
//! controller, but stops announcing its own Connect device and stops the
//! local librespot player. Audio comes from the Soloist daemon instead:
//!
//! * transport, seek, volume, shuffle, repeat and "Add to queue" go to
//!   Soloist's WebSocket API;
//! * a context started at a given track goes through the Connect
//!   controller to the Soloist device (the WebSocket `play` command takes
//!   one URI and cannot skip to a track); when the device id is not known
//!   yet, `play` with the track or context URI is the fallback;
//! * the player bar and MPRIS follow Soloist's own events.

use iced::Task;

use super::state::{PlayStatus, Playback, Repeat};
use super::{App, Done, Message, RemoteKind, RemotePlayback, remote_track};
use crate::auth::secrets::{KEY_SOLOIST, Secret, normalize_api_key};
use crate::config::AudioEngine;
use crate::net::connect_ctl::{ConnectOp, ControlCommand};
use crate::net::soloist::protocol::{self, Command, Event, RepeatMode};
use crate::net::soloist::{BuildView, Control, EngineStatus, SoloistEvent, SoloistHandle};

/// Device id used for Soloist until the connect-state cluster lists it.
pub const LOCAL_ID: &str = "soloist:local";

/// Soloist engine state (UI + routing).
#[derive(Debug, Default)]
pub struct SoloistState {
    pub status: EngineStatus,
    pub handle: Option<SoloistHandle>,
    pub build: Option<BuildView>,
    /// The WebSocket API is connected.
    pub ws: bool,
    pub logged_in: bool,
    pub is_active: bool,
    /// Soloist's resident memory (separate process).
    pub memory: Option<u64>,
    /// The setup card is open.
    pub card: bool,
    /// What is typed in the API key field (masked, never logged).
    pub key_input: Option<Secret>,
    /// Editing the key (Settings > Replace API key).
    pub editing_key: bool,
    /// The key from the keyring: `None` = not read yet.
    key: Option<Option<Secret>>,
    /// Waiting for the engine to be ready to start this.
    pending: Option<ConnectOp>,
}

impl SoloistState {
    pub fn has_key(&self) -> bool {
        matches!(self.key, Some(Some(_)))
    }

    /// The key field shows (no key yet, or replacing it).
    pub fn wants_key(&self) -> bool {
        self.editing_key || matches!(self.key, Some(None))
    }
}

/// UI actions of the setup card and Settings > Playback.
#[derive(Debug, Clone)]
pub enum SoloistUi {
    OpenSetup,
    CloseSetup,
    /// Open the dashboard's "Spotify Soloist API Key" page.
    OpenKeyPage,
    KeyInput(Secret),
    SaveKey,
    ReplaceKey,
    /// Settings switch: use Soloist (`true`) or the built-in player.
    Enable(bool),
    UpdateNow,
    Repair,
    Output(String),
    OutputCommit,
}

/// What a Soloist status means for the setup card.
pub fn card_needed(engine: AudioEngine, status: &EngineStatus, open: bool) -> bool {
    if open {
        return true;
    }
    engine == AudioEngine::Soloist
        && matches!(
            status,
            EngineStatus::NeedsKey
                | EngineStatus::NeedsPairing
                | EngineStatus::Pairing
                | EngineStatus::Expired
                | EngineStatus::Unsupported
                | EngineStatus::Error(_)
        )
}

fn play_status(s: protocol::Status) -> PlayStatus {
    match s {
        protocol::Status::Playing => PlayStatus::Playing,
        protocol::Status::Buffering => PlayStatus::Loading,
        protocol::Status::Paused | protocol::Status::Unknown => PlayStatus::Paused,
        protocol::Status::Idle => PlayStatus::Stopped,
    }
}

fn repeat_of(m: RepeatMode) -> Repeat {
    match m {
        RepeatMode::Off => Repeat::Off,
        RepeatMode::Context => Repeat::Context,
        RepeatMode::Track => Repeat::Track,
    }
}

#[cfg(test)]
fn repeat_mode(r: Repeat) -> RepeatMode {
    match r {
        Repeat::Off => RepeatMode::Off,
        Repeat::Context => RepeatMode::Context,
        Repeat::Track => RepeatMode::Track,
    }
}

/// WebSocket commands for a controller operation on the Soloist device,
/// or `None` when it must go through connect-state (a context started at
/// a given track, when the device id is known).
pub fn ws_commands(op: &ConnectOp, id_known: bool) -> Option<Vec<Command>> {
    Some(match op {
        ConnectOp::Command(_, c) => match c {
            ControlCommand::Pause => vec![Command::Pause],
            ControlCommand::Resume => vec![Command::Play(None)],
            ControlCommand::Next => vec![Command::SkipNext],
            ControlCommand::Previous => vec![Command::SkipPrev],
            ControlCommand::Seek(ms) => vec![Command::Seek(*ms)],
            ControlCommand::Shuffle(on) => vec![Command::SetShuffle(*on)],
            ControlCommand::Repeat { context, track } => {
                let mode = if *track {
                    RepeatMode::Track
                } else if *context {
                    RepeatMode::Context
                } else {
                    RepeatMode::Off
                };
                protocol::repeat_commands(mode).to_vec()
            }
            ControlCommand::Play(p) => {
                let skips = p.track_uri.is_some() || p.track_index.is_some();
                if id_known && (skips || p.position_ms.is_some()) {
                    return None;
                }
                let mut v = vec![Command::Play(Some(
                    p.track_uri.clone().unwrap_or_else(|| p.context_uri.clone()),
                ))];
                if let Some(ms) = p.position_ms.filter(|ms| *ms > 0) {
                    v.push(Command::Seek(ms));
                }
                v
            }
        },
        // Picking "This computer": the official way is a transfer; without
        // the device id, ask Soloist to become the active device.
        ConnectOp::Transfer(_) if id_known => return None,
        ConnectOp::Transfer(_) => vec![Command::Activate],
        ConnectOp::Volume(_, v) => vec![Command::SetVolume(protocol::volume_percent(*v))],
    })
}

/// Pressing play while another device (or none) is active: `activate`
/// first, so Soloist becomes the active Spotify Connect device.
pub fn with_activation(mut cmds: Vec<Command>, active: bool) -> Vec<Command> {
    if !active && matches!(cmds.first(), Some(Command::Play(_))) {
        cmds.insert(0, Command::Activate);
    }
    cmds
}

fn op_target(op: &ConnectOp) -> &str {
    match op {
        ConnectOp::Command(id, _) | ConnectOp::Transfer(id) | ConnectOp::Volume(id, _) => id,
    }
}

fn with_target(op: ConnectOp, id: String) -> ConnectOp {
    match op {
        ConnectOp::Command(_, c) => ConnectOp::Command(id, c),
        ConnectOp::Transfer(_) => ConnectOp::Transfer(id),
        ConnectOp::Volume(_, v) => ConnectOp::Volume(id, v),
    }
}

impl App {
    pub fn soloist_enabled(&self) -> bool {
        self.settings.engine == AudioEngine::Soloist
    }

    pub fn soloist_card(&self) -> bool {
        card_needed(
            self.settings.engine,
            &self.soloist.status,
            self.soloist.card,
        )
    }

    /// The Soloist device as `(id, name)` once it is ready. The id is the
    /// connect-state one when the cluster lists a device with our name.
    pub fn soloist_device(&self) -> Option<(String, String)> {
        if !self.soloist_enabled() || !self.soloist.status.is_ready() {
            return None;
        }
        let name = self.settings_device_name();
        let id = self
            .cluster
            .as_ref()
            .and_then(|c| {
                c.devices
                    .iter()
                    .find(|d| d.name.eq_ignore_ascii_case(&name))
            })
            .map_or_else(|| LOCAL_ID.to_owned(), |d| d.id.clone());
        Some((id, name))
    }

    /// Is `id` the local Soloist device?
    pub fn is_soloist_id(&self, id: &str) -> bool {
        if !self.soloist_enabled() {
            return false;
        }
        if id == LOCAL_ID {
            return true;
        }
        let name = self.settings_device_name();
        self.cluster
            .as_ref()
            .and_then(|c| c.device(id))
            .is_some_and(|d| d.name.eq_ignore_ascii_case(&name))
    }

    /// Soloist is the device playing in the player bar.
    pub fn soloist_playing_here(&self) -> bool {
        self.remote_device()
            .is_some_and(|id| self.is_soloist_id(id))
    }

    /// Send a controller operation to Soloist when `op` targets it.
    /// Returns the operation back when it is for another device (or must
    /// go through connect-state).
    pub(super) fn soloist_intercept(&mut self, op: ConnectOp) -> Option<ConnectOp> {
        if !self.is_soloist_id(op_target(&op)) {
            return Some(op);
        }
        let real_id = self
            .soloist_device()
            .map(|d| d.0)
            .filter(|id| id != LOCAL_ID);
        if !self.soloist.status.is_ready() || !self.soloist.ws {
            // Not ready yet: start it once the engine is.
            self.soloist.pending = Some(op);
            self.soloist.card = true;
            return None;
        }
        match ws_commands(&op, real_id.is_some()) {
            Some(cmds) => {
                let cmds = with_activation(cmds, self.soloist.is_active);
                if let Some(h) = &self.soloist.handle {
                    for c in cmds {
                        h.command(c);
                    }
                }
                None
            }
            None => real_id.map(|id| with_target(op, id)),
        }
    }

    pub(super) fn soloist_kind(&self) -> crate::net::soloist::SoloistKind {
        use crate::net::soloist::{SoloistKind, default_url};
        if self.is_mock() {
            SoloistKind::Mock
        } else {
            SoloistKind::Real {
                paths: self.config.soloist_paths(),
                url: default_url(&self.config.soloist_cdn),
            }
        }
    }

    fn soloist_output(&self) -> Option<String> {
        Some(self.settings.soloist_output.trim().to_owned()).filter(|s| !s.is_empty())
    }

    /// (Re)start the engine with the current key and settings.
    pub(super) fn soloist_configure(&mut self) {
        let Some(h) = &self.soloist.handle else {
            return;
        };
        let Some(key) = self.soloist.key.clone() else {
            return; // keyring not read yet
        };
        let volume = (self.now_playback().volume.clamp(0.0, 1.0) * 100.0).round() as u8;
        h.send(Control::Configure {
            key,
            opts: crate::net::soloist::LaunchOptions {
                device_name: self.settings_device_name(),
                pipewire_device: self.soloist_output(),
                cache_mb: self.settings.soloist_cache_mb,
                initial_volume: Some(volume),
            },
        });
    }

    /// Read the key from the keyring (boot, or when Soloist is turned on).
    pub(super) fn soloist_load_key(&self) -> Task<Message> {
        if self.is_mock() {
            return Task::done(Message::Done(Done::SoloistKey(Ok(None))));
        }
        let secrets = self.secrets.clone();
        Task::perform(super::blocking(move || secrets.get(KEY_SOLOIST)), |r| {
            Message::Done(Done::SoloistKey(r))
        })
    }

    pub(super) fn on_soloist_key(&mut self, r: crate::error::Result<Option<Secret>>) {
        let key = match r {
            Ok(k) => k,
            Err(e) => {
                tracing::warn!(error = %e, "could not read the Soloist API key");
                self.show_toast(e.user_message());
                None
            }
        };
        self.soloist.key = Some(key);
        self.soloist_configure();
    }

    pub(super) fn on_soloist_key_saved(
        &mut self,
        r: crate::error::Result<Secret>,
    ) -> Task<Message> {
        match r {
            Ok(key) => {
                self.soloist.key = Some(Some(key));
                self.soloist.key_input = None;
                self.soloist.editing_key = false;
                if self.soloist_enabled() {
                    self.soloist_configure();
                    Task::none()
                } else {
                    self.switch_engine(AudioEngine::Soloist)
                }
            }
            Err(e) => {
                self.show_toast(e.user_message());
                Task::none()
            }
        }
    }

    /// Switch between the built-in player and Soloist: the engine and the
    /// session are rebuilt (with or without our own Connect device).
    pub(super) fn switch_engine(&mut self, engine: AudioEngine) -> Task<Message> {
        if self.settings.engine == engine {
            return Task::none();
        }
        self.settings.engine = engine;
        if engine == AudioEngine::Builtin || engine == AudioEngine::Native {
            if let Some(h) = self.soloist.handle.take() {
                h.send(Control::Shutdown);
            }
            self.soloist.status = EngineStatus::Off;
            self.soloist.ws = false;
            self.soloist.card = false;
            if self.soloist_playing_here_any() {
                self.remote = None;
            }
        } else {
            self.soloist.card = true;
            self.local_denied = false;
            self.playback_block = None;
            if self.playback.is_playing() {
                self.engine_send(crate::net::playback::PlaybackCommand::Pause);
            }
        }
        self.engine_kind = super::engine_kind(self.is_mock(), &self.settings);
        self.engine = None;
        let mut tasks = vec![self.save_settings()];
        // A new session announces (or no longer announces) our own
        // Connect device, so there is never a second "Ferrotune".
        if !self.is_mock()
            && let Some(s) = self.session.take()
        {
            s.shutdown();
            tasks.push(self.connect_audio_task());
        }
        Task::batch(tasks)
    }

    /// Mock state for `--screenshot soloist` / `soloist-settings`.
    pub(super) fn soloist_screenshot(&mut self, enabled: bool) {
        if enabled {
            self.settings.engine = AudioEngine::Soloist;
            self.engine_kind = super::engine_kind(true, &self.settings);
            self.soloist.key = Some(Some(Secret::new("mock-soloist-key")));
        } else {
            self.soloist.key = Some(None);
            self.soloist.card = true;
        }
    }

    /// Remote is the Soloist device, whatever its id.
    fn soloist_playing_here_any(&self) -> bool {
        self.remote_device()
            .is_some_and(|id| id == LOCAL_ID || self.is_soloist_id(id))
    }

    pub(super) fn update_soloist_ui(&mut self, u: SoloistUi) -> Task<Message> {
        match u {
            SoloistUi::OpenSetup => {
                self.soloist.card = true;
                if self.soloist.key.is_none() {
                    return self.soloist_load_key();
                }
                Task::none()
            }
            SoloistUi::CloseSetup => {
                self.soloist.card = false;
                self.soloist.editing_key = false;
                self.soloist.key_input = None;
                Task::none()
            }
            SoloistUi::OpenKeyPage => {
                if let Err(e) = crate::auth::oauth::open_browser(crate::net::soloist::API_KEY_PAGE)
                {
                    tracing::warn!(error = %e, "could not open a browser");
                    self.show_toast(format!(
                        "Open {} in your browser.",
                        crate::net::soloist::API_KEY_PAGE
                    ));
                }
                Task::none()
            }
            SoloistUi::KeyInput(k) => {
                // Bounded: a paste of something huge is cut, then refused.
                let v: String = k.expose().chars().take(600).collect();
                self.soloist.key_input = Some(Secret::new(v));
                Task::none()
            }
            SoloistUi::SaveKey => {
                let raw = self
                    .soloist
                    .key_input
                    .as_ref()
                    .map(|s| s.expose().to_owned())
                    .unwrap_or_default();
                let Some(key) = normalize_api_key(&raw) else {
                    self.show_toast("That does not look like a Spotify Soloist API key.");
                    return Task::none();
                };
                if self.is_mock() {
                    return Task::done(Message::Done(Done::SoloistKeySaved(Ok(key))));
                }
                let secrets = self.secrets.clone();
                Task::perform(
                    super::blocking(move || secrets.set(KEY_SOLOIST, &key).map(|()| key)),
                    |r| Message::Done(Done::SoloistKeySaved(r)),
                )
            }
            SoloistUi::ReplaceKey => {
                self.soloist.editing_key = true;
                self.soloist.card = true;
                Task::none()
            }
            SoloistUi::Enable(on) => {
                if !on {
                    return self.switch_engine(AudioEngine::Builtin);
                }
                if self.soloist.has_key() {
                    return self.switch_engine(AudioEngine::Soloist);
                }
                // The key comes first; saving it switches the engine.
                self.soloist.card = true;
                if self.soloist.key.is_none() {
                    return self.soloist_load_key();
                }
                Task::none()
            }
            SoloistUi::UpdateNow => {
                if let Some(h) = &self.soloist.handle {
                    h.send(Control::UpdateNow);
                }
                Task::none()
            }
            SoloistUi::Repair => {
                if let Some(h) = &self.soloist.handle {
                    h.send(Control::Pair);
                    self.soloist.card = true;
                }
                Task::none()
            }
            SoloistUi::Output(v) => {
                self.settings.soloist_output =
                    v.chars().filter(|c| !c.is_control()).take(128).collect();
                Task::none()
            }
            SoloistUi::OutputCommit => {
                self.settings.soloist_output = self.settings.soloist_output.trim().to_owned();
                self.soloist_configure();
                self.save_settings()
            }
        }
    }

    pub(super) fn on_soloist(&mut self, e: SoloistEvent) -> Task<Message> {
        match e {
            SoloistEvent::Ready(h) => {
                self.soloist.handle = Some(h);
                if self.soloist.key.is_none() {
                    return self.soloist_load_key();
                }
                self.soloist_configure();
            }
            SoloistEvent::Status(s) => {
                let was_ready = self.soloist.status.is_ready();
                self.soloist.status = s;
                if self.soloist.status.is_ready() {
                    if !was_ready {
                        self.soloist.card = false;
                    }
                    return self.soloist_flush_pending();
                }
                if was_ready
                    && self.soloist_playing_here_any()
                    && let Some(r) = self.remote.as_mut()
                {
                    let pos = r.playback.position(self.now);
                    r.playback.report(PlayStatus::Paused, pos, self.now);
                }
            }
            SoloistEvent::Build(b) => self.soloist.build = Some(b),
            SoloistEvent::Connected(on) => {
                self.soloist.ws = on;
                if on {
                    return self.soloist_flush_pending();
                }
            }
            SoloistEvent::Memory(m) => self.soloist.memory = m,
            SoloistEvent::Ws(ev) => self.on_soloist_ws(ev),
        }
        Task::none()
    }

    fn soloist_flush_pending(&mut self) -> Task<Message> {
        if !(self.soloist.status.is_ready() && self.soloist.ws) {
            return Task::none();
        }
        match self.soloist.pending.take() {
            Some(op) => self.connect_task(op),
            None => Task::none(),
        }
    }

    /// Make the player bar follow Soloist.
    fn soloist_take_bar(&mut self) {
        let Some((id, name)) = self.soloist_device() else {
            return;
        };
        match self.remote.as_mut() {
            Some(r) if r.device_id().is_some_and(|i| i == LOCAL_ID || i == id) => {
                // Keep the state; only adopt the cluster id once known.
                r.kind = RemoteKind::Device { id, name };
            }
            _ => {
                if self.playback.is_playing() {
                    self.engine_send(crate::net::playback::PlaybackCommand::Pause);
                }
                self.remote = Some(RemotePlayback {
                    kind: RemoteKind::Device { id, name },
                    track: None,
                    playback: Playback {
                        volume: self.playback.volume,
                        ..Playback::default()
                    },
                    shuffle: false,
                    repeat: Repeat::Off,
                });
            }
        }
    }

    pub(super) fn soloist_active(&mut self, active: bool) {
        self.soloist.is_active = active;
        if active {
            self.soloist_take_bar();
        } else if self.soloist_playing_here_any() {
            self.remote = None;
        }
    }

    pub(super) fn on_soloist_ws(&mut self, ev: Event) {
        let now = self.now;
        let unix_ms = crate::net::soloist::install::now_unix().saturating_mul(1000);
        match ev {
            Event::AuthState {
                logged_in,
                is_active,
                ..
            } => {
                self.soloist.logged_in = logged_in;
                if logged_in && let Some(h) = &self.soloist.handle {
                    h.command(Command::GetState);
                }
                self.soloist_active(logged_in && is_active);
                return;
            }
            Event::DeviceChanged { is_active, .. } => {
                self.soloist_active(is_active);
                return;
            }
            Event::PlaybackState(s) => {
                self.soloist_active(s.is_active);
                let Some(r) = self.soloist_remote() else {
                    return;
                };
                if let Some(item) = s.item.as_ref().filter(|i| !i.uri.is_empty()) {
                    let t = item.to_remote_track();
                    r.playback.duration_ms = t.duration_ms;
                    r.track = Some(remote_track(&t));
                }
                r.playback
                    .report(play_status(s.status), s.position.at(unix_ms), now);
                r.playback.volume = protocol::volume_unit(s.volume);
                r.shuffle = s.options.shuffle;
                r.repeat = repeat_of(s.options.repeat);
                return;
            }
            Event::Error { message } => {
                tracing::debug!(error = %message, "soloist error");
                if message.contains("authentication") {
                    self.soloist.card = true;
                }
                return;
            }
            _ => {}
        }
        let Some(r) = self.soloist_remote() else {
            return;
        };
        match ev {
            Event::TrackChanged { item } => {
                let t = item.to_remote_track();
                r.playback.duration_ms = t.duration_ms;
                r.track = (!t.uri.is_empty()).then(|| remote_track(&t));
            }
            Event::PlaybackChanged { status } => {
                let pos = r.playback.position(now);
                r.playback.report(play_status(status), pos, now);
            }
            Event::PositionSync { position } => {
                let st = r.playback.status;
                let pos = position.at(unix_ms);
                let pos = if r.playback.duration_ms > 0 {
                    pos.min(r.playback.duration_ms)
                } else {
                    pos
                };
                r.playback.report(st, pos, now);
            }
            Event::VolumeChanged { volume } => r.playback.volume = protocol::volume_unit(volume),
            Event::OptionsChanged { options } => {
                r.shuffle = options.shuffle;
                r.repeat = repeat_of(options.repeat);
            }
            _ => {}
        }
    }

    /// The remote playback when it is Soloist.
    fn soloist_remote(&mut self) -> Option<&mut RemotePlayback> {
        if !self.soloist_playing_here_any() {
            return None;
        }
        self.remote.as_mut()
    }

    /// "Add to queue" while Soloist plays: tracks go to its queue.
    pub(super) fn soloist_enqueue(&mut self, uri: &str) -> bool {
        if !self.soloist_playing_here() || !uri.starts_with("spotify:track:") {
            return false;
        }
        if let Some(h) = &self.soloist.handle {
            h.command(Command::AddToQueue(uri.to_owned()));
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::connect_ctl::PlayRequest;

    fn play(track: Option<&str>, pos: Option<u32>) -> ConnectOp {
        ConnectOp::Command(
            LOCAL_ID.into(),
            ControlCommand::Play(PlayRequest {
                context_uri: "spotify:album:a".into(),
                track_uri: track.map(Into::into),
                track_index: None,
                position_ms: pos,
            }),
        )
    }

    #[test]
    fn controller_ops_map_to_websocket_commands() {
        let c = |cmd| ConnectOp::Command("x".into(), cmd);
        assert_eq!(
            ws_commands(&c(ControlCommand::Pause), false),
            Some(vec![Command::Pause])
        );
        assert_eq!(
            ws_commands(&c(ControlCommand::Resume), true),
            Some(vec![Command::Play(None)])
        );
        assert_eq!(
            ws_commands(&c(ControlCommand::Seek(5)), true),
            Some(vec![Command::Seek(5)])
        );
        assert_eq!(
            ws_commands(
                &c(ControlCommand::Repeat {
                    context: true,
                    track: true
                }),
                true
            ),
            Some(vec![
                Command::SetRepeatContext(false),
                Command::SetRepeatTrack(true)
            ])
        );
        assert_eq!(
            ws_commands(&ConnectOp::Volume("x".into(), 0.42), true),
            Some(vec![Command::SetVolume(42)])
        );
        // Start at a track: connect-state when the device id is known…
        assert_eq!(
            ws_commands(&play(Some("spotify:track:t"), None), true),
            None
        );
        // …else the WebSocket fallback plays that track.
        assert_eq!(
            ws_commands(&play(Some("spotify:track:t"), Some(9000)), false),
            Some(vec![
                Command::Play(Some("spotify:track:t".into())),
                Command::Seek(9000)
            ])
        );
        // A whole context from the top always fits the WebSocket API.
        assert_eq!(
            ws_commands(&play(None, None), true),
            Some(vec![Command::Play(Some("spotify:album:a".into()))])
        );
        assert_eq!(ws_commands(&ConnectOp::Transfer("x".into()), true), None);
        assert_eq!(
            ws_commands(&ConnectOp::Transfer("x".into()), false),
            Some(vec![Command::Activate])
        );
    }

    #[test]
    fn play_activates_an_inactive_soloist() {
        let play = vec![Command::Play(Some("spotify:track:t".into()))];
        assert_eq!(
            with_activation(play.clone(), false),
            vec![Command::Activate, play[0].clone()]
        );
        assert_eq!(with_activation(play.clone(), true), play);
        assert_eq!(
            with_activation(vec![Command::Pause], false),
            vec![Command::Pause]
        );
    }

    #[test]
    fn card_shows_when_the_engine_needs_the_user() {
        use AudioEngine::*;
        assert!(card_needed(Builtin, &EngineStatus::Off, true));
        assert!(!card_needed(Builtin, &EngineStatus::NeedsKey, false));
        assert!(card_needed(Soloist, &EngineStatus::NeedsKey, false));
        assert!(card_needed(Soloist, &EngineStatus::NeedsPairing, false));
        assert!(!card_needed(Soloist, &EngineStatus::Ready, false));
        assert!(!card_needed(Soloist, &EngineStatus::Installing, false));
        assert!(card_needed(
            Soloist,
            &EngineStatus::Error("x".into()),
            false
        ));
    }

    #[test]
    fn status_and_repeat_mapping() {
        assert_eq!(
            play_status(protocol::Status::Buffering),
            PlayStatus::Loading
        );
        assert_eq!(play_status(protocol::Status::Idle), PlayStatus::Stopped);
        for r in [Repeat::Off, Repeat::Context, Repeat::Track] {
            assert_eq!(repeat_of(repeat_mode(r)), r);
        }
    }
}
