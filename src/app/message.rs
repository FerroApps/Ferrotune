//! The `Message` enum: UI events, net events, task completions.

use std::time::Instant;

use super::menu::MenuTarget;
use super::playlists::EditSnapshot;
use super::state::{LibraryFilter, Route, SearchFilter};
use crate::error::Result;
use crate::net::DataSource;
use crate::net::images::DecodedImage;
use crate::net::model::{
    Album, ArtistPage, HomeFeed, Library, Lyrics, Page, Playlist, SearchResults, Track, User,
    UserProfile,
};
use crate::net::mpris::MediaEvent;
use crate::net::playback::PlaybackEvent;
use crate::net::session::SpotifySession;

/// A list of tracks the user can start playing from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackSource {
    Playlist(String),
    Album(String),
    Liked,
    ArtistTop(String),
    Search,
}

#[derive(Debug, Clone)]
pub enum Message {
    Ui(Ui),
    Net(Net),
    Done(Done),
    /// Events from the system tray service.
    Tray(crate::tray::Event),
    /// In-app updater: timer ticks, check and install results.
    Update(super::updates::UpdateEvent),
}

#[derive(Debug, Clone)]
pub enum Ui {
    Navigate(Route),
    Back,
    Forward,
    SearchInput(String),
    SearchSubmit,
    SearchFilter(SearchFilter),
    LibraryFilter(LibraryFilter),
    /// Start playback from a list; `None` = from the top (random with shuffle).
    PlayFrom(TrackSource, Option<usize>),
    TogglePlay,
    Next,
    Previous,
    ToggleShuffle,
    CycleRepeat,
    SeekPreview(f32),
    SeekCommit,
    Volume(f32),
    VolumeCommit,
    ToggleMute,
    ToggleQueue,
    ToggleLyrics,
    PlayUpcoming(usize),
    PlayQueued(usize),
    Enqueue(Track),
    ToggleLike(Track),
    HoverRow(Option<String>),
    SelectRow(String),
    Login,
    RetryKeyring,
    CancelLogin,
    OpenLoginUrl,
    CopyLoginUrl,
    Logout,
    Scrolled(f32),
    DismissToast,
    Tick(Instant),
    // keyboard shortcuts and overlays
    FocusSearch,
    VolumeStep(f32),
    Quit,
    ToggleHelp,
    /// Escape: closes the context menu, then any dialog.
    CloseOverlay,
    WindowResized(iced::Size),
    // context menu
    OpenMenu(MenuTarget, iced::Point),
    ToggleMenuPicker,
    CopyLink(String),
    // playlist editing
    NewPlaylist,
    OpenRename(String),
    OpenDelete(String),
    DialogInput(String),
    DialogSubmit,
    AddToPlaylist(String, Track),
    RemoveFromPlaylist(String, usize),
    /// Left button pressed on a row of an editable playlist: `(row key, index)`.
    DragStart(String, usize),
    /// Left button released over row `index` while a drag may be active.
    DragDrop(usize),
    /// Left button released anywhere: ends a drag.
    MouseUp,
    // Spotify Connect controller
    ToggleDevicePicker,
    /// Play on a device: `Some((id, name))`, or `None` = this computer.
    PlayOn(Option<(String, String)>),
    DismissBanner,
    ToggleUserMenu,
    Setting(super::settings::SettingChange),
    Soloist(super::soloist::SoloistUi),
    Update(super::updates::UpdateUi),
    // window and tray
    /// The window's close button (or Alt+F4).
    CloseRequested,
    ToggleWindow,
    ShowWindow,
    HideWindow,
    ToggleStartMinimized,
    WindowMoved(iced::Point),
}

#[derive(Debug, Clone)]
pub enum Net {
    Playback(PlaybackEvent),
    Media(MediaEvent),
    Soloist(crate::net::soloist::SoloistEvent),
}

/// A signed-in session ready for use.
#[derive(Debug, Clone)]
pub struct SessionReady {
    pub source: DataSource,
    pub user: Option<User>,
    /// The connected librespot session (`None` in mock mode).
    pub session: Option<SpotifySession>,
}

#[derive(Debug, Clone)]
pub enum Done {
    /// Keyring restore at boot: `None` = no stored session.
    Restored(Result<Option<SessionReady>>),
    LoggedIn(Result<SessionReady>),
    Profile(Result<User>),
    Audio(Result<SpotifySession>),
    Library(Result<Library>),
    Home(Result<HomeFeed>),
    Playlist(String, Result<(Playlist, Page<Track>)>),
    PlaylistMore(String, Result<Page<Track>>),
    Liked(Result<Page<Track>>),
    Album(String, Result<Album>),
    Artist(String, Result<ArtistPage>),
    SearchDebounced(u64),
    Search(u64, Result<SearchResults>),
    Image(String, Result<DecodedImage>),
    Lyrics(String, Result<Option<Lyrics>>),
    LikeSaved(String, bool, Result<()>),
    /// Create finished: `(temporary id, result)`.
    PlaylistCreated(String, Result<Playlist>),
    /// An edit finished; on error the snapshot is restored.
    PlaylistEdited(Box<EditSnapshot>, &'static str, Result<()>),
    LikedFlags(Vec<String>, Result<Vec<bool>>),
    ReconnectAudio,
    /// A Spotify Connect controller request finished.
    ConnectSent(Result<()>),
    /// Owner display names: `(username, name)`.
    OwnerNames(Vec<(String, String)>),
    /// Playlists completed lazily: `(id, header, first tracks)`.
    PlaylistFill(Vec<(String, Playlist, Vec<Track>)>),
    UserProfile(String, Result<UserProfile>),
    /// Size of the local cache in bytes.
    CacheSize(Result<u64>),
    CacheCleared(Result<()>),
    SettingsSaved(Result<()>),
    /// Autostart entry written or removed: the resulting state.
    AutostartChanged(std::result::Result<bool, String>),
    /// Initial autostart state read from disk.
    AutostartLoaded(bool),
    Screenshot(iced::window::Screenshot),
    ScreenshotSaved(Result<()>),
    Quit,
    /// The Soloist API key from the keyring (`None` = not set).
    SoloistKey(Result<Option<crate::auth::secrets::Secret>>),
    SoloistKeySaved(Result<crate::auth::secrets::Secret>),
    Noop,
}

pub fn ui(u: Ui) -> Message {
    Message::Ui(u)
}
