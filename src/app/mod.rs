//! Application state and the single `update` mutation point (Elm style).
//! No I/O happens here directly: side effects are returned as `Task`s that
//! call into `net`, `store` and `auth`.

mod background;
pub mod edit;
pub mod images;
pub mod menu;
pub mod message;
pub mod meta;
pub mod playlists;
pub mod settings;
pub mod shortcuts;
pub mod soloist;
pub mod state;
pub mod updates;
pub mod window;

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iced::{Subscription, Task};

pub use message::{Done, Message, Net, SessionReady, TrackSource, Ui, ui};
use state::{
    Bounded, Connection, LibraryFilter, Nav, PlayContext, PlayStatus, Playback, Previous, Queue,
    Repeat, Route, SearchFilter, backoff_delay,
};

use crate::auth::oauth;
use crate::auth::secrets::{
    KEY_LEGACY_OAUTH, KEY_LIBRESPOT, KeyringStore, MemoryStore, SecretStore,
};
use crate::auth::{AuthEvent, AuthState};
use crate::config::{self, Config};
use crate::error::{Error, Result};
use crate::net::DataSource;
use crate::net::backend::Backend;
use crate::net::client::{InternalClient, http_client};
use crate::net::model::{
    Album, AlbumRef, ArtistPage, ArtistRef, HomeFeed, Image, Library, Lyrics, Page, Playlist,
    SearchResults, Track, TrackList, User, pick_image,
};
use crate::net::mpris::{
    MediaCommand, MediaEvent, MediaHandle, MediaLoop, MediaSnapshot, MediaStatus, MediaTrack,
};
use crate::net::playback::{
    self, EngineKind, PlaybackCommand, PlaybackEvent, PlaybackHandle, RemoteCommand, RemoteEvent,
    RemoteTrack,
};
use crate::net::session::{
    self, ConnectOptions, LiveAuth, SessionCredentials, SessionOptions, SpotifySession,
    StoredCredentials,
};
use crate::store::{Kind, Store};

/// Max images requested per screen (rows beyond this load lazily later).
const MAX_IMAGE_REQUESTS: usize = 80;
const TOAST_SECS: u64 = 5;
const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct ScreenshotPlan {
    pub screen: String,
    pub out: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub screenshot: Option<ScreenshotPlan>,
    /// `--minimized`: start with only the tray icon, no window.
    pub minimized: bool,
    pub started: Option<Instant>,
}

#[derive(Debug, Default)]
pub struct SearchState {
    pub query: String,
    pub results: Option<SearchResults>,
    pub loading: bool,
    pub filter: SearchFilter,
    pub seq: u64,
    pub error: Option<String>,
}

/// Who plays while [`App::remote`] is set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteKind {
    /// Another device casts to our Spotify Connect device.
    Cast,
    /// We control another device (phone, desktop app, speaker).
    Device { id: String, name: String },
}

/// Playback that is not the local queue: a cast to us, or another device
/// we control through connect-state.
#[derive(Debug, Clone)]
pub struct RemotePlayback {
    pub kind: RemoteKind,
    pub track: Option<Track>,
    pub playback: Playback,
    pub shuffle: bool,
    pub repeat: Repeat,
}

impl RemotePlayback {
    pub fn device_name(&self) -> Option<&str> {
        match &self.kind {
            RemoteKind::Device { name, .. } => Some(name),
            RemoteKind::Cast => None,
        }
    }

    fn device_id(&self) -> Option<&str> {
        match &self.kind {
            RemoteKind::Device { id, .. } => Some(id),
            RemoteKind::Cast => None,
        }
    }
}

/// A persistent playback problem shown as a banner (not a toast).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackBlock {
    pub reason: crate::net::audio_diag::UnavailableReason,
    pub message: String,
}

/// Map a track list to the Connect context the official clients use.
pub fn context_uri(src: &TrackSource, user: &str, first_track: Option<&str>) -> Option<String> {
    Some(match src {
        TrackSource::Playlist(id) => format!("spotify:playlist:{id}"),
        TrackSource::Album(id) => format!("spotify:album:{id}"),
        TrackSource::ArtistTop(id) => format!("spotify:artist:{id}"),
        TrackSource::Liked if !user.is_empty() => format!("spotify:user:{user}:collection"),
        TrackSource::Liked => return None,
        TrackSource::Search => first_track?.to_owned(),
    })
}

pub struct App {
    pub config: Arc<Config>,
    /// Settings screen values (persisted in `config.json`).
    pub settings: config::Settings,
    pub secrets: Arc<dyn SecretStore>,
    pub store: Store,
    pub source: Option<DataSource>,
    pub auth: AuthState,
    pub user: Option<User>,
    pub connection: Connection,
    pub nav: Nav,
    pub library: Library,
    pub library_filter: LibraryFilter,
    pub home: Option<HomeFeed>,
    pub playlists: Bounded<(Playlist, TrackList)>,
    pub albums: Bounded<Album>,
    pub artists: Bounded<ArtistPage>,
    pub liked: Option<TrackList>,
    pub liked_ids: HashSet<String>,
    pub loading: HashSet<Route>,
    pub errors: HashMap<Route, String>,
    pub search: SearchState,
    pub queue: Queue,
    pub playback: Playback,
    pub engine_kind: EngineKind,
    pub engine: Option<PlaybackHandle>,
    pub session: Option<SpotifySession>,
    pub media: Option<MediaHandle>,
    pub mpris_enabled: bool,
    last_media: Option<MediaSnapshot>,
    pub images: images::ImageCache,
    image_requests: HashSet<String>,
    pub show_queue: bool,
    pub show_lyrics: bool,
    /// Lyrics for a track id: `None` while loading, `Some(None)` = none.
    pub lyrics: Option<(String, Option<Option<Lyrics>>)>,
    /// Playback on another device: a cast to us, or a device we control.
    pub remote: Option<RemotePlayback>,
    /// The user's Spotify Connect devices (connect-state cluster).
    pub cluster: Option<crate::net::connect_ctl::ClusterState>,
    /// Last device that played something: `(id, name)` (kv `last_device`).
    pub last_device: Option<(String, String)>,
    pub device_picker: bool,
    /// Account menu under the avatar is open.
    pub user_menu: bool,
    /// Spotify refused the audio key: this computer cannot play, so
    /// playback goes to a Connect device instead.
    pub local_denied: bool,
    /// One persistent playback error (instead of a looping toast).
    pub playback_block: Option<PlaybackBlock>,
    /// Spotify Soloist, the official player, as the local audio engine.
    pub soloist: soloist::SoloistState,
    skip_guard: state::SkipGuard,
    /// Playlist owner display names by username (`meta.rs`).
    pub owner_names: HashMap<String, String>,
    owner_requested: HashSet<String>,
    fill_requested: HashSet<String>,
    /// Loaded profile pages by username.
    pub profiles: HashMap<String, crate::net::model::UserProfile>,
    /// Local cache size (Settings), when measured.
    pub cache_bytes: Option<u64>,
    /// `session.username()` of the current librespot session.
    session_user: String,
    /// Tasks started from helpers that cannot return one (run by `update`).
    pending: Vec<Task<Message>>,
    /// Reusable credentials are in the keyring (silent login possible).
    has_stored_credentials: bool,
    /// Stable librespot device id (kv `device_id`), so Spotify Connect does
    /// not list a new "Ferrotune" device after every start.
    device_id: String,
    pub hovered_row: Option<String>,
    /// Open context menu (right click).
    pub menu: Option<menu::ContextMenu>,
    /// Open modal dialog (shortcuts help, rename, delete confirmation).
    pub dialog: Option<menu::Dialog>,
    /// Window size, to keep menus on screen.
    pub window_size: iced::Size,
    /// Row being dragged in an editable playlist: `(row key, index)`.
    pub drag: Option<(String, usize)>,
    pub selected_row: Option<String>,
    pub toast: Option<(String, Instant)>,
    pub now: Instant,
    login_abort: Option<iced::task::Handle>,
    loading_more: bool,
    audio_attempt: u32,
    /// The engine lost the loaded track (session died): reload on play.
    reload_on_play: bool,
    /// Bumped on every seek so MPRIS emits `Seeked`.
    seek_seq: u64,
    screenshot: Option<ScreenshotPlan>,
    started: Instant,
    pub first_frame_logged: Cell<bool>,
    /// Main-window lifecycle (visible / hidden to the tray).
    pub win: window::WindowState,
    window_id: Option<iced::window::Id>,
    /// Size and position remembered while the window is closed.
    geometry: Option<window::Geometry>,
    /// Last reported window position (`None` on Wayland).
    window_pos: Option<iced::Point>,
    /// "Open at login" (the autostart file exists).
    pub autostart: bool,
    /// Whether a tray icon is requested (never for screenshots).
    tray_enabled: bool,
    /// Handle to the running tray, once it registered.
    tray_control: Option<crate::tray::Control>,
    /// In-app updater (`app/updates.rs`).
    pub updater: updates::UpdaterState,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("auth", &self.auth)
            .field("route", self.nav.current())
            .finish_non_exhaustive()
    }
}

fn store_write<T>(store: &Store, kind: Kind, id: String, value: T) -> Task<Message>
where
    T: serde::Serialize + Send + 'static,
{
    let store = store.clone();
    Task::perform(
        async move {
            let r = store
                .run(move |s| {
                    s.put(kind, &id, &value)?;
                    s.prune(crate::store::MAX_ENTITIES).map(|_| ())
                })
                .await;
            if let Err(e) = r {
                tracing::warn!(error = %e, "cache write failed");
            }
        },
        |()| Message::Done(Done::Noop),
    )
}

fn store_read<T, F>(store: &Store, kind: Kind, id: String, f: F) -> Task<Message>
where
    T: serde::de::DeserializeOwned + Send + 'static,
    F: Fn(T) -> Done + Send + 'static,
{
    let store = store.clone();
    Task::perform(
        async move { store.run(move |s| s.get::<T>(kind, &id)).await },
        move |r| match r {
            Ok(Some(v)) => Message::Done(f(v)),
            _ => Message::Done(Done::Noop),
        },
    )
}

fn session_options(
    config: &Config,
    settings: &config::Settings,
    volume: f32,
    device_id: &str,
) -> SessionOptions {
    let name = if settings.device_name.trim().is_empty() {
        config.device_name.clone()
    } else {
        settings.device_name.trim().to_owned()
    };
    SessionOptions {
        client_id: config.client_id.clone(),
        device_id: Some(device_id.to_owned()),
        autoplay: settings.autoplay,
        // With Soloist, Soloist is this computer's Connect device: never
        // announce a second "Ferrotune", but keep the controller.
        controller: settings.engine == config::AudioEngine::Soloist,
        connect: (settings.connect && settings.engine != config::AudioEngine::Soloist).then(|| {
            ConnectOptions {
                name,
                bitrate: settings.bitrate.into(),
                normalisation: settings.normalisation,
                initial_volume: volume,
            }
        }),
    }
}

/// The engine for the current settings (a change restarts it).
pub fn engine_kind(mock: bool, s: &config::Settings) -> EngineKind {
    if mock {
        EngineKind::Mock
    } else {
        EngineKind::Librespot(
            s.bitrate.into(),
            s.normalisation,
            s.engine != config::AudioEngine::Soloist,
        )
    }
}

/// Backends over the session: one identity, one client, one cache.
fn build_source(config: &Config, session: &SpotifySession, store: &Store) -> DataSource {
    let auth = Arc::new(LiveAuth::new(session.session.clone()));
    let client = Arc::new(InternalClient::new(
        http_client(),
        auth.clone(),
        config.spclient_base.clone(),
        config.pathfinder_base.clone(),
        Some(store.clone()),
    ));
    let queries = crate::net::pathfinder::Queries::load(&config.pathfinder_overrides_path());
    DataSource::Spotify(Backend::internal(
        client,
        queries,
        Some(auth),
        Some(store.clone()),
    ))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|_| Error::Cancelled)?
}

async fn save_credentials(secrets: Arc<dyn SecretStore>, creds: StoredCredentials) -> Result<()> {
    let secret = creds.to_secret()?;
    blocking(move || secrets.set(KEY_LIBRESPOT, &secret)).await
}

/// Open the session from stored credentials (no browser). Rejected
/// credentials are deleted so the next login uses the browser.
async fn connect_stored(
    secrets: Arc<dyn SecretStore>,
    opts: SessionOptions,
) -> Result<Option<SpotifySession>> {
    let s = secrets.clone();
    let Some(secret) = blocking(move || s.get(KEY_LIBRESPOT)).await? else {
        return Ok(None);
    };
    let result = match StoredCredentials::from_secret(&secret) {
        Ok(creds) => session::connect(SessionCredentials::Stored(creds), &opts).await,
        Err(e) => Err(e),
    };
    match result {
        Ok((session, creds)) => {
            save_credentials(secrets, creds).await?;
            Ok(Some(session))
        }
        Err(e) => {
            if e.is_revoked() {
                let s = secrets.clone();
                let _ = blocking(move || s.delete(KEY_LIBRESPOT)).await;
            }
            Err(e)
        }
    }
}

async fn restore_session(
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    store: Store,
    opts: SessionOptions,
) -> Result<Option<SessionReady>> {
    // Tokens of the old Web-API releases are useless now: remove them.
    let s = secrets.clone();
    let _ = blocking(move || s.delete(KEY_LEGACY_OAUTH)).await;
    let Some(session) = connect_stored(secrets, opts).await? else {
        return Ok(None);
    };
    let source = build_source(&config, &session, &store);
    let user = source.me().await.ok();
    Ok(Some(SessionReady {
        source,
        user,
        session: Some(session),
    }))
}

async fn login_flow(
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    store: Store,
    listener: std::net::TcpListener,
    state: String,
    verifier: crate::auth::secrets::Secret,
    opts: SessionOptions,
) -> Result<SessionReady> {
    let code = oauth::wait_for_callback(listener, state, LOGIN_TIMEOUT).await?;
    let http = http_client();
    let tokens = oauth::exchange_code(
        &http,
        &config.accounts_base,
        &config.client_id,
        &code,
        &config.redirect_uri(),
        &verifier,
    )
    .await?;
    // The browser token is used once, to open the session; only the
    // session's reusable credentials are kept.
    let (session, creds) =
        session::connect(SessionCredentials::AccessToken(tokens.access_token), &opts).await?;
    save_credentials(secrets, creds).await?;
    let source = build_source(&config, &session, &store);
    let user = source.me().await.ok();
    Ok(SessionReady {
        source,
        user,
        session: Some(session),
    })
}

/// Reconnect after a lost session, with the stored credentials.
async fn reconnect_session(
    secrets: Arc<dyn SecretStore>,
    opts: SessionOptions,
) -> Result<SpotifySession> {
    connect_stored(secrets, opts)
        .await?
        .ok_or_else(|| Error::revoked(crate::auth::SILENT_LOGIN_REJECTED))
}

/// Keyboard shortcuts and window resizes. Key presses a widget handled
/// (typing in the search box) are ignored.
fn window_event(
    event: iced::Event,
    status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    match event {
        iced::Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. })
            if status == iced::event::Status::Ignored =>
        {
            shortcuts::map(&key, modifiers).map(ui)
        }
        iced::Event::Window(iced::window::Event::Resized(size)) => {
            Some(ui(Ui::WindowResized(size)))
        }
        iced::Event::Window(iced::window::Event::Moved(p)) => Some(ui(Ui::WindowMoved(p))),
        iced::Event::Window(iced::window::Event::CloseRequested) => Some(ui(Ui::CloseRequested)),
        _ => None,
    }
}

fn mouse_up_event(
    event: iced::Event,
    _status: iced::event::Status,
    _window: iced::window::Id,
) -> Option<Message> {
    matches!(
        event,
        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left
        ))
    )
    .then(|| ui(Ui::MouseUp))
}

fn playback_stream(kind: &EngineKind) -> impl futures::Stream<Item = PlaybackEvent> + use<> {
    playback::events(*kind)
}

fn soloist_stream(
    kind: &crate::net::soloist::SoloistKind,
) -> impl futures::Stream<Item = crate::net::soloist::SoloistEvent> + use<> {
    crate::net::soloist::events(kind.clone())
}

fn media_stream() -> impl futures::Stream<Item = MediaEvent> {
    crate::net::mpris::events()
}

