use crate::error::{ItemType, ProtocolError, Result};
use std::fmt;

const BASE62_ALPHABET: &[u8; 62] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Spotify's base62 IDs are always 22 characters (zero-padded).
const BASE62_ID_LEN: usize = 22;

// --- Base62 helpers --------------------------------------------------------

/// Decode table: maps ASCII byte -> base62 digit (255 = invalid).
const fn build_decode_table() -> [u8; 128] {
    let mut table = [255u8; 128];
    let mut i = 0;
    while i < 62 {
        table[BASE62_ALPHABET[i] as usize] = i as u8;
        i += 1;
    }
    table
}

static DECODE_TABLE: [u8; 128] = build_decode_table();

fn base62_decode(s: &str) -> Result<u128> {
    let mut acc: u128 = 0;
    for b in s.bytes() {
        if b >= 128 {
            return Err(ProtocolError::InvalidBase62(b as char));
        }
        let digit = DECODE_TABLE[b as usize];
        if digit == 255 {
            return Err(ProtocolError::InvalidBase62(b as char));
        }
        acc = acc
            .checked_mul(62)
            .and_then(|v| v.checked_add(digit as u128))
            .ok_or(ProtocolError::Base62Overflow)?;
    }
    Ok(acc)
}

fn base62_encode(mut value: u128) -> String {
    if value == 0 {
        let mut s = String::with_capacity(BASE62_ID_LEN);
        for _ in 0..BASE62_ID_LEN {
            s.push('0');
        }
        return s;
    }

    let mut buf = [0u8; BASE62_ID_LEN];
    let mut pos = BASE62_ID_LEN;
    while value > 0 {
        pos -= 1;
        buf[pos] = BASE62_ALPHABET[(value % 62) as usize];
        value /= 62;
    }
    // Left-pad with '0'.
    for b in &mut buf[..pos] {
        *b = b'0';
    }

    // SAFETY: every byte is from BASE62_ALPHABET, which is ASCII.
    unsafe { String::from_utf8_unchecked(buf.to_vec()) }
}

// --- SpotifyId -------------------------------------------------------------

/// A 16-byte Spotify identifier (the raw binary "gid").
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpotifyId([u8; 16]);

impl SpotifyId {
    pub fn from_raw(raw: &[u8]) -> Result<Self> {
        let arr: [u8; 16] = raw.try_into().map_err(|_| ProtocolError::InvalidIdLength {
            expected: 16,
            got: raw.len(),
        })?;
        Ok(Self(arr))
    }

    pub fn to_raw(&self) -> &[u8; 16] {
        &self.0
    }

    pub fn from_base62(s: &str) -> Result<Self> {
        let n = base62_decode(s)?;
        Ok(Self(n.to_be_bytes()))
    }

    pub fn to_base62(&self) -> String {
        let n = u128::from_be_bytes(self.0);
        base62_encode(n)
    }

    /// Parse `spotify:<type>:<base62>` and return the item type alongside the id.
    pub fn from_uri(uri: &str) -> Result<(ItemType, Self)> {
        let (item_type, id_str) = split_uri(uri)?;
        let id = Self::from_base62(id_str)?;
        Ok((item_type, id))
    }
}

impl fmt::Debug for SpotifyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SpotifyId").field(&self.to_base62()).finish()
    }
}

impl fmt::Display for SpotifyId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base62())
    }
}

// --- SpotifyUri ------------------------------------------------------------

/// A parsed `spotify:<type>:<id>` URI.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct SpotifyUri {
    item_type: ItemType,
    id: SpotifyId,
}

impl SpotifyUri {
    pub fn from_uri(s: &str) -> Result<Self> {
        let (item_type, id) = SpotifyId::from_uri(s)?;
        Ok(Self { item_type, id })
    }

    pub fn to_uri(&self) -> String {
        format!("spotify:{}:{}", self.item_type, self.id.to_base62())
    }

    pub fn id(&self) -> &SpotifyId {
        &self.id
    }

    pub fn item_type(&self) -> ItemType {
        self.item_type
    }
}

impl fmt::Debug for SpotifyUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpotifyUri")
            .field("type", &self.item_type)
            .field("id", &self.id.to_base62())
            .finish()
    }
}

