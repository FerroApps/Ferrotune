//! Metadata resolution — types and URI builders for fetching track, album,
//! artist, episode and show metadata via Mercury or SpClient.
//!
//! # Mercury
//!
//! Metadata is fetched by sending a Mercury GET to:
//!
//! ```text
//! hm://metadata/4/<type>/<32-char hex gid>
//! ```
//!
//! # SpClient
//!
//! The equivalent HTTP path is:
//!
//! ```text
//! /metadata/4/<type>/<32-char hex gid>
//! ```

use crate::id::{FileId, SpotifyId};

// ---------------------------------------------------------------------------
// MetadataType
// ---------------------------------------------------------------------------

/// Metadata types that can be resolved via Mercury or SpClient.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetadataType {
    Track,
    Album,
    Artist,
    Episode,
    Show,
}

impl MetadataType {
    /// The lowercase string used in Mercury URIs and SpClient paths.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Episode => "episode",
            Self::Show => "show",
        }
    }
}

impl std::fmt::Display for MetadataType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// URI / path builders
// ---------------------------------------------------------------------------

/// Build the Mercury URI for a metadata request.
///
/// Returns e.g. `hm://metadata/4/track/a1b2c3...` (32-char hex GID).
pub fn mercury_uri(item_type: MetadataType, id: &SpotifyId) -> String {
    format!(
        "hm://metadata/4/{}/{}",
        item_type.as_str(),
        hex::encode(id.to_raw())
    )
}

/// Build the SpClient HTTP path for a metadata request.
///
/// Returns e.g. `/metadata/4/track/a1b2c3...` (32-char hex GID).
pub fn spclient_path(item_type: MetadataType, id: &SpotifyId) -> String {
    format!(
        "/metadata/4/{}/{}",
        item_type.as_str(),
        hex::encode(id.to_raw())
    )
}

// ---------------------------------------------------------------------------
// AudioFileFormat
// ---------------------------------------------------------------------------

/// Audio file format identifiers from the metadata protobuf.
///
/// These match the `AudioFile.Format` enum values in Spotify's protobuf
/// definitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum AudioFileFormat {
    OggVorbis96 = 0,
    OggVorbis160 = 1,
    OggVorbis320 = 2,
    Mp3256 = 3,
    Mp3320 = 4,
    Mp3160 = 5,
    Mp3096 = 6,
    Mp3160Enc = 7,
    Aac24 = 8,
    Aac48 = 9,
    Aac24Ohe = 10,
    Aac48Ohe = 11,
    Flac = 16,
    FlacOhe = 17,
}

impl AudioFileFormat {
    /// Try to convert from the raw protobuf enum value.
    pub fn from_raw(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self::OggVorbis96),
            1 => Some(Self::OggVorbis160),
            2 => Some(Self::OggVorbis320),
            3 => Some(Self::Mp3256),
            4 => Some(Self::Mp3320),
            5 => Some(Self::Mp3160),
            6 => Some(Self::Mp3096),
            7 => Some(Self::Mp3160Enc),
            8 => Some(Self::Aac24),
            9 => Some(Self::Aac48),
            10 => Some(Self::Aac24Ohe),
            11 => Some(Self::Aac48Ohe),
            16 => Some(Self::Flac),
            17 => Some(Self::FlacOhe),
            _ => None,
        }
    }

    /// The raw u32 value.
    pub fn as_raw(self) -> u32 {
        self as u32
    }

    /// A human-readable label for this format.
    pub fn label(self) -> &'static str {
        match self {
            Self::OggVorbis96 => "OGG Vorbis 96 kbps",
            Self::OggVorbis160 => "OGG Vorbis 160 kbps",
            Self::OggVorbis320 => "OGG Vorbis 320 kbps",
            Self::Mp3256 => "MP3 256 kbps",
            Self::Mp3320 => "MP3 320 kbps",
            Self::Mp3160 => "MP3 160 kbps",
            Self::Mp3096 => "MP3 96 kbps",
            Self::Mp3160Enc => "MP3 160 kbps (encrypted)",
            Self::Aac24 => "AAC 24 kbps",
            Self::Aac48 => "AAC 48 kbps",
            Self::Aac24Ohe => "AAC 24 kbps OHE",
            Self::Aac48Ohe => "AAC 48 kbps OHE",
            Self::Flac => "FLAC",
            Self::FlacOhe => "FLAC OHE",
        }
    }

    /// Whether this is a lossless format.
    pub fn is_lossless(self) -> bool {
        matches!(self, Self::Flac | Self::FlacOhe)
    }
}