impl App {
    pub fn boot(config: Config, opts: Options) -> (Self, Task<Message>) {
        let config = Arc::new(config);
        let mock = config.mock;
        let secrets: Arc<dyn SecretStore> = if mock {
            Arc::new(MemoryStore::new())
        } else {
            Arc::new(KeyringStore::default())
        };
        let mut toast = None;
        let store = if mock {
            Store::in_memory()
        } else {
            Store::open(&config.db_path())
        }
        .or_else(|e| {
            tracing::warn!(error = %e, "falling back to an in-memory cache");
            toast = Some(e.user_message());
            Store::in_memory()
        });
        let store = match store {
            Ok(s) => s,
            Err(e) => {
                // Opening an in-memory database cannot realistically fail;
                // if it does, there is nothing sensible to run.
                tracing::error!(error = %e, "no SQLite available");
                std::process::exit(1);
            }
        };

        // First frame comes from the cache: no network on the critical path.
        let library: Library = store
            .get(Kind::Library, "me")
            .ok()
            .flatten()
            .unwrap_or_default();
        let home: Option<HomeFeed> = store.get(Kind::Home, "me").ok().flatten();
        let cached_user: Option<User> = store.get(Kind::User, "me").ok().flatten();
        let setting = |k: &str| store.setting(k).ok().flatten();
        let mut playback_state = Playback::default();
        if let Some(v) = setting("volume").and_then(|v| v.parse::<f32>().ok()) {
            playback_state.volume = v.clamp(0.0, 1.0);
        }
        let mut queue = Queue::default();
        queue.shuffle = setting("shuffle").as_deref() == Some("1");
        queue.repeat = setting("repeat")
            .map(|r| Repeat::parse(&r))
            .unwrap_or_default();
        let start_route = setting("route")
            .and_then(|r| Route::from_key(&r))
            .unwrap_or(Route::Home);

        let device_id = load_device_id(&store);
        let last_device = setting("last_device").and_then(|v| {
            let (id, name) = v.split_once('\n')?;
            Some((id.to_owned(), name.to_owned()))
        });
        let screenshot = opts.screenshot.clone();
        let tray_enabled = screenshot.is_none();
        let (win, first_window) = window::WindowState::boot(
            tray_enabled,
            opts.minimized || config.settings.start_minimized,
            config.settings.close_to_tray,
        );
        let mut app = Self {
            auth: AuthState::initial(),
            engine_kind: engine_kind(mock, &config.settings),
            settings: config.settings.clone(),
            mpris_enabled: screenshot.is_none() && cfg!(target_os = "linux"),
            config,
            secrets,
            store,
            source: None,
            user: cached_user,
            connection: Connection::Disconnected,
            nav: Nav::new(start_route),
            library,
            library_filter: LibraryFilter::All,
            home,
            playlists: Bounded::new(state::DETAIL_CACHE),
            albums: Bounded::new(state::DETAIL_CACHE),
            artists: Bounded::new(state::DETAIL_CACHE),
            liked: None,
            liked_ids: HashSet::new(),
            loading: HashSet::new(),
            errors: HashMap::new(),
            search: SearchState::default(),
            queue,
            playback: playback_state,
            engine: None,
            session: None,
            media: None,
            last_media: None,
            images: images::ImageCache::new(images::DEFAULT_BUDGET),
            image_requests: HashSet::new(),
            show_queue: false,
            show_lyrics: false,
            lyrics: None,
            remote: None,
            cluster: None,
            last_device,
            device_picker: false,
            user_menu: false,
            local_denied: false,
            playback_block: None,
            soloist: soloist::SoloistState::default(),
            skip_guard: state::SkipGuard::default(),
            owner_names: HashMap::new(),
            owner_requested: HashSet::new(),
            fill_requested: HashSet::new(),
            profiles: HashMap::new(),
            cache_bytes: None,
            session_user: String::new(),
            pending: Vec::new(),
            has_stored_credentials: false,
            device_id,
            hovered_row: None,
            menu: None,
            dialog: None,
            window_size: iced::Size::new(1280.0, 800.0),
            drag: None,
            selected_row: None,
            toast: toast.map(|t| (t, Instant::now())),
            now: Instant::now(),
            login_abort: None,
            loading_more: false,
            audio_attempt: 0,
            reload_on_play: false,
            seek_seq: 0,
            screenshot,
            started: opts.started.unwrap_or_else(Instant::now),
            first_frame_logged: Cell::new(false),
            win,
            window_id: None,
            geometry: None,
            window_pos: None,
            autostart: false,
            tray_enabled,
            tray_control: None,
            updater: updates::UpdaterState::new(crate::update::schedule::auto_allowed(
                mock,
                opts.screenshot.is_some(),
                cfg!(debug_assertions),
                crate::update::schedule::force_from_env(),
            )),
        };

        let mut tasks = vec![app.run_window_action(first_window)];
        if !mock && tray_enabled {
            tasks.push(Task::perform(
                async { tokio::task::spawn_blocking(crate::tray::autostart::current).await },
                |r| Message::Done(Done::AutostartLoaded(r.unwrap_or(false))),
            ));
        }
        if mock {
            tasks.push(app.signed_in(SessionReady {
                source: DataSource::Mock,
                user: Some(crate::mock::user()),
                session: None,
            }));
            if let Some(plan) = app.screenshot.clone() {
                tasks.push(app.prepare_screenshot(&plan));
            }
        } else {
            tasks.push(app.check_keyring_and_restore());
        }
        tasks.push(app.request_images());
        tasks.push(app.update_boot());
        (app, Task::batch(tasks))
    }

    /// Fail early with setup guidance when no keyring is available,
    /// otherwise try the silent login.
    fn check_keyring_and_restore(&mut self) -> Task<Message> {
        match KeyringStore::check_available() {
            Err(e) => {
                self.auth = self.auth.apply(AuthEvent::Failed(e));
                Task::none()
            }
            Ok(()) => {
                self.auth = AuthState::Restoring;
                self.restore_task()
            }
        }
    }

    fn session_opts(&self) -> SessionOptions {
        session_options(
            &self.config,
            &self.settings,
            self.playback.effective_volume(),
            &self.device_id,
        )
    }

    fn restore_task(&self) -> Task<Message> {
        let (config, secrets, store) = (
            self.config.clone(),
            self.secrets.clone(),
            self.store.clone(),
        );
        let opts = self.session_opts();
        Task::perform(restore_session(config, secrets, store, opts), |r| {
            Message::Done(Done::Restored(r))
        })
    }

    /// The track shown in the player bar / MPRIS (Connect playback wins).
    pub fn now_track(&self) -> Option<&Track> {
        match &self.remote {
            Some(r) => r.track.as_ref(),
            None => self.queue.current.as_ref(),
        }
    }

    /// Playback state shown in the player bar / MPRIS.
    pub fn now_playback(&self) -> &Playback {
        match &self.remote {
            Some(r) => &r.playback,
            None => &self.playback,
        }
    }

    pub fn title(&self) -> String {
        match self.now_track() {
            Some(t) if self.now_playback().is_playing() => {
                format!("{} • {}", t.name, t.artist_line())
            }
            _ => config::APP_NAME.to_owned(),
        }
    }

    pub fn route(&self) -> &Route {
        self.nav.current()
    }

    pub fn is_mock(&self) -> bool {
        self.config.mock
    }

    /// Log time-to-first-frame once (FERRO_TRACE_STARTUP=1).
    pub fn mark_frame(&self) {
        if !self.first_frame_logged.get() {
            self.first_frame_logged.set(true);
            if self.config.trace_startup {
                let ms = self.started.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(first_frame_ms = format!("{ms:.1}"), "first frame");
                eprintln!("first_frame_ms={ms:.1}");
            }
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            Subscription::run_with(self.engine_kind, playback_stream)
                .map(|e| Message::Net(Net::Playback(e))),
        ];
        if self.mpris_enabled {
            subs.push(Subscription::run(media_stream).map(|e| Message::Net(Net::Media(e))));
        }
        if self.soloist_enabled() {
            subs.push(
                Subscription::run_with(self.soloist_kind(), soloist_stream)
                    .map(|e| Message::Net(Net::Soloist(e))),
            );
        }
        subs.push(iced::event::listen_with(window_event));
        if self.tray_enabled {
            subs.push(Subscription::run(crate::tray::events).map(Message::Tray));
            subs.push(
                Subscription::run(crate::tray::instance::show_requests)
                    .map(|()| Message::Ui(Ui::ShowWindow)),
            );
        }
        if self.drag.is_some() {
            subs.push(iced::event::listen_with(mouse_up_event));
        }
        if let Some(s) = self.update_subscription() {
            subs.push(s);
        }
        // The tick only redraws the progress bar and expires toasts: not needed
        // while the window is hidden to the tray.
        if self.win.visible
            && (self.now_playback().status == PlayStatus::Playing || self.toast.is_some())
        {
            subs.push(
                iced::time::every(Duration::from_millis(500)).map(|i| Message::Ui(Ui::Tick(i))),
            );
        }
        Subscription::batch(subs)
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let is_tick = matches!(message, Message::Ui(Ui::Tick(_)));
        self.now = Instant::now();
        let task = match message {
            Message::Ui(u) => self.update_ui(u),
            Message::Net(Net::Playback(e)) => self.on_playback(e),
            Message::Net(Net::Media(e)) => self.on_media(e),
            Message::Net(Net::Soloist(e)) => self.on_soloist(e),
            Message::Done(d) => self.on_done(d),
            Message::Tray(ev) => self.on_tray(ev),
            Message::Update(ev) => self.on_update_event(ev),
        };
        let pending = std::mem::take(&mut self.pending);
        let task = if pending.is_empty() {
            task
        } else {
            Task::batch(std::iter::once(task).chain(pending))
        };
        if is_tick {
            return task;
        }
        self.push_media_snapshot();
        self.sync_tray();
        Task::batch([
            task,
            self.request_images(),
            self.request_lyrics(),
            self.request_owner_names(),
            self.request_playlist_fills(),
        ])
    }

    // ----------------------------------------------------------------- auth

    fn signed_in(&mut self, ready: SessionReady) -> Task<Message> {
        let user = ready
            .user
            .clone()
            .or_else(|| self.user.clone())
            .unwrap_or_else(|| User {
                id: String::new(),
                display_name: String::new(),
                image: None,
                product: None,
            });
        self.auth = self.auth.apply(AuthEvent::Connected(user.clone()));
        self.user = Some(user);
        self.source = Some(ready.source.clone());
        self.connection = if self.is_mock() {
            Connection::Connected
        } else {
            Connection::Connecting
        };
        let mut tasks = vec![self.fetch_library(), self.fetch_home(), self.load_route()];
        if let Some(session) = ready.session {
            self.session_user = session.session.username();
            tracing::debug!(
                has_username = !self.session_user.is_empty(),
                profile_loaded = ready.user.is_some(),
                "signed in"
            );
            self.has_stored_credentials = true;
            self.audio_attempt = 0;
            self.connection = Connection::Connected;
            if let Some(h) = &self.engine {
                h.send(PlaybackCommand::Attach(session.clone()));
            }
            self.session = Some(session);
        }
        match ready.user {
            Some(u) => tasks.push(store_write(&self.store, Kind::User, "me".into(), u)),
            None => {
                let src = ready.source.clone();
                tasks.push(Task::perform(async move { src.me().await }, |r| {
                    Message::Done(Done::Profile(r))
                }));
            }
        }
        if !self.is_mock() && self.session.is_none() {
            tasks.push(self.connect_audio_task());
        }
        Task::batch(tasks)
    }

    fn connect_audio_task(&self) -> Task<Message> {
        let (secrets, opts) = (self.secrets.clone(), self.session_opts());
        Task::perform(reconnect_session(secrets, opts), |r| {
            Message::Done(Done::Audio(r))
        })
    }

    fn start_login(&mut self) -> Task<Message> {
        if self.has_stored_credentials {
            // A previous silent login failed for a transient reason (e.g.
            // offline): retry it before falling back to the browser.
            self.has_stored_credentials = false;
            self.auth = self.auth.apply(AuthEvent::StoredSessionFound);
            return self.restore_task();
        }
        let client_id = self.config.client_id.clone();
        let pkce = oauth::Pkce::generate();
        let state = oauth::random_state();
        let url = match oauth::authorize_url(
            &self.config.accounts_base,
            &client_id,
            &self.config.redirect_uri(),
            &pkce.challenge,
            &state,
            config::SCOPES,
        ) {
            Ok(u) => u,
            Err(e) => {
                self.auth = self.auth.apply(AuthEvent::Failed(e));
                return Task::none();
            }
        };
        if let Some(h) = self.login_abort.take() {
            h.abort();
        }
        // Bind first so the browser redirect can never beat the listener.
        let listener = match oauth::bind_callback(self.config.redirect_port) {
            Ok(l) => l,
            Err(e) => {
                self.auth = self.auth.apply(AuthEvent::Failed(e));
                return Task::none();
            }
        };
        if let Err(e) = oauth::open_browser(&url) {
            tracing::warn!(error = %e, "could not open browser; showing URL");
        }
        self.auth = self.auth.apply(AuthEvent::LoginStarted { url });
        let (task, handle) = Task::perform(
            login_flow(
                self.config.clone(),
                self.secrets.clone(),
                self.store.clone(),
                listener,
                state,
                pkce.verifier,
                self.session_opts(),
            ),
            |r| Message::Done(Done::LoggedIn(r)),
        )
        .abortable();
        self.login_abort = Some(handle);
        task
    }

    fn logout(&mut self) -> Task<Message> {
        if let Some(e) = &self.engine {
            e.send(PlaybackCommand::Stop);
        }
        if let Some(s) = self.session.take() {
            s.shutdown();
        }
        self.remote = None;
        self.lyrics = None;
        self.has_stored_credentials = false;
        self.source = None;
        self.user = None;
        self.session_user.clear();
        self.user_menu = false;
        self.device_picker = false;
        self.playback_block = None;
        self.local_denied = false;
        self.cluster = None;
        self.owner_names.clear();
        self.owner_requested.clear();
        self.fill_requested.clear();
        self.profiles.clear();
        self.queue.clear();
        self.playback = Playback {
            volume: self.playback.volume,
            ..Playback::default()
        };
        self.library = Library::default();
        self.home = None;
        self.playlists.clear();
        self.albums.clear();
        self.artists.clear();
        self.liked = None;
        self.liked_ids.clear();
        self.search = SearchState::default();
        self.connection = Connection::Disconnected;
        self.nav = Nav::new(Route::Home);
        self.auth = self.auth.apply(AuthEvent::LoggedOut);
        let secrets = self.secrets.clone();
        let store = self.store.clone();
        Task::perform(
            async move {
                let s = secrets.clone();
                let _ = blocking(move || s.delete(KEY_LEGACY_OAUTH)).await;
                let _ = blocking(move || secrets.delete(KEY_LIBRESPOT)).await;
                let _ = store.run(|s| s.clear()).await;
            },
            |()| Message::Done(Done::Noop),
        )
    }

    fn fail(&mut self, e: &Error) -> Task<Message> {
        tracing::warn!(error = %e, "request failed");
        if e.is_revoked() {
            let t = self.logout();
            self.auth = AuthState::SignedOut {
                error: Some(e.user_message()),
            };
            return t;
        }
        self.show_toast(e.user_message());
        Task::none()
    }

    /// Quit: Soloist is asked to stop first (SIGTERM), so it can remove its
    /// runtime files; the child also dies with us (`PR_SET_PDEATHSIG`).
    pub(super) fn quit(&mut self) -> Task<Message> {
        if let Some(h) = self.soloist.handle.take() {
            h.send(crate::net::soloist::Control::Shutdown);
            return Task::perform(
                async { tokio::time::sleep(Duration::from_millis(400)).await },
                |()| (),
            )
            .then(|()| iced::exit());
        }
        iced::exit()
    }

    fn show_toast(&mut self, msg: impl Into<String>) {
        self.toast = Some(state::next_toast(
            self.toast.as_ref(),
            msg.into(),
            Instant::now(),
            Duration::from_secs(TOAST_SECS),
        ));
    }

    // ------------------------------------------------------------- loading

    fn source(&self) -> Option<DataSource> {
        self.source.clone()
    }

    fn fetch_library(&self) -> Task<Message> {
        let Some(src) = self.source() else {
            return Task::none();
        };
        Task::perform(async move { src.library().await }, |r| {
            Message::Done(Done::Library(r))
        })
    }

    fn fetch_home(&self) -> Task<Message> {
        let Some(src) = self.source() else {
            return Task::none();
        };
        Task::perform(async move { src.home().await }, |r| {
            Message::Done(Done::Home(r))
        })
    }

    fn fetch_liked_flags(&self, tracks: &[Track]) -> Task<Message> {
        let Some(src) = self.source() else {
            return Task::none();
        };
        let ids: Vec<String> = tracks
            .iter()
            .filter(|t| !t.id.is_empty())
            .take(50)
            .map(|t| t.id.clone())
            .collect();
        if ids.is_empty() {
            return Task::none();
        }
        let ids2 = ids.clone();
        Task::perform(async move { src.contains_saved(ids2).await }, move |r| {
            Message::Done(Done::LikedFlags(ids.clone(), r))
        })
    }

    /// Load the data behind the current route: cache first, then network.
    fn load_route(&mut self) -> Task<Message> {
        let route = self.route().clone();
        let Some(src) = self.source() else {
            return Task::none();
        };
        let store = &self.store;
        let task = match &route {
            Route::Home => {
                if self.home.is_some() {
                    return Task::none();
                }
                self.fetch_home()
            }
            Route::Search | Route::Library => return Task::none(),
            Route::Settings => return self.measure_cache(),
            Route::Profile(user) => {
                if self.profiles.contains_key(user) {
                    return Task::none();
                }
                let u = user.clone();
                Task::perform(async move { src.profile(u, 50).await }, {
                    let u = user.clone();
                    move |r| Message::Done(Done::UserProfile(u.clone(), r))
                })
            }
            Route::Liked => {
                if self.liked.is_some() {
                    return Task::none();
                }
                Task::batch([
                    store_read(store, Kind::Liked, "me".into(), |p: Page<Track>| {
                        Done::Liked(Ok(p))
                    }),
                    Task::perform(async move { src.saved_tracks(0).await }, |r| {
                        Message::Done(Done::Liked(r))
                    }),
                ])
            }
            Route::Playlist(id) => {
                if self.playlists.contains_key(id) {
                    return Task::none();
                }
                let (id1, id2) = (id.clone(), id.clone());
                Task::batch([
                    store_read(store, Kind::Playlist, id.clone(), move |v| {
                        Done::Playlist(id1.clone(), Ok(v))
                    }),
                    Task::perform(async move { src.playlist(&id2).await }, {
                        let id = id.clone();
                        move |r| Message::Done(Done::Playlist(id.clone(), r))
                    }),
                ])
            }
            Route::Album(id) => {
                if self.albums.contains_key(id) {
                    return Task::none();
                }
                let (id1, id2) = (id.clone(), id.clone());
                Task::batch([
                    store_read(store, Kind::Album, id.clone(), move |v| {
                        Done::Album(id1.clone(), Ok(v))
                    }),
                    Task::perform(async move { src.album(&id2).await }, {
                        let id = id.clone();
                        move |r| Message::Done(Done::Album(id.clone(), r))
                    }),
                ])
            }
            Route::Artist(id) => {
                if self.artists.contains_key(id) {
                    return Task::none();
                }
                let (id1, id2) = (id.clone(), id.clone());
                Task::batch([
                    store_read(store, Kind::Artist, id.clone(), move |v| {
                        Done::Artist(id1.clone(), Ok(v))
                    }),
                    Task::perform(async move { src.artist_page(&id2).await }, {
                        let id = id.clone();
                        move |r| Message::Done(Done::Artist(id.clone(), r))
                    }),
                ])
            }
        };
        self.loading.insert(route.clone());
        self.errors.remove(&route);
        task
    }

