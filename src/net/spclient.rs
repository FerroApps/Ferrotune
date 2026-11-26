//! spclient backends: the endpoints the desktop client uses for playlists,
//! metadata, the user's collection and lyrics (protobuf where the desktop
//! client uses protobuf).
//!
//! | Data | Endpoint |
//! |---|---|
//! | Profile name/avatar | `GET /user-profile-view/v3/profile/{user}` (JSON) |
//! | Playlists in the sidebar | `GET /playlist/v2/user/{user}/rootlist?decorate=…` (playlist4 protobuf) |
//! | Playlist | `GET /playlist/v2/playlist/{id}?from=&length=` (playlist4 protobuf) |
//! | Track / album / artist metadata | `POST /extended-metadata/v0/extended-metadata` (batched, `TRACK_V4`/`ALBUM_V4`/`ARTIST_V4`) |
//! | Liked Songs + saved albums | `POST /collection/v2/paging` + `/collection/v2/delta`, set `collection` (collection2v2 protobuf) |
//! | Followed artists | same, set `artist` |
//! | Like / unlike | `POST /collection/v2/write` |
//! | Lyrics | `GET /color-lyrics/v2/track/{id}` (JSON) |
//! | Home fallback | `GET /recently-played/v3/user/{user}/recently-played` (JSON) |
//! | Search fallback | `GET /searchview/km/v4/search/{q}` (JSON) |

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use protobuf::{EnumOrUnknown, Message as _, MessageField};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use spotify_proto::extended_metadata::{
    BatchedEntityRequest, BatchedEntityRequestHeader, BatchedExtensionResponse, EntityRequest,
    ExtensionQuery,
};
use spotify_proto::extension_kind::ExtensionKind;
use spotify_proto::metadata;
use spotify_proto::playlist4_external::SelectedListContent;

use super::backend::{CatalogApi, LibraryApi, LyricsApi, PlaylistApi};
use super::client::{COLLECTION_PROTO, InternalClient, JSON, PROTOBUF, Request, Target};
use super::model::{
    Album, Artist, ArtistPage, Card, HomeFeed, Library, Lyrics, Page, Playlist, SearchResults,
    Shelf, Track, User, UserProfile,
};
use super::playlist_edit::{self, Edit};
use super::proto::{self, CollectionItem};
use super::types;
use crate::error::{Error, Result};
use crate::store::{Kind, Store};

pub const TTL_METADATA: Duration = Duration::from_secs(24 * 3600);
pub const TTL_PLAYLIST: Duration = Duration::from_secs(10 * 60);
pub const TTL_PROFILE: Duration = Duration::from_secs(24 * 3600);
pub const TTL_LYRICS: Duration = Duration::from_secs(7 * 24 * 3600);
pub const TTL_RECENT: Duration = Duration::from_secs(5 * 60);
/// A synced collection index is reused this long before a delta sync.
pub const COLLECTION_FRESH: Duration = Duration::from_secs(10 * 60);
/// Entities per extended-metadata request.
pub const METADATA_BATCH: usize = 100;
/// Items per collection page (the desktop client pages similarly).
pub const COLLECTION_PAGE: i32 = 300;
/// Hard cap on collection entries we keep.
pub const COLLECTION_CAP: usize = 10_000;
pub const LIBRARY_CAP: usize = 500;
pub const LIKED_PAGE: usize = 50;
pub const PLAYLIST_PAGE: u32 = 100;

/// A synced collection set (`collection` or `artist`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectionIndex {
    /// `(uri, added_at)`, newest first.
    pub items: Vec<(String, i32)>,
    pub sync_token: String,
    #[serde(skip)]
    pub fetched_at: Option<std::time::Instant>,
}

impl CollectionIndex {
    fn from_items(items: Vec<CollectionItem>, sync_token: String) -> Self {
        let mut idx = Self {
            items: Vec::new(),
            sync_token,
            fetched_at: Some(std::time::Instant::now()),
        };
        idx.apply(items);
        idx
    }

    /// Apply additions/removals (a full page list or a delta).
    pub fn apply(&mut self, changes: Vec<CollectionItem>) {
        let mut by_uri: HashMap<String, i32> = self.items.drain(..).collect();
        for c in changes {
            if c.is_removed {
                by_uri.remove(&c.uri);
            } else {
                by_uri.insert(c.uri, c.added_at);
            }
        }
        let mut v: Vec<(String, i32)> = by_uri.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        if v.len() > COLLECTION_CAP {
            v.truncate(COLLECTION_CAP);
            // An incomplete index must not be delta-synced.
            self.sync_token.clear();
        }
        self.items = v;
    }

