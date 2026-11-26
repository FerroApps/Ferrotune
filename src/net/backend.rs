//! The backends the app reads from, one trait per concern so each can be
//! mocked, tested against fixtures, or replaced when Spotify changes an
//! internal API.
//!
//! | Trait | Real implementation | Transport |
//! |---|---|---|
//! | [`CatalogApi`] (playlists, albums, artists) | `spclient::Internal` | spclient protobuf (playlist4, extended-metadata) |
//! | [`LibraryApi`] (profile, rootlist, liked songs, saved albums, followed artists, like/unlike) | `spclient::Internal` | spclient protobuf (rootlist, collection2v2) |
//! | [`PlaylistApi`] (create, rename, add/remove/move tracks, delete) | `spclient::Internal` | spclient protobuf (playlist4 `changes`) |
//! | [`SearchApi`] | `pathfinder::Pathfinder` | GraphQL persisted query `searchDesktop`, fallback spclient `searchview` |
//! | [`HomeApi`] | `pathfinder::Pathfinder` | GraphQL persisted query `home`, fallback spclient `recently-played` |
//! | [`LyricsApi`] | `spclient::Internal` | spclient `color-lyrics/v2` JSON |
//!
//! All of them share one `InternalClient`, i.e. one session, one token
//! source, one cache and one rate-limit state.

use std::sync::Arc;

use futures::future::BoxFuture;

use super::model::{
    Album, ArtistPage, HomeFeed, Library, Lyrics, Page, Playlist, SearchResults, Track, User,
    UserProfile,
};
use super::playlist_edit::Edit;
use crate::error::Result;

pub trait CatalogApi: Send + Sync {
    /// Playlist header + its first page of tracks.
    fn playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(Playlist, Page<Track>)>>;
    fn playlist_tracks<'a>(
        &'a self,
        id: &'a str,
        offset: u32,
    ) -> BoxFuture<'a, Result<Page<Track>>>;
    fn album<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Album>>;
    fn artist_page<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<ArtistPage>>;
}

pub trait LibraryApi: Send + Sync {
    fn me(&self) -> BoxFuture<'_, Result<User>>;
    fn library(&self) -> BoxFuture<'_, Result<Library>>;
    /// One page (50) of Liked Songs, newest first.
    fn saved_tracks(&self, offset: u32) -> BoxFuture<'_, Result<Page<Track>>>;
    fn contains_saved(&self, ids: Vec<String>) -> BoxFuture<'_, Result<Vec<bool>>>;
    fn set_saved(&self, ids: Vec<String>, saved: bool) -> BoxFuture<'_, Result<()>>;
    /// Public profile of `user`; `playlists` = how many public playlists.
    fn profile<'a>(&'a self, user: &'a str, playlists: u32) -> BoxFuture<'a, Result<UserProfile>>;
}

/// Playlist editing (playlist4 changes). Every call refetches the list's
/// current revision first, so edits apply on top of what the server has.
pub trait PlaylistApi: Send + Sync {
    /// Create a playlist and add it to the sidebar; returns the new playlist.
    fn create_playlist<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Playlist>>;
    fn edit_playlist<'a>(&'a self, id: &'a str, edit: Edit) -> BoxFuture<'a, Result<()>>;
    /// Remove a playlist from the user's library (delete when owned).
    fn delete_playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>>;
}

pub trait SearchApi: Send + Sync {
    fn search<'a>(&'a self, query: &'a str) -> BoxFuture<'a, Result<SearchResults>>;
}

pub trait HomeApi: Send + Sync {
    fn home(&self) -> BoxFuture<'_, Result<HomeFeed>>;
}

pub trait LyricsApi: Send + Sync {
    /// `Ok(None)` when the track has no lyrics.
    fn lyrics<'a>(&'a self, track_id: &'a str) -> BoxFuture<'a, Result<Option<Lyrics>>>;
}

/// The set of backends a signed-in session uses.
#[derive(Clone)]
pub struct Backend {
    pub catalog: Arc<dyn CatalogApi>,
    pub library: Arc<dyn LibraryApi>,
    pub playlists: Arc<dyn PlaylistApi>,
    pub search: Arc<dyn SearchApi>,
    pub home: Arc<dyn HomeApi>,
    pub lyrics: Arc<dyn LyricsApi>,
    /// Lets the app swap the librespot session after a reconnect.
    pub session: Option<Arc<super::native_auth::NativeAuth>>,
    pub client: Option<Arc<super::client::InternalClient>>,
}

impl Backend {
    /// Wire the real internal-API backends to one client.
    pub fn internal(
        client: Arc<super::client::InternalClient>,
        queries: super::pathfinder::Queries,
        session: Option<Arc<super::native_auth::NativeAuth>>,
        store: Option<crate::store::Store>,
    ) -> Self {
        let internal = Arc::new(super::spclient::Internal::new(client.clone(), store));
        let pathfinder = Arc::new(super::pathfinder::Pathfinder::new(
            client.clone(),
            queries,
            internal.clone(),
        ));
        Self {
            catalog: internal.clone(),
            library: internal.clone(),
            playlists: internal.clone(),
            search: pathfinder.clone(),
            home: pathfinder,
            lyrics: internal,
            session,
            client: Some(client),
        }
    }

    /// Point the backends at a freshly reconnected librespot session.
    pub async fn rebind(&self, session: librespot_core::Session) {
        if let Some(live) = &self.session {
            live.replace(session);
        }
        if let Some(c) = &self.client {
            c.session_changed().await;
        }
    }
}