    fn navigate(&mut self, route: Route) -> Task<Message> {
        self.user_menu = false;
        self.device_picker = false;
        self.hovered_row = None;
        self.selected_row = None;
        if !self.nav.push(route) {
            return Task::none();
        }
        self.after_nav()
    }

    fn after_nav(&mut self) -> Task<Message> {
        let key = self.route().key();
        let persist = if self.is_mock() {
            Task::none()
        } else {
            let store = self.store.clone();
            Task::perform(
                async move {
                    let _ = store.run(move |s| s.set_setting("route", &key)).await;
                },
                |()| Message::Done(Done::Noop),
            )
        };
        Task::batch([self.load_route(), persist])
    }

    fn load_more(&mut self) -> Task<Message> {
        if self.loading_more {
            return Task::none();
        }
        let Some(src) = self.source() else {
            return Task::none();
        };
        match self.route().clone() {
            Route::Liked => {
                let Some(list) = &self.liked else {
                    return Task::none();
                };
                if !list.has_more() {
                    return Task::none();
                }
                let Some(offset) = list.next else {
                    return Task::none();
                };
                self.loading_more = true;
                Task::perform(async move { src.saved_tracks(offset).await }, |r| {
                    Message::Done(Done::Liked(r))
                })
            }
            Route::Playlist(id) => {
                let Some((_, list)) = self.playlists.get(&id) else {
                    return Task::none();
                };
                if !list.has_more() {
                    return Task::none();
                }
                let Some(offset) = list.next else {
                    return Task::none();
                };
                self.loading_more = true;
                let id2 = id.clone();
                Task::perform(
                    async move { src.playlist_tracks(&id2, offset).await },
                    move |r| Message::Done(Done::PlaylistMore(id.clone(), r)),
                )
            }
            _ => Task::none(),
        }
    }

    // --------------------------------------------------------------- images

    /// Image URLs the current screen wants, with their target size.
    fn wanted_images(&self) -> Vec<(String, u32)> {
        let mut out: Vec<(String, u32)> = Vec::new();
        let mut out_extra: Vec<(String, u32)> = Vec::new();
        let mut push = |imgs: &[crate::net::model::Image], px: u32| {
            if let Some(i) = pick_image(imgs, px) {
                out.push((i.url.clone(), px));
            }
        };
        // Sidebar
        for p in &self.library.playlists {
            push(&p.images, 64);
            for i in meta::mosaic_images(p) {
                out_extra.push((i.url, 64));
            }
        }
        if let Some(u) = self.user.as_ref().and_then(|u| u.image.clone()) {
            out_extra.push((u, 64));
        }
        for a in &self.library.albums {
            push(&a.images, 64);
        }
        for a in &self.library.artists {
            push(&a.images, 64);
        }
        if let Some(t) = self.now_track() {
            push(t.images(), 64);
        }
        if self.show_queue {
            for t in self.queue.user_queue.iter().take(20) {
                push(t.images(), 64);
            }
            for t in self.queue.upcoming(20) {
                push(t.images(), 64);
            }
        }
        match self.route() {
            Route::Home => {
                if let Some(h) = &self.home {
                    for shelf in &h.shelves {
                        for c in shelf.items.iter().take(6) {
                            push(c.images(), 300);
                        }
                    }
                }
                for p in self.library.playlists.iter().take(8) {
                    push(&p.images, 64);
                }
            }
            Route::Library => {
                for p in &self.library.playlists {
                    push(&p.images, 300);
                    for i in meta::mosaic_images(p) {
                        out_extra.push((i.url, 300));
                    }
                }
                for a in &self.library.albums {
                    push(&a.images, 300);
                }
                for a in &self.library.artists {
                    push(&a.images, 300);
                }
            }
            Route::Search => {
                if let Some(r) = &self.search.results {
                    for t in &r.tracks {
                        push(t.images(), 64);
                    }
                    if let Some(t) = r.tracks.first() {
                        push(t.images(), 300);
                    }
                    for a in &r.artists {
                        push(&a.images, 300);
                    }
                    for a in &r.albums {
                        push(&a.images, 300);
                    }
                    for p in &r.playlists {
                        push(&p.images, 300);
                    }
                }
            }
            Route::Liked => {
                if let Some(l) = &self.liked {
                    for t in l.tracks.iter().take(60) {
                        push(t.images(), 64);
                    }
                }
            }
            Route::Playlist(id) => {
                if let Some((p, l)) = self.playlists.get(id) {
                    push(&p.images, 300);
                    for t in l.tracks.iter().take(60) {
                        push(t.images(), 64);
                    }
                }
            }
            Route::Album(id) => {
                if let Some(a) = self.albums.get(id) {
                    push(&a.images, 300);
                }
            }
            Route::Artist(id) => {
                if let Some(a) = self.artists.get(id) {
                    push(&a.artist.images, 300);
                    for t in &a.top_tracks {
                        push(t.images(), 64);
                    }
                    for al in &a.albums {
                        push(&al.images, 300);
                    }
                }
            }
            Route::Profile(u) => {
                if let Some(p) = self.profiles.get(u) {
                    if let Some(i) = &p.image {
                        out_extra.push((i.clone(), 300));
                    }
                    for pl in &p.playlists {
                        push(&pl.images, 300);
                    }
                }
            }
            Route::Settings => {}
        }
        if let Route::Playlist(id) = self.route()
            && let Some((p, _)) = self.playlists.get(id)
        {
            for i in meta::mosaic_images(p) {
                out_extra.push((i.url, 300));
            }
        }
        if *self.route() == Route::Home
            && let Some(h) = &self.home
        {
            for shelf in &h.shelves {
                for c in shelf.items.iter().take(6) {
                    if let crate::net::model::Card::Playlist(p) = c {
                        for i in meta::mosaic_images(p) {
                            out_extra.push((i.url, 300));
                        }
                    }
                }
            }
        }
        out.extend(out_extra);
        out
    }

    fn request_images(&mut self) -> Task<Message> {
        if !self.win.visible {
            return Task::none();
        }
        let Some(src) = self.source() else {
            return Task::none();
        };
        let mut tasks = Vec::new();
        for (url, px) in self.wanted_images() {
            if self.images.touch(&url) || self.image_requests.contains(&url) {
                continue;
            }
            if tasks.len() >= MAX_IMAGE_REQUESTS {
                break;
            }
            self.image_requests.insert(url.clone());
            let src = src.clone();
            let u = url.clone();
            tasks.push(Task::perform(
                async move { src.image(u, px).await },
                move |r| Message::Done(Done::Image(url.clone(), r)),
            ));
        }
        Task::batch(tasks)
    }

    /// Average colour of an entity's art (for gradient headers).
    pub fn art_color(&self, images: &[crate::net::model::Image]) -> Option<[u8; 3]> {
        let url = &pick_image(images, 300)?.url;
        self.images.peek(url).map(|c| c.average)
    }

    pub fn image(
        &self,
        images: &[crate::net::model::Image],
        px: u32,
    ) -> Option<iced::widget::image::Handle> {
        let url = &pick_image(images, px)?.url;
        self.images.peek(url).map(|c| c.handle.clone())
    }

    // ------------------------------------------------------------- playback

    /// Tracks behind a source, plus the context label.
    pub fn tracks_for(&self, src: &TrackSource) -> Option<(PlayContext, Vec<Track>)> {
        match src {
            TrackSource::Playlist(id) => self.playlists.get(id).map(|(p, l)| {
                (
                    PlayContext {
                        name: p.name.clone(),
                        route: Some(Route::Playlist(id.clone())),
                    },
                    l.tracks.clone(),
                )
            }),
            TrackSource::Album(id) => self.albums.get(id).map(|a| {
                (
                    PlayContext {
                        name: a.name.clone(),
                        route: Some(Route::Album(id.clone())),
                    },
                    a.tracks.clone(),
                )
            }),
            TrackSource::Liked => self.liked.as_ref().map(|l| {
                (
                    PlayContext {
                        name: "Liked Songs".into(),
                        route: Some(Route::Liked),
                    },
                    l.tracks.clone(),
                )
            }),
            TrackSource::ArtistTop(id) => self.artists.get(id).map(|a| {
                (
                    PlayContext {
                        name: a.artist.name.clone(),
                        route: Some(Route::Artist(id.clone())),
                    },
                    a.top_tracks.clone(),
                )
            }),
            TrackSource::Search => self.search.results.as_ref().map(|r| {
                (
                    PlayContext {
                        name: format!("Search: {}", r.query),
                        route: Some(Route::Search),
                    },
                    r.tracks.clone(),
                )
            }),
        }
    }

    /// Is `src` the context currently playing?
    pub fn is_playing_source(&self, src: &TrackSource) -> bool {
        let Some(ctx) = &self.queue.context else {
            return false;
        };
        let route = match src {
            TrackSource::Playlist(id) => Route::Playlist(id.clone()),
            TrackSource::Album(id) => Route::Album(id.clone()),
            TrackSource::Liked => Route::Liked,
            TrackSource::ArtistTop(id) => Route::Artist(id.clone()),
            _ => return false,
        };
        ctx.route.as_ref() == Some(&route) && self.playback.is_playing()
    }

    fn engine_send(&mut self, cmd: PlaybackCommand) {
        match &self.engine {
            Some(e) => e.send(cmd),
            None => tracing::debug!("engine not ready yet"),
        }
    }

    fn start_track(&mut self, track: Track, position_ms: u32) {
        self.reload_on_play = false;
        self.playback.duration_ms = track.duration_ms;
        self.playback
            .report(PlayStatus::Loading, position_ms, self.now);
        let cmd = PlaybackCommand::Load {
            uri: track.uri.clone(),
            duration_ms: track.duration_ms,
            start: true,
            position_ms,
        };
        if !self.is_mock() && self.session.is_none() {
            self.show_toast("Connecting to Spotify audio…");
        }
        self.engine_send(cmd);
    }

    /// Resume after pause; reloads the track if the engine lost it.
    fn resume(&mut self) {
        if self.reload_on_play
            && let Some(t) = self.queue.current.clone()
        {
            let pos = self.playback.position(self.now);
            self.start_track(t, pos);
        } else {
            self.engine_send(PlaybackCommand::Play);
        }
    }

    fn pause_for_reconnect(&mut self) {
        let pos = self.playback.position(self.now);
        self.playback.report(PlayStatus::Paused, pos, self.now);
        self.reload_on_play = true;
    }

    /// The engine reported a dead audio session: keep the queue, pause, and
    /// reconnect with backoff.
    fn on_session_lost(&mut self) -> Task<Message> {
        if self.is_mock() {
            return Task::none();
        }
        let had_session = self.session.take().is_some();
        if self.queue.current.is_some() {
            self.pause_for_reconnect();
        }
        if !had_session || self.source.is_none() {
            // A reconnect is already pending (or we are logged out).
            return Task::none();
        }
        self.show_toast("Connection to Spotify lost. Reconnecting…");
        self.schedule_audio_reconnect(&Error::Net("audio session lost".into()))
    }

    fn schedule_audio_reconnect(&mut self, e: &Error) -> Task<Message> {
        tracing::warn!(error = %e, attempt = self.audio_attempt, "audio session failed");
        self.connection = Connection::Reconnecting {
            attempt: self.audio_attempt,
        };
        let delay = backoff_delay(self.audio_attempt, rand::random());
        self.audio_attempt = self.audio_attempt.saturating_add(1);
        if self.audio_attempt == 3 {
            self.show_toast(e.user_message());
        }
        Task::perform(async move { tokio::time::sleep(delay).await }, |()| {
            Message::Done(Done::ReconnectAudio)
        })
    }

    fn stop_playback(&mut self) {
        self.engine_send(PlaybackCommand::Stop);
        self.playback.report(PlayStatus::Stopped, 0, self.now);
    }

    fn play_from(&mut self, src: TrackSource, index: Option<usize>) -> Task<Message> {
        // Clicking the big play button of the context that is playing pauses it.
        if index.is_none() && self.is_playing_source(&src) {
            self.engine_send(PlaybackCommand::Pause);
            return Task::none();
        }
        if index.is_none()
            && let Some(ctx) = &self.queue.context
            && self.playback.status == PlayStatus::Paused
            && self
                .tracks_for(&src)
                .is_some_and(|(c, _)| c.route == ctx.route)
        {
            self.engine_send(PlaybackCommand::Play);
            return Task::none();
        }
        let Some((ctx, tracks)) = self.tracks_for(&src) else {
            return Task::none();
        };
        match self.queue.play_context(ctx, tracks, index) {
            Some(t) => self.start_track(t, 0),
            None => self.show_toast("Nothing playable here."),
        }
        Task::none()
    }

    fn advance(&mut self, auto: bool) {
        match self.queue.next(auto) {
            Some(t) => self.start_track(t, 0),
            None => self.stop_playback(),
        }
    }

    fn persist_setting(&self, key: &'static str, value: String) -> Task<Message> {
        let store = self.store.clone();
        Task::perform(
            async move {
                let _ = store.run(move |s| s.set_setting(key, &value)).await;
            },
            |()| Message::Done(Done::Noop),
        )
    }

    fn current_uri(&self) -> Option<&str> {
        self.queue.current.as_ref().map(|t| t.uri.as_str())
    }

    fn on_playback(&mut self, e: PlaybackEvent) -> Task<Message> {
        match e {
            PlaybackEvent::Ready(h) => {
                h.send(PlaybackCommand::SetVolume(self.playback.effective_volume()));
                if let Some(s) = &self.session {
                    h.send(PlaybackCommand::Attach(s.clone()));
                }
                self.engine = Some(h);
                if let Some(plan) = &self.screenshot
                    && plan.screen != "login"
                    && let Some(t) = self.queue.current.clone()
                {
                    // Screenshots show a track mid-way through.
                    self.start_track(t, 72_000);
                }
            }
            PlaybackEvent::Loading { uri } if self.current_uri() == Some(&uri) => {
                self.playback.status = PlayStatus::Loading;
            }
            PlaybackEvent::Playing { uri, position_ms } if self.current_uri() == Some(&uri) => {
                self.skip_guard.on_playing();
                self.playback
                    .report(PlayStatus::Playing, position_ms, self.now);
            }
            PlaybackEvent::Paused { uri, position_ms } if self.current_uri() == Some(&uri) => {
                self.playback
                    .report(PlayStatus::Paused, position_ms, self.now);
            }
            PlaybackEvent::Position { uri, position_ms } if self.current_uri() == Some(&uri) => {
                let status = self.playback.status;
                self.playback.report(status, position_ms, self.now);
            }
            PlaybackEvent::EndOfTrack { uri } if self.current_uri() == Some(&uri) => {
                self.advance(true);
            }
            PlaybackEvent::Unavailable { uri, reason } if self.current_uri() == Some(&uri) => {
                if !self.is_mock() && self.session.is_none() {
                    // No audio session: every track would be "unavailable".
                    // Pause instead of skipping through the whole queue.
                    self.pause_for_reconnect();
                } else {
                    return self.on_unavailable(&uri, reason);
                }
            }
            PlaybackEvent::Cluster(c) => return self.on_cluster(c),
            PlaybackEvent::PreloadNext => {
                // Gapless playback: fetch the next track now, so it starts
                // the moment this one ends.
                if self.remote.is_none()
                    && self.settings.gapless
                    && let Some(t) = self.queue.peek_next()
                {
                    let uri = t.uri.clone();
                    self.engine_send(PlaybackCommand::Preload { uri });
                }
            }
            PlaybackEvent::SessionLost => return self.on_session_lost(),
            PlaybackEvent::Remote(ev) => self.on_remote(ev),
            PlaybackEvent::Stopped => {
                if self.playback.status != PlayStatus::Loading {
                    self.playback.status = PlayStatus::Stopped;
                }
            }
            PlaybackEvent::Error(m) => {
                tracing::warn!(error = %m, "playback");
                self.show_toast(Error::Playback(m).user_message());
            }
            _ => {}
        }
        Task::none()
    }

    fn on_unavailable(
        &mut self,
        uri: &str,
        reason: crate::net::audio_diag::UnavailableReason,
    ) -> Task<Message> {
        use state::UnavailableAction;
        tracing::debug!(
            ?reason,
            streak = self.skip_guard.streak(),
            "track unavailable"
        );
        match self.skip_guard.on_unavailable(uri, reason) {
            UnavailableAction::Retry => {
                if let Some(t) = self.queue.current.clone() {
                    self.start_track(t, 0);
                }
            }
            UnavailableAction::Skip => {
                self.show_toast(format!("{} Skipping.", reason.describe()));
                self.advance(false);
            }
            UnavailableAction::Stop(reason) => {
                let pos = self.playback.position(self.now);
                self.playback.report(PlayStatus::Paused, pos, self.now);
                self.reload_on_play = true;
                let message = if reason.is_key_denied() {
                    self.local_denied = true;
                    // Spotify's own player can play here: offer it.
                    if !self.soloist_enabled() {
                        self.soloist.card = true;
                    }
                    let mut m = reason.describe().to_owned();
                    m.push_str(" You can play on another device instead.");
                    m
                } else {
                    format!(
                        "Playback stopped: {} songs in a row could not be played. {}",
                        state::SkipGuard::MAX_SKIPS + 1,
                        reason.describe()
                    )
                };
                tracing::warn!(?reason, "playback stopped");
                self.toast = None;
                self.playback_block = Some(PlaybackBlock { reason, message });
                // Show the user's devices right away.
                self.engine_send(PlaybackCommand::RefreshDevices);
            }
        }
        Task::none()
    }

    /// The device to play on instead of this computer, if any.
    pub fn suggested_device(&self) -> Option<(String, String)> {
        let c = self.cluster.as_ref()?;
        if let Some(d) = c.active_remote() {
            return Some((d.id.clone(), d.name.clone()));
        }
        if let Some((id, _)) = &self.last_device
            && let Some(d) = c.device(id)
        {
            return Some((d.id.clone(), d.name.clone()));
        }
        c.devices.first().map(|d| (d.id.clone(), d.name.clone()))
    }

