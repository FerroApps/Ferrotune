//! Wire types of Spotify's internal endpoints and their conversion to the
//! domain types in `net::model`.
//!
//! * spclient metadata and playlists are protobuf (`librespot-protocol`
//!   messages: `metadata.Track/Album/Artist`, `playlist4.SelectedListContent`).
//! * pathfinder (GraphQL) and a few spclient views are JSON; those are read
//!   leniently through `serde_json::Value` pointers because their shapes
//!   change over time. An entry we cannot understand is dropped, never fatal.

use serde_json::Value;
use spotify_proto::metadata;
use spotify_proto::playlist4_external::{ListAttributes, SelectedListContent};

use super::model::{
    Album, AlbumKind, AlbumRef, Artist, ArtistRef, Image, LyricLine, Lyrics, Playlist, Track,
    UserProfile,
};

pub const IMAGE_CDN: &str = "https://i.scdn.co/image/";

/// 16-byte gid -> base62 id.
pub fn gid_to_id(gid: &[u8]) -> Option<String> {
    Some(spotify_protocol::SpotifyId::from_raw(gid).ok()?.to_base62())
}

/// base62 id -> 16-byte gid.
pub fn id_to_gid(id: &str) -> Option<[u8; 16]> {
    Some(*spotify_protocol::SpotifyId::from_base62(id).ok()?.to_raw())
}

