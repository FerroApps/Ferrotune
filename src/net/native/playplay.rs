//! PlayPlay DRM key exchange: obtains AES decryption keys for audio files
//! via the spclient `/playplay/v1/key/{file_id}` endpoint, then decrypts
//! the downloaded audio with AES-128-CTR.
//!
//! The endpoint returns an *obfuscated* key that must be transformed into
//! the actual AES key.  The deobfuscation runs in Spotify's own PlayPlay
//! VM, so we delegate to an external helper binary (compatible with the
//! community "unplayplay" tool).  The helper is auto-downloaded from a
//! user-configured URL (`FERRO_PLAYPLAY_URL`) on first use.

use std::path::PathBuf;

use aes::Aes128;
use cipher::{KeyIvInit, StreamCipher};
use librespot_core::{FileId, Session};
use sha2::{Digest, Sha256};

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Fixed AES-128-CTR IV used by Spotify for all audio files.
const AES_IV: [u8; 16] = [
    0x72, 0xe0, 0x67, 0xfb, 0xdd, 0xcb, 0xcf, 0x77, 0xeb, 0xe8, 0xbc, 0x64, 0x3f, 0x63, 0x0d, 0x93,
];

/// Build-specific license-request marker sent in every PlayPlayLicenseRequest.
const PLAYPLAY_TOKEN: [u8; 16] = [
    0x02, 0x19, 0x83, 0x16, 0xbe, 0xae, 0x6d, 0x4a, 0xf4, 0x35, 0x24, 0xd0, 0x2c, 0x37, 0x2b, 0x7e,
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

fn encode_varint(mut val: u64, out: &mut Vec<u8>) {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Build the PlayPlayLicenseRequest protobuf body.
fn build_license_request() -> Vec<u8> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut body = Vec::with_capacity(36);
    body.push(0x08);
    body.push(0x05);
    body.push(0x12);
    body.push(0x10);
    body.extend_from_slice(&PLAYPLAY_TOKEN);
    body.push(0x20);
    body.push(0x01);
    body.push(0x28);
    body.push(0x01);
    body.push(0x30);
    encode_varint(now, &mut body);
    body
}

/// Request a PlayPlay license and return the obfuscated key bytes.
///
/// Uses reqwest directly (bypassing librespot's spclient) so that
/// error responses include the full body for diagnostics.
pub async fn request_obfuscated_key(
    session: &Session,
    file_id: &FileId,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let hex = file_id_hex(file_id);
    let endpoint = format!("/playplay/v1/key/{hex}");

    let body = build_license_request();

    let base_url = session
        .spclient()
        .base_url()
        .await
        .map_err(|e| format!("playplay: could not resolve spclient base URL: {e}"))?;

    let url = format!("{base_url}{endpoint}");

    let token = session
        .login5()
        .auth_token()
        .await
        .map_err(|e| format!("playplay: could not get auth token: {e}"))?;

    let client_token = session.spclient().client_token().await.ok();

    tracing::debug!(
        url = %url,
        body_hex = %to_hex(&body),
        has_client_token = client_token.is_some(),
        "playplay: requesting license key (direct)"
    );

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let dpop_proof = session
        .create_dpop_proof("POST", &url, Some(&token.access_token))
        .map_err(|e| format!("playplay: DPoP proof creation failed: {e}"))?;

    tracing::debug!("playplay: attaching DPoP proof to license request");

    let mut req = http
        .post(&url)
        .header("Content-Type", "application/x-protobuf")
        .header("Accept", "application/x-protobuf")
        .header(
            "Authorization",
            format!("{} {}", token.token_type, token.access_token),
        )
        .header("DPoP", &dpop_proof);

    if let Some(ref ct) = client_token {
        req = req.header("client-token", ct.as_str());
    }

    let resp = req.body(body).send().await?;

    let status = resp.status();
    let resp_headers = format!("{:?}", resp.headers());
    let resp_body = resp.bytes().await?;

    if !status.is_success() {
        let body_text = String::from_utf8_lossy(&resp_body);
        tracing::warn!(
            status = %status,
            headers = %resp_headers,
            body_len = resp_body.len(),
            body_text = %body_text,
            body_hex = %to_hex(&resp_body[..resp_body.len().min(256)]),
            url = %url,
            "playplay: license request failed with full diagnostics"
        );
        return Err(format!("PlayPlay license request failed: HTTP {status} — {body_text}").into());
    }

    tracing::debug!(len = resp_body.len(), "playplay: license response OK");
    parse_obfuscated_key(&resp_body)
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
fn helper_path() -> PathBuf {
    if let Ok(p) = std::env::var("FERRO_PLAYPLAY_HELPER") {
        return PathBuf::from(p);
    }
    let base = directories::BaseDirs::new()
        .map(|b| b.data_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ferrotune").join("playplay-helper")
}

const HELPER_URL: &str =
    "https://git.zenvairo.es/dasemu/unplayplay/releases/latest/download/unplayplay-main-linux-x64";

/// Download the helper binary if it's not cached.  Uses `FERRO_PLAYPLAY_URL`
/// when set, otherwise the built-in default.  Optional `FERRO_PLAYPLAY_SHA256`
/// verifies integrity after download.
async fn ensure_helper() -> Result<PathBuf, Box<dyn std::error::Error + Send + Sync>> {
    let path = helper_path();
    if path.exists() {
        return Ok(path);
    }

    let url = std::env::var("FERRO_PLAYPLAY_URL").unwrap_or_else(|_| HELPER_URL.to_owned());

    tracing::info!(url = %url, dest = %path.display(), "playplay: downloading helper");

    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;

    let resp = http.get(&url).send().await?;
    if !resp.status().is_success() {
        return Err(format!("playplay helper download failed: HTTP {}", resp.status()).into());
    }

    let bytes = resp.bytes().await?;

    if let Ok(expected_hex) = std::env::var("FERRO_PLAYPLAY_SHA256") {
        let actual_hex = to_hex(&Sha256::digest(&bytes));
        if !actual_hex.eq_ignore_ascii_case(&expected_hex) {
            return Err(format!(
                "playplay helper checksum mismatch: expected {expected_hex}, got {actual_hex}"
            )
            .into());
        }
        tracing::debug!("playplay: checksum verified");
    }

    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let tmp = path.with_extension("part");
    tokio::fs::write(&tmp, &bytes).await?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }

    tokio::fs::rename(&tmp, &path).await?;
    tracing::info!(path = %path.display(), "playplay: helper installed");
    Ok(path)
}

/// Call the external deobfuscation helper to transform the obfuscated key
/// into the actual AES key.
///
/// The helper takes two hex arguments (file_id, obfuscated_key) and prints
/// the 16-byte AES key as hex on stdout.  If missing, it is downloaded from
/// `FERRO_PLAYPLAY_URL`.
pub async fn deobfuscate_key(
    file_id: &FileId,
    obfuscated_key: &[u8],
) -> Result<[u8; 16], Box<dyn std::error::Error + Send + Sync>> {
    let path = ensure_helper().await?;

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

/// Returns `true` when the downloaded file looks encrypted rather than
/// being a recognisable audio container (OGG, FLAC, MP3).
pub fn is_encrypted(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    if data.starts_with(b"OggS") || data.starts_with(b"fLaC") {
        return false;
    }
    // MP3 frame sync or ID3v2 tag
    if data.starts_with(b"ID3") || (data[0] == 0xFF && data[1] & 0xE0 == 0xE0) {
        return false;
    }
    // Anything else (including the common 0xa7 Spotify header) is encrypted.
    true
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