    pub fn uris_of(&self, kind: &str) -> Vec<&(String, i32)> {
        self.items
            .iter()
            .filter(|(u, _)| types::split_uri(u).is_some_and(|(k, _)| k == kind))
            .collect()
    }

    pub fn contains(&self, uri: &str) -> bool {
        self.items.iter().any(|(u, _)| u == uri)
    }
}

pub struct Internal {
    client: Arc<InternalClient>,
    store: Option<Store>,
    collections: tokio::sync::Mutex<HashMap<&'static str, CollectionIndex>>,
    write_seq: AtomicU64,
}

fn parse_proto<M: protobuf::Message>(bytes: &[u8], what: &str) -> Result<M> {
    M::parse_from_bytes(bytes).map_err(|_| Error::Protocol(format!("{what}: bad protobuf")))
}

fn parse_json(bytes: &[u8], what: &str) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|_| Error::Protocol(format!("{what}: bad JSON")))
}

fn enc(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

impl Internal {
    pub fn new(client: Arc<InternalClient>, store: Option<Store>) -> Self {
        Self {
            client,
            store,
            collections: tokio::sync::Mutex::new(HashMap::new()),
            write_seq: AtomicU64::new(0),
        }
    }

    fn username(&self) -> String {
        self.client.auth().username()
    }

    // ------------------------------------------------------------ metadata

    /// Batched extended-metadata lookup. Returns messages keyed by URI.
    pub async fn metadata<M: protobuf::Message>(
        &self,
        kind: ExtensionKind,
        uris: &[String],
    ) -> Result<HashMap<String, M>> {
        let mut out = HashMap::new();
        let mut unique: Vec<&String> = Vec::new();
        let mut seen = HashSet::new();
        for u in uris {
            if seen.insert(u) {
                unique.push(u);
            }
        }
        for chunk in unique.chunks(METADATA_BATCH) {
            let req = BatchedEntityRequest {
                header: MessageField::some(BatchedEntityRequestHeader {
                    country: self.client.auth().country(),
                    catalogue: "premium".into(),
                    ..Default::default()
                }),
                entity_request: chunk
                    .iter()
                    .map(|u| EntityRequest {
                        entity_uri: (*u).clone(),
                        query: vec![ExtensionQuery {
                            extension_kind: EnumOrUnknown::new(kind),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            };
            let body = req
                .write_to_bytes()
                .map_err(|_| Error::Protocol("could not encode metadata request".into()))?;
            let bytes = self
                .client
                .send(
                    Request::post(
                        Target::Spclient("/extended-metadata/v0/extended-metadata".into()),
                        PROTOBUF,
                        body,
                    )
                    .ttl(TTL_METADATA),
                )
                .await?;
            let resp: BatchedExtensionResponse = parse_proto(&bytes, "extended-metadata")?;
            for arr in resp.extended_metadata {
                for data in arr.extension_data {
                    let status = data.header.as_ref().map_or(200, |h| h.status_code);
                    if status != 200 && status != 0 {
                        continue;
                    }
                    let Some(any) = data.extension_data.as_ref() else {
                        continue;
                    };
                    if let Ok(m) = M::parse_from_bytes(&any.value) {
                        out.insert(data.entity_uri.clone(), m);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Tracks for `uris` in the given order (unknown/unavailable dropped).
    pub async fn tracks(&self, uris: &[String]) -> Result<Vec<Track>> {
        let track_uris: Vec<String> = uris
            .iter()
            .filter(|u| u.starts_with("spotify:track:"))
            .cloned()
            .collect();
        let map: HashMap<String, metadata::Track> =
            self.metadata(ExtensionKind::TRACK_V4, &track_uris).await?;
        Ok(track_uris
            .iter()
            .filter_map(|u| map.get(u).and_then(types::track))
            .collect())
    }

    pub async fn albums(&self, uris: &[String]) -> Result<Vec<(Album, Vec<String>)>> {
        let map: HashMap<String, metadata::Album> =
            self.metadata(ExtensionKind::ALBUM_V4, uris).await?;
        Ok(uris
            .iter()
            .filter_map(|u| map.get(u).and_then(types::album))
            .collect())
    }

    pub async fn artists(&self, uris: &[String]) -> Result<Vec<(Artist, metadata::Artist)>> {
        let map: HashMap<String, metadata::Artist> =
            self.metadata(ExtensionKind::ARTIST_V4, uris).await?;
        Ok(uris
            .iter()
            .filter_map(|u| {
                let raw = map.get(u)?;
                Some((types::artist(raw)?, raw.clone()))
            })
            .collect())
    }

    // ----------------------------------------------------------- playlists

    async fn playlist_content(
        &self,
        id: &str,
        from: u32,
        length: u32,
    ) -> Result<SelectedListContent> {
        if !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(Error::Http {
                status: 404,
                message: "bad playlist id".into(),
            });
        }
        let bytes = self
            .client
            .send(
                Request::get(
                    Target::Spclient(format!(
                        "/playlist/v2/playlist/{id}?decorate=revision,length,attributes,timestamp,owner,capabilities&from={from}&length={length}"
                    )),
                    PROTOBUF,
                )
                .ttl(TTL_PLAYLIST),
            )
            .await?;
        parse_proto(&bytes, "playlist")
    }

    async fn playlist_page(&self, c: &SelectedListContent, offset: u32) -> Result<Page<Track>> {
        let entries = types::playlist_items(c);
        let raw_len = u32::try_from(entries.len()).unwrap_or(0);
        let uris: Vec<String> = entries.iter().map(|(u, _)| u.clone()).collect();
        let added: HashMap<&str, Option<String>> = entries
            .iter()
            .map(|(u, a)| (u.as_str(), a.clone()))
            .collect();
        let mut tracks = self.tracks(&uris).await?;
        for t in &mut tracks {
            t.added_at = added.get(t.uri.as_str()).cloned().flatten();
        }
        let total = u32::try_from(c.length()).unwrap_or(0).max(offset + raw_len);
        let mut page = Page::new(tracks, offset, total);
        page.raw_len = raw_len;
        Ok(page)
    }

    /// Current revision of a playlist (`id`) or the rootlist (`None`),
    /// fetched uncached so a change applies on top of the server state.
    async fn revision(&self, id: Option<&str>) -> Result<Vec<u8>> {
        let path = match id {
            Some(id) => {
                if !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                    return Err(Error::Http {
                        status: 404,
                        message: "bad playlist id".into(),
                    });
                }
                format!("/playlist/v2/playlist/{id}?from=0&length=1")
            }
            None => format!(
                "/playlist/v2/user/{}/rootlist?decorate=revision&from=0&length=1",
                enc(&self.username())
            ),
        };
        let bytes = self
            .client
            .send(Request::get(Target::Spclient(path), PROTOBUF))
            .await?;
        let c: SelectedListContent = parse_proto(&bytes, "revision")?;
        Ok(c.revision().to_vec())
    }

    async fn post_changes(&self, path: String, body: Vec<u8>) -> Result<()> {
        self.client
            .send(Request::post(Target::Spclient(path), PROTOBUF, body))
            .await?;
        // Playlist and rootlist reads are cached for minutes: drop them.
        self.client.invalidate("/playlist/v2/").await;
        Ok(())
    }

    fn rootlist_changes_path(&self) -> String {
        format!(
            "/playlist/v2/user/{}/rootlist/changes",
            enc(&self.username())
        )
    }

    async fn rootlist(&self) -> Result<Vec<Playlist>> {
        let user = enc(&self.username());
        let bytes = self
            .client
            .send(
                Request::get(
                    Target::Spclient(format!(
                        "/playlist/v2/user/{user}/rootlist?decorate=revision,attributes,length,owner,capabilities,status_code&from=0&length={LIBRARY_CAP}"
                    )),
                    PROTOBUF,
                )
                .ttl(TTL_PLAYLIST),
            )
            .await?;
        let c: SelectedListContent = parse_proto(&bytes, "rootlist")?;
        Ok(types::rootlist(&c))
    }

    // ---------------------------------------------------------- collection

    async fn load_persisted(&self, set: &'static str) -> Option<CollectionIndex> {
        let store = self.store.clone()?;
        let key = format!("{}:{set}", self.username());
        store
            .run(move |s| s.get::<CollectionIndex>(Kind::Collection, &key))
            .await
            .ok()
            .flatten()
    }

    async fn persist(&self, set: &'static str, idx: &CollectionIndex) {
        if let Some(store) = self.store.clone() {
            let key = format!("{}:{set}", self.username());
            let idx = idx.clone();
            let _ = store
                .run(move |s| s.put(Kind::Collection, &key, &idx))
                .await;
        }
    }

    async fn full_sync(&self, set: &'static str) -> Result<CollectionIndex> {
        let user = self.username();
        let mut items = Vec::new();
        let mut token = String::new();
        let mut sync_token = String::new();
        loop {
            let body = proto::page_request(&user, set, &token, COLLECTION_PAGE);
            let bytes = self
                .client
                .send(Request::post(
                    Target::Spclient("/collection/v2/paging".into()),
                    COLLECTION_PROTO,
                    body,
                ))
                .await?;
            let page = proto::decode_page(&bytes)?;
            if sync_token.is_empty() {
                sync_token = page.sync_token.clone();
            }
            let n = page.items.len();
            items.extend(page.items);
            if page.next_page_token.is_empty() || n == 0 {
                break;
            }
            if items.len() >= COLLECTION_CAP {
                // Truncated: a delta from this token would be applied to an
                // incomplete index, so do a full sync next time instead.
                tracing::info!(
                    set,
                    "collection larger than the cap; not keeping a sync token"
                );
                sync_token.clear();
                break;
            }
            token = page.next_page_token;
        }
        Ok(CollectionIndex::from_items(items, sync_token))
    }

    /// The synced index of a collection set: memory, then a delta from the
    /// last sync token (persisted across restarts), then a full sync.
    ///
    /// The lock is only held to read and to store the index, never across
    /// the (possibly multi-page) network sync; identical concurrent page
    /// requests are coalesced by the client.
    pub async fn collection(&self, set: &'static str) -> Result<CollectionIndex> {
        let known = {
            let map = self.collections.lock().await;
            if let Some(idx) = map.get(set)
                && idx
                    .fetched_at
                    .is_some_and(|t| t.elapsed() < COLLECTION_FRESH)
            {
                return Ok(idx.clone());
            }
            map.get(set).cloned()
        };
        let known = match known {
            Some(i) => Some(i),
            None => self.load_persisted(set).await,
        };
        let idx = match known.filter(|k| !k.sync_token.is_empty()) {
            Some(mut k) => {
                let body = proto::delta_request(&self.username(), set, &k.sync_token);
                let delta = self
                    .client
                    .send(Request::post(
                        Target::Spclient("/collection/v2/delta".into()),
                        COLLECTION_PROTO,
                        body,
                    ))
                    .await
                    .and_then(|b| proto::decode_delta(&b));
                match delta {
                    Ok(d) if d.delta_update_possible => {
                        if !d.sync_token.is_empty() {
                            k.sync_token = d.sync_token;
                        }
                        // May clear the token again if the cap truncates.
                        k.apply(d.items);
                        k.fetched_at = Some(std::time::Instant::now());
                        k
                    }
                    Ok(_) => self.full_sync(set).await?,
                    // Offline: keep what we have rather than failing.
                    Err(e) if matches!(e, Error::Net(_) | Error::RateLimited(_)) => {
                        if k.items.is_empty() {
                            return Err(e);
                        }
                        k
                    }
                    Err(_) => self.full_sync(set).await?,
                }
            }
            None => self.full_sync(set).await?,
        };
        self.persist(set, &idx).await;
        self.collections.lock().await.insert(set, idx.clone());
        Ok(idx)
    }

    fn client_update_id(&self) -> String {
        let seq = self.write_seq.fetch_add(1, Ordering::Relaxed);
        let r: u64 = rand::random();
        format!("{r:016x}{seq:04x}")
    }

    // ------------------------------------------------------------ fallbacks

    /// Home built from recently played contexts (spclient).
    pub async fn recent_home(&self) -> Result<HomeFeed> {
        let user = enc(&self.username());
        let bytes = self
            .client
            .send(
                Request::get(
                    Target::Spclient(format!(
                        "/recently-played/v3/user/{user}/recently-played?format=json&offset=0&limit=50&filter=default"
                    )),
                    JSON,
                )
                .ttl(TTL_RECENT),
            )
            .await?;
        let v = parse_json(&bytes, "recently-played")?;
        let uris: Vec<String> = v
            .get("playContexts")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.get("uri")?.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        let pick = |kind: &str, n: usize| -> Vec<String> {
            uris.iter()
                .filter(|u| types::split_uri(u).is_some_and(|(k, _)| k == kind))
                .take(n)
                .cloned()
                .collect()
        };
        let (album_uris, artist_uris, playlist_uris) =
            (pick("album", 10), pick("artist", 10), pick("playlist", 6));
        let (albums, artists) =
            futures::join!(self.albums(&album_uris), self.artists(&artist_uris));
        let mut cards: HashMap<String, Card> = HashMap::new();
        for (a, _) in albums.unwrap_or_default() {
            cards.insert(a.uri.clone(), Card::Album(a));
        }
        for (a, _) in artists.unwrap_or_default() {
            cards.insert(a.uri.clone(), Card::Artist(a));
        }
        for u in &playlist_uris {
            if let Some((_, id)) = types::split_uri(u)
                && let Ok(c) = self.playlist_content(id, 0, 0).await
            {
                cards.insert(u.clone(), Card::Playlist(types::playlist(id, &c)));
            }
        }
        let items: Vec<Card> = uris.iter().filter_map(|u| cards.remove(u)).collect();
        Ok(HomeFeed {
            shelves: if items.is_empty() {
                Vec::new()
            } else {
                vec![Shelf {
                    title: "Jump back in".into(),
                    items,
                }]
            },
        })
    }

    /// Search through the older spclient `searchview` endpoint.
    pub async fn searchview(&self, query: &str) -> Result<SearchResults> {
        let auth = self.client.auth();
        let path = format!(
            "/searchview/km/v4/search/{}?limit=10&entityVersion=2&catalogue=premium&country={}&username={}",
            enc(query),
            enc(&auth.country()),
            enc(&auth.username()),
        );
        let bytes = self
            .client
            .send(Request::get(Target::Spclient(path), JSON).ttl(TTL_PLAYLIST))
            .await?;
        Ok(searchview_results(
            query,
            &parse_json(&bytes, "searchview")?,
        ))
    }
}

/// Parse the `searchview/km/v4` shape: `results.<kind>.hits[]`.
pub fn searchview_results(query: &str, v: &Value) -> SearchResults {
    let hits = |kind: &str| -> Vec<Value> {
        v.pointer(&format!("/results/{kind}/hits"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    };
    let image = |h: &Value| -> Vec<super::model::Image> {
        h.get("image")
            .and_then(Value::as_str)
            .map(|u| {
                vec![super::model::Image {
                    url: u.to_owned(),
                    width: Some(300),
                    height: Some(300),
                }]
            })
            .unwrap_or_default()
    };
    let id = |h: &Value, kind: &str| -> Option<(String, String)> {
        let uri = h.get("uri")?.as_str()?.to_owned();
        let (k, id) = types::split_uri(&uri)?;
        (k == kind).then(|| (id.to_owned(), uri.clone()))
    };
    let name = |h: &Value| {
        h.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    };
    let artist_refs = |h: &Value| -> Vec<super::model::ArtistRef> {
        h.get("artists")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|x| {
                        Some(super::model::ArtistRef {
                            id: id(x, "artist")?.0,
                            name: name(x),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    SearchResults {
        query: query.to_owned(),
        tracks: hits("tracks")
            .iter()
            .filter_map(|h| {
                let (tid, uri) = id(h, "track")?;
                Some(Track {
                    id: tid,
                    uri,
                    name: name(h),
                    artists: artist_refs(h),
                    album: h.get("album").and_then(|a| {
                        Some(super::model::AlbumRef {
                            id: id(a, "album")?.0,
                            name: name(a),
                            images: image(h),
                        })
                    }),
                    duration_ms: h
                        .get("duration")
                        .and_then(Value::as_u64)
                        .map_or(0, |d| d as u32),
                    explicit: h.get("explicit").and_then(Value::as_bool).unwrap_or(false),
                    playable: true,
                    added_at: None,
                })
            })
            .collect(),
        artists: hits("artists")
            .iter()
            .filter_map(|h| {
                let (aid, uri) = id(h, "artist")?;
                Some(Artist {
                    id: aid,
                    uri,
                    name: name(h),
                    images: image(h),
                    followers: None,
                    genres: Vec::new(),
                })
            })
            .collect(),
        albums: hits("albums")
            .iter()
            .filter_map(|h| {
                let (aid, uri) = id(h, "album")?;
                Some(Album {
                    id: aid,
                    uri,
                    name: name(h),
                    artists: artist_refs(h),
                    images: image(h),
                    release_date: String::new(),
                    kind: Default::default(),
                    total_tracks: 0,
                    tracks: Vec::new(),
                })
            })
            .collect(),
        playlists: hits("playlists")
            .iter()
            .filter_map(|h| {
                let (pid, uri) = id(h, "playlist")?;
                Some(Playlist {
                    id: pid,
                    uri,
                    name: name(h),
                    description: String::new(),
                    owner: h
                        .pointer("/author/name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    images: image(h),
                    can_edit: None,
                    mosaic: Vec::new(),
                    total_tracks: 0,
                })
            })
            .collect(),
    }
}

impl CatalogApi for Internal {
    fn playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(Playlist, Page<Track>)>> {
        Box::pin(async move {
            let c = self.playlist_content(id, 0, PLAYLIST_PAGE).await?;
            let header = types::playlist(id, &c);
            let page = self.playlist_page(&c, 0).await?;
            Ok((header, page))
        })
    }

    fn playlist_tracks<'a>(
        &'a self,
        id: &'a str,
        offset: u32,
    ) -> BoxFuture<'a, Result<Page<Track>>> {
        Box::pin(async move {
            let c = self.playlist_content(id, offset, PLAYLIST_PAGE).await?;
            self.playlist_page(&c, offset).await
        })
    }

    fn album<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Album>> {
        Box::pin(async move {
            let uri = format!("spotify:album:{id}");
            let (mut album, track_ids) =
                self.albums(&[uri])
                    .await?
                    .into_iter()
                    .next()
                    .ok_or(Error::Http {
                        status: 404,
                        message: "album not found".into(),
                    })?;
            let uris: Vec<String> = track_ids
                .iter()
                .map(|t| format!("spotify:track:{t}"))
                .collect();
            let album_ref = album.as_ref();
            let mut tracks = self.tracks(&uris).await?;
            for t in &mut tracks {
                match t.album.as_mut() {
                    Some(a) if !a.images.is_empty() => {}
                    _ => t.album = Some(album_ref.clone()),
                }
            }
            album.total_tracks = u32::try_from(tracks.len()).unwrap_or(0);
            album.tracks = tracks;
            Ok(album)
        })
    }

    fn artist_page<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<ArtistPage>> {
        Box::pin(async move {
            let uri = format!("spotify:artist:{id}");
            let (artist, raw) =
                self.artists(&[uri])
                    .await?
                    .into_iter()
                    .next()
                    .ok_or(Error::Http {
                        status: 404,
                        message: "artist not found".into(),
                    })?;
            let country = self.client.auth().country();
            let top: Vec<String> = types::artist_top_track_ids(&raw, &country)
                .into_iter()
                .take(10)
                .map(|t| format!("spotify:track:{t}"))
                .collect();
            let releases: Vec<String> = types::artist_release_ids(&raw, 20)
                .into_iter()
                .map(|a| format!("spotify:album:{a}"))
                .collect();
            let (tracks, albums) = futures::join!(self.tracks(&top), self.albums(&releases));
            Ok(ArtistPage {
                artist,
                top_tracks: tracks.unwrap_or_default(),
                albums: albums
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(a, _)| a)
                    .collect(),
            })
        })
    }
}

impl LibraryApi for Internal {
    fn me(&self) -> BoxFuture<'_, Result<User>> {
        Box::pin(async move {
            let auth = self.client.auth();
            let username = auth.username();
            if username.is_empty() {
                tracing::warn!("the session has no username yet; profile left empty");
            }
            // The profile view is cosmetic: fall back to the username, so
            // nothing (ownership checks included) depends on it.
            let (display_name, image) = match self.profile(&username, 0).await {
                Ok(p) => (p.name, p.image),
                Err(e) => {
                    tracing::debug!(error = %e, "profile fetch failed; using the username");
                    (username.clone(), None)
                }
            };
            Ok(User {
                display_name,
                image,
                id: username,
                product: auth.product(),
            })
        })
    }

    fn profile<'a>(&'a self, user: &'a str, playlists: u32) -> BoxFuture<'a, Result<UserProfile>> {
        Box::pin(async move {
            if user.is_empty() {
                return Err(Error::Http {
                    status: 404,
                    message: "no username".into(),
                });
            }
            let n = playlists.min(50);
            let bytes = self
                .client
                .send(
                    Request::get(
                        Target::Spclient(format!(
                            "/user-profile-view/v3/profile/{}?playlist_limit={n}&artist_limit=0&episode_limit=0&market=from_token",
                            enc(user)
                        )),
                        JSON,
                    )
                    .ttl(TTL_PROFILE),
                )
                .await?;
            let v: Value = serde_json::from_slice(&bytes)?;
            Ok(types::user_profile(user, &v))
        })
    }

    fn library(&self) -> BoxFuture<'_, Result<Library>> {
        Box::pin(async move {
            let (playlists, coll, followed) = futures::join!(
                self.rootlist(),
                self.collection("collection"),
                self.collection("artist")
            );
            let coll = coll?;
            let album_uris: Vec<String> = coll
                .uris_of("album")
                .into_iter()
                .take(LIBRARY_CAP)
                .map(|(u, _)| u.clone())
                .collect();
            let artist_uris: Vec<String> = followed
                .as_ref()
                .map(|f| {
                    f.uris_of("artist")
                        .into_iter()
                        .take(LIBRARY_CAP)
                        .map(|(u, _)| u.clone())
                        .collect()
                })
                .unwrap_or_default();
            let (albums, artists) =
                futures::join!(self.albums(&album_uris), self.artists(&artist_uris));
            Ok(Library {
                playlists: playlists?,
                albums: albums?.into_iter().map(|(a, _)| a).collect(),
                artists: artists
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(a, _)| a)
                    .collect(),
                liked_total: u32::try_from(coll.uris_of("track").len()).unwrap_or(0),
            })
        })
    }

    fn saved_tracks(&self, offset: u32) -> BoxFuture<'_, Result<Page<Track>>> {
        Box::pin(async move {
            let idx = self.collection("collection").await?;
            let liked = idx.uris_of("track");
            let total = u32::try_from(liked.len()).unwrap_or(0);
            let slice: Vec<&(String, i32)> = liked
                .into_iter()
                .skip(offset as usize)
                .take(LIKED_PAGE)
                .collect();
            let raw_len = u32::try_from(slice.len()).unwrap_or(0);
            let uris: Vec<String> = slice.iter().map(|(u, _)| u.clone()).collect();
            let added: HashMap<&str, i32> = slice.iter().map(|(u, a)| (u.as_str(), *a)).collect();
            let mut tracks = self.tracks(&uris).await?;
            for t in &mut tracks {
                t.added_at = added
                    .get(t.uri.as_str())
                    .and_then(|s| types::iso_date_from_secs(i64::from(*s)));
            }
            let mut page = Page::new(tracks, offset, total);
            page.raw_len = raw_len;
            Ok(page)
        })
    }

    fn contains_saved(&self, ids: Vec<String>) -> BoxFuture<'_, Result<Vec<bool>>> {
        Box::pin(async move {
            // Answered from the synced collection, like the desktop client:
            // no request per screen.
            let idx = self.collection("collection").await?;
            let set: HashSet<&str> = idx.items.iter().map(|(u, _)| u.as_str()).collect();
            Ok(ids
                .iter()
                .map(|id| set.contains(format!("spotify:track:{id}").as_str()))
                .collect())
        })
    }

    fn set_saved(&self, ids: Vec<String>, saved: bool) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let now = i32::try_from(crate::auth::oauth::now_secs()).unwrap_or(i32::MAX);
            let items: Vec<CollectionItem> = ids
                .iter()
                .map(|id| CollectionItem {
                    uri: format!("spotify:track:{id}"),
                    added_at: now,
                    is_removed: !saved,
                })
                .collect();
            let body = proto::write_request(
                &self.username(),
                "collection",
                &items,
                &self.client_update_id(),
            );
            self.client
                .send(Request::post(
                    Target::Spclient("/collection/v2/write".into()),
                    COLLECTION_PROTO,
                    body,
                ))
                .await?;
            let mut map = self.collections.lock().await;
            if let Some(idx) = map.get_mut("collection") {
                idx.apply(items);
                let idx = idx.clone();
                drop(map);
                self.persist("collection", &idx).await;
            }
            Ok(())
        })
    }
}

impl PlaylistApi for Internal {
    fn create_playlist<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Playlist>> {
        Box::pin(async move {
            let bytes = self
                .client
                .send(Request::post(
                    Target::Spclient("/playlist/v2/playlist".into()),
                    PROTOBUF,
                    playlist_edit::create_request(name),
                ))
                .await?;
            let id = playlist_edit::parse_created(&bytes)?;
            let uri = format!("spotify:playlist:{id}");
            let rev = self.revision(None).await?;
            self.post_changes(
                self.rootlist_changes_path(),
                playlist_edit::rootlist_add(&rev, &uri),
            )
            .await?;
            Ok(Playlist {
                id,
                uri,
                name: name.to_owned(),
                description: String::new(),
                owner: self.username(),
                images: vec![],
                can_edit: None,
                mosaic: Vec::new(),
                total_tracks: 0,
            })
        })
    }

    fn edit_playlist<'a>(&'a self, id: &'a str, edit: Edit) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let rev = self.revision(Some(id)).await?;
            self.post_changes(
                format!("/playlist/v2/playlist/{id}/changes"),
                playlist_edit::playlist_changes(&rev, &edit),
            )
            .await
        })
    }

    fn delete_playlist<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            if !id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Err(Error::Http {
                    status: 404,
                    message: "bad playlist id".into(),
                });
            }
            let rev = self.revision(None).await?;
            self.post_changes(
                self.rootlist_changes_path(),
                playlist_edit::rootlist_remove(&rev, &format!("spotify:playlist:{id}")),
            )
            .await
        })
    }
}

