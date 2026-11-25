//! App side of the in-app updater (`crate::update`): state, messages,
//! the timed-check subscription, and the tasks that check, install and
//! restart. The views are in `ui/update.rs`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iced::{Subscription, Task};

use super::{App, Message};
use crate::update::install::{Installed, Stage};
use crate::update::method::Layout;
use crate::update::{
    self, CheckOutcome, Endpoints, InstallMethod, Release, UpdateError, schedule, state, version,
};

/// Buttons and toggles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateUi {
    Check,
    Install,
    Cancel,
    /// Banner [x]: never offer this version again.
    Dismiss,
    OpenReleaseNotes,
    CopyCommand,
    Restart,
    Later,
    /// Hide a failure message.
    ClearError,
}

/// Results and timer ticks.
#[derive(Debug, Clone)]
pub enum UpdateEvent {
    Tick,
    Method(InstallMethod),
    Checked {
        manual: bool,
        result: Result<Checked, UpdateError>,
    },
    Progress(Stage),
    Finished(Result<Installed, UpdateError>),
    Saved,
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checked {
    /// The latest installable release (newer or not).
    pub release: Option<Release>,
    /// The version dismissed from the banner, from `update.json`.
    pub dismissed: Option<String>,
}

/// The install pipeline.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Working(Stage),
    Installed(Installed),
    Failed(String),
}

/// Result of the last check, for Settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckStatus {
    UpToDate,
    Available(String),
    Failed(String),
}

#[derive(Debug, Default)]
pub struct UpdaterState {
    pub ends: Endpoints,
    /// Timed checks may run in this process (not mock / screenshot /
    /// debug, unless `FERRO_UPDATE_FORCE=1`).
    pub auto_allowed: bool,
    pub method: Option<InstallMethod>,
    /// The newest release found, when newer than this binary.
    pub available: Option<Release>,
    pub dismissed: Option<String>,
    pub checking: bool,
    pub last_check: Option<CheckStatus>,
    pub phase: Phase,
    /// "Later" after an install: the banner hides, Settings keeps the
    /// restart button.
    pub restart_later: bool,
    abort: Option<iced::task::Handle>,
    backoff_until: Option<Instant>,
}

impl UpdaterState {
    pub fn new(auto_allowed: bool) -> Self {
        Self {
            ends: Endpoints::from_env(),
            auto_allowed,
            ..Self::default()
        }
    }

    /// The version the banner is about, if it should show.
    pub fn banner_version(&self) -> Option<&str> {
        let r = self.available.as_ref()?;
        match &self.phase {
            Phase::Installed(_) if self.restart_later => None,
            Phase::Idle if self.dismissed.as_deref() == Some(r.version.as_str()) => None,
            _ => Some(r.version.as_str()),
        }
    }

    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Working(_))
    }

    /// Apply a check result. Returns true when a newer release is offered.
    pub fn apply_check(&mut self, manual: bool, r: Result<Checked, UpdateError>, now: Instant) {
        self.checking = false;
        match r {
            Ok(c) => {
                if c.dismissed.is_some() {
                    self.dismissed = c.dismissed;
                }
                let newer = c
                    .release
                    .filter(|r| version::is_newer(update::CURRENT_VERSION, &r.tag));
                self.last_check = Some(match &newer {
                    Some(r) => CheckStatus::Available(r.version.clone()),
                    None => CheckStatus::UpToDate,
                });
                if let Phase::Failed(_) = self.phase
                    && newer.as_ref().map(|r| &r.version)
                        != self.available.as_ref().map(|r| &r.version)
                {
                    self.phase = Phase::Idle;
                }
                if !matches!(self.phase, Phase::Installed(_)) {
                    self.available = newer;
                }
            }
            Err(e) => {
                if let UpdateError::RateLimited(s) = e {
                    self.backoff_until = now.checked_add(Duration::from_secs(s));
                }
                tracing::info!(error = %e, manual, "update check failed");
                // Timed checks fail silently; keep the previous result.
                if manual {
                    self.last_check = Some(CheckStatus::Failed(e.user_message()));
                }
            }
        }
    }

    /// Mock data for `--screenshot update-banner` / `updates`.
    pub fn screenshot_state(&mut self) {
        let v = "0.9.0".to_owned();
        self.available = Some(Release {
            tag: format!("v{v}"),
            version: v.clone(),
            html_url: update::releases_page(),
            assets: Vec::new(),
        });
        self.last_check = Some(CheckStatus::Available(v));
        self.method = Some(InstallMethod::InstallScript(Layout {
            bin_dir: "/home/user/.local/bin".into(),
            data_dir: "/home/user/.local/share".into(),
        }));
    }

    fn backing_off(&self, now: Instant) -> bool {
        self.backoff_until.is_some_and(|t| now < t)
    }
}