impl fmt::Display for SpotifyUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_uri())
    }
}

// --- FileId ----------------------------------------------------------------

/// A 20-byte file identifier (audio files, images, etc.).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId([u8; 20]);

impl FileId {
    pub fn from_raw(raw: &[u8]) -> Result<Self> {
        let arr: [u8; 20] = raw.try_into().map_err(|_| ProtocolError::InvalidIdLength {
            expected: 20,
            got: raw.len(),
        })?;
        Ok(Self(arr))
    }

    pub fn to_raw(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn to_base16(&self) -> String {
        let mut s = String::with_capacity(40);
        for b in &self.0 {
            s.push(HEX_CHARS[(b >> 4) as usize]);
            s.push(HEX_CHARS[(b & 0x0f) as usize]);
        }
        s
    }
}

const HEX_CHARS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

impl fmt::Debug for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("FileId").field(&self.to_base16()).finish()
    }
}

impl fmt::Display for FileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_base16())
    }
}

// --- URI splitting ---------------------------------------------------------

fn split_uri(uri: &str) -> Result<(ItemType, &str)> {
    let mut parts = uri.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("spotify"), Some(kind), Some(id), None) if !id.is_empty() => {
            let item_type = kind.parse::<ItemType>()?;
            Ok((item_type, id))
        }
        _ => Err(ProtocolError::InvalidUri(uri.to_owned())),
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base62_roundtrip() {
        let id = SpotifyId::from_base62("6rqhFgbbKwnb9MLmUQDhG6").unwrap();
        assert_eq!(id.to_base62(), "6rqhFgbbKwnb9MLmUQDhG6");
    }

    #[test]
    fn base62_zero() {
        let id = SpotifyId::from_raw(&[0u8; 16]).unwrap();
        assert_eq!(id.to_base62(), "0000000000000000000000");
    }

    #[test]
    fn raw_roundtrip() {
        let raw = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10,
        ];
        let id = SpotifyId::from_raw(&raw).unwrap();
        assert_eq!(id.to_raw(), &raw);
    }

    #[test]
    fn raw_wrong_len() {
        assert!(SpotifyId::from_raw(&[0; 15]).is_err());
        assert!(SpotifyId::from_raw(&[0; 17]).is_err());
    }

    #[test]
    fn uri_parse() {
        let (ty, id) = SpotifyId::from_uri("spotify:track:6rqhFgbbKwnb9MLmUQDhG6").unwrap();
        assert_eq!(ty, ItemType::Track);
        assert_eq!(id.to_base62(), "6rqhFgbbKwnb9MLmUQDhG6");
    }

    #[test]
    fn uri_bad_scheme() {
        assert!(SpotifyId::from_uri("notspotify:track:abc").is_err());
    }

    #[test]
    fn uri_unknown_type() {
        assert!(SpotifyId::from_uri("spotify:bogus:6rqhFgbbKwnb9MLmUQDhG6").is_err());
    }

    #[test]
    fn spotify_uri_roundtrip() {
        let uri = SpotifyUri::from_uri("spotify:album:6rqhFgbbKwnb9MLmUQDhG6").unwrap();
        assert_eq!(uri.item_type(), ItemType::Album);
        assert_eq!(uri.to_uri(), "spotify:album:6rqhFgbbKwnb9MLmUQDhG6");
    }

    #[test]
    fn file_id_roundtrip() {
        let raw = [
            0xde, 0xad, 0xbe, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99,
            0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        ];
        let fid = FileId::from_raw(&raw).unwrap();
        assert_eq!(fid.to_base16(), "deadbeef00112233445566778899aabbccddeeff");
        assert_eq!(fid.to_raw(), &raw);
    }

    #[test]
    fn file_id_wrong_len() {
        assert!(FileId::from_raw(&[0; 19]).is_err());
        assert!(FileId::from_raw(&[0; 21]).is_err());
    }

    #[test]
    fn invalid_base62_char() {
        assert!(SpotifyId::from_base62("6rqhFgbbKwnb9MLm!QDhG6").is_err());
    }
}
