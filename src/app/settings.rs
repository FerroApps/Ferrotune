//! Settings screen: a pure reducer over [`Settings`] plus the side effects
//! (persist to `config.json`, restart the audio engine for audio changes,
//! measure / clear the cache).

use iced::Task;

use super::{App, Done, Message, engine_kind};
use crate::config::{Bitrate, Settings, ThemeChoice};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SettingChange {
    Quality(Bitrate),
    Normalisation(bool),
    Gapless(bool),
    Autoplay(bool),
    PlayLocal(bool),
    Connect(bool),
    Theme(ThemeChoice),
    CloseToTray(bool),
    StartMinimized(bool),
    /// "Check for updates automatically".
    AutoUpdateCheck(bool),
    /// Writes or removes the XDG autostart entry (not stored in config.json).
    Autostart(bool),
    /// Typing in the device name field (saved on commit).
    DeviceName(String),
    DeviceNameCommit,
    ClearCache,
}

/// What a change needs besides saving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Effects {
    pub save: bool,
    /// The player must be rebuilt (bitrate, normalisation).
    pub restart_engine: bool,
    /// Takes effect at the next session (Connect name/visibility, autoplay).
    pub next_session: bool,
}

pub const MAX_DEVICE_NAME: usize = 64;

/// Apply `c` to `s`. Pure: the caller performs the [`Effects`].
pub fn apply(s: &mut Settings, c: &SettingChange) -> Effects {
    let mut e = Effects {
        save: true,
        ..Effects::default()
    };
    match c {
        SettingChange::Quality(b) => {
            e.restart_engine = s.bitrate != *b;
            s.bitrate = *b;
        }
        SettingChange::Normalisation(on) => {
            e.restart_engine = s.normalisation != *on;
            s.normalisation = *on;
        }
        SettingChange::Gapless(on) => s.gapless = *on,
        SettingChange::Autoplay(on) => {
            e.next_session = s.autoplay != *on;
            s.autoplay = *on;
        }
        SettingChange::PlayLocal(on) => s.play_local = *on,
        SettingChange::Connect(on) => {
            e.next_session = s.connect != *on;
            s.connect = *on;
        }
        SettingChange::Theme(t) => s.theme = *t,
        SettingChange::CloseToTray(on) => s.close_to_tray = *on,
        SettingChange::StartMinimized(on) => s.start_minimized = *on,
        SettingChange::AutoUpdateCheck(on) => s.auto_update_check = *on,
        SettingChange::Autostart(_) => e.save = false,
        SettingChange::DeviceName(n) => {
            s.device_name = n
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_DEVICE_NAME)
                .collect();
            e.save = false;
        }
        SettingChange::DeviceNameCommit => {
            s.device_name = s.device_name.trim().to_owned();
            e.next_session = true;
        }
        SettingChange::ClearCache => e.save = false,
    }
    e
}

/// Labels of the audio quality choices, as the official client names them.
pub fn quality_label(b: Bitrate) -> &'static str {
    match b {
        Bitrate::Low => "Normal (96 kbit/s)",
        Bitrate::Normal => "High (160 kbit/s)",
        Bitrate::High => "Very high (320 kbit/s)",
    }
}

impl App {
    pub(super) fn measure_cache(&self) -> Task<Message> {
        let store = self.store.clone();
        Task::perform(async move { store.run(|s| s.cache_bytes()).await }, |r| {
            Message::Done(Done::CacheSize(r))
        })
    }

    pub(super) fn change_setting(&mut self, c: SettingChange) -> Task<Message> {
        if c == SettingChange::ClearCache {
            let store = self.store.clone();
            return Task::perform(async move { store.run(|s| s.clear_cache()).await }, |r| {
                Message::Done(Done::CacheCleared(r))
            });
        }
        if let SettingChange::Autostart(on) = c {
            return self.set_autostart(on);
        }
        let fx = apply(&mut self.settings, &c);
        if let SettingChange::CloseToTray(on) = c {
            self.win.close_to_tray = on;
        }
        if fx.restart_engine {
            // A new subscription identity restarts the engine; its `Ready`
            // re-attaches the session.
            self.engine_kind = engine_kind(self.is_mock(), &self.settings);
            self.engine = None;
            self.reload_on_play = self.queue.current.is_some();
            let pos = self.playback.position(self.now);
            if self.playback.is_playing() {
                self.playback
                    .report(super::state::PlayStatus::Paused, pos, self.now);
            }
        }
        if c == SettingChange::DeviceNameCommit && self.soloist_enabled() {
            // Soloist's device name: restart it with the new one.
            self.soloist_configure();
        } else if fx.next_session {
            self.show_toast("Applies the next time Ferrotune connects to Spotify.");
        }
        if !fx.save {
            return Task::none();
        }
        self.save_settings()
    }

    /// Persist the settings to `config.json` (not in mock mode).
    pub(super) fn save_settings(&self) -> Task<Message> {
        if self.is_mock() {
            return Task::none();
        }
        let settings = self.settings.clone();
        let path = self.config.config_path();
        Task::perform(
            async move {
                tokio::task::spawn_blocking(move || settings.save(&path))
                    .await
                    .map_err(|_| crate::error::Error::Cancelled)?
                    .map_err(|e| crate::error::Error::Net(format!("config.json: {e}")))
            },
            |r| Message::Done(Done::SettingsSaved(r)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_changes_restart_the_engine_only_when_different() {
        let mut s = Settings::default();
        let e = apply(&mut s, &SettingChange::Quality(Bitrate::High));
        assert!(e.restart_engine && e.save);
        assert_eq!(s.bitrate, Bitrate::High);
        assert!(!apply(&mut s, &SettingChange::Quality(Bitrate::High)).restart_engine);
        assert!(apply(&mut s, &SettingChange::Normalisation(true)).restart_engine);
        let e = apply(&mut s, &SettingChange::Theme(ThemeChoice::Light));
        assert!(!e.restart_engine && e.save);
        assert_eq!(s.theme, ThemeChoice::Light);
    }

    #[test]
    fn device_name_is_bounded_and_saved_on_commit() {
        let mut s = Settings::default();
        let long = format!("{}\u{7}", "x".repeat(200));
        assert!(!apply(&mut s, &SettingChange::DeviceName(long)).save);
        assert_eq!(s.device_name.len(), MAX_DEVICE_NAME);
        apply(&mut s, &SettingChange::DeviceName("  Desk  ".into()));
        let e = apply(&mut s, &SettingChange::DeviceNameCommit);
        assert!(e.save && e.next_session);
        assert_eq!(s.device_name, "Desk");
        assert!(apply(&mut s, &SettingChange::Autoplay(false)).next_session);
        assert!(!s.autoplay);
        assert!(!apply(&mut s, &SettingChange::ClearCache).save);
    }

    #[test]
    fn tray_settings_default_and_save() {
        let mut s = Settings::default();
        assert!(s.close_to_tray && !s.start_minimized);
        let e = apply(&mut s, &SettingChange::CloseToTray(false));
        assert!(e.save && !e.restart_engine && !s.close_to_tray);
        assert!(apply(&mut s, &SettingChange::StartMinimized(true)).save);
        assert!(s.start_minimized);
        assert!(apply(&mut s, &SettingChange::AutoUpdateCheck(false)).save);
        assert!(!s.auto_update_check);
        // The autostart file is the source of truth, not config.json.
        assert!(!apply(&mut s, &SettingChange::Autostart(true)).save);
    }
}