    /// Should new playback go to another device?
    fn play_target(&self) -> Option<(String, String)> {
        if let Some(r) = &self.remote
            && let RemoteKind::Device { id, name } = &r.kind
        {
            return Some((id.clone(), name.clone()));
        }
        if self.soloist_enabled() {
            // Soloist is this computer; until it is ready the request
            // waits (and the setup card explains why).
            return Some(
                self.soloist_device()
                    .unwrap_or_else(|| (soloist::LOCAL_ID.to_owned(), self.settings_device_name())),
            );
        }
        if self.local_denied || !self.settings.play_local {
            return self.suggested_device();
        }
        None
    }

    fn connect_task(&mut self, op: crate::net::connect_ctl::ConnectOp) -> Task<Message> {
        let Some(op) = self.soloist_intercept(op) else {
            return Task::none();
        };
        let Some(src) = self.source() else {
            return Task::none();
        };
        let from = self.device_id.clone();
        Task::perform(async move { src.connect_op(from, op).await }, |r| {
            Message::Done(Done::ConnectSent(r))
        })
    }

    /// Start `src` on another device through connect-state.
    fn play_remote(
        &mut self,
        src: &TrackSource,
        index: Option<usize>,
        device: (String, String),
    ) -> Task<Message> {
        use crate::net::connect_ctl::{ConnectOp, ControlCommand, PlayRequest};
        let tracks = self.tracks_for(src).map(|(_, t)| t).unwrap_or_default();
        let track = index.and_then(|i| tracks.get(i)).or_else(|| tracks.first());
        let user = self.username();
        let Some(context_uri) = context_uri(src, &user, track.map(|t| t.uri.as_str())) else {
            self.show_toast("Nothing playable here.");
            return Task::none();
        };
        if self.playback.is_playing() {
            self.engine_send(PlaybackCommand::Pause);
        }
        let req = PlayRequest {
            context_uri,
            track_uri: index.and(track).map(|t| t.uri.clone()),
            track_index: index,
            position_ms: None,
        };
        self.playback_block = None;
        self.set_remote_device(&device.0, &device.1);
        if let Some(r) = self.remote.as_mut() {
            if let Some(t) = track {
                r.track = Some(t.clone());
                r.playback.duration_ms = t.duration_ms;
            }
            r.playback.report(PlayStatus::Loading, 0, self.now);
        }
        self.connect_task(ConnectOp::Command(device.0, ControlCommand::Play(req)))
    }

    /// Mark `id` as the device we control (until the cluster says otherwise).
    fn set_remote_device(&mut self, id: &str, name: &str) {
        let kind = RemoteKind::Device {
            id: id.to_owned(),
            name: name.to_owned(),
        };
        match self.remote.as_mut() {
            Some(r) if r.kind == kind => {}
            _ => {
                self.remote = Some(RemotePlayback {
                    kind,
                    track: None,
                    playback: Playback {
                        volume: self.playback.volume,
                        ..Playback::default()
                    },
                    shuffle: false,
                    repeat: Repeat::Off,
                })
            }
        }
    }

    fn remember_device(&mut self, id: &str, name: &str) -> Task<Message> {
        if self
            .last_device
            .as_ref()
            .is_some_and(|(i, n)| i == id && n == name)
        {
            return Task::none();
        }
        self.last_device = Some((id.to_owned(), name.to_owned()));
        self.persist_setting("last_device", format!("{id}\n{name}"))
    }

    /// Picker choice: `None` = this computer.
    fn transfer_to(&mut self, target: Option<(String, String)>) -> Task<Message> {
        use crate::net::connect_ctl::{ConnectOp, ControlCommand, PlayRequest};
        self.device_picker = false;
        // "This computer" is the Soloist device when Soloist plays here.
        let target =
            match target {
                None if self.soloist_enabled() => {
                    if self.soloist_playing_here() {
                        return Task::none();
                    }
                    Some(self.soloist_device().unwrap_or_else(|| {
                        (soloist::LOCAL_ID.to_owned(), self.settings_device_name())
                    }))
                }
                t => t,
            };
        let remote_id = self
            .remote
            .as_ref()
            .and_then(|r| r.device_id().map(str::to_owned));
        let Some((id, name)) = target else {
            // Back to this computer.
            let Some(from) = remote_id else {
                return Task::none();
            };
            self.remote = None;
            let own = self.device_id.clone();
            tracing::debug!("transfer playback to this computer");
            let _ = from;
            return self.connect_task(ConnectOp::Transfer(own));
        };
        let remember = if self.is_soloist_id(&id) {
            Task::none()
        } else {
            self.remember_device(&id, &name)
        };
        self.playback_block = None;
        let op = if remote_id.is_some() || self.queue.current.is_none() {
            ConnectOp::Transfer(id.clone())
        } else {
            // Hand the local queue over: same context, track and position.
            let pos = self.playback.position(self.now);
            let t = self.queue.current.clone();
            if self.playback.is_playing() {
                self.engine_send(PlaybackCommand::Pause);
            }
            let ctx = self
                .queue
                .context
                .as_ref()
                .and_then(|c| c.route.clone())
                .and_then(|r| match r {
                    Route::Playlist(id) => Some(format!("spotify:playlist:{id}")),
                    Route::Album(id) => Some(format!("spotify:album:{id}")),
                    Route::Artist(id) => Some(format!("spotify:artist:{id}")),
                    Route::Liked => Some(format!("spotify:user:{}:collection", self.username())),
                    _ => None,
                })
                .or_else(|| t.as_ref().map(|t| t.uri.clone()));
            match ctx {
                Some(context_uri) => ConnectOp::Command(
                    id.clone(),
                    ControlCommand::Play(PlayRequest {
                        context_uri,
                        track_uri: t.as_ref().map(|t| t.uri.clone()),
                        track_index: None,
                        position_ms: Some(pos),
                    }),
                ),
                None => ConnectOp::Transfer(id.clone()),
            }
        };
        let track = self.queue.current.clone();
        self.set_remote_device(&id, &name);
        if let Some(r) = self.remote.as_mut()
            && r.track.is_none()
        {
            r.playback.duration_ms = track.as_ref().map_or(0, |t| t.duration_ms);
            r.track = track;
        }
        Task::batch([remember, self.connect_task(op)])
    }

    /// New connect-state cluster from the engine.
    fn on_cluster(&mut self, c: crate::net::connect_ctl::ClusterState) -> Task<Message> {
        let now = self.now;
        let active = c.active_remote().cloned();
        let player = c.player.clone();
        self.cluster = Some(c);
        // Soloist's own WebSocket events are faster and more precise than
        // cluster pushes: they drive the bar while connected.
        let soloist_ws = self.soloist.ws && self.soloist.status.is_ready();
        let Some(dev) = active else {
            if self
                .remote
                .as_ref()
                .is_some_and(|r| matches!(r.kind, RemoteKind::Device { .. }))
                && !(soloist_ws && self.soloist_playing_here())
            {
                self.remote = None;
            }
            return Task::none();
        };
        if self.is_soloist_id(&dev.id) && soloist_ws {
            self.soloist_active(true);
            return Task::none();
        }
        if self
            .remote
            .as_ref()
            .is_some_and(|r| r.kind == RemoteKind::Cast)
        {
            return Task::none();
        }
        // Another device plays: the local player yields, as in the official app.
        if self.playback.is_playing() {
            self.engine_send(PlaybackCommand::Pause);
            let pos = self.playback.position(now);
            self.playback.report(PlayStatus::Paused, pos, now);
        }
        let remember = if self.is_soloist_id(&dev.id) {
            Task::none()
        } else {
            self.remember_device(&dev.id, &dev.name)
        };
        self.set_remote_device(&dev.id, &dev.name);
        if let Some(r) = self.remote.as_mut() {
            if let Some(v) = dev.volume {
                r.playback.volume = v;
            }
            if let Some(p) = player {
                if let Some(t) = &p.track {
                    r.track = Some(remote_track(&RemoteTrack {
                        uri: t.uri.clone(),
                        name: t.name.clone(),
                        artists: t.artists.clone(),
                        album: t.album.clone(),
                        cover_url: t.cover_url.clone(),
                        duration_ms: p.duration_ms,
                    }));
                }
                r.playback.duration_ms = p.duration_ms;
                let status = if p.is_playing && !p.is_paused {
                    PlayStatus::Playing
                } else {
                    PlayStatus::Paused
                };
                r.playback.report(status, p.position_ms, now);
                r.shuffle = p.shuffle;
                r.repeat = if p.repeat_track {
                    Repeat::Track
                } else if p.repeat_context {
                    Repeat::Context
                } else {
                    Repeat::Off
                };
            }
        }
        remember
    }

    /// Name this computer announces on Spotify Connect.
    pub fn settings_device_name(&self) -> String {
        let n = self.settings.device_name.trim();
        if n.is_empty() {
            self.config.device_name.clone()
        } else {
            n.to_owned()
        }
    }

    /// Id of the device we control, if any.
    pub fn remote_device(&self) -> Option<&str> {
        self.remote.as_ref().and_then(RemotePlayback::device_id)
    }

    /// Shuffle / repeat as shown in the player bar.
    pub fn shown_shuffle(&self) -> bool {
        match &self.remote {
            Some(r) if r.device_id().is_some() => r.shuffle,
            _ => self.queue.shuffle,
        }
    }

    pub fn shown_repeat(&self) -> Repeat {
        match &self.remote {
            Some(r) if r.device_id().is_some() => r.repeat,
            _ => self.queue.repeat,
        }
    }

    /// The session's canonical username (what playlist owners are), or
    /// the profile id when no session has reported one.
    fn username(&self) -> String {
        if !self.session_user.is_empty() {
            return self.session_user.clone();
        }
        self.user.as_ref().map(|u| u.id.clone()).unwrap_or_default()
    }

    /// Spotify Connect: another device plays through us.
    ///
    /// Only a track change or playback start makes Connect the active
    /// source; volume/position/pause events for an idle Connect device
    /// (e.g. the phone changing our volume) must not hijack the local queue.
    fn on_remote(&mut self, ev: RemoteEvent) {
        let now = self.now;
        if matches!(ev, RemoteEvent::Track(_) | RemoteEvent::Playing { .. })
            && self
                .remote
                .as_ref()
                .is_none_or(|r| r.kind != RemoteKind::Cast)
        {
            if self.playback.is_playing() {
                // The engine paused the local player; mirror that here.
                let pos = self.playback.position(now);
                self.playback.report(PlayStatus::Paused, pos, now);
            }
            self.remote = Some(RemotePlayback {
                kind: RemoteKind::Cast,
                track: None,
                playback: Playback {
                    volume: self.playback.volume,
                    ..Playback::default()
                },
                shuffle: false,
                repeat: Repeat::Off,
            });
        }
        if matches!(ev, RemoteEvent::Stopped) {
            if self
                .remote
                .as_ref()
                .is_some_and(|r| r.kind == RemoteKind::Cast)
            {
                self.remote = None;
            }
            return;
        }
        let Some(r) = self.remote.as_mut().filter(|r| r.kind == RemoteKind::Cast) else {
            if let RemoteEvent::Volume(v) = ev {
                tracing::debug!(volume = v, "connect volume while idle");
            }
            return;
        };
        match ev {
            RemoteEvent::Track(t) => {
                r.playback.duration_ms = t.duration_ms;
                r.track = Some(remote_track(&t));
            }
            RemoteEvent::Playing { position_ms } => {
                r.playback.report(PlayStatus::Playing, position_ms, now);
            }
            RemoteEvent::Paused { position_ms } => {
                r.playback.report(PlayStatus::Paused, position_ms, now);
            }
            RemoteEvent::Position { position_ms } => {
                let st = r.playback.status;
                r.playback.report(st, position_ms, now);
            }
            RemoteEvent::Volume(v) => r.playback.volume = v.clamp(0.0, 1.0),
            RemoteEvent::Stopped => {}
        }
    }

    /// Control the remote playback: Spirc for a cast to us, connect-state
    /// commands for a device we control.
    fn remote_send(&mut self, cmd: RemoteCommand) {
        use crate::net::connect_ctl::{ConnectOp, ControlCommand};
        let Some(id) = self
            .remote
            .as_ref()
            .and_then(|r| r.device_id().map(str::to_owned))
        else {
            if let Some(e) = &self.engine {
                e.send(PlaybackCommand::Remote(cmd));
            }
            return;
        };
        // Optimistic: the next cluster push confirms or corrects it.
        if let Some(r) = self.remote.as_mut() {
            let pos = r.playback.position(self.now);
            match cmd {
                RemoteCommand::Play => r.playback.report(PlayStatus::Playing, pos, self.now),
                RemoteCommand::Pause => r.playback.report(PlayStatus::Paused, pos, self.now),
                _ => {}
            }
        }
        let op = match cmd {
            RemoteCommand::Play => ConnectOp::Command(id, ControlCommand::Resume),
            RemoteCommand::Pause => ConnectOp::Command(id, ControlCommand::Pause),
            RemoteCommand::Next => ConnectOp::Command(id, ControlCommand::Next),
            RemoteCommand::Previous => ConnectOp::Command(id, ControlCommand::Previous),
            RemoteCommand::Seek(ms) => ConnectOp::Command(id, ControlCommand::Seek(ms)),
            RemoteCommand::Volume(v) => ConnectOp::Volume(id, v),
        };
        let t = self.connect_task(op);
        self.pending.push(t);
    }

    fn on_media(&mut self, e: MediaEvent) -> Task<Message> {
        match e {
            MediaEvent::Ready(h) => {
                self.media = Some(h);
                self.last_media = None;
                Task::none()
            }
            MediaEvent::Unavailable(reason) => {
                tracing::info!(%reason, "MPRIS disabled");
                self.mpris_enabled = false;
                Task::none()
            }
            MediaEvent::Command(c) if self.remote.is_some() => match c {
                MediaCommand::Play => {
                    self.remote_send(RemoteCommand::Play);
                    Task::none()
                }
                MediaCommand::Pause | MediaCommand::Stop => {
                    self.remote_send(RemoteCommand::Pause);
                    Task::none()
                }
                MediaCommand::PlayPause => self.update_ui(Ui::TogglePlay),
                MediaCommand::Next => self.update_ui(Ui::Next),
                MediaCommand::Previous => self.update_ui(Ui::Previous),
                MediaCommand::Seek(offset_us) => {
                    let pos = i64::from(self.now_playback().position(self.now))
                        .saturating_add(offset_us / 1000);
                    self.seek_to(crate::net::mpris::clamp_ms(pos));
                    Task::none()
                }
                MediaCommand::SetPosition { position_us, .. } => {
                    self.seek_to(crate::net::mpris::clamp_ms(position_us / 1000));
                    Task::none()
                }
                MediaCommand::SetVolume(v) => self.update_ui(Ui::Volume(v as f32)),
                MediaCommand::Raise => self.show_window(),
                MediaCommand::Quit => self.quit(),
                MediaCommand::SetShuffle(on) if self.remote_device().is_some() => {
                    if on != self.shown_shuffle() {
                        return self.update_ui(Ui::ToggleShuffle);
                    }
                    Task::none()
                }
                MediaCommand::SetLoop(l) if self.remote_device().is_some() => {
                    let want = match l {
                        MediaLoop::None => Repeat::Off,
                        MediaLoop::Playlist => Repeat::Context,
                        MediaLoop::Track => Repeat::Track,
                    };
                    let mut tasks = Vec::new();
                    for _ in 0..3 {
                        if self.shown_repeat() == want {
                            break;
                        }
                        tasks.push(self.update_ui(Ui::CycleRepeat));
                    }
                    Task::batch(tasks)
                }
                MediaCommand::SetShuffle(_) | MediaCommand::SetLoop(_) => Task::none(),
            },
            MediaEvent::Command(c) => {
                match c {
                    MediaCommand::Play => {
                        if self.playback.status == PlayStatus::Paused {
                            self.resume();
                        } else if self.playback.status == PlayStatus::Stopped
                            && let Some(t) = self.queue.current.clone()
                        {
                            self.start_track(t, 0);
                        }
                        Task::none()
                    }
                    MediaCommand::Pause | MediaCommand::Stop => {
                        self.engine_send(PlaybackCommand::Pause);
                        Task::none()
                    }
                    MediaCommand::PlayPause => self.update_ui(Ui::TogglePlay),
                    MediaCommand::Next => self.update_ui(Ui::Next),
                    MediaCommand::Previous => self.update_ui(Ui::Previous),
                    MediaCommand::Seek(offset_us) => {
                        let pos = i64::from(self.playback.position(self.now))
                            .saturating_add(offset_us / 1000);
                        self.seek_to(crate::net::mpris::clamp_ms(pos));
                        Task::none()
                    }
                    MediaCommand::SetPosition {
                        track_id,
                        position_us,
                    } => {
                        if self.queue.current.as_ref().is_some_and(|t| {
                            crate::net::mpris::track_path(&t.id).ends_with(&track_id)
                        }) {
                            self.seek_to(crate::net::mpris::clamp_ms(position_us / 1000));
                        }
                        Task::none()
                    }
                    MediaCommand::SetVolume(v) => {
                        let t = self.update_ui(Ui::Volume(v as f32));
                        Task::batch([t, self.update_ui(Ui::VolumeCommit)])
                    }
                    MediaCommand::SetShuffle(on) => {
                        if on != self.queue.shuffle {
                            return self.update_ui(Ui::ToggleShuffle);
                        }
                        Task::none()
                    }
                    MediaCommand::SetLoop(l) => {
                        self.queue.repeat = match l {
                            MediaLoop::None => Repeat::Off,
                            MediaLoop::Playlist => Repeat::Context,
                            MediaLoop::Track => Repeat::Track,
                        };
                        self.persist_setting("repeat", self.queue.repeat.as_str().into())
                    }
                    MediaCommand::Raise => self.show_window(),
                    MediaCommand::Quit => self.quit(),
                }
            }
        }
    }

    /// Build the MPRIS snapshot and push it if anything changed.
    fn push_media_snapshot(&mut self) {
        let Some(media) = &self.media else {
            return;
        };
        let snap = self.media_snapshot();
        if self.last_media.as_ref() != Some(&snap) {
            media.update(snap.clone());
            self.last_media = Some(snap);
        }
    }