impl std::fmt::Display for AudioFileFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// AudioFile
// ---------------------------------------------------------------------------

/// An available audio file for playback, extracted from track metadata.
#[derive(Debug, Clone)]
pub struct AudioFile {
    /// The 20-byte file identifier used to request audio-key and CDN download.
    pub file_id: FileId,
    /// The encoding format.
    pub format: AudioFileFormat,
}

// ---------------------------------------------------------------------------
// TrackMetadata
// ---------------------------------------------------------------------------

/// Minimal track metadata needed for playback decisions.
///
/// Parsed from the protobuf response to a metadata request.
#[derive(Debug, Clone)]
pub struct TrackMetadata {
    /// The 16-byte track GID.
    pub gid: [u8; 16],
    /// Track title.
    pub name: String,
    /// Album name, if present.
    pub album: Option<String>,
    /// Artist names.
    pub artists: Vec<String>,
    /// Duration in milliseconds.
    pub duration_ms: u32,
    /// Available audio files for this track.
    pub files: Vec<AudioFile>,
    /// Alternative tracks (e.g. re-releases) with their own file lists.
    pub alternatives: Vec<Vec<AudioFile>>,
    /// Whether the track is marked explicit.
    pub is_explicit: bool,
    /// Whether the track is available (considering restrictions, etc.).
    pub available: bool,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::SpotifyId;

    #[test]
    fn metadata_type_as_str() {
        assert_eq!(MetadataType::Track.as_str(), "track");
        assert_eq!(MetadataType::Album.as_str(), "album");
        assert_eq!(MetadataType::Artist.as_str(), "artist");
        assert_eq!(MetadataType::Episode.as_str(), "episode");
        assert_eq!(MetadataType::Show.as_str(), "show");
    }

    #[test]
    fn metadata_type_display() {
        assert_eq!(format!("{}", MetadataType::Track), "track");
        assert_eq!(format!("{}", MetadataType::Artist), "artist");
    }

    #[test]
    fn mercury_uri_track() {
        let id = SpotifyId::from_raw(&[
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ])
        .unwrap();
        let uri = mercury_uri(MetadataType::Track, &id);
        assert_eq!(
            uri,
            "hm://metadata/4/track/0102030405060708090a0b0c0d0e0f10"
        );
    }

