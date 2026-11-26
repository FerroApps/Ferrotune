//! Crate-wide error type. Every variant carries a message that is already
//! free of secrets so it can be shown to the user or logged.

use std::fmt;

/// Errors grouped by origin (see docs/architecture.md §7).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Transport-level failure (DNS, TLS, connection reset, timeout).
    #[error("network error: {0}")]
    Net(String),
    /// The server answered with something we did not expect.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// HTTP error status from Spotify (status code + short reason).
    #[error("http {status}: {message}")]
    Http { status: u16, message: String },
    /// Rate limited by Spotify; retry after the given number of seconds.
    #[error("rate limited, retry after {0}s")]
    RateLimited(u64),
    /// A GraphQL persisted query (operation name) was rejected: its hash is
    /// stale or the response shape changed.
    #[error("persisted query rejected: {0}")]
    QueryRejected(String),
    /// Login / token problems. `revoked` means the session is gone for good.
    #[error("authentication error: {message}")]
    Auth { message: String, revoked: bool },
    #[error("store error: {0}")]
    Store(String),
    #[error("keyring error: {0}")]
    Keyring(String),
    #[error("i/o error: {0}")]
    Io(String),
    #[error("playback error: {0}")]
    Playback(String),
    #[error("cancelled")]
    Cancelled,
}

impl Error {
    pub fn auth(message: impl Into<String>) -> Self {
        Self::Auth {
            message: message.into(),
            revoked: false,
        }
    }

    pub fn revoked(message: impl Into<String>) -> Self {
        Self::Auth {
            message: message.into(),
            revoked: true,
        }
    }

    /// True when the stored session can no longer be used and the user must
    /// log in again.
    pub fn is_revoked(&self) -> bool {
        matches!(self, Self::Auth { revoked: true, .. })
    }

    /// Short message suitable for a toast or an inline error.
    pub fn user_message(&self) -> String {
        match self {
            Self::Net(_) => "Can't reach Spotify. Check your connection.".into(),
            Self::Protocol(_) => "Spotify sent an unexpected response.".into(),
            Self::Http { status: 403, .. } => {
                "Spotify refused the request (a Premium account is required).".into()
            }
            Self::Http { status: 404, .. } => "That item is not available.".into(),
            Self::Http { status, .. } => format!("Spotify returned an error ({status})."),
            Self::RateLimited(s) => {
                format!("Spotify is limiting requests. Try again in {s}s.")
            }
            Self::QueryRejected(op) => format!(
                "Spotify changed an internal API ({op}). Some results may be missing until Ferrotune is updated."
            ),
            Self::Auth { message, .. } => message.clone(),
            Self::Store(_) => "The local cache could not be read.".into(),
            Self::Keyring(m) => format!("System keyring unavailable: {m}"),
            Self::Io(_) => "A local file operation failed.".into(),
            Self::Playback(m) => format!("Playback failed: {m}"),
            Self::Cancelled => "Cancelled.".into(),
        }
    }
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Store(e.to_string())
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Self::Protocol(e.to_string())
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        // reqwest's Display can include the full URL (with query string).
        // Strip it so tokens/codes in query strings never reach a log.
        let e = e.without_url();
        if e.is_decode() {
            Self::Protocol(e.to_string())
        } else {
            Self::Net(e.to_string())
        }
    }
}

impl From<spotify_protocol::ProtocolError> for Error {
    fn from(e: spotify_protocol::ProtocolError) -> Self {
        use spotify_protocol::ProtocolError as PE;
        match e {
            PE::AuthFailed(m) => Self::auth(m),
            PE::Token(m) => Self::auth(m),
            PE::Connection(m) | PE::ApResolve(m) => Self::Net(m),
            PE::NotConnected => Self::Net("session not connected".into()),
            PE::Http(m) => Self::Net(m),
            other => Self::Protocol(other.to_string()),
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Wrapper used in `tracing` fields for values that must never be logged
/// verbatim. Displays only the length.
pub struct Redacted<'a, T: ?Sized>(pub &'a T);

impl<T: AsRef<str> + ?Sized> fmt::Display for Redacted<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED len={}]", self.0.as_ref().len())
    }
}

impl<T: AsRef<str> + ?Sized> fmt::Debug for Redacted<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacted_hides_value() {
        let s = format!("{}", Redacted("BQDsecret-token"));
        assert!(!s.contains("secret"));
        assert!(s.contains("len=15"));
    }

    #[test]
    fn user_messages_are_secret_free() {
        let e = Error::Net("https://api.spotify.com/v1?access_token=abc".into());
        assert!(!e.user_message().contains("abc"));
        assert!(Error::revoked("x").is_revoked());
        assert!(!Error::auth("x").is_revoked());
    }
}