    pub fn media_snapshot(&self) -> MediaSnapshot {
        let pb = self.now_playback();
        let track = self.now_track().map(|t| MediaTrack {
            id: t.id.clone(),
            title: t.name.clone(),
            artists: t.artists.iter().map(|a| a.name.clone()).collect(),
            album: t.album.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
            length_ms: t.duration_ms,
            art_url: pick_image(t.images(), 300)
                .map(|i| i.url.clone())
                .filter(|u| u.starts_with("http")),
        });
        MediaSnapshot {
            status: match pb.status {
                PlayStatus::Playing | PlayStatus::Loading => MediaStatus::Playing,
                PlayStatus::Paused => MediaStatus::Paused,
                PlayStatus::Stopped => MediaStatus::Stopped,
            },
            can_next: track.is_some() && (self.remote.is_some() || self.queue.has_next()),
            can_previous: track.is_some(),
            track,
            position_ms: pb.position(self.now),
            volume: f64::from(pb.effective_volume()),
            shuffle: self.shown_shuffle(),
            seek_seq: self.seek_seq,
            loop_status: match self.shown_repeat() {
                Repeat::Off => MediaLoop::None,
                Repeat::Context => MediaLoop::Playlist,
                Repeat::Track => MediaLoop::Track,
            },
        }
    }

    fn seek_to(&mut self, ms: u32) {
        if let Some(r) = self.remote.as_mut() {
            let ms = ms.min(r.playback.duration_ms);
            let st = r.playback.status;
            r.playback.report(st, ms, self.now);
            self.seek_seq = self.seek_seq.wrapping_add(1);
            self.remote_send(RemoteCommand::Seek(ms));
            return;
        }
        let ms = ms.min(self.playback.duration_ms);
        self.seek_seq = self.seek_seq.wrapping_add(1);
        self.engine_send(PlaybackCommand::Seek(ms));
        let status = self.playback.status;
        self.playback.report(status, ms, self.now);
    }

    // ------------------------------------------------------------------- ui

    fn update_ui(&mut self, u: Ui) -> Task<Message> {
        // Any action other than opening/expanding the menu dismisses it.
        if !matches!(
            u,
            Ui::OpenMenu(..)
                | Ui::ToggleMenuPicker
                | Ui::CloseOverlay
                | Ui::Tick(_)
                | Ui::HoverRow(_)
                | Ui::Scrolled(_)
                | Ui::WindowResized(_)
                | Ui::WindowMoved(_)
        ) {
            self.menu = None;
        }
        match u {
            Ui::Navigate(r) => self.navigate(r),
            Ui::Back => {
                if self.nav.back() {
                    self.after_nav()
                } else {
                    Task::none()
                }
            }
            Ui::Forward => {
                if self.nav.forward() {
                    self.after_nav()
                } else {
                    Task::none()
                }
            }
            Ui::SearchInput(q) => {
                self.search.query = q;
                self.search.seq += 1;
                let nav = self.navigate(Route::Search);
                if self.search.query.trim().is_empty() {
                    self.search.results = None;
                    self.search.loading = false;
                    return nav;
                }
                let seq = self.search.seq;
                Task::batch([
                    nav,
                    Task::perform(
                        async move { tokio::time::sleep(Duration::from_millis(250)).await },
                        move |()| Message::Done(Done::SearchDebounced(seq)),
                    ),
                ])
            }
            Ui::SearchSubmit => self.on_done(Done::SearchDebounced(self.search.seq)),
            Ui::SearchFilter(f) => {
                self.search.filter = f;
                Task::none()
            }
            Ui::LibraryFilter(f) => {
                self.library_filter = if self.library_filter == f {
                    LibraryFilter::All
                } else {
                    f
                };
                Task::none()
            }
            Ui::PlayFrom(src, index) => {
                self.skip_guard.reset();
                if let Some(dev) = self.play_target() {
                    return self.play_remote(&src, index, dev);
                }
                // Starting something locally takes playback back from Connect
                // (the engine releases the Connect session).
                self.remote = None;
                self.playback_block = None;
                self.play_from(src, index)
            }
            Ui::ToggleShuffle if self.remote_device().is_some() => {
                use crate::net::connect_ctl::{ConnectOp, ControlCommand};
                let id = self.remote_device().map(str::to_owned).unwrap_or_default();
                let on = self.remote.as_ref().is_some_and(|r| !r.shuffle);
                if let Some(r) = self.remote.as_mut() {
                    r.shuffle = on;
                }
                self.connect_task(ConnectOp::Command(id, ControlCommand::Shuffle(on)))
            }
            Ui::CycleRepeat if self.remote_device().is_some() => {
                use crate::net::connect_ctl::{ConnectOp, ControlCommand};
                let id = self.remote_device().map(str::to_owned).unwrap_or_default();
                let next = self
                    .remote
                    .as_ref()
                    .map_or(Repeat::Off, |r| r.repeat.cycle());
                if let Some(r) = self.remote.as_mut() {
                    r.repeat = next;
                }
                self.connect_task(ConnectOp::Command(
                    id,
                    ControlCommand::Repeat {
                        context: next != Repeat::Off,
                        track: next == Repeat::Track,
                    },
                ))
            }
            Ui::ToggleDevicePicker => {
                self.device_picker = !self.device_picker;
                if self.device_picker {
                    self.engine_send(PlaybackCommand::RefreshDevices);
                }
                Task::none()
            }
            Ui::PlayOn(target) => self.transfer_to(target),
            Ui::ToggleUserMenu => {
                self.user_menu = !self.user_menu;
                Task::none()
            }
            Ui::Setting(c) => self.change_setting(c),
            Ui::Soloist(u) => self.update_soloist_ui(u),
            Ui::Update(u) => self.on_update_ui(u),
            Ui::DismissBanner => {
                self.playback_block = None;
                Task::none()
            }
            Ui::TogglePlay if self.remote.is_some() => {
                let playing = self.now_playback().is_playing();
                self.remote_send(if playing {
                    RemoteCommand::Pause
                } else {
                    RemoteCommand::Play
                });
                Task::none()
            }
            Ui::Next if self.remote.is_some() => {
                self.remote_send(RemoteCommand::Next);
                Task::none()
            }
            Ui::Previous if self.remote.is_some() => {
                self.remote_send(RemoteCommand::Previous);
                Task::none()
            }
            Ui::SeekPreview(f) if self.remote.is_some() => {
                if let Some(r) = self.remote.as_mut()
                    && r.playback.duration_ms > 0
                {
                    r.playback.seek_preview =
                        Some((f.clamp(0.0, 1.0) * r.playback.duration_ms as f32) as u32);
                }
                Task::none()
            }
            Ui::SeekCommit if self.remote.is_some() => {
                if let Some(p) = self
                    .remote
                    .as_mut()
                    .and_then(|r| r.playback.seek_preview.take())
                {
                    self.seek_to(p);
                }
                Task::none()
            }
            Ui::Volume(v) if self.remote.is_some() => {
                let v = v.clamp(0.0, 1.0);
                if let Some(r) = self.remote.as_mut() {
                    r.playback.volume = v;
                    r.playback.muted = false;
                }
                self.remote_send(RemoteCommand::Volume(v));
                Task::none()
            }
            Ui::TogglePlay => {
                match self.playback.status {
                    PlayStatus::Playing | PlayStatus::Loading => {
                        self.engine_send(PlaybackCommand::Pause)
                    }
                    PlayStatus::Paused => self.resume(),
                    PlayStatus::Stopped => {
                        if let Some(t) = self.queue.current.clone() {
                            self.start_track(t, 0);
                        }
                    }
                }
                Task::none()
            }
            Ui::Next => {
                if self.queue.current.is_some() {
                    self.advance(false);
                }
                Task::none()
            }
            Ui::Previous => {
                match self.queue.previous(self.playback.position(self.now)) {
                    Previous::Restart => {
                        if self.queue.current.is_some() {
                            self.seek_to(0);
                        }
                    }
                    Previous::Play(t) => self.start_track(t, 0),
                }
                Task::none()
            }
            Ui::ToggleShuffle => {
                let on = !self.queue.shuffle;
                self.queue.set_shuffle(on);
                self.persist_setting("shuffle", if on { "1" } else { "0" }.into())
            }
            Ui::CycleRepeat => {
                let r = self.queue.cycle_repeat();
                self.persist_setting("repeat", r.as_str().into())
            }
            Ui::SeekPreview(f) => {
                if self.playback.duration_ms > 0 {
                    self.playback.seek_preview =
                        Some((f.clamp(0.0, 1.0) * self.playback.duration_ms as f32) as u32);
                }
                Task::none()
            }
            Ui::SeekCommit => {
                if let Some(p) = self.playback.seek_preview.take() {
                    self.seek_to(p);
                }
                Task::none()
            }
            Ui::Volume(v) => {
                self.playback.volume = v.clamp(0.0, 1.0);
                self.playback.muted = false;
                self.engine_send(PlaybackCommand::SetVolume(self.playback.volume));
                Task::none()
            }
            Ui::VolumeCommit => {
                self.persist_setting("volume", format!("{:.3}", self.playback.volume))
            }
            Ui::ToggleMute => {
                self.playback.muted = !self.playback.muted;
                self.engine_send(PlaybackCommand::SetVolume(self.playback.effective_volume()));
                Task::none()
            }
            Ui::ToggleQueue => {
                self.show_queue = !self.show_queue;
                if self.show_queue {
                    self.show_lyrics = false;
                }
                Task::none()
            }
            Ui::ToggleLyrics => {
                self.show_lyrics = !self.show_lyrics;
                if self.show_lyrics {
                    self.show_queue = false;
                }
                self.request_lyrics()
            }
            Ui::PlayUpcoming(k) => {
                if let Some(t) = self.queue.jump_upcoming(k) {
                    self.start_track(t, 0);
                }
                Task::none()
            }
            Ui::PlayQueued(k) => {
                if let Some(t) = self.queue.jump_user_queue(k) {
                    self.start_track(t, 0);
                }
                Task::none()
            }
            Ui::Enqueue(t) if self.soloist_enqueue(&t.uri) => {
                self.show_toast("Added to queue");
                Task::none()
            }
            Ui::Enqueue(t) => {
                self.queue.enqueue(t);
                self.show_toast("Added to queue");
                Task::none()
            }
            Ui::ToggleLike(t) => {
                let Some(src) = self.source() else {
                    return Task::none();
                };
                let saved = !self.liked_ids.contains(&t.id);
                if saved {
                    self.liked_ids.insert(t.id.clone());
                } else {
                    self.liked_ids.remove(&t.id);
                }
                self.show_toast(if saved {
                    "Added to Liked Songs"
                } else {
                    "Removed from Liked Songs"
                });
                let id = t.id.clone();
                Task::perform(
                    async move { src.set_saved(vec![t.id], saved).await },
                    move |r| Message::Done(Done::LikeSaved(id.clone(), saved, r)),
                )
            }
            Ui::HoverRow(k) => {
                self.hovered_row = k;
                Task::none()
            }
            Ui::SelectRow(k) => {
                self.selected_row = Some(k);
                Task::none()
            }
            Ui::Login => self.start_login(),
            Ui::RetryKeyring => self.check_keyring_and_restore(),
            Ui::CancelLogin => {
                if let Some(h) = self.login_abort.take() {
                    h.abort();
                }
                self.auth = AuthState::SignedOut { error: None };
                Task::none()
            }
            Ui::OpenLoginUrl => {
                if let AuthState::WaitingForBrowser { url } = &self.auth
                    && let Err(e) = oauth::open_browser(url)
                {
                    self.show_toast(e.user_message());
                }
                Task::none()
            }
            Ui::CopyLoginUrl => {
                if let AuthState::WaitingForBrowser { url } = &self.auth {
                    let url = url.clone();
                    self.show_toast("Login link copied");
                    return iced::clipboard::write(url);
                }
                Task::none()
            }
            Ui::Logout => self.logout(),
            Ui::Scrolled(rel_y) => {
                if rel_y > 0.9 {
                    self.load_more()
                } else {
                    Task::none()
                }
            }
            Ui::DismissToast => {
                self.toast = None;
                Task::none()
            }
            Ui::FocusSearch => {
                self.menu = None;
                self.dialog = None;
                Task::batch([
                    self.navigate(Route::Search),
                    iced::widget::operation::focus(crate::ui::topbar::SEARCH_ID),
                    iced::widget::operation::select_all(crate::ui::topbar::SEARCH_ID),
                ])
            }
            Ui::VolumeStep(d) => {
                let v = (self.now_playback().volume + d).clamp(0.0, 1.0);
                let t = self.update_ui(Ui::Volume(v));
                Task::batch([t, self.update_ui(Ui::VolumeCommit)])
            }
            Ui::Quit => self.quit(),
            Ui::CloseRequested
            | Ui::ToggleWindow
            | Ui::ShowWindow
            | Ui::HideWindow
            | Ui::ToggleStartMinimized => self.update_background(u),
            Ui::WindowMoved(p) => {
                self.window_pos = Some(p);
                Task::none()
            }
            Ui::ToggleHelp => {
                self.menu = None;
                self.dialog = match self.dialog {
                    Some(menu::Dialog::Shortcuts) => None,
                    _ => Some(menu::Dialog::Shortcuts),
                };
                Task::none()
            }
            Ui::CloseOverlay => {
                if self.menu.take().is_none() {
                    self.dialog = None;
                }
                Task::none()
            }
            Ui::WindowResized(size) => {
                self.window_size = size;
                Task::none()
            }
            Ui::OpenMenu(target, at) => {
                self.dialog = None;
                self.menu = Some(menu::ContextMenu {
                    target,
                    at,
                    picker: false,
                });
                Task::none()
            }
            Ui::ToggleMenuPicker => {
                if let Some(m) = self.menu.as_mut() {
                    m.picker = !m.picker;
                }
                Task::none()
            }
            Ui::CopyLink(link) => {
                self.show_toast("Link copied to clipboard");
                iced::clipboard::write(link)
            }
            Ui::NewPlaylist => self.new_playlist(),
            Ui::OpenRename(id) => {
                self.open_rename(id);
                iced::widget::operation::focus(crate::ui::menu::DIALOG_INPUT_ID)
            }
            Ui::OpenDelete(id) => {
                self.menu = None;
                self.open_delete(id);
                Task::none()
            }
            Ui::DialogInput(text) => {
                if let Some(menu::Dialog::Rename { name, .. }) = self.dialog.as_mut() {
                    *name = text.chars().take(100).collect();
                }
                Task::none()
            }
            Ui::DialogSubmit => self.dialog_submit(),
            Ui::AddToPlaylist(id, t) => {
                self.menu = None;
                self.add_to_playlist(id, t)
            }
            Ui::RemoveFromPlaylist(id, i) => self.remove_from_playlist(id, i),
            Ui::DragStart(key, i) => {
                self.selected_row = Some(key.clone());
                self.drag = Some((key, i));
                Task::none()
            }
            Ui::DragDrop(to) => {
                let Some((key, from)) = self.drag.take() else {
                    return Task::none();
                };
                let Route::Playlist(id) = self.route().clone() else {
                    return Task::none();
                };
                // The key carries the table it came from; ignore stale drags.
                if from == to
                    || !key.starts_with(&format!("{:?}", TrackSource::Playlist(id.clone())))
                {
                    return Task::none();
                }
                self.move_in_playlist(id, from, to)
            }
            Ui::MouseUp => {
                self.drag = None;
                Task::none()
            }
            Ui::Tick(now) => {
                self.now = now;
                if self.toast.as_ref().is_some_and(|(_, at)| {
                    now.saturating_duration_since(*at) > Duration::from_secs(TOAST_SECS)
                }) {
                    self.toast = None;
                }
                Task::none()
            }
        }
    }

    // ----------------------------------------------------------------- done

