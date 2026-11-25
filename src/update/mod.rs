//! In-app updater: finds the latest GitHub Release of Ferrotune and, for
//! an `install.sh` install, downloads, verifies and installs it.
//!
//! No iced in here (the app glue is `app/updates.rs`, the views
//! `ui/update.rs`). Every function that touches the network or the disk is
//! async or meant for `spawn_blocking`; the pure parts (version compare,
//! asset choice, install-method detection, schedule, desktop-entry rewrite,
//! safe extraction) are unit-tested.
//!
//! * [`release`]: `GET /repos/FerroApps/Ferrotune/releases/latest` with the
//!   stored ETag; drafts and prereleases are ignored.
//! * [`method`]: how this copy was installed, which decides what "update"
//!   means (self-update, `cargo install` hint, or the release page).
//! * [`install`]: download + SHA-256 + safe extract + atomic swap, the same
//!   files and paths `install.sh` writes.
//!
//! Integrity: the `.sha256` comes from the same release, so it only proves
//! the tarball arrived intact. Releases are not signed yet (see
//! `docs/architecture.md`, "Updates").

pub mod download;
pub mod extract;
pub mod install;
pub mod method;
pub mod release;
pub mod restart;
pub mod schedule;
pub mod state;
pub mod version;

pub use method::InstallMethod;
pub use release::{CheckOutcome, Release};

/// GitHub `owner/repo` of the releases.
pub const REPO: &str = "FerroApps/Ferrotune";
/// Crate / binary name (asset names start with it).
pub const CRATE: &str = "ferrotune";
/// Version of the running binary.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Every binary a release tarball ships and `install.sh` installs.
pub const BINARIES: &[&str] = &[CRATE];

/// The "latest release" endpoint.
pub fn default_api_url() -> String {
    format!("https://api.github.com/repos/{REPO}/releases/latest")
}

/// Page shown when Ferrotune cannot update itself.
pub fn releases_page() -> String {
    format!("https://github.com/{REPO}/releases/latest")
}

/// The command for `cargo install` users.
pub fn cargo_command() -> String {
    format!("cargo install --git https://github.com/{REPO} --locked --force")
}

/// `User-Agent`: the app name and version, nothing else.
pub fn user_agent() -> String {
    format!("{CRATE}/{CURRENT_VERSION}")
}

/// Where the updater talks to. `FERRO_RELEASE_API` (the same variable the
/// `install.sh` tests use) replaces the full "latest release" URL; only
/// `https://`, or `http://` on loopback for local test servers, is taken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub api_url: String,
    /// Plain-HTTP loopback URLs (API, assets, redirects) are allowed: only
    /// when the override itself points at a local test server.
    pub allow_loopback: bool,
}

impl Endpoints {
    pub fn from_env() -> Self {
        let raw = std::env::var("FERRO_RELEASE_API")
            .ok()
            .filter(|v| !v.trim().is_empty());
        Self::resolve(raw.as_deref())
    }

    pub fn resolve(raw: Option<&str>) -> Self {
        match raw.and_then(|r| crate::config::safe_endpoint("FERRO_RELEASE_API", r)) {
            Some(api_url) => Self {
                allow_loopback: api_url.starts_with("http://"),
                api_url,
            },
            None => Self::default(),
        }
    }
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            api_url: default_api_url(),
            allow_loopback: false,
        }
    }
}

/// Everything that can go wrong; each message is safe to show and to log.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpdateError {
    #[error("network: {0}")]
    Net(String),
    #[error("GitHub answered {0}")]
    Http(u16),
    /// 403/429 from the API: back off for this many seconds.
    #[error("rate limited for {0}s")]
    RateLimited(u64),
    #[error("unexpected release data: {0}")]
    Release(String),
    #[error("the download is larger than allowed")]
    TooLarge,
    #[error("checksum mismatch")]
    Checksum,
    #[error("unsafe archive: {0}")]
    Archive(String),
    #[error("blocked URL: {0}")]
    BlockedUrl(String),
    #[error("cannot update here: {0}")]
    Unsupported(String),
    #[error("file error: {0}")]
    Io(String),
    #[error("cancelled")]
    Cancelled,
}

impl UpdateError {
    /// One sentence for the banner / settings.
    pub fn user_message(&self) -> String {
        match self {
            Self::Net(_) => "Could not reach GitHub. Check your connection.".into(),
            Self::Http(s) => format!("GitHub returned an error ({s})."),
            Self::RateLimited(_) => "GitHub is limiting update checks. Try again later.".into(),
            Self::Release(_) => "The release on GitHub is not in the expected format.".into(),
            Self::TooLarge => "The download is larger than allowed.".into(),
            Self::Checksum => {
                "The download is damaged (checksum mismatch). Nothing was changed.".into()
            }
            Self::Archive(_) => "The downloaded archive is not valid. Nothing was changed.".into(),
            Self::BlockedUrl(_) => "The download link points to an unexpected host.".into(),
            Self::Unsupported(m) => m.clone(),
            Self::Io(m) => format!("Could not install the update: {m}"),
            Self::Cancelled => "Update cancelled.".into(),
        }
    }
}

impl From<std::io::Error> for UpdateError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<reqwest::Error> for UpdateError {
    fn from(e: reqwest::Error) -> Self {
        // reqwest's Display may include the URL (with a signed query):
        // keep only the kind of failure.
        let kind = if e.is_timeout() {
            "timeout"
        } else if e.is_connect() {
            "connect"
        } else if e.is_redirect() {
            "redirect"
        } else if e.is_body() || e.is_decode() {
            "body"
        } else {
            "request"
        };
        Self::Net(kind.into())
    }
}

/// `scheme://host/path` of a URL, without query or fragment (GitHub asset
/// redirects carry signed query strings that must not reach the log).
pub fn redact_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => "<invalid url>".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_are_logged_without_queries() {
        assert_eq!(
            redact_url("https://objects.githubusercontent.com/a/b?X-Amz-Signature=abc#f"),
            "https://objects.githubusercontent.com/a/b"
        );
        assert_eq!(redact_url("not a url"), "<invalid url>");
    }

    #[test]
    fn release_api_override_is_restricted() {
        assert_eq!(Endpoints::resolve(None), Endpoints::default());
        let e = Endpoints::resolve(Some("http://127.0.0.1:8080/latest.json"));
        assert!(e.allow_loopback);
        assert_eq!(e.api_url, "http://127.0.0.1:8080/latest.json");
        let e = Endpoints::resolve(Some("https://example.org/latest"));
        assert!(!e.allow_loopback);
        // file:// works for install.sh but never for the app.
        assert_eq!(
            Endpoints::resolve(Some("file:///tmp/latest.json")),
            Endpoints::default()
        );
        assert_eq!(
            Endpoints::resolve(Some("http://example.org/latest")),
            Endpoints::default()
        );
    }

    #[test]
    fn identity_is_only_name_and_version() {
        assert_eq!(
            user_agent(),
            format!("ferrotune/{}", env!("CARGO_PKG_VERSION"))
        );
        assert_eq!(
            cargo_command(),
            "cargo install --git https://github.com/FerroApps/Ferrotune --locked --force"
        );
    }
}