/// Timer for automatic checks: first after 10-20 s, then every 6 h ± 30 min.
fn ticks() -> impl futures::Stream<Item = ()> {
    futures::stream::unfold(true, |first| async move {
        let u: f64 = rand::random();
        let d = if first {
            schedule::first_delay(u)
        } else {
            schedule::next_delay(u)
        };
        tokio::time::sleep(d).await;
        Some(((), false))
    })
}

/// Look up the latest release, keeping `update.json` in step.
async fn check_task(ends: Endpoints, dir: PathBuf) -> Result<Checked, UpdateError> {
    let d = dir.clone();
    let mut persisted = tokio::task::spawn_blocking(move || state::Persisted::load(&d))
        .await
        .map_err(|_| UpdateError::Cancelled)?;
    let etag = persisted.release.as_ref().and(persisted.etag.clone());
    let now = crate::net::soloist::install::now_unix();
    let release = match update::release::check(&ends, etag.as_deref(), now).await? {
        CheckOutcome::NotModified => persisted.release.clone(),
        CheckOutcome::Latest { release, etag } => {
            persisted.etag = etag;
            persisted.release = release.clone();
            let p = persisted.clone();
            let saved = tokio::task::spawn_blocking(move || p.save(&dir)).await;
            if !matches!(saved, Ok(Ok(()))) {
                tracing::warn!("update: could not save update.json");
            }
            release
        }
    };
    Ok(Checked {
        release,
        dismissed: persisted.dismissed,
    })
}

/// Progress callback that forwards at most one message per percent.
struct Throttle {
    tx: futures::channel::mpsc::Sender<UpdateEvent>,
    last: Option<(Stage, u64)>,
}

fn install_stream(
    ends: Endpoints,
    release: Release,
    layout: Layout,
) -> impl futures::Stream<Item = UpdateEvent> {
    iced::stream::channel(
        16,
        async move |mut tx: futures::channel::mpsc::Sender<UpdateEvent>| {
            use futures::SinkExt;
            let throttle = Arc::new(Mutex::new(Throttle {
                tx: tx.clone(),
                last: None,
            }));
            let progress = move |s: Stage| {
                let Ok(mut t) = throttle.lock() else { return };
                let pct = match s {
                    Stage::Downloading {
                        received,
                        total: Some(total),
                    } if total > 0 => received.saturating_mul(100) / total,
                    Stage::Downloading { received, .. } => received / (1024 * 1024),
                    _ => 0,
                };
                let same = t.last.is_some_and(|(prev, p)| {
                    std::mem::discriminant(&prev) == std::mem::discriminant(&s) && p == pct
                });
                if !same {
                    t.last = Some((s, pct));
                    let _ = t.tx.try_send(UpdateEvent::Progress(s));
                }
            };
            let r = update::install::run(&ends, &release, &layout, &progress).await;
            if r.is_ok() {
                update::install::refresh_desktop_caches(&layout);
            }
            let _ = tx.send(UpdateEvent::Finished(r)).await;
        },
    )
}

impl App {
    /// The timed-check subscription (none while turned off).
    pub(super) fn update_subscription(&self) -> Option<Subscription<Message>> {
        (self.updater.auto_allowed && self.settings.auto_update_check)
            .then(|| Subscription::run(ticks).map(|()| Message::Update(UpdateEvent::Tick)))
    }

    /// Boot: detect the install method off the UI thread (not in mock).
    pub(super) fn update_boot(&mut self) -> Task<Message> {
        if self.is_mock() {
            // (Screenshots may already have set one.)
            self.updater.method.get_or_insert(InstallMethod::Unmanaged(
                update::method::Unmanaged::Development,
            ));
            return Task::none();
        }
        Task::perform(
            async {
                tokio::task::spawn_blocking(update::method::detect_current)
                    .await
                    .unwrap_or(InstallMethod::Unmanaged(update::method::Unmanaged::Unknown))
            },
            |m| Message::Update(UpdateEvent::Method(m)),
        )
    }