    fn on_done(&mut self, d: Done) -> Task<Message> {
        match d {
            Done::Restored(Ok(Some(ready))) => {
                self.auth = self.auth.apply(AuthEvent::StoredSessionFound);
                self.signed_in(ready)
            }
            Done::Restored(Ok(None)) => {
                self.auth = self.auth.apply(AuthEvent::NoStoredSession);
                Task::none()
            }
            Done::Restored(Err(e)) => {
                // Silent login failed: revoked credentials were deleted and
                // the browser login is next; transient errors keep them.
                let (event, deleted) = crate::auth::silent_login_failure(&e);
                self.has_stored_credentials = !deleted;
                self.auth = self.auth.apply(event);
                Task::none()
            }
            Done::LoggedIn(Err(e)) => {
                self.login_abort = None;
                self.auth = self.auth.apply(AuthEvent::Failed(e));
                Task::none()
            }
            Done::LoggedIn(Ok(ready)) => {
                self.login_abort = None;
                self.auth = self.auth.apply(AuthEvent::CallbackReceived);
                self.signed_in(ready)
            }
            Done::Profile(Ok(user)) => {
                if !user.is_premium() && !self.is_mock() {
                    self.show_toast("Spotify Premium is required for playback.");
                }
                self.auth = AuthState::SignedIn { user: user.clone() };
                self.user = Some(user.clone());
                store_write(&self.store, Kind::User, "me".into(), user)
            }
            Done::Profile(Err(e)) => self.fail(&e),
            Done::Audio(Ok(session)) => {
                let u = session.session.username();
                if !u.is_empty() {
                    self.session_user = u;
                }
                self.audio_attempt = 0;
                self.connection = Connection::Connected;
                self.remote = None;
                if let Some(h) = &self.engine {
                    h.send(PlaybackCommand::Attach(session.clone()));
                }
                let rebind = match self.source.clone() {
                    Some(DataSource::Spotify(b)) => {
                        let s = session.session.clone();
                        Task::perform(async move { b.rebind(s).await }, |()| {
                            Message::Done(Done::Noop)
                        })
                    }
                    _ => Task::none(),
                };
                self.session = Some(session);
                rebind
            }
            Done::Audio(Err(e)) => {
                if e.is_revoked() || self.source.is_none() {
                    return self.fail(&e);
                }
                self.schedule_audio_reconnect(&e)
            }
            Done::ReconnectAudio => {
                if self.source.is_some() && self.session.is_none() {
                    self.connection = Connection::Connecting;
                    self.connect_audio_task()
                } else {
                    Task::none()
                }
            }
            Done::Library(Ok(lib)) => {
                self.library = lib.clone();
                if self.is_mock() {
                    Task::none()
                } else {
                    store_write(&self.store, Kind::Library, "me".into(), lib)
                }
            }
            Done::Home(Ok(home)) => {
                self.loading.remove(&Route::Home);
                self.home = Some(home.clone());
                store_write(&self.store, Kind::Home, "me".into(), home)
            }
            Done::Lyrics(id, r) => {
                match r {
                    Ok(l) => {
                        if self.lyrics.as_ref().is_some_and(|(cur, _)| *cur == id) {
                            self.lyrics = Some((id, Some(l)));
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "lyrics unavailable");
                        self.lyrics = Some((id, Some(None)));
                    }
                }
                Task::none()
            }
            Done::Library(Err(e)) => self.fail(&e),
            Done::Home(Err(e)) => {
                self.loading.remove(&Route::Home);
                self.errors.insert(Route::Home, e.user_message());
                self.fail(&e)
            }
            Done::Playlist(id, Ok((mut pl, page))) => {
                let route = Route::Playlist(id.clone());
                self.loading.remove(&route);
                // Complete the header (mosaic) and the library entry.
                let first: Vec<Track> = page.items.iter().take(12).cloned().collect();
                let header = pl.clone();
                meta::fill_playlist(&mut pl, &header, &first);
                self.apply_fill(&id, &pl, &first);
                let flags = self.fetch_liked_flags(&page.items);
                let list = TrackList::from_page(page.clone());
                self.playlists.insert(id.clone(), (pl.clone(), list));
                Task::batch([
                    flags,
                    store_write(&self.store, Kind::Playlist, id, (pl, page)),
                ])
            }
            Done::PlaylistMore(id, Ok(page)) => {
                self.loading_more = false;
                if let Some((_, list)) = self.playlists.get_mut(&id) {
                    list.append(page);
                }
                Task::none()
            }
            Done::Liked(Ok(page)) => {
                self.loading.remove(&Route::Liked);
                self.loading_more = false;
                for t in &page.items {
                    self.liked_ids.insert(t.id.clone());
                }
                let first = page.offset == 0;
                if first {
                    self.liked = Some(TrackList::from_page(page.clone()));
                } else if let Some(list) = self.liked.as_mut() {
                    list.append(page.clone());
                }
                if first {
                    store_write(&self.store, Kind::Liked, "me".into(), page)
                } else {
                    Task::none()
                }
            }
            Done::Album(id, Ok(album)) => {
                self.loading.remove(&Route::Album(id.clone()));
                let flags = self.fetch_liked_flags(&album.tracks);
                self.albums.insert(id.clone(), album.clone());
                Task::batch([flags, store_write(&self.store, Kind::Album, id, album)])
            }
            Done::Artist(id, Ok(page)) => {
                self.loading.remove(&Route::Artist(id.clone()));
                let flags = self.fetch_liked_flags(&page.top_tracks);
                self.artists.insert(id.clone(), page.clone());
                Task::batch([flags, store_write(&self.store, Kind::Artist, id, page)])
            }
            Done::Playlist(id, Err(e)) => self.route_failed(Route::Playlist(id), &e),
            Done::Album(id, Err(e)) => self.route_failed(Route::Album(id), &e),
            Done::Artist(id, Err(e)) => self.route_failed(Route::Artist(id), &e),
            Done::Liked(Err(e)) => self.route_failed(Route::Liked, &e),
            Done::PlaylistMore(_, Err(e)) => {
                self.loading_more = false;
                self.fail(&e)
            }
            Done::SearchDebounced(seq) => {
                if seq != self.search.seq || self.search.query.trim().is_empty() {
                    return Task::none();
                }
                let Some(src) = self.source() else {
                    return Task::none();
                };
                self.search.loading = true;
                let q = self.search.query.trim().to_owned();
                Task::perform(async move { src.search(&q).await }, move |r| {
                    Message::Done(Done::Search(seq, r))
                })
            }
            Done::Search(seq, r) => {
                if seq != self.search.seq {
                    return Task::none();
                }
                self.search.loading = false;
                match r {
                    Ok(res) => {
                        self.search.error = None;
                        let flags = self.fetch_liked_flags(&res.tracks);
                        self.search.results = Some(res);
                        flags
                    }
                    Err(e) => {
                        self.search.error = Some(e.user_message());
                        self.fail(&e)
                    }
                }
            }
            Done::Image(url, r) => {
                self.image_requests.remove(&url);
                match r {
                    Ok(img) => self.images.insert(url, img),
                    Err(e) => tracing::debug!(error = %e, "image fetch failed"),
                }
                Task::none()
            }
            Done::LikeSaved(id, saved, r) => {
                if let Err(e) = r {
                    // Roll back the optimistic update.
                    if saved {
                        self.liked_ids.remove(&id);
                    } else {
                        self.liked_ids.insert(id);
                    }
                    return self.fail(&e);
                }
                // Liked Songs changed: refetch it next time it is opened.
                self.liked = None;
                Task::none()
            }
            Done::LikedFlags(ids, Ok(flags)) => {
                for (id, liked) in ids.into_iter().zip(flags) {
                    if liked {
                        self.liked_ids.insert(id);
                    } else {
                        self.liked_ids.remove(&id);
                    }
                }
                Task::none()
            }
            Done::LikedFlags(_, Err(e)) => {
                tracing::debug!(error = %e, "liked flags unavailable");
                Task::none()
            }
            Done::Screenshot(shot) => {
                let Some(plan) = self.screenshot.clone() else {
                    return Task::none();
                };
                Task::perform(
                    async move {
                        tokio::task::spawn_blocking(move || save_png(&plan.out, &shot))
                            .await
                            .map_err(|_| Error::Cancelled)?
                    },
                    |r| Message::Done(Done::ScreenshotSaved(r)),
                )
            }
            Done::ScreenshotSaved(r) => {
                match r {
                    Ok(()) => eprintln!("screenshot saved"),
                    Err(e) => eprintln!("screenshot failed: {e}"),
                }
                iced::exit()
            }
            Done::PlaylistCreated(tmp, r) => self.on_playlist_created(tmp, r),
            Done::PlaylistEdited(snap, what, r) => self.on_playlist_edited(*snap, what, r),
            Done::SoloistKey(r) => {
                self.on_soloist_key(r);
                Task::none()
            }
            Done::SoloistKeySaved(r) => self.on_soloist_key_saved(r),
            Done::AutostartLoaded(on) => {
                self.autostart = on;
                Task::none()
            }
            Done::AutostartChanged(r) => {
                match r {
                    Ok(on) => self.autostart = on,
                    Err(e) => self.show_toast(&e),
                }
                Task::none()
            }
            Done::Quit => self.quit(),
            Done::ConnectSent(Ok(())) => Task::none(),
            Done::OwnerNames(v) => self.on_owner_names(v),
            Done::PlaylistFill(v) => self.on_playlist_fill(v),
            Done::UserProfile(u, r) => {
                self.loading.remove(&Route::Profile(u.clone()));
                match r {
                    Ok(p) => {
                        self.owner_names
                            .insert(playlists::owner_username(&u), p.name.clone());
                        self.profiles.insert(u, p);
                        Task::none()
                    }
                    Err(e) => self.route_failed(Route::Profile(u), &e),
                }
            }
            Done::CacheSize(r) => {
                self.cache_bytes = r.ok();
                Task::none()
            }
            Done::CacheCleared(r) => {
                match r {
                    Ok(()) => self.show_toast("Cache cleared"),
                    Err(e) => self.show_toast(e.user_message()),
                }
                self.measure_cache()
            }
            Done::SettingsSaved(r) => {
                if let Err(e) = r {
                    tracing::warn!(error = %e, "settings not saved");
                    self.show_toast("Could not save the settings");
                }
                Task::none()
            }
            Done::ConnectSent(Err(e)) => {
                tracing::warn!(error = %e, "connect command failed");
                self.show_toast(format!("Could not reach the device: {}", e.user_message()));
                self.engine_send(PlaybackCommand::RefreshDevices);
                Task::none()
            }
            Done::Noop => Task::none(),
        }
    }

    /// Fetch lyrics for the current track when the lyrics panel is open.
    fn request_lyrics(&mut self) -> Task<Message> {
        if !self.show_lyrics {
            return Task::none();
        }
        let Some(id) = self
            .now_track()
            .map(|t| t.id.clone())
            .filter(|i| !i.is_empty())
        else {
            return Task::none();
        };
        if self.lyrics.as_ref().is_some_and(|(cur, _)| *cur == id) {
            return Task::none();
        }
        let Some(src) = self.source() else {
            return Task::none();
        };
        self.lyrics = Some((id.clone(), None));
        let id2 = id.clone();
        Task::perform(async move { src.lyrics(id2).await }, move |r| {
            Message::Done(Done::Lyrics(id.clone(), r))
        })
    }

    fn route_failed(&mut self, route: Route, e: &Error) -> Task<Message> {
        self.loading.remove(&route);
        self.errors.insert(route, e.user_message());
        if e.is_revoked() {
            self.fail(e)
        } else {
            Task::none()
        }
    }

    // ------------------------------------------------------------ screenshot

    fn prepare_screenshot(&mut self, plan: &ScreenshotPlan) -> Task<Message> {
        // Pre-populate a playing queue so the now-playing bar has content.
        if plan.screen != "login"
            && plan.screen != "keyring"
            && let Ok((pl, page)) = crate::mock::playlist("pl01")
        {
            self.queue.play_context(
                PlayContext {
                    name: pl.name.clone(),
                    route: Some(Route::Playlist(pl.id.clone())),
                },
                page.items.clone(),
                Some(2),
            );
            self.playback.duration_ms = self.queue.current.as_ref().map_or(0, |t| t.duration_ms);
            self.queue.enqueue(page.items[7].clone());
            self.queue.enqueue(page.items[9].clone());
            if let Some(t) = &self.queue.current {
                self.liked_ids.insert(t.id.clone());
            }
        }
        let mut tasks = Vec::new();
        match plan.screen.as_str() {
            "login" => {
                self.auth = AuthState::SignedOut { error: None };
                self.source = None;
            }
            "search" => {
                self.nav = Nav::new(Route::Search);
                self.search.query = "nova".into();
                self.search.seq = 1;
                tasks.push(self.on_done(Done::SearchDebounced(1)));
            }
            "library" => {
                self.nav = Nav::new(Route::Library);
            }
            "playlist" => {
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Playlist("pl01".into())));
            }
            "album" => {
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Album("al03".into())));
            }
            "artist" => {
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Artist("ar03".into())));
            }
            "liked" => {
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Liked));
            }
            "keyring" => {
                self.auth = AuthState::KeyringUnavailable {
                    detail: "no Secret Service / keyring is available".into(),
                };
                self.source = None;
            }
            "lyrics" => {
                self.show_lyrics = true;
                self.nav = Nav::new(Route::Home);
            }
            "connect" => {
                // Another device casting to us through Spotify Connect.
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Artist("ar03".into())));
                if let Ok(a) = crate::mock::artist_page("ar03")
                    && let Some(t) = a.top_tracks.first()
                {
                    self.on_remote(RemoteEvent::Track(RemoteTrack {
                        uri: t.uri.clone(),
                        name: t.name.clone(),
                        artists: t.artists.iter().map(|a| a.name.clone()).collect(),
                        album: t.album.as_ref().map(|a| a.name.clone()).unwrap_or_default(),
                        cover_url: pick_image(t.images(), 300).map(|i| i.url.clone()),
                        duration_ms: t.duration_ms,
                    }));
                    self.on_remote(RemoteEvent::Playing {
                        position_ms: 41_000,
                    });
                }
            }
            "menu" | "shortcuts" | "rename" => {
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Playlist("pl01".into())));
                match plan.screen.as_str() {
                    "menu" => {
                        if let Ok((_, page)) = crate::mock::playlist("pl01")
                            && let Some(t) = page.items.get(2)
                        {
                            self.menu = Some(menu::ContextMenu {
                                target: menu::MenuTarget::track(t, Some(("pl01".into(), 2))),
                                at: iced::Point::new(620.0, 330.0),
                                picker: true,
                            });
                        }
                    }
                    "shortcuts" => self.dialog = Some(menu::Dialog::Shortcuts),
                    _ => {
                        self.dialog = Some(menu::Dialog::Rename {
                            id: "pl01".into(),
                            name: "Late Night Drive".into(),
                        })
                    }
                }
            }
            "settings" => {
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Settings));
                self.cache_bytes = Some(23_400_000);
                // Show the bottom of the page, where the window / tray
                // options live.
                tasks.push(
                    Task::perform(
                        async { tokio::time::sleep(Duration::from_millis(1200)).await },
                        |()| (),
                    )
                    .then(|()| {
                        iced::widget::operation::snap_to(
                            crate::ui::MAIN_SCROLL,
                            iced::widget::scrollable::RelativeOffset { x: 0.0, y: 1.0 },
                        )
                    }),
                );
            }
            "update-banner" | "updates" => {
                self.nav = Nav::new(Route::Home);
                self.updater.screenshot_state();
                if plan.screen == "updates" {
                    tasks.push(self.navigate(Route::Settings));
                    self.cache_bytes = Some(23_400_000);
                    tasks.push(
                        Task::perform(
                            async { tokio::time::sleep(Duration::from_millis(1200)).await },
                            |()| (),
                        )
                        .then(|()| {
                            iced::widget::operation::snap_to(
                                crate::ui::MAIN_SCROLL,
                                iced::widget::scrollable::RelativeOffset { x: 0.0, y: 0.85 },
                            )
                        }),
                    );
                }
            }
            "soloist" => {
                // First use: Spotify refused librespot's audio key.
                self.nav = Nav::new(Route::Home);
                self.soloist_screenshot(false);
            }
            "soloist-settings" => {
                // Soloist on and ready (the mock engine walks the states).
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Settings));
                self.cache_bytes = Some(23_400_000);
                self.soloist_screenshot(true);
            }
            "profile" => {
                self.nav = Nav::new(Route::Home);
                self.profiles
                    .insert("mockuser".into(), crate::mock::profile("mockuser"));
                tasks.push(self.navigate(Route::Profile("mockuser".into())));
            }
            "devices" | "denied" => {
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Library));
                let mut cluster = crate::mock::cluster();
                if plan.screen == "denied" {
                    cluster.active_device_id = None;
                    for d in &mut cluster.devices {
                        d.is_active = false;
                    }
                    cluster.player = None;
                    self.local_denied = true;
                    let reason =
                        crate::net::audio_diag::UnavailableReason::AudioKeyDenied { code: 1 };
                    self.playback_block = Some(PlaybackBlock {
                        reason,
                        message: format!(
                            "{} You can play on another device instead.",
                            reason.describe()
                        ),
                    });
                    self.last_device = Some(("phone".into(), "Pixel 8".into()));
                } else {
                    self.device_picker = true;
                }
                tasks.push(self.on_cluster(cluster));
            }
            "queue" => {
                self.show_queue = true;
                self.nav = Nav::new(Route::Home);
                tasks.push(self.navigate(Route::Album("al01".into())));
            }
            _ => {
                self.nav = Nav::new(Route::Home);
            }
        }
        tasks.push(
            Task::perform(
                async move { tokio::time::sleep(Duration::from_millis(2500)).await },
                |()| (),
            )
            .then(|()| iced::window::latest())
            .then(|id| match id {
                Some(id) => iced::window::screenshot(id),
                None => Task::none(),
            })
            .map(|shot| Message::Done(Done::Screenshot(shot))),
        );
        Task::batch(tasks)
    }
}