    #[test]
    fn mercury_uri_album() {
        let id = SpotifyId::from_raw(&[0xaa; 16]).unwrap();
        let uri = mercury_uri(MetadataType::Album, &id);
        assert_eq!(
            uri,
            "hm://metadata/4/album/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn spclient_path_track() {
        let id = SpotifyId::from_raw(&[
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ])
        .unwrap();
        let path = spclient_path(MetadataType::Track, &id);
        assert_eq!(path, "/metadata/4/track/0102030405060708090a0b0c0d0e0f10");
    }

    #[test]
    fn spclient_path_episode() {
        let id = SpotifyId::from_raw(&[0xff; 16]).unwrap();
        let path = spclient_path(MetadataType::Episode, &id);
        assert_eq!(path, "/metadata/4/episode/ffffffffffffffffffffffffffffffff");
    }

    #[test]
    fn audio_file_format_from_raw_known() {
        assert_eq!(
            AudioFileFormat::from_raw(0),
            Some(AudioFileFormat::OggVorbis96)
        );
        assert_eq!(
            AudioFileFormat::from_raw(2),
            Some(AudioFileFormat::OggVorbis320)
        );
        assert_eq!(AudioFileFormat::from_raw(16), Some(AudioFileFormat::Flac));
        assert_eq!(
            AudioFileFormat::from_raw(17),
            Some(AudioFileFormat::FlacOhe)
        );
    }

    #[test]
    fn audio_file_format_from_raw_unknown() {
        assert_eq!(AudioFileFormat::from_raw(99), None);
        assert_eq!(AudioFileFormat::from_raw(12), None);
        assert_eq!(AudioFileFormat::from_raw(15), None);
    }

    #[test]
    fn audio_file_format_roundtrip() {
        let format = AudioFileFormat::OggVorbis320;
        assert_eq!(AudioFileFormat::from_raw(format.as_raw()), Some(format));
    }

    #[test]
    fn audio_file_format_all_roundtrip() {
        let all = [
            AudioFileFormat::OggVorbis96,
            AudioFileFormat::OggVorbis160,
            AudioFileFormat::OggVorbis320,
            AudioFileFormat::Mp3256,
            AudioFileFormat::Mp3320,
            AudioFileFormat::Mp3160,
            AudioFileFormat::Mp3096,
            AudioFileFormat::Mp3160Enc,
            AudioFileFormat::Aac24,
            AudioFileFormat::Aac48,
            AudioFileFormat::Aac24Ohe,
            AudioFileFormat::Aac48Ohe,
            AudioFileFormat::Flac,
            AudioFileFormat::FlacOhe,
        ];
        for fmt in all {
            assert_eq!(AudioFileFormat::from_raw(fmt.as_raw()), Some(fmt));
        }
    }

    #[test]
    fn audio_file_format_is_lossless() {
        assert!(AudioFileFormat::Flac.is_lossless());
        assert!(AudioFileFormat::FlacOhe.is_lossless());
        assert!(!AudioFileFormat::OggVorbis320.is_lossless());
        assert!(!AudioFileFormat::Mp3320.is_lossless());
        assert!(!AudioFileFormat::Aac48.is_lossless());
    }

    #[test]
    fn audio_file_format_label() {
        assert_eq!(AudioFileFormat::OggVorbis320.label(), "OGG Vorbis 320 kbps");
        assert_eq!(AudioFileFormat::Flac.label(), "FLAC");
        assert_eq!(
            AudioFileFormat::Mp3160Enc.label(),
            "MP3 160 kbps (encrypted)"
        );
    }

    #[test]
    fn audio_file_format_display() {
        assert_eq!(
            format!("{}", AudioFileFormat::OggVorbis160),
            "OGG Vorbis 160 kbps"
        );
    }

    #[test]
    fn mercury_uri_from_base62() {
        // Parse a real-looking Spotify ID and verify the URI format.
        let id = SpotifyId::from_base62("6rqhFgbbKwnb9MLmUQDhG6").unwrap();
        let uri = mercury_uri(MetadataType::Track, &id);
        assert!(uri.starts_with("hm://metadata/4/track/"));
        // The hex part should be exactly 32 characters.
        let hex_part = uri.strip_prefix("hm://metadata/4/track/").unwrap();
        assert_eq!(hex_part.len(), 32);
        // Should be valid hex.
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn track_metadata_struct() {
        // Verify the struct can be constructed and cloned.
        let meta = TrackMetadata {
            gid: [0u8; 16],
            name: "Test Track".to_string(),
            album: Some("Test Album".to_string()),
            artists: vec!["Artist One".to_string(), "Artist Two".to_string()],
            duration_ms: 240_000,
            files: vec![],
            alternatives: vec![],
            is_explicit: false,
            available: true,
        };
        let cloned = meta.clone();
        assert_eq!(cloned.name, "Test Track");
        assert_eq!(cloned.artists.len(), 2);
        assert_eq!(cloned.duration_ms, 240_000);
    }
}