    fn start_check(&mut self, manual: bool) -> Task<Message> {
        if self.updater.checking || self.updater.busy() {
            return Task::none();
        }
        if self.is_mock() {
            if manual {
                self.updater.last_check = Some(CheckStatus::Failed(
                    "Update checks are off in mock mode.".into(),
                ));
            }
            return Task::none();
        }
        if self.updater.backing_off(self.now) {
            if manual {
                self.updater.last_check = Some(CheckStatus::Failed(
                    UpdateError::RateLimited(0).user_message(),
                ));
            }
            return Task::none();
        }
        self.updater.checking = true;
        let (ends, dir) = (self.updater.ends.clone(), self.config.state_dir.clone());
        Task::perform(check_task(ends, dir), move |result| {
            Message::Update(UpdateEvent::Checked { manual, result })
        })
    }

    fn open_url(&mut self, url: &str) {
        if self.is_mock() || cfg!(test) {
            return;
        }
        if let Err(e) = crate::auth::oauth::open_browser(url) {
            tracing::warn!(error = %e, "could not open the release page");
            self.show_toast("Could not open a browser.");
        }
    }

    fn release_page(&self) -> String {
        self.updater
            .available
            .as_ref()
            .map_or_else(update::releases_page, |r| r.html_url.clone())
    }

    pub(super) fn on_update_ui(&mut self, u: UpdateUi) -> Task<Message> {
        match u {
            UpdateUi::Check => self.start_check(true),
            UpdateUi::Install => self.start_install(),
            UpdateUi::Cancel => {
                if let Some(h) = self.updater.abort.take() {
                    h.abort();
                }
                if matches!(self.updater.phase, Phase::Working(_)) {
                    self.updater.phase = Phase::Idle;
                }
                Task::none()
            }
            UpdateUi::Dismiss => {
                let Some(v) = self.updater.available.as_ref().map(|r| r.version.clone()) else {
                    return Task::none();
                };
                if matches!(self.updater.phase, Phase::Failed(_)) {
                    self.updater.phase = Phase::Idle;
                }
                self.updater.dismissed = Some(v.clone());
                if self.is_mock() {
                    return Task::none();
                }
                let dir = self.config.state_dir.clone();
                Task::perform(
                    async move {
                        let r = tokio::task::spawn_blocking(move || state::dismiss(&dir, &v)).await;
                        if !matches!(r, Ok(Ok(()))) {
                            tracing::warn!("update: could not save the dismissed version");
                        }
                    },
                    |()| Message::Update(UpdateEvent::Saved),
                )
            }
            UpdateUi::OpenReleaseNotes => {
                let url = self.release_page();
                self.open_url(&url);
                Task::none()
            }
            UpdateUi::CopyCommand => {
                self.show_toast("Command copied.");
                iced::clipboard::write(update::cargo_command())
            }
            UpdateUi::Restart => self.restart_after_update(),
            UpdateUi::Later => {
                self.updater.restart_later = true;
                Task::none()
            }
            UpdateUi::ClearError => {
                if matches!(self.updater.phase, Phase::Failed(_)) {
                    self.updater.phase = Phase::Idle;
                }
                Task::none()
            }
        }
    }

    fn start_install(&mut self) -> Task<Message> {
        if self.updater.busy() || self.is_mock() {
            return Task::none();
        }
        let Some(release) = self.updater.available.clone() else {
            return Task::none();
        };
        let Some(InstallMethod::InstallScript(layout)) = self.updater.method.clone() else {
            // Not ours to replace: show the release page instead.
            let url = self.release_page();
            self.open_url(&url);
            return Task::none();
        };
        self.updater.phase = Phase::Working(Stage::Downloading {
            received: 0,
            total: None,
        });
        let (task, handle) = Task::run(
            install_stream(self.updater.ends.clone(), release, layout),
            Message::Update,
        )
        .abortable();
        self.updater.abort = Some(handle);
        task
    }

