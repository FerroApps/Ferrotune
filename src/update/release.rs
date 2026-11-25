//! The GitHub "latest release" lookup and the choice of assets.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::{Endpoints, UpdateError, download, version};

/// Largest API response we read.
const MAX_JSON: usize = 2 * 1024 * 1024;
/// When GitHub gives no reset time, wait this long after a 403/429.
pub const DEFAULT_BACKOFF_SECS: u64 = 3600;
/// Longest back-off we honour (a server value is untrusted input).
const MAX_BACKOFF_SECS: u64 = 24 * 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    pub url: String,
    #[serde(default)]
    pub size: u64,
}

/// The parts of a GitHub release the updater needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub tag: String,
    /// Version without the `v` (strict semver, no prerelease).
    pub version: String,
    /// Release page (release notes), always on github.com.
    pub html_url: String,
    pub assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct WireAsset {
    name: Option<String>,
    browser_download_url: Option<String>,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct WireRelease {
    tag_name: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    html_url: Option<String>,
    #[serde(default)]
    assets: Vec<WireAsset>,
}

/// Parse the API body. `Ok(None)` for a draft, a prerelease or a tag that
/// is not a strict semver release (never offered).
pub fn parse_release(body: &[u8]) -> Result<Option<Release>, UpdateError> {
    let w: WireRelease =
        serde_json::from_slice(body).map_err(|e| UpdateError::Release(e.to_string()))?;
    let tag = w
        .tag_name
        .ok_or_else(|| UpdateError::Release("no tag_name".into()))?;
    if w.draft || w.prerelease {
        return Ok(None);
    }
    let Some(v) = version::parse_tag(&tag) else {
        return Ok(None);
    };
    let html_url = w
        .html_url
        .filter(|u| u.starts_with("https://github.com/"))
        .unwrap_or_else(super::releases_page);
    let assets = w
        .assets
        .into_iter()
        .filter_map(|a| {
            Some(Asset {
                name: a.name?,
                url: a.browser_download_url?,
                size: a.size,
            })
        })
        .take(64)
        .collect();
    Ok(Some(Release {
        tag: tag.chars().take(64).collect(),
        version: v.to_string(),
        html_url,
        assets,
    }))
}

/// The tarball and its checksum for one platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assets {
    pub archive: Asset,
    pub checksum: Asset,
}

/// `ferrotune-v<ver>-<os>-<arch>.tar.gz`, as `release.yml` names it. Only
/// Linux x86_64 is built.
pub fn archive_name(krate: &str, version: &str, os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        ("linux", "x86_64") => Some(format!("{krate}-v{version}-linux-x86_64.tar.gz")),
        _ => None,
    }
}

/// The assets for `os`/`arch`, matched by exact name.
pub fn select_assets(release: &Release, krate: &str, os: &str, arch: &str) -> Option<Assets> {
    let name = archive_name(krate, &release.version, os, arch)?;
    let sum = format!("{name}.sha256");
    let find = |n: &str| release.assets.iter().find(|a| a.name == n).cloned();
    Some(Assets {
        archive: find(&name)?,
        checksum: find(&sum)?,
    })
}

/// Result of one API request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    /// `304`: same as the release cached with this ETag.
    NotModified,
    /// The latest release (`None`: nothing installable is published).
    Latest {
        release: Option<Release>,
        etag: Option<String>,
    },
}

/// Seconds to wait after a 403/429: `Retry-After`, else
/// `x-ratelimit-reset` (Unix time), else an hour; clamped.
pub fn backoff_secs(retry_after: Option<&str>, reset: Option<&str>, now_unix: u64) -> u64 {
    let secs = retry_after
        .and_then(|v| v.trim().parse::<u64>().ok())
        .or_else(|| {
            reset
                .and_then(|v| v.trim().parse::<u64>().ok())
                .map(|r| r.saturating_sub(now_unix))
        })
        .unwrap_or(DEFAULT_BACKOFF_SECS);
    secs.clamp(60, MAX_BACKOFF_SECS)
}