/// 40 lowercase hex characters, the form Spotify clients use.
fn new_device_id() -> String {
    let bytes: [u8; 20] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The persisted device id, created on first use.
fn load_device_id(store: &Store) -> String {
    if let Some(id) = store
        .setting("device_id")
        .ok()
        .flatten()
        .filter(|d| is_device_id(d))
    {
        return id;
    }
    let id = new_device_id();
    if let Err(e) = store.set_setting("device_id", &id) {
        tracing::warn!(error = %e, "could not persist the device id");
    }
    id
}

fn is_device_id(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Track model for what the Connect device plays.
fn remote_track(t: &RemoteTrack) -> Track {
    let id = crate::net::types::split_uri(&t.uri)
        .map(|(_, id)| id.to_owned())
        .unwrap_or_default();
    Track {
        id,
        uri: t.uri.clone(),
        name: t.name.clone(),
        artists: t
            .artists
            .iter()
            .map(|a| ArtistRef {
                id: String::new(),
                name: a.clone(),
            })
            .collect(),
        album: Some(AlbumRef {
            id: String::new(),
            name: t.album.clone(),
            images: t
                .cover_url
                .iter()
                .map(|u| Image {
                    url: u.clone(),
                    width: Some(300),
                    height: Some(300),
                })
                .collect(),
        }),
        duration_ms: t.duration_ms,
        explicit: false,
        playable: true,
        added_at: None,
    }
}

fn save_png(path: &std::path::Path, shot: &iced::window::Screenshot) -> Result<()> {
    let (w, h) = (shot.size.width, shot.size.height);
    let img = image::RgbaImage::from_raw(w, h, shot.rgba.to_vec())
        .ok_or_else(|| Error::Io("screenshot buffer has the wrong size".into()))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    img.save_with_format(path, image::ImageFormat::Png)
        .map_err(|e| Error::Io(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_app() -> App {
        let (app, _) = App::boot(Config::for_tests(), Options::default());
        app
    }

    fn feed(app: &mut App, d: Done) {
        let _ = app.update(Message::Done(d));
    }

    /// A real (non-mock) app with a fake Web source and a fake engine.
    fn real_app_playing() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<PlaybackCommand>,
        Vec<Track>,
    ) {
        let mut cfg = Config::for_tests();
        cfg.mock = false;
        let (mut app, _) = App::boot(cfg, Options::default());
        app.secrets = Arc::new(MemoryStore::new());
        app.store = Store::in_memory().unwrap();
        app.source = Some(DataSource::Mock);
        app.auth = AuthState::SignedIn {
            user: crate::mock::user(),
        };
        let (h, rx) = PlaybackHandle::channel();
        app.engine = Some(h);
        let (pl, page) = crate::mock::playlist("pl01").unwrap();
        app.playlists
            .insert("pl01".into(), (pl, TrackList::from_page(page.clone())));
        (app, rx, page.items)
    }

    fn fake_session() -> SpotifySession {
        SpotifySession {
            session: librespot_core::Session::new(librespot_core::SessionConfig::default(), None),
            connect: None,
            dealer: false,
        }
    }

    fn drain_loads(rx: &mut tokio::sync::mpsc::UnboundedReceiver<PlaybackCommand>) -> Vec<String> {
        let mut v = Vec::new();
        while let Ok(c) = rx.try_recv() {
            if let PlaybackCommand::Load { uri, .. } = c {
                v.push(uri);
            }
        }
        v
    }

    fn unavailable(app: &mut App, reason: crate::net::audio_diag::UnavailableReason) {
        let uri = app.queue.current.as_ref().unwrap().uri.clone();
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Unavailable {
            uri,
            reason,
        })));
    }

    #[tokio::test]
    async fn unavailable_tracks_skip_at_most_three_then_one_banner() {
        use crate::net::audio_diag::UnavailableReason as R;
        let (mut app, mut rx, tracks) = real_app_playing();
        app.session = Some(fake_session());
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(0),
        )));
        drain_loads(&mut rx);
        let mut toast_at = None;
        for i in 0..3 {
            unavailable(&mut app, R::Unknown);
            assert_eq!(app.queue.current.as_ref().unwrap().uri, tracks[i + 1].uri);
            assert_eq!(drain_loads(&mut rx).len(), 1, "one skip per failure");
            let (msg, at) = app.toast.clone().unwrap();
            assert!(msg.contains("Skipping"));
            // Identical toasts keep their timer: no flicker.
            if let Some(prev) = toast_at {
                assert_eq!(at, prev);
            }
            toast_at = Some(at);
        }
        unavailable(&mut app, R::Unknown);
        assert_eq!(
            app.queue.current.as_ref().unwrap().uri,
            tracks[3].uri,
            "no 4th skip"
        );
        assert!(drain_loads(&mut rx).is_empty());
        assert!(app.toast.is_none(), "the banner replaces the toast");
        let block = app.playback_block.clone().unwrap();
        assert!(block.message.contains("4 songs in a row"));
        assert_eq!(app.playback.status, PlayStatus::Paused);
        // Starting something new clears the banner and the streak.
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(5),
        )));
        assert!(app.playback_block.is_none());
        assert_eq!(drain_loads(&mut rx).len(), 1);
    }

    #[tokio::test]
    async fn audio_key_denial_stops_at_once_and_offers_a_device() {
        use crate::net::audio_diag::UnavailableReason as R;
        use crate::net::connect_ctl::{ClusterState, DeviceKind, RemoteDevice};
        let (mut app, mut rx, tracks) = real_app_playing();
        app.session = Some(fake_session());
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(0),
        )));
        drain_loads(&mut rx);
        unavailable(&mut app, R::AudioKeyDenied { code: 1 });
        assert_eq!(
            app.queue.current.as_ref().unwrap().uri,
            tracks[0].uri,
            "no skip"
        );
        assert!(app.local_denied);
        let block = app.playback_block.clone().unwrap();
        assert!(block.message.contains("audio key denied"));
        let mut refreshed = false;
        while let Ok(c) = rx.try_recv() {
            refreshed |= matches!(c, PlaybackCommand::RefreshDevices);
        }
        assert!(refreshed, "the device list is fetched right away");

        // A phone shows up: it is suggested, and play goes there.
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Cluster(
            ClusterState {
                active_device_id: None,
                devices: vec![RemoteDevice {
                    id: "phone".into(),
                    name: "Pixel".into(),
                    kind: DeviceKind::Smartphone,
                    volume: Some(0.5),
                    is_active: false,
                }],
                player: None,
            },
        ))));
        assert_eq!(app.suggested_device().unwrap().1, "Pixel");
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(2),
        )));
        assert!(drain_loads(&mut rx).is_empty(), "nothing loads locally");
        assert_eq!(app.remote_device(), Some("phone"));
        assert_eq!(app.now_track().unwrap().uri, tracks[2].uri);
        assert!(app.playback_block.is_none());
    }

    #[tokio::test]
    async fn transient_key_refusal_retries_once() {
        use crate::net::audio_diag::UnavailableReason as R;
        let (mut app, mut rx, tracks) = real_app_playing();
        app.session = Some(fake_session());
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(0),
        )));
        drain_loads(&mut rx);
        unavailable(&mut app, R::AudioKeyTransient { code: 2 });
        assert_eq!(
            drain_loads(&mut rx),
            vec![tracks[0].uri.clone()],
            "same track again"
        );
        unavailable(&mut app, R::AudioKeyTransient { code: 2 });
        assert_eq!(
            drain_loads(&mut rx),
            vec![tracks[1].uri.clone()],
            "then skipped"
        );
    }

    #[test]
    fn cluster_with_active_remote_device_drives_the_player_bar() {
        use crate::net::connect_ctl::{
            ClusterPlayer, ClusterState, ClusterTrack, DeviceKind, RemoteDevice,
        };
        let mut app = mock_app();
        let state = ClusterState {
            active_device_id: Some("desk".into()),
            devices: vec![RemoteDevice {
                id: "desk".into(),
                name: "Office".into(),
                kind: DeviceKind::Computer,
                volume: Some(0.25),
                is_active: true,
            }],
            player: Some(ClusterPlayer {
                track: Some(ClusterTrack {
                    uri: "spotify:track:abc".into(),
                    name: "Song".into(),
                    artists: vec!["Band".into()],
                    album: "LP".into(),
                    cover_url: None,
                }),
                is_playing: true,
                position_ms: 1000,
                duration_ms: 200_000,
                shuffle: true,
                repeat_track: true,
                ..ClusterPlayer::default()
            }),
        };
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Cluster(
            state.clone(),
        ))));
        assert_eq!(app.remote.as_ref().unwrap().device_name(), Some("Office"));
        assert_eq!(app.now_track().unwrap().name, "Song");
        assert!(app.now_playback().is_playing());
        assert!(app.shown_shuffle());
        assert_eq!(app.shown_repeat(), Repeat::Track);
        assert_eq!(app.last_device.as_ref().unwrap().0, "desk");
        // Controls go to the device (optimistic state).
        let _ = app.update(ui(Ui::TogglePlay));
        assert!(!app.now_playback().is_playing());
        // The device stops being active: back to the local player.
        let mut idle = state;
        idle.active_device_id = None;
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Cluster(idle))));
        assert!(app.remote.is_none());
    }

    #[test]
    fn ownership_does_not_need_the_profile() {
        let mut app = mock_app();
        app.user = None;
        app.session_user = "0d2xmxvdik4b9wpltcoke".into();
        let mut p = crate::mock::playlist("pl01").unwrap().0;
        p.owner = "0d2xmxvdik4b9wpltcoke".into();
        assert!(app.owns(&p));
        p.owner = "spotify:user:0D2XMXVDIK4B9WPLTCOKE".into();
        assert!(app.owns(&p), "URI form and case do not matter");
        p.owner = "someoneelse".into();
        assert!(!app.owns(&p));
        // Collaborative: Spotify says we may edit it.
        p.can_edit = Some(true);
        assert!(app.owns(&p));
        // And Spotify's "no" wins over a matching owner.
        p.owner = "0d2xmxvdik4b9wpltcoke".into();
        p.can_edit = Some(false);
        assert!(!app.owns(&p));
        // No session username and no profile: nothing is editable.
        app.session_user.clear();
        p.can_edit = None;
        assert!(!app.owns(&p));
    }

    #[test]
    fn owner_names_and_missing_metadata_fill_in_mock_mode() {
        let mut app = mock_app();
        feed(&mut app, Done::Library(Ok(crate::mock::library())));
        let bare = app
            .library
            .playlists
            .iter()
            .find(|p| p.id == "pl08")
            .unwrap();
        assert!(bare.name.is_empty() && bare.images.is_empty());
        assert_eq!(app.owner_label("spotify"), "Spotify");
        assert_eq!(app.owner_label("mockuser"), "Mock Listener");
        // The batches are started once, not on every update.
        let _ = app.request_playlist_fills();
        assert!(app.fill_requested.contains("pl08"));
        let _ = app.request_owner_names();
        assert!(app.owner_requested.contains("x7friend"));
        assert!(!app.owner_requested.contains("spotify"));
        assert!(
            !app.owner_requested.contains("mockuser"),
            "own name comes from the profile"
        );
        // Results arrive.
        let (header, page) = crate::mock::playlist("pl08").unwrap();
        feed(
            &mut app,
            Done::PlaylistFill(vec![("pl08".into(), header.clone(), page.items)]),
        );
        let filled = app
            .library
            .playlists
            .iter()
            .find(|p| p.id == "pl08")
            .unwrap();
        assert_eq!(filled.name, header.name);
        assert!(!filled.images.is_empty());
        feed(
            &mut app,
            Done::OwnerNames(vec![("x7friend".into(), "Riley".into())]),
        );
        assert_eq!(app.owner_label("x7friend"), "Riley");
    }

    #[test]
    fn settings_change_restarts_engine_and_routes_open() {
        let mut cfg = Config::for_tests();
        cfg.mock = false;
        let (mut app, _) = App::boot(cfg, Options::default());
        let before = app.engine_kind;
        let _ = app.update(ui(Ui::Setting(settings::SettingChange::Quality(
            config::Bitrate::High,
        ))));
        assert_ne!(app.engine_kind, before, "new engine for the new bitrate");
        assert_eq!(app.settings.bitrate, config::Bitrate::High);
        let _ = app.update(ui(Ui::ToggleUserMenu));
        assert!(app.user_menu);
        let _ = app.update(ui(Ui::Navigate(Route::Settings)));
        assert!(!app.user_menu, "navigating closes the account menu");
        assert_eq!(*app.route(), Route::Settings);
        assert_eq!(
            Route::from_key(&Route::Profile("u1".into()).key()),
            Some(Route::Profile("u1".into()))
        );
    }

    #[tokio::test]
    async fn lost_session_pauses_and_reloads_instead_of_skipping() {
        let (mut app, mut rx, tracks) = real_app_playing();
        // Pretend a session was attached (librespot sessions can't be built
        // without a network, so use the "had a session" path via the flag).
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(0),
        )));
        let uri = tracks[0].uri.clone();
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Playing {
            uri: uri.clone(),
            position_ms: 30_000,
        })));
        while rx.try_recv().is_ok() {}

        // An unconnected librespot session stands in for the dead one.
        app.session = Some(SpotifySession {
            session: librespot_core::Session::new(librespot_core::SessionConfig::default(), None),
            connect: None,
            dealer: false,
        });
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::SessionLost)));
        assert!(app.session.is_none());
        assert_eq!(app.connection, Connection::Reconnecting { attempt: 0 });
        assert_eq!(app.playback.status, PlayStatus::Paused);

        // Unavailable while the session is gone must not skip the queue.
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Unavailable {
            uri: uri.clone(),
            reason: crate::net::audio_diag::UnavailableReason::Unknown,
        })));
        assert_eq!(app.queue.current.as_ref().unwrap().uri, uri);
        assert_eq!(app.playback.status, PlayStatus::Paused);

        // Pressing play reloads the track at the saved position.
        let _ = app.update(ui(Ui::TogglePlay));
        let mut reloaded = None;
        while let Ok(cmd) = rx.try_recv() {
            if let PlaybackCommand::Load {
                uri, position_ms, ..
            } = cmd
            {
                reloaded = Some((uri, position_ms));
            }
        }
        let (u, pos) = reloaded.expect("track reloaded");
        assert_eq!(u, uri);
        assert!(pos >= 30_000);
    }

    #[test]
    fn session_lost_schedules_reconnect_with_backoff() {
        let (mut app, _rx, _) = real_app_playing();
        app.reload_on_play = false;
        // Simulate an established session being reported dead twice.
        app.audio_attempt = 0;
        let _ = app.schedule_audio_reconnect(&Error::Net("x".into()));
        assert_eq!(app.connection, Connection::Reconnecting { attempt: 0 });
        let _ = app.schedule_audio_reconnect(&Error::Net("x".into()));
        assert_eq!(app.connection, Connection::Reconnecting { attempt: 1 });
        let _ = app.update(Message::Done(Done::ReconnectAudio));
        assert_eq!(
            app.connection,
            Connection::Connecting,
            "retries while no session"
        );
    }

    #[test]
    fn mock_boot_is_signed_in() {
        let app = mock_app();
        assert!(app.auth.is_signed_in());
        assert!(app.source.is_some());
        assert_eq!(app.route(), &Route::Home);
    }

    #[test]
    fn playing_a_playlist_and_advancing() {
        let mut app = mock_app();
        let (h, mut rx) = PlaybackHandle::channel();
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Ready(h))));
        let (pl, page) = crate::mock::playlist("pl01").unwrap();
        feed(
            &mut app,
            Done::Playlist("pl01".into(), Ok((pl, page.clone()))),
        );
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(1),
        )));
        assert_eq!(app.queue.current.as_ref().unwrap().id, page.items[1].id);
        // Drain: SetVolume, Load
        let mut loads = 0;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, PlaybackCommand::Load { .. }) {
                loads += 1;
            }
        }
        assert_eq!(loads, 1);
        let uri = page.items[1].uri.clone();
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Playing {
            uri: uri.clone(),
            position_ms: 0,
        })));
        assert_eq!(app.playback.status, PlayStatus::Playing);
        // A stale event for another track is ignored.
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::EndOfTrack {
            uri: "spotify:track:other".into(),
        })));
        assert_eq!(app.queue.current.as_ref().unwrap().id, page.items[1].id);
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::EndOfTrack {
            uri,
        })));
        assert_eq!(app.queue.current.as_ref().unwrap().id, page.items[2].id);
        let snap = app.media_snapshot();
        assert_eq!(snap.track.unwrap().title, page.items[2].name);
    }

    #[test]
    fn navigation_and_search_debounce() {
        let mut app = mock_app();
        let _ = app.update(ui(Ui::SearchInput("nova".into())));
        assert_eq!(app.route(), &Route::Search);
        let seq = app.search.seq;
        let _ = app.update(ui(Ui::SearchInput("nova h".into())));
        // The stale debounce is ignored, the current one starts a search.
        feed(&mut app, Done::SearchDebounced(seq));
        assert!(!app.search.loading);
        feed(&mut app, Done::SearchDebounced(seq + 1));
        assert!(app.search.loading);
        feed(&mut app, Done::Search(seq, Ok(SearchResults::default())));
        assert!(app.search.loading, "stale results are dropped");
        feed(
            &mut app,
            Done::Search(seq + 1, Ok(crate::mock::search("nova"))),
        );
        assert!(!app.search.loading);
        assert!(app.search.results.is_some());
        let _ = app.update(ui(Ui::Back));
        assert_eq!(app.route(), &Route::Home);
    }

    #[test]
    fn revoked_error_logs_out() {
        let mut app = mock_app();
        feed(&mut app, Done::Library(Err(Error::revoked("gone"))));
        assert!(matches!(app.auth, AuthState::SignedOut { error: Some(_) }));
        assert!(app.source.is_none());
    }

    #[test]
    fn like_is_optimistic_and_rolls_back() {
        let mut app = mock_app();
        let t = crate::mock::album("al01").unwrap().tracks[1].clone();
        let _ = app.update(ui(Ui::ToggleLike(t.clone())));
        assert!(app.liked_ids.contains(&t.id));
        feed(
            &mut app,
            Done::LikeSaved(t.id.clone(), true, Err(Error::Net("x".into()))),
        );
        assert!(!app.liked_ids.contains(&t.id));
    }

    #[test]
    fn media_commands_map_to_actions() {
        let mut app = mock_app();
        let _ = app.update(Message::Net(Net::Media(MediaEvent::Command(
            MediaCommand::SetShuffle(true),
        ))));
        assert!(app.queue.shuffle);
        let _ = app.update(Message::Net(Net::Media(MediaEvent::Command(
            MediaCommand::SetLoop(MediaLoop::Track),
        ))));
        assert_eq!(app.queue.repeat, Repeat::Track);
        let _ = app.update(Message::Net(Net::Media(MediaEvent::Command(
            MediaCommand::SetVolume(0.25),
        ))));
        assert!((app.playback.volume - 0.25).abs() < 1e-6);
        assert_eq!(app.media_snapshot().loop_status, MediaLoop::Track);
    }

    #[test]
    fn preload_event_sends_the_next_track_to_the_engine() {
        let (mut app, mut rx, tracks) = real_app_playing();
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(0),
        )));
        while rx.try_recv().is_ok() {}
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::PreloadNext)));
        match rx.try_recv() {
            Ok(PlaybackCommand::Preload { uri }) => assert_eq!(uri, tracks[1].uri),
            other => panic!("expected a preload, got {other:?}"),
        }
        // Repeat-one has nothing different to preload.
        let _ = app.update(ui(Ui::CycleRepeat));
        let _ = app.update(ui(Ui::CycleRepeat));
        assert_eq!(app.queue.repeat, Repeat::Track);
        while rx.try_recv().is_ok() {}
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::PreloadNext)));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn connect_playback_takes_over_the_player_bar() {
        let (mut app, mut rx, tracks) = real_app_playing();
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(0),
        )));
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Playing {
            uri: tracks[0].uri.clone(),
            position_ms: 1_000,
        })));
        while rx.try_recv().is_ok() {}
        let remote = |e| Message::Net(Net::Playback(PlaybackEvent::Remote(e)));
        let _ = app.update(remote(RemoteEvent::Track(RemoteTrack {
            uri: "spotify:track:remote1".into(),
            name: "From the phone".into(),
            artists: vec!["Someone".into()],
            album: "Elsewhere".into(),
            cover_url: Some("https://i.scdn.co/image/x".into()),
            duration_ms: 200_000,
        })));
        let _ = app.update(remote(RemoteEvent::Playing { position_ms: 5_000 }));
        assert_eq!(app.now_track().unwrap().name, "From the phone");
        assert_eq!(app.media_snapshot().track.unwrap().title, "From the phone");
        assert_eq!(
            app.playback.status,
            PlayStatus::Paused,
            "local queue yields"
        );
        // Transport controls go to the Connect device.
        let _ = app.update(ui(Ui::TogglePlay));
        let _ = app.update(ui(Ui::Next));
        let cmds: Vec<_> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert!(matches!(
            cmds.as_slice(),
            [
                PlaybackCommand::Remote(RemoteCommand::Pause),
                PlaybackCommand::Remote(RemoteCommand::Next)
            ]
        ));
        // The phone moves playback elsewhere: back to the local queue.
        let _ = app.update(remote(RemoteEvent::Stopped));
        assert!(app.remote.is_none());
        assert_eq!(app.now_track().unwrap().uri, tracks[0].uri);
    }

    // ------------------------------------------------------------ soloist

    use crate::net::soloist::protocol::{Command as SCmd, Event as SEv};
    use crate::net::soloist::{Control, EngineStatus, SoloistEvent, SoloistHandle};

    /// A real app with Soloist on, ready and connected, and a captured
    /// command channel.
    fn soloist_app() -> (
        App,
        tokio::sync::mpsc::UnboundedReceiver<PlaybackCommand>,
        tokio::sync::mpsc::UnboundedReceiver<Control>,
        Vec<Track>,
    ) {
        let (mut app, rx, tracks) = real_app_playing();
        app.settings.engine = config::AudioEngine::Soloist;
        let (h, ctl) = SoloistHandle::channel();
        let sol = |e| Message::Net(Net::Soloist(e));
        let _ = app.update(sol(SoloistEvent::Ready(h)));
        feed(
            &mut app,
            Done::SoloistKey(Ok(Some(crate::auth::secrets::Secret::new(
                "test-key-123456",
            )))),
        );
        let _ = app.update(sol(SoloistEvent::Status(EngineStatus::Ready)));
        let _ = app.update(sol(SoloistEvent::Connected(true)));
        (app, rx, ctl, tracks)
    }

    fn ws_cmds(ctl: &mut tokio::sync::mpsc::UnboundedReceiver<Control>) -> Vec<SCmd> {
        std::iter::from_fn(|| ctl.try_recv().ok())
            .filter_map(|c| match c {
                Control::Ws(c) => Some(c),
                _ => None,
            })
            .collect()
    }

    fn sws(app: &mut App, e: SEv) {
        let _ = app.update(Message::Net(Net::Soloist(SoloistEvent::Ws(e))));
    }

    #[test]
    fn soloist_turns_off_our_connect_device_and_local_player() {
        let mut s = config::Settings {
            engine: config::AudioEngine::Soloist,
            ..config::Settings::default()
        };
        let o = session_options(&Config::for_tests(), &s, 0.5, "id");
        assert!(o.connect.is_none(), "no second Ferrotune device");
        assert!(o.controller, "the Connect controller stays");
        assert!(matches!(
            engine_kind(false, &s),
            EngineKind::Librespot(_, _, false)
        ));
        s.engine = config::AudioEngine::Builtin;
        let o = session_options(&Config::for_tests(), &s, 0.5, "id");
        assert!(o.connect.is_some() && !o.controller);
        assert!(matches!(
            engine_kind(false, &s),
            EngineKind::Librespot(_, _, true)
        ));
    }

    #[test]
    fn soloist_plays_and_follows_its_events() {
        let (mut app, mut rx, mut ctl, tracks) = soloist_app();
        let configured = std::iter::from_fn(|| ctl.try_recv().ok())
            .any(|c| matches!(c, Control::Configure { key: Some(_), .. }));
        assert!(configured, "the key from the keyring starts the engine");
        // No cluster yet: the WebSocket fallback plays the chosen track.
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(2),
        )));
        assert_eq!(
            ws_cmds(&mut ctl),
            vec![SCmd::Activate, SCmd::Play(Some(tracks[2].uri.clone()))]
        );
        assert!(drain_loads(&mut rx).is_empty(), "librespot plays nothing");
        assert!(app.soloist_playing_here());
        // Soloist reports what it plays: the bar and MPRIS follow.
        let snap: crate::net::soloist::protocol::PlaybackSnapshot =
            serde_json::from_value(serde_json::json!({
                "status": "playing", "is_active": true, "volume": 40,
                "item": {"uri": "spotify:track:abc", "entity_type": "track", "decorations": {
                    "identity": {"name": "From Soloist"},
                    "playback": {"duration_ms": 200000},
                    "creators": [{"entity": {"decorations": {"identity": {"name": "Band"}}}}]
                }},
                "position": {"position_ms": 1000, "timestamp_ms": 0, "speed": 0.0},
                "options": {"shuffle": true, "repeat": "context"}
            }))
            .unwrap();
        sws(&mut app, SEv::PlaybackState(Box::new(snap)));
        assert_eq!(app.now_track().unwrap().name, "From Soloist");
        assert_eq!(app.media_snapshot().track.unwrap().title, "From Soloist");
        assert!(app.now_playback().is_playing());
        assert!(app.shown_shuffle());
        assert_eq!(app.shown_repeat(), Repeat::Context);
        assert!((app.now_playback().volume - 0.4).abs() < 1e-6);
        assert!(
            super::super::ui::devices::playing_on(&app).is_none(),
            "this computer"
        );
        // Transport goes to the WebSocket API.
        let _ = app.update(ui(Ui::TogglePlay));
        let _ = app.update(ui(Ui::Next));
        let _ = app.update(ui(Ui::Volume(0.5)));
        let _ = app.update(ui(Ui::CycleRepeat));
        let _ = app.update(ui(Ui::ToggleShuffle));
        let _ = app.update(ui(Ui::Enqueue(tracks[5].clone())));
        assert_eq!(
            ws_cmds(&mut ctl),
            vec![
                SCmd::Pause,
                SCmd::SkipNext,
                SCmd::SetVolume(50),
                SCmd::SetRepeatContext(false),
                SCmd::SetRepeatTrack(true),
                SCmd::SetShuffle(false),
                SCmd::AddToQueue(tracks[5].uri.clone()),
            ]
        );
        sws(
            &mut app,
            SEv::PlaybackChanged {
                status: crate::net::soloist::protocol::Status::Paused,
            },
        );
        assert!(!app.now_playback().is_playing());
        // Another device takes over: the bar leaves Soloist.
        sws(
            &mut app,
            SEv::DeviceChanged {
                is_active: false,
                device_name: String::new(),
            },
        );
        assert!(app.remote.is_none());
    }

    #[test]
    fn soloist_in_the_cluster_gets_contexts_through_connect_state() {
        let (mut app, _rx, mut ctl, tracks) = soloist_app();
        let mut c = crate::mock::cluster();
        c.devices.push(crate::net::connect_ctl::RemoteDevice {
            id: "soloist-device-id".into(),
            name: app.settings_device_name(),
            kind: crate::net::connect_ctl::DeviceKind::Computer,
            volume: Some(0.5),
            is_active: false,
        });
        c.active_device_id = None;
        for d in &mut c.devices {
            d.is_active = false;
        }
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Cluster(c))));
        assert_eq!(
            app.soloist_device().map(|d| d.0).as_deref(),
            Some("soloist-device-id")
        );
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(3),
        )));
        // Start-at-track needs connect-state (skip_to): nothing on the WS.
        assert!(ws_cmds(&mut ctl).is_empty());
        assert_eq!(app.remote_device(), Some("soloist-device-id"));
        assert_eq!(app.now_track().unwrap().uri, tracks[3].uri);
        // A whole context from the top fits the WebSocket `play`.
        let _ = app.update(ui(Ui::PlayFrom(TrackSource::Album("al01".into()), None)));
        assert_eq!(
            ws_cmds(&mut ctl),
            vec![
                SCmd::Activate,
                SCmd::Play(Some("spotify:album:al01".into()))
            ]
        );
        // The picker lists Soloist as "This computer", not as a device.
        assert!(app.is_soloist_id("soloist-device-id"));
        assert!(!app.is_soloist_id("phone"));
    }

    #[test]
    fn playing_before_soloist_is_ready_waits_and_shows_the_card() {
        let (mut app, mut rx, mut ctl, tracks) = soloist_app();
        let sol = |e| Message::Net(Net::Soloist(e));
        let _ = app.update(sol(SoloistEvent::Status(EngineStatus::NeedsPairing)));
        assert!(app.soloist_card(), "pairing instruction");
        ws_cmds(&mut ctl);
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(1),
        )));
        assert!(ws_cmds(&mut ctl).is_empty());
        assert!(
            drain_loads(&mut rx).is_empty(),
            "never the denied local path"
        );
        // Paired from the phone: the request goes out, the card closes.
        let _ = app.update(sol(SoloistEvent::Status(EngineStatus::Ready)));
        assert_eq!(
            ws_cmds(&mut ctl),
            vec![SCmd::Activate, SCmd::Play(Some(tracks[1].uri.clone()))]
        );
        assert!(!app.soloist_card());
    }

    #[tokio::test]
    async fn audio_key_denied_offers_soloist_and_a_saved_key_switches_engine() {
        use crate::net::audio_diag::UnavailableReason as R;
        let (mut app, _rx, _tracks) = real_app_playing();
        app.session = Some(fake_session());
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(0),
        )));
        unavailable(&mut app, R::AudioKeyDenied { code: 1 });
        assert!(app.soloist_card(), "the setup card opens");
        feed(&mut app, Done::SoloistKey(Ok(None)));
        assert!(app.soloist.wants_key());
        // A bad paste is refused without touching the keyring.
        let s = |u| ui(Ui::Soloist(soloist::SoloistUi::KeyInput(u)));
        let _ = app.update(s(crate::auth::secrets::Secret::new("bad key")));
        let _ = app.update(ui(Ui::Soloist(soloist::SoloistUi::SaveKey)));
        assert!(app.toast.as_ref().unwrap().0.contains("API key"));
        assert_eq!(app.settings.engine, config::AudioEngine::Builtin);
        // Saved: Soloist becomes the engine and the session is rebuilt.
        feed(
            &mut app,
            Done::SoloistKeySaved(Ok(crate::auth::secrets::Secret::new("good-key-123456"))),
        );
        assert_eq!(app.settings.engine, config::AudioEngine::Soloist);
        assert!(
            app.session.is_none(),
            "reconnects without our Connect device"
        );
        assert!(!app.local_denied);
        assert!(matches!(
            app.engine_kind,
            EngineKind::Librespot(_, _, false)
        ));
        assert!(app.soloist.has_key());
        // Back to the built-in player.
        let _ = app.update(ui(Ui::Soloist(soloist::SoloistUi::Enable(false))));
        assert_eq!(app.settings.engine, config::AudioEngine::Builtin);
        assert!(app.soloist.handle.is_none());
    }

    #[test]
    fn silent_login_rejection_falls_back_to_browser_login() {
        let mut cfg = Config::for_tests();
        cfg.mock = false;
        let (mut app, _) = App::boot(cfg, Options::default());
        app.secrets = Arc::new(MemoryStore::new());
        app.auth = AuthState::Restoring;
        feed(
            &mut app,
            Done::Restored(Err(Error::revoked(crate::auth::SILENT_LOGIN_REJECTED))),
        );
        assert_eq!(
            app.auth,
            AuthState::SignedOut {
                error: Some(crate::auth::SILENT_LOGIN_REJECTED.into())
            }
        );
        assert!(!app.has_stored_credentials);

        // A transient failure keeps the credentials: "Log in" retries silently.
        app.auth = AuthState::Restoring;
        feed(&mut app, Done::Restored(Err(Error::Net("offline".into()))));
        assert!(app.has_stored_credentials);
        let _ = app.update(ui(Ui::Login));
        assert_eq!(app.auth, AuthState::Connecting, "no browser for a retry");
    }

    #[test]
    fn connect_volume_while_idle_does_not_hijack_the_player() {
        let (mut app, _rx, _) = real_app_playing();
        for ev in [
            RemoteEvent::Volume(0.3),
            RemoteEvent::Position { position_ms: 9 },
            RemoteEvent::Paused { position_ms: 9 },
        ] {
            let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Remote(ev))));
            assert!(app.remote.is_none());
        }
    }

    #[test]
    fn device_id_is_stable_across_boots() {
        let store = Store::in_memory().unwrap();
        let first = load_device_id(&store);
        assert!(is_device_id(&first));
        assert_eq!(load_device_id(&store), first, "reused, not regenerated");
        store.set_setting("device_id", "garbage").unwrap();
        assert_ne!(load_device_id(&store), "garbage");
        let app = mock_app();
        assert_eq!(
            app.session_opts().device_id.as_deref(),
            Some(app.device_id.as_str())
        );
    }

    fn tray_cmd(app: &mut App, c: crate::tray::Command) {
        let _ = app.update(Message::Tray(crate::tray::Event::Command(c)));
    }

    fn tray_registered(app: &mut App) {
        let (tx, _rx) = tokio::sync::watch::channel(crate::tray::TrayModel::default());
        let control = crate::tray::Control::new(Arc::new(tx));
        let _ = app.update(Message::Tray(crate::tray::Event::Ready(control)));
    }

    #[tokio::test]
    async fn playback_and_tray_transport_keep_working_while_hidden() {
        use crate::tray::Command;
        let (mut app, mut rx, tracks) = real_app_playing();
        app.session = Some(fake_session());
        tray_registered(&mut app);
        let _ = app.update(ui(Ui::PlayFrom(
            TrackSource::Playlist("pl01".into()),
            Some(0),
        )));
        let uri = tracks[0].uri.clone();
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Playing {
            uri: uri.clone(),
            position_ms: 1_000,
        })));
        while rx.try_recv().is_ok() {}
        assert!(app.win.visible);

        // Close button: hidden to the tray, window closed, state kept.
        let _ = app.update(ui(Ui::CloseRequested));
        assert!(!app.win.visible);
        assert!(app.window_id.is_none());
        assert!(app.session.is_some(), "the session stays up");
        assert_eq!(app.playback.status, PlayStatus::Playing);
        let m = app.tray_model();
        assert!(m.playing && !m.visible);
        assert_eq!(
            m.tooltip_title(),
            format!("{} \u{2014} {}", tracks[0].artist_line(), tracks[0].name)
        );

        // Tray "Pause" reaches the engine; the engine's answer updates state.
        tray_cmd(&mut app, Command::PlayPause);
        assert!(matches!(rx.try_recv(), Ok(PlaybackCommand::Pause)));
        let _ = app.update(Message::Net(Net::Playback(PlaybackEvent::Paused {
            uri,
            position_ms: 2_000,
        })));
        assert_eq!(app.playback.status, PlayStatus::Paused);
        assert!(!app.tray_model().playing);
        assert_eq!(app.tray_model().tooltip_title(), "Ferrotune");

        // Next from the tray advances the queue, still hidden.
        tray_cmd(&mut app, Command::Next);
        assert_eq!(drain_loads(&mut rx), vec![tracks[1].uri.clone()]);
        assert!(!app.win.visible);

        // Left click brings the window back.
        tray_cmd(&mut app, Command::Toggle);
        assert!(app.win.visible);
        assert!(app.window_id.is_some());
    }

    #[test]
    fn tray_transport_controls_the_connect_device_like_the_player_bar() {
        // The tray maps to the same `Ui` messages as the player bar, and
        // `update_ui` routes those to the remote device when one is active
        // (see `cluster_with_active_remote_device_drives_the_player_bar`).
        use crate::tray::Command::*;
        assert!(matches!(window::command_to_ui(PlayPause), Ui::TogglePlay));
        assert!(matches!(window::command_to_ui(Next), Ui::Next));
        assert!(matches!(window::command_to_ui(Previous), Ui::Previous));
    }

    #[test]
    fn no_tray_host_means_closing_quits_and_hidden_start_recovers() {
        // No watcher answered yet: the close button exits (as before).
        let mut app = mock_app();
        assert_eq!(app.win.close_requested(), window::Action::Exit);
        // `--minimized` opens no window until a tray host shows up.
        let (mut app, _) = App::boot(
            Config::for_tests(),
            Options {
                minimized: true,
                ..Options::default()
            },
        );
        assert!(!app.win.visible && app.window_id.is_none());
        let _ = app.update(Message::Tray(crate::tray::Event::Unavailable));
        assert!(
            app.win.visible && app.window_id.is_some(),
            "never invisible"
        );
    }

    #[test]
    fn screenshots_never_use_the_tray() {
        let (app, _) = App::boot(
            Config::for_tests(),
            Options {
                screenshot: Some(ScreenshotPlan {
                    screen: "settings".into(),
                    out: "x.png".into(),
                }),
                minimized: true,
                ..Options::default()
            },
        );
        assert!(app.win.visible && !app.tray_enabled);
    }

    #[test]
    fn scroll_on_the_tray_icon_steps_the_volume() {
        let mut app = mock_app();
        let before = app.now_playback().volume;
        tray_cmd(&mut app, crate::tray::Command::VolumeStep(-0.05));
        assert!(app.now_playback().volume < before);
    }

    #[test]
    fn start_minimized_toggle_from_the_tray_is_saved_in_settings() {
        let mut app = mock_app();
        assert!(!app.settings.start_minimized);
        tray_cmd(&mut app, crate::tray::Command::ToggleStartMinimized);
        assert!(app.settings.start_minimized);
        assert!(app.tray_model().start_minimized);
        tray_cmd(&mut app, crate::tray::Command::ToggleStartMinimized);
        assert!(!app.settings.start_minimized);
    }

    #[test]
    fn close_to_tray_setting_reaches_the_window_state() {
        let mut app = mock_app();
        tray_registered(&mut app);
        assert!(app.win.close_to_tray);
        let _ = app.update(ui(Ui::Setting(settings::SettingChange::CloseToTray(false))));
        assert!(!app.win.close_to_tray);
        assert_eq!(app.win.close_requested(), window::Action::Exit);
    }
}