impl LyricsApi for Internal {
    fn lyrics<'a>(&'a self, track_id: &'a str) -> BoxFuture<'a, Result<Option<Lyrics>>> {
        Box::pin(async move {
            if !track_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
                return Ok(None);
            }
            let r = self
                .client
                .send(
                    Request::get(
                        Target::Spclient(format!(
                            "/color-lyrics/v2/track/{track_id}?format=json&vocalRemoval=false&market=from_token"
                        )),
                        JSON,
                    )
                    .ttl(TTL_LYRICS),
                )
                .await;
            match r {
                Ok(b) => Ok(types::lyrics(&parse_json(&b, "lyrics")?)),
                Err(Error::Http { status: 404, .. }) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn it(uri: &str, at: i32, removed: bool) -> CollectionItem {
        CollectionItem {
            uri: uri.into(),
            added_at: at,
            is_removed: removed,
        }
    }

    #[test]
    fn collection_index_applies_deltas_newest_first() {
        let mut idx = CollectionIndex::from_items(
            vec![
                it("spotify:track:a", 10, false),
                it("spotify:album:b", 20, false),
                it("spotify:track:c", 30, false),
            ],
            "s1".into(),
        );
        assert_eq!(idx.items[0].0, "spotify:track:c");
        idx.apply(vec![
            it("spotify:track:a", 0, true),
            it("spotify:track:d", 40, false),
        ]);
        let tracks: Vec<&str> = idx
            .uris_of("track")
            .iter()
            .map(|(u, _)| u.as_str())
            .collect();
        assert_eq!(tracks, ["spotify:track:d", "spotify:track:c"]);
        assert!(idx.contains("spotify:album:b"));
        assert!(!idx.contains("spotify:track:a"));
    }

    #[test]
    fn truncated_index_drops_its_sync_token() {
        let items: Vec<CollectionItem> = (0..=COLLECTION_CAP as i32)
            .map(|i| it(&format!("spotify:track:{i}"), i, false))
            .collect();
        let idx = CollectionIndex::from_items(items, "s1".into());
        assert_eq!(idx.items.len(), COLLECTION_CAP);
        assert!(idx.sync_token.is_empty());
    }

    #[test]
    fn searchview_shape() {
        let v = serde_json::json!({"results": {
            "tracks": {"hits": [{"uri": "spotify:track:t", "name": "T", "duration": 1000,
                "artists": [{"uri": "spotify:artist:a", "name": "A"}],
                "album": {"uri": "spotify:album:b", "name": "B"}, "image": "https://i/x"}]},
            "artists": {"hits": [{"uri": "spotify:artist:a", "name": "A"}]},
            "albums": {"hits": [{"uri": "spotify:album:b", "name": "B"}]},
            "playlists": {"hits": [{"uri": "spotify:playlist:p", "name": "P", "author": {"name": "me"}}]}
        }});
        let r = searchview_results("q", &v);
        assert_eq!(r.tracks[0].artist_line(), "A");
        assert_eq!(r.tracks[0].images()[0].url, "https://i/x");
        assert_eq!(r.playlists[0].owner, "me");
        assert_eq!((r.artists.len(), r.albums.len()), (1, 1));
    }
}
