//! HTTPS client for the updater: GitHub hosts only, capped bodies,
//! timeouts, streaming SHA-256.

use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use super::{Endpoints, UpdateError, redact_url};

/// Largest download (tarball or checksum).
pub const MAX_DOWNLOAD: u64 = 300 * 1024 * 1024;
/// Largest `.sha256` file.
pub const MAX_CHECKSUM: usize = 4096;

/// Hosts a download may start at or be redirected to.
pub const ALLOWED_HOSTS: &[&str] = &[
    "api.github.com",
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Is `url` a host the updater may fetch from? `https` to a GitHub host,
/// or (only with a loopback `FERRO_RELEASE_API`) `http` to loopback.
pub fn url_allowed(url: &url::Url, allow_loopback: bool) -> bool {
    match url.scheme() {
        "https" => url
            .host_str()
            .is_some_and(|h| ALLOWED_HOSTS.iter().any(|a| h.eq_ignore_ascii_case(a))),
        "http" if allow_loopback => match url.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
            None => false,
        },
        _ => false,
    }
}

pub fn check_url(raw: &str, allow_loopback: bool) -> Result<url::Url, UpdateError> {
    let u = url::Url::parse(raw).map_err(|_| UpdateError::BlockedUrl("<invalid url>".into()))?;
    if url_allowed(&u, allow_loopback) {
        Ok(u)
    } else {
        Err(UpdateError::BlockedUrl(redact_url(raw)))
    }
}

/// A client whose redirects stay on the allowed hosts.
pub fn client(ends: &Endpoints) -> Result<reqwest::Client, UpdateError> {
    let loopback = ends.allow_loopback;
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 8 {
            attempt.error("too many redirects")
        } else if url_allowed(attempt.url(), loopback) {
            attempt.follow()
        } else {
            tracing::warn!(to = %redact_url(attempt.url().as_str()), "update: redirect blocked");
            attempt.stop()
        }
    });
    reqwest::Client::builder()
        .user_agent(super::user_agent())
        .redirect(policy)
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(60))
        .https_only(!loopback)
        .build()
        .map_err(UpdateError::from)
}

/// Read a whole (small) body, refusing more than `cap` bytes.
pub async fn read_capped(mut resp: reqwest::Response, cap: usize) -> Result<Vec<u8>, UpdateError> {
    if resp.content_length().is_some_and(|n| n > cap as u64) {
        return Err(UpdateError::TooLarge);
    }
    let mut out = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if out.len() + chunk.len() > cap {
            return Err(UpdateError::TooLarge);
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

async fn get(
    http: &reqwest::Client,
    url: &str,
    allow_loopback: bool,
) -> Result<reqwest::Response, UpdateError> {
    check_url(url, allow_loopback)?;
    tracing::info!(url = %redact_url(url), "update: downloading");
    let resp = http
        .get(url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .await?;
    // A blocked redirect stops with the 3xx response itself.
    if !resp.status().is_success() {
        return Err(UpdateError::Http(resp.status().as_u16()));
    }
    Ok(resp)
}

/// Fetch a small text file (the `.sha256`).
pub async fn fetch_small(
    http: &reqwest::Client,
    url: &str,
    allow_loopback: bool,
) -> Result<Vec<u8>, UpdateError> {
    let resp = get(http, url, allow_loopback).await?;
    read_capped(resp, MAX_CHECKSUM).await
}

/// Stream `url` into `dest` (created), hashing on the way. `progress`
/// gets `(received, total)`. Returns the lowercase hex SHA-256.
pub async fn fetch_to_file(
    http: &reqwest::Client,
    url: &str,
    allow_loopback: bool,
    dest: &Path,
    cap: u64,
    progress: &(dyn Fn(u64, Option<u64>) + Send + Sync),
) -> Result<String, UpdateError> {
    let mut resp = get(http, url, allow_loopback).await?;
    let total = resp.content_length();
    if total.is_some_and(|n| n > cap) {
        return Err(UpdateError::TooLarge);
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    progress(0, total);
    while let Some(chunk) = resp.chunk().await? {
        received = received.saturating_add(chunk.len() as u64);
        if received > cap {
            return Err(UpdateError::TooLarge);
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        progress(received, total);
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(hex(&hasher.finalize()))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The digest for `file_name` in a `sha256sum` file (`<hex>  <name>` or
/// `<hex> *<name>`). A file with one bare digest is accepted too.
pub fn parse_checksum(text: &str, file_name: &str) -> Option<String> {
    let valid = |h: &str| h.len() == 64 && h.bytes().all(|c| c.is_ascii_hexdigit());
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    let first = lines.clone().next()?;
    if valid(first) && lines.nth(1).is_none() {
        return Some(first.to_ascii_lowercase());
    }
    text.lines().find_map(|l| {
        let (hash, name) = l.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        let name = name.rsplit('/').next().unwrap_or(name);
        (valid(hash) && name == file_name).then(|| hash.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_github_hosts_over_https() {
        let ok = |s: &str, lb: bool| url_allowed(&url::Url::parse(s).unwrap(), lb);
        assert!(ok(
            "https://github.com/FerroApps/Ferrotune/releases/download/v1/x",
            false
        ));
        assert!(ok("https://objects.githubusercontent.com/x?sig=1", false));
        assert!(ok("https://release-assets.githubusercontent.com/x", false));
        assert!(ok("https://api.github.com/repos/x", false));
        assert!(!ok("http://github.com/x", false), "https only");
        assert!(!ok("https://github.com.evil.example/x", false));
        assert!(!ok("https://evil.example/x", false));
        assert!(!ok("http://127.0.0.1:9/x", false), "loopback only in tests");
        assert!(ok("http://127.0.0.1:9/x", true));
        assert!(!ok("http://10.0.0.1/x", true));
        assert!(!ok("file:///etc/passwd", true));
    }

    #[test]
    fn checksum_files_are_parsed() {
        let h = "a".repeat(64);
        let name = "ferrotune-v1.0.0-linux-x86_64.tar.gz";
        assert_eq!(
            parse_checksum(&format!("{h}  {name}\n"), name),
            Some(h.clone())
        );
        assert_eq!(
            parse_checksum(&format!("{h} *{name}"), name),
            Some(h.clone())
        );
        assert_eq!(parse_checksum(&format!("{h}\n"), name), Some(h.clone()));
        let upper = "A".repeat(64);
        assert_eq!(
            parse_checksum(&format!("{upper}  {name}"), name),
            Some(h.clone())
        );
        assert_eq!(parse_checksum(&format!("{h}  other.tar.gz"), name), None);
        assert_eq!(parse_checksum("xyz  ferrotune", name), None);
        assert_eq!(parse_checksum("", name), None);
    }
}
