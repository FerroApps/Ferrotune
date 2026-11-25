//! Audio resolution: fetch track metadata, select the best file for the
//! requested bitrate, resolve CDN URLs via storage-resolve, and download
//! the full file.  Uses the same spclient endpoints as the official client.

use librespot_core::{FileId, Session, SpotifyUri};
use librespot_metadata::audio::AudioFileFormat;
use librespot_metadata::{Metadata, Track};

use super::super::playback::BitrateKey;
use super::playplay;

/// Raw audio bytes ready for decoding + the format they are in.
pub struct AudioData {
    pub bytes: Vec<u8>,
    #[allow(dead_code)]
    pub format: AudioFileFormat,
}

/// Preferred OGG Vorbis formats in descending quality for each bitrate.
fn preferred_formats(bitrate: BitrateKey) -> &'static [AudioFileFormat] {
    match bitrate {
        BitrateKey::High => &[
            AudioFileFormat::OGG_VORBIS_320,
            AudioFileFormat::OGG_VORBIS_160,
            AudioFileFormat::OGG_VORBIS_96,
        ],
        BitrateKey::Normal => &[
            AudioFileFormat::OGG_VORBIS_160,
            AudioFileFormat::OGG_VORBIS_320,
            AudioFileFormat::OGG_VORBIS_96,
        ],
        BitrateKey::Low => &[
            AudioFileFormat::OGG_VORBIS_96,
            AudioFileFormat::OGG_VORBIS_160,
            AudioFileFormat::OGG_VORBIS_320,
        ],
    }
}

fn select_file(
    files: &librespot_metadata::audio::AudioFiles,
    bitrate: BitrateKey,
) -> Option<(AudioFileFormat, FileId)> {
    for &fmt in preferred_formats(bitrate) {
        if let Some(&fid) = files.get(&fmt) {
            return Some((fmt, fid));
        }
    }
    None
}

pub async fn load_track(
    session: &Session,
    uri: &str,
    bitrate: BitrateKey,
) -> Result<AudioData, Box<dyn std::error::Error + Send + Sync>> {
    let spotify_uri = SpotifyUri::from_uri(uri)?;

    // 1. Fetch track metadata (file IDs, alternatives).
    let track = Track::get(session, &spotify_uri).await?;

    // 2. Select the best available file.
    let (format, file_id) = select_file(&track.files, bitrate)
        .or_else(|| {
            // Try alternatives (region-restricted tracks).
            track.alternatives.iter().find_map(|_alt_uri| {
                // We only have the URI of the alternative, not its full
                // metadata.  The caller can retry with the alternative URI
                // if needed; for now we skip alternatives that require a
                // second metadata fetch.
                None::<(AudioFileFormat, FileId)>
            })
        })
        .ok_or("no suitable audio file found for this track")?;

    tracing::debug!(
        format = ?format,
        file = %file_id_hex(&file_id),
        "native: selected audio file"
    );

    // 3. Resolve CDN URL via storage-resolve (v2 first, v1 fallback).
    let cdn_url = resolve_cdn(session, &file_id).await?;

    // 4. Download the full file from CDN.
    let raw = download(session, &cdn_url).await?;

    // 5. If the file is DRM-encrypted, use PlayPlay to decrypt it.
    let bytes = if playplay::is_encrypted(&raw) {
        tracing::info!("native: file is encrypted, using PlayPlay to decrypt");
        playplay::decrypt_file(session, &file_id, &raw).await?
    } else {
        raw
    };

    Ok(AudioData { bytes, format })
}

fn file_id_hex(fid: &FileId) -> String {
    fid.to_base16().unwrap_or_default()
}

/// Resolve a file id to a CDN URL.  Tries storage-resolve v2 first
/// (what the official client uses), falling back to v1 (librespot's
/// path).  Both return the same `StorageResolveResponse` protobuf.
async fn resolve_cdn(
    session: &Session,
    file_id: &FileId,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let hex = file_id_hex(file_id);

    // Try v2 first (official client path).
    let v2_endpoint = format!("/storage-resolve/v2/files/audio/interactive/{hex}");
    let response = session
        .spclient()
        .request(&http::Method::GET, &v2_endpoint, None, None)
        .await;

    let raw = match response {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::debug!(error = %e, "storage-resolve v2 failed, trying v1");
            session.spclient().get_audio_storage(file_id).await?
        }
    };

    // Parse StorageResolveResponse protobuf.
    use librespot_protocol::storage_resolve::StorageResolveResponse;
    use protobuf::Message;
    let msg = StorageResolveResponse::parse_from_bytes(&raw)?;

    // Pick the first CDN URL.
    msg.cdnurl
        .first()
        .cloned()
        .ok_or_else(|| "no CDN URLs in storage-resolve response".into())
}

/// Download from a CDN URL.  CDN URLs are pre-signed and need no auth.
async fn download(
    session: &Session,
    url: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let response = session.spclient().request_url(url).await?;
    Ok(response.to_vec())
}