    /// Release the instance lock, start the new binary, quit normally.
    fn restart_after_update(&mut self) -> Task<Message> {
        let Phase::Installed(installed) = &self.updater.phase else {
            return Task::none();
        };
        if self.is_mock() {
            return Task::none();
        }
        let current: Vec<String> = std::env::args().skip(1).collect();
        let args = update::restart::restart_args(&current, !self.win.visible);
        crate::tray::instance::release();
        match update::restart::spawn(&installed.binary, &args) {
            Ok(()) => {
                tracing::info!("update: restarting into the new version");
                self.quit()
            }
            Err(e) => {
                tracing::warn!(error = %e, "update: could not start the new version");
                self.updater.phase = Phase::Failed(
                    "Could not start the new version. Restart Ferrotune yourself.".into(),
                );
                Task::none()
            }
        }
    }

    pub(super) fn on_update_event(&mut self, ev: UpdateEvent) -> Task<Message> {
        match ev {
            UpdateEvent::Tick => {
                if self.updater.auto_allowed && self.settings.auto_update_check {
                    return self.start_check(false);
                }
                Task::none()
            }
            UpdateEvent::Method(m) => {
                tracing::debug!(method = m.label(), "update: install method");
                self.updater.method = Some(m);
                Task::none()
            }
            UpdateEvent::Checked { manual, result } => {
                self.updater.apply_check(manual, result, self.now);
                Task::none()
            }
            UpdateEvent::Progress(s) => {
                if matches!(self.updater.phase, Phase::Working(_)) {
                    self.updater.phase = Phase::Working(s);
                }
                Task::none()
            }
            UpdateEvent::Finished(r) => {
                self.updater.abort = None;
                self.updater.phase = match r {
                    Ok(i) => Phase::Installed(i),
                    Err(UpdateError::Cancelled) => Phase::Idle,
                    Err(e) => {
                        tracing::warn!(error = %e, "update failed");
                        Phase::Failed(e.user_message())
                    }
                };
                Task::none()
            }
            UpdateEvent::Saved => Task::none(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Options, Ui, ui};
    use crate::config::Config;

    fn release(v: &str) -> Release {
        Release {
            tag: format!("v{v}"),
            version: v.into(),
            html_url: format!("https://github.com/FerroApps/Ferrotune/releases/tag/v{v}"),
            assets: Vec::new(),
        }
    }

    fn checked(v: &str, dismissed: Option<&str>) -> Message {
        Message::Update(UpdateEvent::Checked {
            manual: false,
            result: Ok(Checked {
                release: Some(release(v)),
                dismissed: dismissed.map(str::to_owned),
            }),
        })
    }

    fn real_app() -> App {
        let mut cfg = Config::for_tests();
        cfg.mock = false;
        let (app, _) = App::boot(cfg, Options::default());
        app
    }

    #[test]
    fn newer_release_shows_the_banner_and_dismissal_sticks() {
        let mut app = real_app();
        assert!(app.updater.banner_version().is_none());
        let _ = app.update(checked("999.0.0", None));
        assert_eq!(app.updater.banner_version(), Some("999.0.0"));
        assert_eq!(
            app.updater.last_check,
            Some(CheckStatus::Available("999.0.0".into()))
        );
        let _ = app.update(ui(Ui::Update(UpdateUi::Dismiss)));
        assert!(app.updater.banner_version().is_none());
        assert_eq!(app.updater.dismissed.as_deref(), Some("999.0.0"));
        // The next check finds the same version: still hidden.
        let _ = app.update(checked("999.0.0", Some("999.0.0")));
        assert!(app.updater.banner_version().is_none());
        // A newer one shows again.
        let _ = app.update(checked("999.0.1", Some("999.0.0")));
        assert_eq!(app.updater.banner_version(), Some("999.0.1"));
    }

    #[test]
    fn older_or_same_release_is_up_to_date() {
        let mut app = real_app();
        let _ = app.update(checked(update::CURRENT_VERSION, None));
        assert!(app.updater.available.is_none());
        assert_eq!(app.updater.last_check, Some(CheckStatus::UpToDate));
        let _ = app.update(checked("0.0.1", None));
        assert!(app.updater.banner_version().is_none());
    }

    #[test]
    fn manual_check_starts_and_reports_failures() {
        let mut app = real_app();
        let _ = app.update(ui(Ui::Update(UpdateUi::Check)));
        assert!(app.updater.checking);
        // A second click while checking does nothing.
        let _ = app.update(ui(Ui::Update(UpdateUi::Check)));
        let _ = app.update(Message::Update(UpdateEvent::Checked {
            manual: true,
            result: Err(UpdateError::RateLimited(600)),
        }));
        assert!(!app.updater.checking);
        assert!(matches!(
            app.updater.last_check,
            Some(CheckStatus::Failed(_))
        ));
        // Backing off: the next click does not hit GitHub.
        let _ = app.update(ui(Ui::Update(UpdateUi::Check)));
        assert!(!app.updater.checking);
    }

    #[test]
    fn timed_check_failures_are_silent() {
        let mut app = real_app();
        let _ = app.update(Message::Update(UpdateEvent::Checked {
            manual: false,
            result: Err(UpdateError::Net("connect".into())),
        }));
        assert!(app.updater.last_check.is_none());
    }

    #[test]
    fn nothing_runs_in_mock() {
        let (mut app, _) = App::boot(Config::for_tests(), Options::default());
        assert!(!app.updater.auto_allowed || schedule::force_from_env());
        let _ = app.update(ui(Ui::Update(UpdateUi::Check)));
        assert!(!app.updater.checking);
        assert!(matches!(
            app.updater.last_check,
            Some(CheckStatus::Failed(_))
        ));
        let _ = app.update(Message::Update(UpdateEvent::Tick));
        assert!(!app.updater.checking);
        app.updater.available = Some(release("999.0.0"));
        let _ = app.update(ui(Ui::Update(UpdateUi::Install)));
        assert_eq!(app.updater.phase, Phase::Idle);
    }

    #[test]
    fn install_needs_an_install_sh_layout() {
        let mut app = real_app();
        let _ = app.update(checked("999.0.0", None));
        // Would open the release page; never starts a download.
        app.updater.method = Some(InstallMethod::Cargo);
        let _ = app.update(ui(Ui::Update(UpdateUi::Install)));
        assert_eq!(app.updater.phase, Phase::Idle);
        app.updater.method = None;
        let _ = app.update(ui(Ui::Update(UpdateUi::Install)));
        assert_eq!(app.updater.phase, Phase::Idle);
    }

    #[test]
    fn progress_failure_and_restart_later() {
        let mut app = real_app();
        let _ = app.update(checked("999.0.0", None));
        app.updater.phase = Phase::Working(Stage::Verifying);
        let _ = app.update(Message::Update(UpdateEvent::Finished(Err(
            UpdateError::Checksum,
        ))));
        assert!(matches!(app.updater.phase, Phase::Failed(_)));
        assert_eq!(app.updater.banner_version(), Some("999.0.0"), "error shows");
        app.updater.phase = Phase::Working(Stage::Installing);
        let _ = app.update(Message::Update(UpdateEvent::Finished(Ok(Installed {
            version: "999.0.0".into(),
            binary: "/x/ferrotune".into(),
        }))));
        assert!(matches!(app.updater.phase, Phase::Installed(_)));
        assert!(app.updater.banner_version().is_some());
        let _ = app.update(ui(Ui::Update(UpdateUi::Later)));
        assert!(app.updater.banner_version().is_none());
        // A check after installing keeps the installed state.
        let _ = app.update(checked("999.0.0", None));
        assert!(matches!(app.updater.phase, Phase::Installed(_)));
    }

    #[test]
    fn cancel_returns_to_idle() {
        let mut app = real_app();
        let _ = app.update(checked("999.0.0", None));
        app.updater.phase = Phase::Working(Stage::Downloading {
            received: 5,
            total: Some(10),
        });
        let _ = app.update(ui(Ui::Update(UpdateUi::Cancel)));
        assert_eq!(app.updater.phase, Phase::Idle);
        assert_eq!(app.updater.banner_version(), Some("999.0.0"));
    }

    #[test]
    fn auto_check_setting_controls_the_timer() {
        let mut app = real_app();
        app.updater.auto_allowed = true;
        assert!(app.update_subscription().is_some());
        let _ = app.update(ui(Ui::Setting(
            crate::app::settings::SettingChange::AutoUpdateCheck(false),
        )));
        assert!(!app.settings.auto_update_check);
        assert!(app.update_subscription().is_none());
        // Ticks that were already queued do nothing.
        let _ = app.update(Message::Update(UpdateEvent::Tick));
        assert!(!app.updater.checking);
        // A manual check still works.
        let _ = app.update(ui(Ui::Update(UpdateUi::Check)));
        assert!(app.updater.checking);
    }
}