/// `GET releases/latest` with `If-None-Match`.
pub async fn check(
    ends: &Endpoints,
    etag: Option<&str>,
    now_unix: u64,
) -> Result<CheckOutcome, UpdateError> {
    let http = download::client(ends)?;
    download::check_url(&ends.api_url, ends.allow_loopback)?;
    let mut req = http
        .get(&ends.api_url)
        .timeout(Duration::from_secs(30))
        .header(reqwest::header::ACCEPT, "application/vnd.github+json");
    if let Some(e) = etag {
        req = req.header(reqwest::header::IF_NONE_MATCH, e);
    }
    let resp = req.send().await?;
    let status = resp.status().as_u16();
    let header = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    match status {
        304 => return Ok(CheckOutcome::NotModified),
        403 | 429 => {
            return Err(UpdateError::RateLimited(backoff_secs(
                header("retry-after").as_deref(),
                header("x-ratelimit-reset").as_deref(),
                now_unix,
            )));
        }
        200 => {}
        // No release published yet.
        404 => {
            return Ok(CheckOutcome::Latest {
                release: None,
                etag: None,
            });
        }
        s => return Err(UpdateError::Http(s)),
    }
    let new_etag = header("etag").map(|e| e.chars().take(256).collect());
    let body = download::read_capped(resp, MAX_JSON).await?;
    Ok(CheckOutcome::Latest {
        release: parse_release(&body)?,
        etag: new_etag,
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn release_json(tag: &str, base: &str) -> String {
        let name = format!("ferrotune-{tag}-linux-x86_64.tar.gz");
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "html_url": format!("https://github.com/FerroApps/Ferrotune/releases/tag/{tag}"),
            "assets": [
                {"name": format!("{name}.sha256"), "browser_download_url": format!("{base}/{name}.sha256"), "size": 99},
                {"name": "other.zip", "browser_download_url": format!("{base}/other.zip"), "size": 5},
                {"name": name.clone(), "browser_download_url": format!("{base}/{name}"), "size": 1234},
            ]
        })
        .to_string()
    }

    #[test]
    fn parses_and_selects_assets() {
        let r = parse_release(release_json("v0.9.0", "https://github.com/x").as_bytes())
            .unwrap()
            .unwrap();
        assert_eq!(r.version, "0.9.0");
        assert!(r.html_url.ends_with("/tag/v0.9.0"));
        let a = select_assets(&r, "ferrotune", "linux", "x86_64").unwrap();
        assert_eq!(a.archive.name, "ferrotune-v0.9.0-linux-x86_64.tar.gz");
        assert_eq!(a.archive.size, 1234);
        assert_eq!(
            a.checksum.name,
            "ferrotune-v0.9.0-linux-x86_64.tar.gz.sha256"
        );
        assert!(select_assets(&r, "ferrotune", "windows", "x86_64").is_none());
        assert!(select_assets(&r, "ferrotune", "linux", "aarch64").is_none());
        assert!(select_assets(&r, "ferrocord", "linux", "x86_64").is_none());
    }

    #[test]
    fn missing_checksum_means_no_assets() {
        let mut r = parse_release(release_json("v0.9.0", "https://github.com/x").as_bytes())
            .unwrap()
            .unwrap();
        r.assets.retain(|a| !a.name.ends_with(".sha256"));
        assert!(select_assets(&r, "ferrotune", "linux", "x86_64").is_none());
    }

    #[test]
    fn drafts_prereleases_and_odd_tags_are_ignored() {
        let j = |extra: serde_json::Value| {
            let mut v: serde_json::Value =
                serde_json::from_str(&release_json("v0.9.0", "https://github.com/x")).unwrap();
            for (k, val) in extra.as_object().unwrap() {
                v[k] = val.clone();
            }
            parse_release(v.to_string().as_bytes()).unwrap()
        };
        assert!(j(serde_json::json!({"draft": true})).is_none());
        assert!(j(serde_json::json!({"prerelease": true})).is_none());
        assert!(j(serde_json::json!({"tag_name": "v1.0.0-beta.1"})).is_none());
        assert!(j(serde_json::json!({"tag_name": "nightly"})).is_none());
        // A non-GitHub release page is replaced by the default one.
        let r = j(serde_json::json!({"html_url": "https://evil.example/"})).unwrap();
        assert_eq!(r.html_url, crate::update::releases_page());
        assert!(parse_release(b"{not json").is_err());
        assert!(parse_release(b"{}").is_err());
    }

    #[test]
    fn backoff_follows_github_headers_within_bounds() {
        assert_eq!(backoff_secs(Some("120"), None, 0), 120);
        assert_eq!(backoff_secs(None, Some("1000"), 400), 600);
        assert_eq!(backoff_secs(None, None, 0), DEFAULT_BACKOFF_SECS);
        assert_eq!(backoff_secs(Some("1"), None, 0), 60);
        assert_eq!(backoff_secs(Some("99999999"), None, 0), MAX_BACKOFF_SECS);
        assert_eq!(backoff_secs(None, Some("10"), 400), 60, "reset in the past");
    }
}
