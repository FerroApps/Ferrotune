use std::fmt;
use std::str::FromStr;

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid Spotify ID length: expected {expected}, got {got}")]
    InvalidIdLength { expected: usize, got: usize },

    #[error("invalid base62 character: {0:?}")]
    InvalidBase62(char),

    #[error("base62 value overflows 128-bit ID space")]
    Base62Overflow,

    #[error("invalid Spotify URI: {0}")]
    InvalidUri(String),

    #[error("unknown item type: {0}")]
    UnknownItemType(String),

    #[error("invalid hex: {0}")]
    InvalidHex(String),
}

/// Convenience alias used throughout this crate.
pub type Result<T, E = ProtocolError> = std::result::Result<T, E>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ItemType {
    Track,
    Album,
    Artist,
    Playlist,
    Episode,
    Show,
    User,
}

impl FromStr for ItemType {
    type Err = ProtocolError;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "track" => Ok(Self::Track),
            "album" => Ok(Self::Album),
            "artist" => Ok(Self::Artist),
            "playlist" => Ok(Self::Playlist),
            "episode" => Ok(Self::Episode),
            "show" => Ok(Self::Show),
            "user" => Ok(Self::User),
            other => Err(ProtocolError::UnknownItemType(other.to_owned())),
        }
    }
}

impl ItemType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Artist => "artist",
            Self::Playlist => "playlist",
            Self::Episode => "episode",
            Self::Show => "show",
            Self::User => "user",
        }
    }
}

impl fmt::Display for ItemType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
