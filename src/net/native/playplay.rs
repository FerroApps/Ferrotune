//! PlayPlay DRM key exchange: obtains AES decryption keys for audio files
//! via the spclient `/playplay/v1/key/{file_id}` endpoint, then decrypts
//! the downloaded audio with AES-128-CTR.
//!
//! The endpoint returns an *obfuscated* key that must be transformed into
//! the actual AES key.  The deobfuscation runs in Spotify's own PlayPlay
//! VM, so we delegate to an external helper binary (compatible with the
//! community "unplayplay" tool).

use aes::Aes128;
use cipher::{KeyIvInit, StreamCipher};
use librespot_core::{FileId, Session};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Fixed AES-128-CTR IV used by Spotify for all audio files.
const AES_IV: [u8; 16] = [
    0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f, 0x63, 0x0d, 0x93,
];

/// Fixed token sent in every PlayPlayLicenseRequest (version 2).
const PLAYPLAY_TOKEN: [u8; 16] = [
    0x01, 0xe1, 0x32, 0xca, 0xe5, 0x27, 0xbd, 0x21, 0x62, 0x0e, 0x82, 0x2f, 0x58, 0x51, 0x49, 0x32,
];

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn from_hex(s: &str) -> Result<Vec<u8>, String> {
    if !s.len().is_multiple_of(2) {
        return Err("odd-length hex string".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn file_id_hex(fid: &FileId) -> String {
    fid.to_base16().unwrap_or_default()
}

/// Request a PlayPlay license and return the obfuscated key bytes.
pub async fn request_obfuscated_key(
    session: &Session,
    file_id: &FileId,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let hex = file_id_hex(file_id);
    let endpoint = format!("/playplay/v1/key/{hex}");

    // Hand-coded PlayPlayLicenseRequest protobuf:
    //   field 1 (version):       tag 0x08, varint 2
    //   field 2 (token):         tag 0x12, len 16, [16 bytes]
    //   field 4 (interactivity): tag 0x20, varint 1  (INTERACTIVE)
    //   field 5 (content_type):  tag 0x28, varint 1  (AUDIO_TRACK)
    let mut body = Vec::with_capacity(24);
    body.push(0x08);
    body.push(0x02);
    body.push(0x12);
    body.push(0x10);
    body.extend_from_slice(&PLAYPLAY_TOKEN);
    body.push(0x20);
    body.push(0x01);
    body.push(0x28);
    body.push(0x01);

    let raw = session
        .spclient()
        .request(&http::Method::POST, &endpoint, None, Some(&body))
        .await?;

    parse_obfuscated_key(&raw)
}

/// Minimal protobuf parser: extract the bytes field at tag 0x0A (field 1,
/// length-delimited) from a PlayPlayLicenseResponse.
fn parse_obfuscated_key(data: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut pos = 0;
    while pos < data.len() {
        let tag = data[pos];
        pos += 1;
        if tag == 0x0A {
            let (len, consumed) = decode_varint(&data[pos..])?;
            pos += consumed;
            let end = pos + len as usize;
            if end > data.len() {
                return Err("PlayPlay response truncated".into());
            }
            return Ok(data[pos..end].to_vec());
        }
        let wire_type = tag & 0x07;
        match wire_type {
            0 => {
                let (_, consumed) = decode_varint(&data[pos..])?;
                pos += consumed;
            }
            2 => {
                let (len, consumed) = decode_varint(&data[pos..])?;
                pos += consumed + len as usize;
            }
            _ => return Err(format!("unknown wire type {wire_type}").into()),
        }
    }
    Err("obfuscated_key not found in PlayPlay response".into())
}

fn decode_varint(data: &[u8]) -> Result<(u64, usize), Box<dyn std::error::Error + Send + Sync>> {
    let mut result: u64 = 0;
    let mut shift = 0;
    for (i, &byte) in data.iter().enumerate() {
        result |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 {
            return Ok((result, i + 1));
        }
        shift += 7;
        if shift >= 64 {
            return Err("varint too long".into());
        }
    }
    Err("incomplete varint".into())
}

/// Resolve the helper binary path.
fn helper_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("FERRO_PLAYPLAY_HELPER") {
        return std::path::PathBuf::from(p);
    }
    let base = directories::BaseDirs::new()
        .map(|b| b.data_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    base.join("ferrotune").join("playplay-helper")
}

/// Call the external deobfuscation helper to transform the obfuscated key
/// into the actual AES key.
///
/// The helper takes two hex arguments (file_id, obfuscated_key) and prints
/// the 16-byte AES key as hex on stdout.
pub async fn deobfuscate_key(
    file_id: &FileId,
    obfuscated_key: &[u8],
) -> Result<[u8; 16], Box<dyn std::error::Error + Send + Sync>> {
    let path = helper_path();
    if !path.exists() {
        return Err(format!(
            "PlayPlay helper not found at {}. \
             Set FERRO_PLAYPLAY_HELPER or place an unplayplay-compatible \
             binary there to enable audio decryption.",
            path.display()
        )
        .into());
    }

    let file_hex = file_id_hex(file_id);
    let key_hex = to_hex(obfuscated_key);

    let output = tokio::process::Command::new(&path)
        .args([&file_hex, &key_hex])
        .output()
        .await
        .map_err(|e| format!("failed to run PlayPlay helper: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("PlayPlay helper failed: {stderr}").into());
    }

    let hex_key = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let key_bytes =
        from_hex(&hex_key).map_err(|e| format!("PlayPlay helper returned invalid hex: {e}"))?;

    if key_bytes.len() != 16 {
        return Err(format!(
            "PlayPlay helper returned {} bytes, expected 16",
            key_bytes.len()
        )
        .into());
    }

    let mut key = [0u8; 16];
    key.copy_from_slice(&key_bytes);
    Ok(key)
}

/// Decrypt audio data in-place with AES-128-CTR.
pub fn decrypt_audio(key: &[u8; 16], data: &mut [u8]) {
    let mut cipher = Aes128Ctr::new(key.into(), &AES_IV.into());
    cipher.apply_keystream(data);
}

/// Returns `true` when the downloaded file looks encrypted (0xa7 header
/// without an immediately following OGG/MP3/FLAC magic).
pub fn is_encrypted(data: &[u8]) -> bool {
    if data.first() == Some(&0xa7) {
        return !data
            .get(..8)
            .is_some_and(|h| h.windows(4).any(|w| w == b"OggS" || w == b"fLaC"));
    }
    false
}

/// Full PlayPlay pipeline: request key, deobfuscate, decrypt, strip header.
/// Returns the decrypted OGG data ready for playback.
pub async fn decrypt_file(
    session: &Session,
    file_id: &FileId,
    encrypted: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    tracing::debug!("playplay: requesting license key");
    let obfuscated = request_obfuscated_key(session, file_id).await?;
    tracing::debug!(
        obfuscated_len = obfuscated.len(),
        "playplay: got obfuscated key"
    );

    let key = deobfuscate_key(file_id, &obfuscated).await?;
    tracing::debug!("playplay: key deobfuscated");

    let mut buf = encrypted.to_vec();
    decrypt_audio(&key, &mut buf);

    // Find OggS magic in the decrypted data.
    if let Some(pos) = buf.windows(4).position(|w| w == b"OggS") {
        Ok(buf[pos..].to_vec())
    } else {
        Err("decryption failed: output does not contain OGG data".into())
    }
}
