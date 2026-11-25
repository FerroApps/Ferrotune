//! Protocol clients. Everything goes through the librespot session, the way
//! the official desktop client talks to Spotify:
//! * `session` — login, reusable credentials, Spotify Connect device;
//! * `client` + `tokens` — authenticated HTTP to spclient/pathfinder;
//! * `spclient`, `pathfinder` — the backends behind the `backend` traits;
//! * `playback` — librespot players; `images` — cover art CDN; `mpris`.
//!
//! Never touches UI types. The public Web API (api.spotify.com) is not used.

pub mod audio_diag;
pub mod backend;
pub mod client;
pub mod connect_ctl;
pub mod images;
pub mod model;
pub mod mpris;
pub mod native;
pub mod paging;
pub mod pathfinder;
pub mod playback;
pub mod playlist_edit;
pub mod proto;
pub mod session;
pub mod soloist;
pub mod spclient;
pub mod tokens;
pub mod types;

use std::time::Duration;

use model::{
    Album, ArtistPage, HomeFeed, Library, Lyrics, Page, Playlist, SearchResults, Track, User,
    UserProfile,
};

use crate::error::Result;
use crate::mock;

/// Where library data comes from: the session-backed internal APIs or the
/// mock fixtures.
#[derive(Clone)]
pub enum DataSource {
    Spotify(backend::Backend),
    Mock,
}

impl std::fmt::Debug for DataSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Spotify(_) => "DataSource::Spotify",
            Self::Mock => "DataSource::Mock",
        })
    }
}

/// Simulated latency for mock calls so loading states are exercised.
const MOCK_LATENCY: Duration = Duration::from_millis(30);

impl DataSource {
    pub async fn me(&self) -> Result<User> {
        match self {
            Self::Spotify(b) => b.library.me().await,
            Self::Mock => Ok(mock::user()),
        }
    }

    pub async fn profile(&self, user: String, playlists: u32) -> Result<UserProfile> {
        match self {
            Self::Spotify(b) => b.library.profile(&user, playlists).await,
            Self::Mock => Ok(mock::profile(&user)),
        }
    }

    pub async fn library(&self) -> Result<Library> {
        match self {
            Self::Spotify(b) => b.library.library().await,
            Self::Mock => {
                tokio::time::sleep(MOCK_LATENCY).await;
                Ok(mock::library())
            }
        }
    }

    pub async fn home(&self) -> Result<HomeFeed> {
        match self {
            Self::Spotify(b) => b.home.home().await,
            Self::Mock => Ok(mock::home()),
        }
    }

    pub async fn playlist(&self, id: &str) -> Result<(Playlist, Page<Track>)> {
        match self {
            Self::Spotify(b) => b.catalog.playlist(id).await,
            Self::Mock => mock::playlist(id),
        }
    }

    pub async fn playlist_tracks(&self, id: &str, offset: u32) -> Result<Page<Track>> {
        match self {
            Self::Spotify(b) => b.catalog.playlist_tracks(id, offset).await,
            Self::Mock => Ok(Page::new(vec![], offset, offset)),
        }
    }

    pub async fn saved_tracks(&self, offset: u32) -> Result<Page<Track>> {
        match self {
            Self::Spotify(b) => b.library.saved_tracks(offset).await,
            Self::Mock => Ok(mock::liked(offset)),
        }
    }

    pub async fn album(&self, id: &str) -> Result<Album> {
        match self {
            Self::Spotify(b) => b.catalog.album(id).await,
            Self::Mock => mock::album(id),
        }
    }

    pub async fn artist_page(&self, id: &str) -> Result<ArtistPage> {
        match self {
            Self::Spotify(b) => b.catalog.artist_page(id).await,
            Self::Mock => mock::artist_page(id),
        }
    }

    pub async fn search(&self, query: &str) -> Result<SearchResults> {
        match self {
            Self::Spotify(b) => b.search.search(query).await,
            Self::Mock => {
                tokio::time::sleep(MOCK_LATENCY).await;
                Ok(mock::search(query))
            }
        }
    }

    pub async fn set_saved(&self, ids: Vec<String>, saved: bool) -> Result<()> {
        match self {
            Self::Spotify(b) => b.library.set_saved(ids, saved).await,
            Self::Mock => Ok(()),
        }
    }

    pub async fn contains_saved(&self, ids: Vec<String>) -> Result<Vec<bool>> {
        match self {
            Self::Spotify(b) => b.library.contains_saved(ids).await,
            Self::Mock => Ok(ids.iter().map(|id| mock::is_liked(id)).collect()),
        }
    }

    pub async fn create_playlist(&self, name: &str) -> Result<Playlist> {
        match self {
            Self::Spotify(b) => b.playlists.create_playlist(name).await,
            Self::Mock => Ok(mock::new_playlist(name)),
        }
    }

    pub async fn edit_playlist(&self, id: &str, edit: playlist_edit::Edit) -> Result<()> {
        match self {
            Self::Spotify(b) => b.playlists.edit_playlist(id, edit).await,
            Self::Mock => Ok(()),
        }
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<()> {
        match self {
            Self::Spotify(b) => b.playlists.delete_playlist(id).await,
            Self::Mock => Ok(()),
        }
    }

    /// Spotify Connect controller call (`from` is our device id).
    pub async fn connect_op(&self, from: String, op: connect_ctl::ConnectOp) -> Result<()> {
        use connect_ctl::ConnectOp;
        let client = match self {
            Self::Spotify(b) => b
                .client
                .clone()
                .ok_or_else(|| crate::error::Error::Net("not connected".into()))?,
            Self::Mock => return Ok(()),
        };
        match op {
            ConnectOp::Command(to, cmd) => {
                connect_ctl::send_command(&client, &from, &to, &cmd).await
            }
            ConnectOp::Transfer(to) => connect_ctl::transfer(&client, &from, &to).await,
            ConnectOp::Volume(to, v) => connect_ctl::set_volume(&client, &from, &to, v).await,
        }
    }

    pub async fn lyrics(&self, track_id: String) -> Result<Option<Lyrics>> {
        match self {
            Self::Spotify(b) => b.lyrics.lyrics(&track_id).await,
            Self::Mock => Ok(mock::lyrics(&track_id)),
        }
    }

    pub async fn image(&self, url: String, max_px: u32) -> Result<images::DecodedImage> {
        match self {
            Self::Spotify(_) => {
                // A plain client: image CDN URLs need no auth header.
                static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
                let http = HTTP.get_or_init(client::http_client);
                images::fetch(http, &url, max_px).await
            }
            Self::Mock => Ok(mock::image(&url, max_px)),
        }
    }
}