/// `spotify:<kind>:<id>` -> `(kind, id)`.
pub fn split_uri(uri: &str) -> Option<(&str, &str)> {
    let mut it = uri.split(':');
    match (it.next(), it.next(), it.next(), it.next()) {
        (Some("spotify"), Some(kind), Some(id), None) if !id.is_empty() => Some((kind, id)),
        // Legacy `spotify:user:<name>:playlist:<id>`.
        (Some("spotify"), Some("user"), Some(_), Some("playlist")) => {
            uri.rsplit(':').next().map(|id| ("playlist", id))
        }
        _ => None,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn image_url(file_id: &[u8]) -> String {
    format!("{IMAGE_CDN}{}", hex(file_id))
}

fn size_px(size: metadata::image::Size) -> u32 {
    use metadata::image::Size;
    match size {
        Size::SMALL => 64,
        Size::DEFAULT => 300,
        Size::LARGE => 640,
        Size::XLARGE => 1280,
    }
}

pub fn images(list: &[metadata::Image]) -> Vec<Image> {
    list.iter()
        .filter(|i| !i.file_id().is_empty())
        .map(|i| {
            let w = u32::try_from(i.width())
                .ok()
                .filter(|w| *w > 0)
                .unwrap_or_else(|| size_px(i.size()));
            Image {
                url: image_url(i.file_id()),
                width: Some(w),
                height: Some(w),
            }
        })
        .collect()
}

fn group_images(group: Option<&metadata::ImageGroup>, plain: &[metadata::Image]) -> Vec<Image> {
    let mut v = group.map(|g| images(&g.image)).unwrap_or_default();
    if v.is_empty() {
        v = images(plain);
    }
    v
}

fn artist_refs(list: &[metadata::Artist]) -> Vec<ArtistRef> {
    list.iter()
        .filter_map(|a| {
            Some(ArtistRef {
                id: gid_to_id(a.gid())?,
                name: a.name().to_owned(),
            })
        })
        .collect()
}

pub fn album_kind(t: metadata::album::Type) -> AlbumKind {
    use metadata::album::Type;
    match t {
        Type::SINGLE | Type::EP => AlbumKind::Single,
        Type::COMPILATION => AlbumKind::Compilation,
        _ => AlbumKind::Album,
    }
}

fn date_string(d: Option<&metadata::Date>) -> String {
    match d {
        Some(d) if d.year() > 0 => {
            if d.month() > 0 && d.day() > 0 {
                format!("{:04}-{:02}-{:02}", d.year(), d.month(), d.day())
            } else {
                format!("{:04}", d.year())
            }
        }
        _ => String::new(),
    }
}

pub fn track(t: &metadata::Track) -> Option<Track> {
    let id = gid_to_id(t.gid())?;
    let album = t.album.as_ref().and_then(|a| {
        Some(AlbumRef {
            id: gid_to_id(a.gid())?,
            name: a.name().to_owned(),
            images: group_images(a.cover_group.as_ref(), &a.cover),
        })
    });
    let mut artists = artist_refs(&t.artist);
    if artists.is_empty() {
        artists = t
            .artist_with_role
            .iter()
            .filter_map(|a| {
                Some(ArtistRef {
                    id: gid_to_id(a.artist_gid())?,
                    name: a.artist_name().to_owned(),
                })
            })
            .collect();
    }
    Some(Track {
        uri: format!("spotify:track:{id}"),
        id,
        name: t.name().to_owned(),
        artists,
        album,
        duration_ms: u32::try_from(t.duration()).unwrap_or(0),
        explicit: t.explicit(),
        // Availability is decided by librespot at load time (it also picks
        // alternatives for restricted files); an unplayable track then
        // reports `Unavailable` and the queue skips it.
        playable: true,
        added_at: None,
    })
}

/// Album without its tracks, plus the track gids from its discs (in order).
pub fn album(a: &metadata::Album) -> Option<(Album, Vec<String>)> {
    let id = gid_to_id(a.gid())?;
    let track_ids: Vec<String> = a
        .disc
        .iter()
        .flat_map(|d| d.track.iter())
        .filter_map(|t| gid_to_id(t.gid()))
        .collect();
    Some((
        Album {
            uri: format!("spotify:album:{id}"),
            id,
            name: a.name().to_owned(),
            artists: artist_refs(&a.artist),
            images: group_images(a.cover_group.as_ref(), &a.cover),
            release_date: date_string(a.date.as_ref()),
            kind: album_kind(a.type_()),
            total_tracks: u32::try_from(track_ids.len()).unwrap_or(0),
            tracks: Vec::new(),
        },
        track_ids,
    ))
}

pub fn artist(a: &metadata::Artist) -> Option<Artist> {
    let id = gid_to_id(a.gid())?;
    Some(Artist {
        uri: format!("spotify:artist:{id}"),
        id,
        name: a.name().to_owned(),
        images: group_images(a.portrait_group.as_ref(), &a.portrait),
        followers: None,
        genres: Vec::new(),
    })
}

/// Top track ids for `country` (falls back to the first list).
pub fn artist_top_track_ids(a: &metadata::Artist, country: &str) -> Vec<String> {
    let list = a
        .top_track
        .iter()
        .find(|t| t.country() == country)
        .or_else(|| a.top_track.first());
    list.map(|l| l.track.iter().filter_map(|t| gid_to_id(t.gid())).collect())
        .unwrap_or_default()
}

/// Album + single ids of an artist (first album of each group), newest
/// groups first as Spotify returns them.
pub fn artist_release_ids(a: &metadata::Artist, cap: usize) -> Vec<String> {
    a.album_group
        .iter()
        .chain(a.single_group.iter())
        .filter_map(|g| g.album.first())
        .filter_map(|al| gid_to_id(al.gid()))
        .take(cap)
        .collect()
}

/// Cover of a playlist: its own `picture`, else the server-rendered sizes
/// (`picture_size`, used by mosaics and personalised lists such as Daily
/// Mix), else an `image_url` format attribute. Only https URLs are kept.
pub fn list_images(attrs: Option<&ListAttributes>) -> Vec<Image> {
    let Some(a) = attrs else {
        return Vec::new();
    };
    if !a.picture().is_empty() {
        return vec![Image {
            url: image_url(a.picture()),
            width: Some(300),
            height: Some(300),
        }];
    }
    let px = |target: &str| match target {
        "small" => Some(60),
        "default" => Some(300),
        "large" => Some(640),
        "xlarge" => Some(1000),
        _ => None,
    };
    let sized: Vec<Image> = a
        .picture_size
        .iter()
        .filter(|p| p.url().starts_with("https://"))
        .map(|p| {
            let w = px(p.target_name());
            Image {
                url: p.url().to_owned(),
                width: w,
                height: w,
            }
        })
        .collect();
    if !sized.is_empty() {
        return sized;
    }
    a.format_attributes
        .iter()
        .find(|f| f.key() == "image_url" && f.value().starts_with("https://"))
        .map(|f| {
            vec![Image {
                url: f.value().to_owned(),
                width: None,
                height: None,
            }]
        })
        .unwrap_or_default()
}

/// Name of a playlist; personalised lists may carry it only as a format
/// attribute.
fn list_name(attrs: Option<&ListAttributes>) -> String {
    let Some(a) = attrs else {
        return String::new();
    };
    if !a.name().trim().is_empty() {
        return a.name().to_owned();
    }
    a.format_attributes
        .iter()
        .find(|f| matches!(f.key(), "title" | "name") && !f.value().trim().is_empty())
        .map(|f| f.value().to_owned())
        .unwrap_or_default()
}

/// Playlist header from a `SelectedListContent`.
pub fn playlist(id: &str, c: &SelectedListContent) -> Playlist {
    let attrs = c.attributes.as_ref();
    Playlist {
        id: id.to_owned(),
        uri: format!("spotify:playlist:{id}"),
        name: list_name(attrs),
        description: attrs
            .map(|a| a.description().to_owned())
            .unwrap_or_default(),
        owner: c.owner_username().to_owned(),
        images: list_images(attrs),
        can_edit: c.capabilities.as_ref().and_then(|k| k.can_edit_items),
        mosaic: Vec::new(),
        total_tracks: u32::try_from(c.length()).unwrap_or(0),
    }
}

/// Entries of a playlist page: `(uri, added_at ISO date)`.
pub fn playlist_items(c: &SelectedListContent) -> Vec<(String, Option<String>)> {
    c.contents
        .as_ref()
        .map(|l| {
            l.items
                .iter()
                .map(|it| {
                    let ts = it.attributes.as_ref().map(|a| a.timestamp()).unwrap_or(0);
                    (it.uri().to_owned(), iso_date_from_ms(ts))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Playlists of the user's rootlist (folders are flattened away).
pub fn rootlist(c: &SelectedListContent) -> Vec<Playlist> {
    let Some(contents) = c.contents.as_ref() else {
        return Vec::new();
    };
    contents
        .items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| {
            let (kind, id) = split_uri(it.uri())?;
            if kind != "playlist" {
                return None; // spotify:start-group / end-group (folders)
            }
            let meta = contents.meta_items.get(i);
            let attrs = meta.and_then(|m| m.attributes.as_ref());
            Some(Playlist {
                id: id.to_owned(),
                uri: format!("spotify:playlist:{id}"),
                name: list_name(attrs),
                description: attrs
                    .map(|a| a.description().to_owned())
                    .unwrap_or_default(),
                owner: meta
                    .map(|m| m.owner_username().to_owned())
                    .unwrap_or_default(),
                images: list_images(attrs),
                can_edit: meta
                    .and_then(|m| m.capabilities.as_ref())
                    .and_then(|k| k.can_edit_items),
                mosaic: Vec::new(),
                total_tracks: meta
                    .and_then(|m| u32::try_from(m.length()).ok())
                    .unwrap_or(0),
            })
        })
        .collect()
}

/// Milliseconds since the epoch -> `YYYY-MM-DD` (civil date, UTC).
pub fn iso_date_from_ms(ms: i64) -> Option<String> {
    iso_date_from_secs(ms / 1000)
}

pub fn iso_date_from_secs(secs: i64) -> Option<String> {
    if secs <= 0 {
        return None;
    }
    // Howard Hinnant's days-from-civil inverse.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

// --------------------------------------------------------------- JSON side

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn id_of(uri: &str, kind: &str) -> Option<String> {
    let (k, id) = split_uri(uri)?;
    (k == kind).then(|| id.to_owned())
}

/// GraphQL `sources: [{url, width, height}]`.
fn sources(v: Option<&Value>) -> Vec<Image> {
    v.and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|src| {
                    Some(Image {
                        url: src.get("url")?.as_str()?.to_owned(),
                        width: src.get("width").and_then(Value::as_u64).map(|w| w as u32),
                        height: src.get("height").and_then(Value::as_u64).map(|h| h as u32),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn gql_artists(v: Option<&Value>) -> Vec<ArtistRef> {
    v.and_then(|a| a.get("items"))
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|a| {
                    Some(ArtistRef {
                        id: id_of(a.get("uri")?.as_str()?, "artist")?,
                        name: s(a, "/profile/name").unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A pathfinder `Track` object (searchV2 `tracksV2.items[].item.data`).
pub fn gql_track(d: &Value) -> Option<Track> {
    let uri = s(d, "/uri")?;
    let id = id_of(&uri, "track")?;
    let album = d.get("albumOfTrack").and_then(|a| {
        Some(AlbumRef {
            id: id_of(a.get("uri")?.as_str()?, "album")?,
            name: s(a, "/name").unwrap_or_default(),
            images: sources(a.pointer("/coverArt/sources")),
        })
    });
    Some(Track {
        id,
        uri,
        name: s(d, "/name").unwrap_or_default(),
        artists: gql_artists(d.get("artists")),
        album,
        duration_ms: d
            .pointer("/duration/totalMilliseconds")
            .and_then(Value::as_u64)
            .map_or(0, |v| v as u32),
        explicit: s(d, "/contentRating/label").as_deref() == Some("EXPLICIT"),
        playable: d
            .pointer("/playability/playable")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        added_at: None,
    })
}

pub fn gql_album(d: &Value) -> Option<Album> {
    let uri = s(d, "/uri")?;
    let id = id_of(&uri, "album")?;
    let kind = match s(d, "/type").as_deref() {
        Some("SINGLE" | "EP") => AlbumKind::Single,
        Some("COMPILATION") => AlbumKind::Compilation,
        _ => AlbumKind::Album,
    };
    Some(Album {
        id,
        uri,
        name: s(d, "/name").unwrap_or_default(),
        artists: gql_artists(d.get("artists")),
        images: sources(d.pointer("/coverArt/sources")),
        release_date: d
            .pointer("/date/year")
            .and_then(Value::as_u64)
            .map(|y| y.to_string())
            .unwrap_or_default(),
        kind,
        total_tracks: 0,
        tracks: Vec::new(),
    })
}

pub fn gql_artist(d: &Value) -> Option<Artist> {
    let uri = s(d, "/uri")?;
    let id = id_of(&uri, "artist")?;
    Some(Artist {
        id,
        uri,
        name: s(d, "/profile/name").unwrap_or_default(),
        images: sources(d.pointer("/visuals/avatarImage/sources")),
        followers: None,
        genres: Vec::new(),
    })
}

pub fn gql_playlist(d: &Value) -> Option<Playlist> {
    let uri = s(d, "/uri")?;
    let id = id_of(&uri, "playlist")?;
    Some(Playlist {
        id,
        uri,
        name: s(d, "/name").unwrap_or_default(),
        description: s(d, "/description").unwrap_or_default(),
        owner: s(d, "/ownerV2/data/name").unwrap_or_default(),
        images: sources(d.pointer("/images/items/0/sources")),
        can_edit: None,
        mosaic: Vec::new(),
        total_tracks: 0,
    })
}

/// `color-lyrics/v2` JSON.
pub fn lyrics(v: &Value) -> Option<Lyrics> {
    let l = v.get("lyrics")?;
    let lines = l
        .get("lines")?
        .as_array()?
        .iter()
        .filter_map(|line| {
            let text = line.get("words")?.as_str()?.to_owned();
            let start_ms = line
                .get("startTimeMs")
                .and_then(|t| t.as_str().and_then(|s| s.parse().ok()).or(t.as_u64()))
                .map_or(0, |t| t as u32);
            Some(LyricLine { start_ms, text })
        })
        .collect::<Vec<_>>();
    Some(Lyrics {
        synced: l.get("syncType").and_then(Value::as_str) == Some("LINE_SYNCED"),
        lines,
        provider: s(l, "/providerDisplayName"),
    })
}

/// Parse `user-profile-view/v3/profile/<user>` JSON. Unknown or missing
/// fields are left empty; the username is the fallback name.
pub fn user_profile(username: &str, v: &Value) -> UserProfile {
    let str_at = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    let https = |u: Option<String>| u.filter(|u| u.starts_with("https://"));
    let playlists = v
        .get("public_playlists")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|p| {
                    let uri = str_at(p, "uri")?;
                    let (kind, id) = split_uri(&uri)?;
                    if kind != "playlist" {
                        return None;
                    }
                    let owner = str_at(p, "owner_uri")
                        .and_then(|o| o.strip_prefix("spotify:user:").map(str::to_owned))
                        .unwrap_or_else(|| username.to_owned());
                    Some(Playlist {
                        id: id.to_owned(),
                        uri: format!("spotify:playlist:{id}"),
                        name: str_at(p, "name").unwrap_or_default(),
                        description: String::new(),
                        owner,
                        images: https(str_at(p, "image_url"))
                            .map(|url| {
                                vec![Image {
                                    url,
                                    width: None,
                                    height: None,
                                }]
                            })
                            .unwrap_or_default(),
                        can_edit: None,
                        mosaic: Vec::new(),
                        total_tracks: 0,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    UserProfile {
        id: username.to_owned(),
        name: str_at(v, "name")
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| username.to_owned()),
        image: https(str_at(v, "image_url")),
        followers: v.get("followers_count").and_then(Value::as_u64),
        following: v.get("following_count").and_then(Value::as_u64),
        playlists,
        total_playlists: v
            .get("total_public_playlists_count")
            .and_then(Value::as_u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GID: [u8; 16] = [
        0x4c, 0xb6, 0x4f, 0x1a, 0x7d, 0x1f, 0x4b, 0x4f, 0x9a, 0x6f, 0x63, 0x94, 0x39, 0x7b, 0x53,
        0x01,
    ];

    #[test]
    fn gid_roundtrip_and_uris() {
        let id = gid_to_id(&GID).unwrap();
        assert_eq!(id.len(), 22);
        assert_eq!(id_to_gid(&id).unwrap(), GID);
        assert!(gid_to_id(&[1, 2]).is_none());
        assert_eq!(split_uri("spotify:track:abc"), Some(("track", "abc")));
        assert_eq!(
            split_uri("spotify:user:bob:playlist:xyz"),
            Some(("playlist", "xyz"))
        );
        assert_eq!(split_uri("spotify:start-group:1:Folder"), None);
        assert_eq!(split_uri("https://x"), None);
    }

    #[test]
    fn dates() {
        assert_eq!(iso_date_from_secs(0), None);
        assert_eq!(
            iso_date_from_secs(951_782_400).as_deref(),
            Some("2000-02-29")
        );
        assert_eq!(
            iso_date_from_ms(1_700_000_000_000).as_deref(),
            Some("2023-11-14")
        );
    }

    #[test]
    fn track_from_protobuf() {
        let mut t = metadata::Track::new();
        t.set_gid(GID.to_vec());
        t.set_name("Song".into());
        t.set_duration(201_000);
        t.set_explicit(true);
        let mut ar = metadata::Artist::new();
        ar.set_gid(GID.to_vec());
        ar.set_name("Band".into());
        t.artist.push(ar);
        let mut al = metadata::Album::new();
        al.set_gid(GID.to_vec());
        al.set_name("Record".into());
        let mut img = metadata::Image::new();
        img.set_file_id(vec![0xab, 0xcd]);
        img.set_size(metadata::image::Size::LARGE);
        al.cover.push(img);
        t.album = Some(al).into();
        let m = track(&t).unwrap();
        assert_eq!(m.uri, format!("spotify:track:{}", m.id));
        assert_eq!(m.name, "Song");
        assert_eq!(m.duration_ms, 201_000);
        assert!(m.explicit && m.playable);
        assert_eq!(m.artist_line(), "Band");
        let img = &m.album.unwrap().images[0];
        assert_eq!(img.url, "https://i.scdn.co/image/abcd");
        assert_eq!(img.width, Some(640));
        assert!(track(&metadata::Track::new()).is_none(), "no gid");
    }

    #[test]
    fn graphql_track_is_lenient() {
        let v: Value = serde_json::json!({
            "uri": "spotify:track:t1",
            "name": "N",
            "duration": {"totalMilliseconds": 1000},
            "artists": {"items": [{"uri": "spotify:artist:a1", "profile": {"name": "A"}}, {"bogus": 1}]},
            "albumOfTrack": {"uri": "spotify:album:b1", "name": "B", "coverArt": {"sources": [{"url": "u", "width": 64}]}},
            "contentRating": {"label": "EXPLICIT"}
        });
        let t = gql_track(&v).unwrap();
        assert_eq!(t.id, "t1");
        assert_eq!(t.artists.len(), 1);
        assert!(t.explicit);
        assert_eq!(t.images()[0].width, Some(64));
        assert!(gql_track(&serde_json::json!({"uri": "spotify:episode:e"})).is_none());
    }

    #[test]
    fn lyrics_parse() {
        let v: Value = serde_json::json!({"lyrics": {"syncType": "LINE_SYNCED",
            "lines": [{"startTimeMs": "1500", "words": "Hello"}, {"startTimeMs": "3000", "words": "World"}],
            "providerDisplayName": "Provider"}});
        let l = lyrics(&v).unwrap();
        assert!(l.synced);
        assert_eq!(l.lines[1].start_ms, 3000);
        assert_eq!(l.provider.as_deref(), Some("Provider"));
    }

    #[test]
    fn user_profile_parses_and_tolerates_junk() {
        let v: Value = serde_json::json!({
            "uri": "spotify:user:0d2x",
            "name": "Ana",
            "image_url": "https://i.scdn.co/image/ab",
            "followers_count": 12,
            "following_count": 3,
            "total_public_playlists_count": 2,
            "public_playlists": [
                {"uri": "spotify:playlist:p1", "name": "Road", "image_url": "https://mosaic.scdn.co/640/x", "owner_uri": "spotify:user:0d2x"},
                {"uri": "spotify:artist:nope"},
                {"name": "no uri"}
            ]
        });
        let p = user_profile("0d2x", &v);
        assert_eq!(p.name, "Ana");
        assert_eq!(p.followers, Some(12));
        assert_eq!(p.following, Some(3));
        assert_eq!(p.playlists.len(), 1);
        assert_eq!(p.playlists[0].owner, "0d2x");
        let empty = user_profile("bob", &serde_json::json!({"image_url": "http://x"}));
        assert_eq!(empty.name, "bob");
        assert_eq!(empty.image, None);
    }

    #[test]
    fn list_images_fall_back_to_picture_sizes_and_format_attributes() {
        use spotify_proto::playlist4_external::{FormatListAttribute, PictureSize};
        let mut a = ListAttributes::new();
        assert!(list_images(Some(&a)).is_empty());
        let mut f = FormatListAttribute::new();
        f.set_key("image_url".into());
        f.set_value("https://dailymix-images.scdn.co/x".into());
        a.format_attributes.push(f);
        assert_eq!(
            list_images(Some(&a))[0].url,
            "https://dailymix-images.scdn.co/x"
        );
        let mut ps = PictureSize::new();
        ps.set_target_name("large".into());
        ps.set_url("https://mosaic.scdn.co/640/abc".into());
        a.picture_size.push(ps);
        let mut bad = PictureSize::new();
        bad.set_url("javascript:x".into());
        a.picture_size.push(bad);
        let imgs = list_images(Some(&a));
        assert_eq!(imgs.len(), 1);
        assert_eq!(imgs[0].width, Some(640));
        a.set_picture(GID.to_vec());
        assert!(list_images(Some(&a))[0].url.starts_with(IMAGE_CDN));
        let mut named = ListAttributes::new();
        let mut t = FormatListAttribute::new();
        t.set_key("title".into());
        t.set_value("Daily Mix 1".into());
        named.format_attributes.push(t);
        assert_eq!(list_name(Some(&named)), "Daily Mix 1");
    }
}
