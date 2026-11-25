//! Native audio engine: HTTP-based audio delivery using the same endpoints
//! as the official Spotify desktop client, bypassing librespot's AP-based
//! audio-key channel. Works on accounts where Spotify denies audio keys
//! (librespot issue #1649).

mod player;
mod resolve;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use super::playback::{
    BitrateKey, PlaybackCommand, PlaybackEvent, RemoteCommand, RemoteEvent,
};
use super::session::ConnectDevice;
use player::NativePlayer;

pub async fn run(
    bitrate: BitrateKey,
    _normalisation: bool,
    mut cmd_rx: mpsc::UnboundedReceiver<PlaybackCommand>,
    ev_tx: mpsc::UnboundedSender<PlaybackEvent>,
) {
    let mut session: Option<librespot_core::Session> = None;
    let mut connect: Option<Arc<ConnectDevice>> = None;
    let mut remote_events: Option<librespot_playback::player::PlayerEventChannel> = None;
    let mut remote_active = false;
    let mut player: Option<NativePlayer> = None;
    let mut pending_volume: f32 = 0.7;

    // Connect controller (same as run_librespot).
    let refresh = Arc::new(tokio::sync::Notify::new());
    let mut observer: Option<tokio::task::JoinHandle<()>> = None;
    let (cluster_tx, mut cluster_rx) = mpsc::unbounded_channel();
    let mut health = tokio::time::interval(Duration::from_secs(2));
    health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let lost = |session: &mut Option<librespot_core::Session>| {
        if session.take().is_some() {
            tracing::warn!("native: audio session lost");
            let _ = ev_tx.send(PlaybackEvent::SessionLost);
        }
    };

    loop {
        let cmd = tokio::select! {
            cmd = cmd_rx.recv() => match cmd { Some(c) => c, None => return },
            _ = health.tick() => {
                if session.as_ref().is_some_and(librespot_core::Session::is_invalid) {
                    lost(&mut session);
                }
                // Tick position from the player.
                if let Some(p) = &player {
                    for ev in p.tick() {
                        if ev_tx.send(ev).is_err() { return; }
                    }
                }
                continue;
            }
            Some(c) = cluster_rx.recv() => {
                if ev_tx.send(PlaybackEvent::Cluster(c)).is_err() { return; }
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
                if super::playback::starts_cast(&ev) {
                    if !remote_active
                        && let Some(p) = &mut player
                    {
                        p.pause();
                    }
                    remote_active = true;
                }
                if let Some(r) = super::playback::map_remote_event(ev) {
                    if r == RemoteEvent::Stopped { remote_active = false; }
                    if ev_tx.send(PlaybackEvent::Remote(r)).is_err() { return; }
                }
                continue;
            }
        };
        match cmd {
            PlaybackCommand::Attach(new_session) => {
                if let Some(o) = observer.take() { o.abort(); }
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
                // Create the player on first attach.
                if player.is_none() {
                    match NativePlayer::new() {
                        Ok(p) => player = Some(p),
                        Err(e) => {
                            let _ = ev_tx.send(PlaybackEvent::Error(
                                format!("native audio init: {e}"),
                            ));
                        }
                    }
                }
            }
            PlaybackCommand::SetVolume(v) => {
                pending_volume = v.clamp(0.0, 1.0);
                if let Some(p) = &player { p.set_volume(pending_volume); }
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
                    RemoteCommand::Volume(v) => {
                        c.spirc.set_volume(super::playback::volume_to_u16(v))
                    }
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
                    let _ = c.spirc.disconnect(true);
                    remote_active = false;
                }
                let Some(sess) = &session else {
                    let _ = ev_tx.send(PlaybackEvent::Error(
                        "native: not connected yet".into(),
                    ));
                    continue;
                };
                let Some(p) = &mut player else {
                    let _ = ev_tx.send(PlaybackEvent::Error(
                        "native: player not initialised".into(),
                    ));
                    continue;
                };
                match other {
                    PlaybackCommand::Load {
                        uri, start, position_ms, ..
                    } => {
                        let _ = ev_tx.send(PlaybackEvent::Loading { uri: uri.clone() });
                        let sess = sess.clone();
                        let br = bitrate;
                        let vol = pending_volume;
                        // Resolve + download + decode in a blocking-friendly task.
                        let load_result = resolve::load_track(&sess, &uri, br).await;
                        match load_result {
                            Ok(audio) => {
                                match p.load(uri.clone(), audio, start, position_ms, vol).await {
                                    Ok(()) => {
                                        let ev = if start {
                                            PlaybackEvent::Playing { uri, position_ms }
                                        } else {
                                            PlaybackEvent::Paused { uri, position_ms }
                                        };
                                        let _ = ev_tx.send(ev);
                                    }
                                    Err(e) => {
                                        tracing::warn!(error = %e, "native: decode failed");
                                        let _ = ev_tx.send(PlaybackEvent::Unavailable {
                                            uri,
                                            reason: super::audio_diag::UnavailableReason::Fetch,
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "native: resolve/download failed");
                                let _ = ev_tx.send(PlaybackEvent::Unavailable {
                                    uri,
                                    reason: super::audio_diag::UnavailableReason::Fetch,
                                });
                            }
                        }
                    }
                    PlaybackCommand::Play => {
                        if let Some(ev) = p.play() {
                            let _ = ev_tx.send(ev);
                        }
                    }
                    PlaybackCommand::Pause => {
                        if let Some(ev) = p.pause_ev() {
                            let _ = ev_tx.send(ev);
                        }
                    }
                    PlaybackCommand::Stop => {
                        p.stop();
                        let _ = ev_tx.send(PlaybackEvent::Stopped);
                    }
                    PlaybackCommand::Seek(ms) => {
                        if let Some(ev) = p.seek(ms) {
                            let _ = ev_tx.send(ev);
                        }
                    }
                    PlaybackCommand::Preload { .. } => {}
                    PlaybackCommand::Attach(_)
                    | PlaybackCommand::SetVolume(_)
                    | PlaybackCommand::RefreshDevices
                    | PlaybackCommand::Remote(_) => {}
                }
            }
        }
    }
}
