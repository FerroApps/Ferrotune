//! SpClient — Spotify's internal HTTP API gateway.
//!
//! All Spotify internal endpoints (metadata, storage-resolve, canvases, lyrics,
//! colour extraction, etc.) are proxied through the SpClient HTTPS endpoint,
//! whose address is obtained from AP resolution (typically
//! `spclient.wg.spotify.com`).
//!
//! Every request carries:
//! - `Authorization: Bearer <access_token>`
//! - `client-token: <client_token>` (when available)
//! - User-Agent, app-platform and version headers matching the official client.

use reqwest::{Client, Method, Response};
use tracing::debug;

use crate::error::{ProtocolError, Result};
use crate::version;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The `app-platform` header value sent by the official desktop client.
const APP_PLATFORM: &str = "Linux";

/// The `spotify-app-version` header value.
const APP_VERSION: &str = version::SEMANTIC_VERSION;

// ---------------------------------------------------------------------------
// SpClient
// ---------------------------------------------------------------------------

/// HTTP client for Spotify's internal SpClient API.
///
/// The base URL is typically `https://spclient.wg.spotify.com` (obtained from
/// AP resolution).
pub struct SpClient {
    http: Client,
    base_url: String,
}

impl SpClient {
    /// Create a new `SpClient` targeting `base_url`.
    ///
    /// `base_url` should include the scheme (e.g. `https://spclient.wg.spotify.com`).
    /// A trailing slash is stripped for consistency.
    pub fn new(base_url: String) -> Self {
        let base_url = base_url.trim_end_matches('/').to_owned();
        Self {
            http: Client::new(),
            base_url,
        }
    }

    /// Create a `SpClient` from a resolved `(host, port)` pair.
    ///
    /// Uses HTTPS by default; port 443 is omitted from the URL.
    pub fn from_address(host: &str, port: u16) -> Self {
        let base_url = if port == 443 {
            format!("https://{host}")
        } else {
            format!("https://{host}:{port}")
        };
        Self::new(base_url)
    }

    /// The base URL this client targets.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Make an authenticated GET request to `path` (e.g. `/metadata/4/track/<hex>`).
    ///
    /// The path should start with `/`.
    pub async fn get(
        &self,
        path: &str,
        access_token: &str,
        client_token: Option<&str>,
    ) -> Result<Response> {
        self.request(Method::GET, path, access_token, client_token, None, None)
            .await
    }

    /// Make an authenticated request with arbitrary method, body and content
    /// type.
    pub async fn request(
        &self,
        method: Method,
        path: &str,
        access_token: &str,
        client_token: Option<&str>,
        body: Option<Vec<u8>>,
        content_type: Option<&str>,
    ) -> Result<Response> {
        let url = format!("{}{}", self.base_url, path);
        debug!(%method, %url, "spclient request");

        let mut builder = self
            .http
            .request(method, &url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("User-Agent", version::USER_AGENT)
            .header("app-platform", APP_PLATFORM)
            .header("spotify-app-version", APP_VERSION);

        if let Some(ct) = client_token {
            builder = builder.header("client-token", ct);
        }

        let accept = match content_type {
            Some(ct) if ct.contains("protobuf") => "application/x-protobuf",
            _ => "application/json",
        };
        builder = builder.header("Accept", accept);

        if let Some(ct) = content_type {
            builder = builder.header("Content-Type", ct);
        }

        if let Some(data) = body {
            builder = builder.body(data);
        }

        builder
            .send()
            .await
            .map_err(|e| ProtocolError::Connection(format!("spclient request failed: {e}")))
    }
}

impl std::fmt::Debug for SpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SpClient")
            .field("base_url", &self.base_url)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_strips_trailing_slash() {
        let client = SpClient::new("https://spclient.wg.spotify.com/".to_string());
        assert_eq!(client.base_url(), "https://spclient.wg.spotify.com");
    }

    #[test]
    fn new_preserves_clean_url() {
        let client = SpClient::new("https://spclient.wg.spotify.com".to_string());
        assert_eq!(client.base_url(), "https://spclient.wg.spotify.com");
    }

    #[test]
    fn from_address_default_port() {
        let client = SpClient::from_address("spclient.wg.spotify.com", 443);
        assert_eq!(client.base_url(), "https://spclient.wg.spotify.com");
    }

    #[test]
    fn from_address_custom_port() {
        let client = SpClient::from_address("spclient.wg.spotify.com", 4070);
        assert_eq!(client.base_url(), "https://spclient.wg.spotify.com:4070");
    }

    /// Verify that the version constants used in headers match expectations.
    #[test]
    fn header_constants() {
        assert_eq!(APP_PLATFORM, "Linux");
        assert_eq!(APP_VERSION, "1.3.8.620");
        assert_eq!(version::USER_AGENT, "Spotify/103008620 Linux/0 (desktop)");
    }
}
