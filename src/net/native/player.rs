//! Audio output: decode OGG Vorbis (or other Spotify formats) with
//! symphonia via rodio, play through the system's default output device.
//!
//! rodio's `OutputStream` is not `Send`, so the actual audio lives on a
//! dedicated OS thread.  The engine talks to it through a small command
//! channel and keeps track of position in the async context.

use std::io::Cursor;
use std::time::Instant;

use tokio::sync::oneshot;

use super::resolve::AudioData;
use super::super::playback::PlaybackEvent;

enum Cmd {
    Load {
        data: Vec<u8>,
        volume: f32,
        start: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Play,
    Pause,
    Stop,
    Volume(f32),
    /// Is the sink empty (track finished)?
    PollEmpty(oneshot::Sender<bool>),
}

/// Runs on a dedicated OS thread (not Send).
fn audio_thread(rx: std::sync::mpsc::Receiver<Cmd>) {
    use rodio::{OutputStream, Sink};

    let Ok((_stream, handle)) = OutputStream::try_default() else {
        tracing::error!("native: failed to open audio output");
        return;
    };
    let mut sink: Option<Sink> = None;

    while let Ok(cmd) = rx.recv() {
        match cmd {
            Cmd::Load {
                data,
                volume,
                start,
                reply,
            } => {
                // Drop old sink first.
                sink.take();

                let cursor = Cursor::new(data);
                match rodio::Decoder::new(cursor) {
                    Ok(source) => {
                        match Sink::try_new(&handle) {
                            Ok(s) => {
                                s.set_volume(volume);
                                s.append(source);
                                if !start {
                                    s.pause();
                                }
                                sink = Some(s);
                                let _ = reply.send(Ok(()));
                            }
                            Err(e) => {
                                let _ = reply.send(Err(format!("sink: {e}")));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = reply.send(Err(format!("decode: {e}")));
                    }
                }
            }
            Cmd::Play => {
                if let Some(s) = &sink {
                    s.play();
                }
            }
            Cmd::Pause => {
                if let Some(s) = &sink {
                    s.pause();
                }
            }
            Cmd::Stop => {
                sink.take();
            }
            Cmd::Volume(v) => {
                if let Some(s) = &sink {
                    s.set_volume(v);
                }
            }
            Cmd::PollEmpty(reply) => {
                let empty = sink.as_ref().is_none_or(|s| s.empty());
                let _ = reply.send(empty);
            }
        }
    }
}

/// Strip Spotify's 0xa7-prefixed header from encrypted OGG files.
fn skip_spotify_header(data: &[u8]) -> &[u8] {
    if data.first() == Some(&0xa7)
        && let Some(pos) = data.windows(4).position(|w| w == b"OggS")
    {
        return &data[pos..];
    }
    data
}

/// Handle to the audio-output thread. This is `Send` + `Sync`.
pub struct NativePlayer {
    tx: std::sync::mpsc::Sender<Cmd>,
    _thread: std::thread::JoinHandle<()>,
    uri: Option<String>,
    duration_ms: u32,
    position_base_ms: u32,
    playing_since: Option<Instant>,
}

impl NativePlayer {
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("native-audio".into())
            .spawn(move || audio_thread(rx))?;
        Ok(Self {
            tx,
            _thread: thread,
            uri: None,
            duration_ms: 0,
            position_base_ms: 0,
            playing_since: None,
        })
    }

    pub async fn load(
        &mut self,
        uri: String,
        audio: AudioData,
        start: bool,
        position_ms: u32,
        volume: f32,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.playing_since = None;

        let data = skip_spotify_header(&audio.bytes).to_vec();
        let (reply_tx, reply_rx) = oneshot::channel();

        self.tx
            .send(Cmd::Load {
                data,
                volume,
                start,
                reply: reply_tx,
            })
            .map_err(|_| "audio thread gone")?;

        let result = reply_rx.await.map_err(|_| "audio thread gone")?;
        result.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
        self.uri = Some(uri);
        self.duration_ms = 0;
        self.position_base_ms = position_ms;
        self.playing_since = if start { Some(Instant::now()) } else { None };
        Ok(())
    }

    fn current_position_ms(&self) -> u32 {
        let elapsed = self
            .playing_since
            .map(|s| s.elapsed().as_millis() as u32)
            .unwrap_or(0);
        self.position_base_ms.saturating_add(elapsed)
    }

    pub fn play(&mut self) -> Option<PlaybackEvent> {
        let uri = self.uri.clone()?;
        let _ = self.tx.send(Cmd::Play);
        self.position_base_ms = self.current_position_ms();
        self.playing_since = Some(Instant::now());
        Some(PlaybackEvent::Playing {
            uri,
            position_ms: self.position_base_ms,
        })
    }

    pub fn pause(&mut self) {
        self.position_base_ms = self.current_position_ms();
        self.playing_since = None;
        let _ = self.tx.send(Cmd::Pause);
    }

    pub fn pause_ev(&mut self) -> Option<PlaybackEvent> {
        let uri = self.uri.clone()?;
        self.pause();
        Some(PlaybackEvent::Paused {
            uri,
            position_ms: self.position_base_ms,
        })
    }

    pub fn stop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        self.uri = None;
        self.playing_since = None;
        self.position_base_ms = 0;
    }

    pub fn seek(&mut self, ms: u32) -> Option<PlaybackEvent> {
        let uri = self.uri.clone()?;
        self.position_base_ms = ms;
        if self.playing_since.is_some() {
            self.playing_since = Some(Instant::now());
            Some(PlaybackEvent::Playing {
                uri,
                position_ms: ms,
            })
        } else {
            Some(PlaybackEvent::Paused {
                uri,
                position_ms: ms,
            })
        }
    }

    pub fn set_volume(&self, v: f32) {
        let _ = self.tx.send(Cmd::Volume(v));
    }

    pub fn tick(&self) -> Vec<PlaybackEvent> {
        let Some(uri) = &self.uri else { return vec![] };
        if self.playing_since.is_none() {
            return vec![];
        }

        // Check if the sink is empty (track ended).
        let (reply_tx, mut reply_rx) = oneshot::channel();
        if self.tx.send(Cmd::PollEmpty(reply_tx)).is_err() {
            return vec![];
        }
        match reply_rx.try_recv() {
            Ok(true) => vec![PlaybackEvent::EndOfTrack { uri: uri.clone() }],
            Ok(false) => vec![PlaybackEvent::Position {
                uri: uri.clone(),
                position_ms: self.current_position_ms(),
            }],
            Err(_) => vec![PlaybackEvent::Position {
                uri: uri.clone(),
                position_ms: self.current_position_ms(),
            }],
        }
    }
}
