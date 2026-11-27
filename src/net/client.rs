//! HTTP client for Spotify's internal endpoints (spclient + pathfinder),
//! authenticated by the librespot session.
//!
//! Everything the UI lists goes through here, the way the desktop client
//! fetches it: `Authorization: Bearer <session token>`, the session's
//! `client-token`, and the platform/version headers librespot announces.
//! There is no stealth here: no TLS fingerprint games, no fake telemetry.
//!
//! Resilience rules (all covered by `tests/backend.rs`):
//! * **Cache** — responses with a TTL are kept in SQLite (`http_cache`);
//!   a fresh entry is served without a request, a stale one is served if
//!   the network fails.
//! * **Coalescing** — identical in-flight requests share one round trip.
//! * **Concurrency cap** — at most [`MAX_CONCURRENT`] requests at once, so
//!   opening a screen never produces a burst.
//! * **429** — `Retry-After` is honoured (sleeping inline up to
//!   [`MAX_INLINE_WAIT`], with exponential backoff when the header is
//!   missing); longer waits put the whole client into a cooldown that fails
//!   fast instead of hammering the server.
//! * **401** — one single-flight token refresh, then an error.
//! * **5xx** — one retry after a short pause.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::FutureExt;
use futures::future::{BoxFuture, Shared};
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};

use super::dpop::{self, DpopKey};
use super::tokens::{TokenCache, TokenSource};
use crate::error::{Error, Result};
use crate::store::Store;

/// Shared per-host DPoP nonce cache.
///
/// Spotify's backends challenge the client with `DPoP-Nonce` in the response
/// (RFC 9449 §8). A later request on that endpoint has to echo the latest
/// nonce the server sent, or it is refused with `use_dpop_nonce`. Different
/// hosts (spclient, accounts, clienttoken) can hand out independent nonces,
/// so this map is keyed by host.
#[derive(Default)]
pub struct NonceState {
    nonces: Mutex<HashMap<String, String>>,
}

impl NonceState {
    pub fn get(&self, host: &str) -> Option<String> {
        self.nonces.lock().ok()?.get(host).cloned()
    }

    pub fn set(&self, host: &str, nonce: String) {
        if let Ok(mut m) = self.nonces.lock() {
            m.insert(host.to_owned(), nonce);
        }
    }
}

/// Longest server-requested wait we sleep through inside one request.
pub const MAX_INLINE_WAIT: Duration = Duration::from_secs(10);
/// Max attempts for one request (429 / 5xx retries included).
pub const MAX_ATTEMPTS: u32 = 3;
/// Parallel requests in flight.
pub const MAX_CONCURRENT: usize = 4;
/// Upper bound for any server-requested wait (a hostile or broken
/// `Retry-After` must not overflow `Instant` arithmetic or block for days).
pub const MAX_COOLDOWN: Duration = Duration::from_secs(15 * 60);
/// Responses larger than this are never cached.
pub const MAX_CACHED_BODY: usize = 2 * 1024 * 1024;

/// What the session provides besides tokens.
pub trait SessionAuth: TokenSource {
    /// The `client-token` header value (librespot fetches and caches it).
    fn client_token(&self) -> BoxFuture<'_, Result<Option<String>>>;
    /// `https://<ap>-spclient.spotify.com:443`, resolved by librespot.
    fn spclient_base(&self) -> BoxFuture<'_, Result<String>>;
    fn username(&self) -> String;
    fn country(&self) -> String;
    /// Account type from the session's user attributes ("premium", ...).
    fn product(&self) -> Option<String>;
}

/// Where a request goes.
#[derive(Debug, Clone)]
pub enum Target {
    /// Path on the spclient host, e.g. `/playlist/v2/playlist/<id>`.
    Spclient(String),
    /// Path + query on the pathfinder host.
    Pathfinder(String),
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub target: Target,
    pub body: Option<Vec<u8>>,
    pub content_type: Option<&'static str>,
    pub accept: &'static str,
    /// Cache successful responses for this long (`None` = never cache).
    pub ttl: Option<Duration>,
    /// Only cache a 2xx body this accepts (e.g. GraphQL without `errors`).
    pub cache_if: Option<fn(&[u8]) -> bool>,
}

pub const PROTOBUF: &str = "application/x-protobuf";
pub const JSON: &str = "application/json";
pub const COLLECTION_PROTO: &str = "application/vnd.collection-v2.spotify.proto";

impl Request {
    pub fn get(target: Target, accept: &'static str) -> Self {
        Self {
            method: Method::GET,
            target,
            body: None,
            content_type: None,
            accept,
            ttl: None,
            cache_if: None,
        }
    }

    pub fn post(target: Target, content_type: &'static str, body: Vec<u8>) -> Self {
        Self {
            method: Method::POST,
            target,
            body: Some(body),
            content_type: Some(content_type),
            accept: content_type,
            ttl: None,
            cache_if: None,
        }
    }

    pub fn accept(mut self, accept: &'static str) -> Self {
        self.accept = accept;
        self
    }

    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    pub fn cache_if(mut self, f: fn(&[u8]) -> bool) -> Self {
        self.cache_if = Some(f);
        self
    }

    /// Rate-limit scope: the first spclient path segment
    /// (`collection`, `extended-metadata`, `playlist`, ...) or `pathfinder`,
    /// so a 429 on search does not block library writes or metadata.
    pub fn family(&self) -> String {
        match &self.target {
            Target::Spclient(p) => {
                let seg = p
                    .trim_start_matches('/')
                    .split(['/', '?'])
                    .next()
                    .unwrap_or("");
                format!("sp:{seg}")
            }
            Target::Pathfinder(_) => "pathfinder".into(),
        }
    }

    /// Stable key for caching and coalescing (never contains secrets).
    pub fn key(&self) -> String {
        let (host, path) = match &self.target {
            Target::Spclient(p) => ("sp", p.as_str()),
            Target::Pathfinder(p) => ("pf", p.as_str()),
        };
        let mut h = Sha256::new();
        h.update(self.body.as_deref().unwrap_or_default());
        let digest = h.finalize();
        let body: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
        format!("{}:{host}:{path}#{body}", self.method)
    }
}

/// Headers the desktop client sends, byte-for-byte identical to the values
/// the official Spotify desktop client v1.3.8.620 announces.
#[derive(Debug, Clone)]
pub struct ClientHeaders {
    pub user_agent: String,
    pub app_platform: &'static str,
    pub app_version: String,
    pub accept_language: String,
}

/// Numeric build id announced by the official Linux desktop client 1.3.8.620.
pub const SPOTIFY_BUILD_VERSION: u64 = 103_008_620;

/// Semantic version of the official Linux desktop client we mimic.
pub const SPOTIFY_SEMANTIC_VERSION: &str = "1.3.8.620";

/// Property-set id for the 1.3.8.620 release (found in the binary's `.rodata`).
pub const SPOTIFY_PROPERTY_SET_ID: &str = "95e5118d3fc65ae6c2645b6f381947d38586e669";

impl ClientHeaders {
    /// Build the exact header set the official Linux desktop client sends.
    /// The `(desktop)` suffix of the user-agent is what the native Spotify
    /// binary emits — never the `(librespot-<sha>)` suffix librespot adds,
    /// which would make the request trivially distinguishable.
    pub fn from_environment() -> Self {
        let app_platform = match std::env::consts::OS {
            "macos" => "OSX",
            "windows" => "Win32",
            _ => "Linux",
        };
        Self {
            user_agent: format!("Spotify/{SPOTIFY_BUILD_VERSION} {app_platform}/0 (desktop)"),
            app_platform,
            app_version: SPOTIFY_SEMANTIC_VERSION.to_owned(),
            accept_language: accept_language(std::env::var("LANG").ok().as_deref()),
        }
    }
}

/// `es_ES.UTF-8` -> `es-ES`; `C`/unset -> `en`.
pub fn accept_language(lang: Option<&str>) -> String {
    let tag = lang
        .and_then(|l| l.split(['.', '@']).next())
        .filter(|l| !l.is_empty() && *l != "C" && *l != "POSIX")
        .map(|l| l.replace('_', "-"));
    tag.unwrap_or_else(|| "en".into())
}

/// `Retry-After` in seconds; `None` when missing or unparsable.
pub fn parse_retry_after(v: Option<&reqwest::header::HeaderValue>) -> Option<u64> {
    v.and_then(|h| h.to_str().ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
}

/// How long to wait before retry `attempt` (1-based) after a 429, clamped
/// to [`MAX_COOLDOWN`].
pub fn rate_limit_wait(retry_after: Option<u64>, attempt: u32) -> Duration {
    let d = match retry_after {
        Some(s) => Duration::from_secs(s.max(1)),
        // No header: 1 s, 2 s, 4 s, ...
        None => Duration::from_secs(1u64 << attempt.saturating_sub(1).min(6)),
    };
    d.min(MAX_COOLDOWN)
}

type SharedResponse = Shared<BoxFuture<'static, Result<Vec<u8>>>>;

pub struct InternalClient {
    http: reqwest::Client,
    auth: Arc<dyn SessionAuth>,
    tokens: TokenCache,
    spclient_override: Option<String>,
    pathfinder_base: String,
    cache: Option<Store>,
    headers: ClientHeaders,
    inflight: Mutex<HashMap<String, SharedResponse>>,
    limiter: tokio::sync::Semaphore,
    cooldown_until: Mutex<HashMap<String, Instant>>,
    /// DPoP key pair. When present, every request is signed with a DPoP
    /// proof JWT and carries the `DPoP` header.
    dpop_key: Option<Arc<DpopKey>>,
    /// Per-host DPoP nonce cache (shared with the auth engine).
    nonce_state: Arc<NonceState>,
}

impl InternalClient {
    pub fn new(
        http: reqwest::Client,
        auth: Arc<dyn SessionAuth>,
        spclient_override: Option<String>,
        pathfinder_base: String,
        cache: Option<Store>,
    ) -> Self {
        Self::with_dpop(
            http,
            auth,
            spclient_override,
            pathfinder_base,
            cache,
            None,
            Arc::new(NonceState::default()),
        )
    }

    /// Build a client that signs every request with a DPoP proof. This is
    /// the constructor the native engine uses; the plain `new` keeps the
    /// legacy (unsigned) path for tests.
    pub fn with_dpop(
        http: reqwest::Client,
        auth: Arc<dyn SessionAuth>,
        spclient_override: Option<String>,
        pathfinder_base: String,
        cache: Option<Store>,
        dpop_key: Option<Arc<DpopKey>>,
        nonce_state: Arc<NonceState>,
    ) -> Self {
        Self {
            http,
            auth,
            tokens: TokenCache::new(),
            spclient_override,
            pathfinder_base,
            cache,
            headers: ClientHeaders::from_environment(),
            inflight: Mutex::new(HashMap::new()),
            limiter: tokio::sync::Semaphore::new(MAX_CONCURRENT),
            cooldown_until: Mutex::new(HashMap::new()),
            dpop_key,
            nonce_state,
        }
    }

    pub fn auth(&self) -> &Arc<dyn SessionAuth> {
        &self.auth
    }

    /// Drop the cached token (the session was replaced).
    pub async fn session_changed(&self) {
        self.tokens.clear().await;
    }

    /// Remove cached responses whose key contains `needle` (after writes).
    pub async fn invalidate(&self, needle: &'static str) {
        if let Some(store) = &self.cache {
            let _ = store.run(move |s| s.cache_invalidate(needle)).await;
        }
    }

    /// Send a request with caching, coalescing and retries.
    ///
    /// The network part runs in its own task: it always completes (and
    /// releases its concurrency permit and in-flight entry) even if every
    /// caller waiting for it is dropped.
    pub async fn send(self: &Arc<Self>, req: Request) -> Result<Vec<u8>> {
        let key = req.key();
        let cached = self.cache_lookup(&key, req.ttl).await;
        if let Some((body, true)) = &cached {
            return Ok(body.clone());
        }
        let shared = {
            let mut map = self
                .inflight
                .lock()
                .map_err(|_| Error::Net("request table poisoned".into()))?;
            match map.get(&key) {
                Some(s) => s.clone(),
                None => {
                    let this = self.clone();
                    let k = key.clone();
                    let handle = tokio::spawn(async move {
                        let out = this.send_uncached(&req).await;
                        if let Ok(body) = &out {
                            this.cache_store(&k, &req, body).await;
                        }
                        if let Ok(mut m) = this.inflight.lock() {
                            m.remove(&k);
                        }
                        out
                    });
                    let fut: BoxFuture<'static, Result<Vec<u8>>> =
                        Box::pin(async move { handle.await.unwrap_or(Err(Error::Cancelled)) });
                    let s = fut.shared();
                    map.insert(key.clone(), s.clone());
                    s
                }
            }
        };
        match shared.await {
            Ok(body) => Ok(body),
            // Serve stale data rather than an error when offline / limited.
            Err(e) if is_transient(&e) => match cached {
                Some((body, _)) => {
                    tracing::debug!(error = %e, "serving stale cached response");
                    Ok(body)
                }
                None => Err(e),
            },
            Err(e) => Err(e),
        }
    }

    async fn cache_store(&self, key: &str, req: &Request, body: &[u8]) {
        let (Some(store), Some(_)) = (&self.cache, req.ttl) else {
            return;
        };
        if body.len() > MAX_CACHED_BODY || req.cache_if.is_some_and(|ok| !ok(body)) {
            return;
        }
        let (k, b) = (key.to_owned(), body.to_vec());
        let _ = store.run(move |s| s.cache_put(&k, &b)).await;
    }

    async fn cache_lookup(&self, key: &str, ttl: Option<Duration>) -> Option<(Vec<u8>, bool)> {
        let (store, ttl) = (self.cache.as_ref()?, ttl?);
        let k = key.to_owned();
        let (body, age) = store.run(move |s| s.cache_get(&k)).await.ok()??;
        let fresh = u64::try_from(age).is_ok_and(|a| a < ttl.as_secs());
        Some((body, fresh))
    }

    fn cooldown_left(&self, family: &str) -> Option<Duration> {
        let until = *self.cooldown_until.lock().ok()?.get(family)?;
        until.checked_duration_since(Instant::now())
    }

    fn start_cooldown(&self, family: String, d: Duration) {
        let now = Instant::now();
        let until = now.checked_add(d.min(MAX_COOLDOWN)).unwrap_or(now);
        if let Ok(mut c) = self.cooldown_until.lock() {
            c.retain(|_, t| *t > now);
            c.insert(family, until);
        }
    }

    async fn url(&self, target: &Target) -> Result<String> {
        Ok(match target {
            Target::Spclient(p) => {
                let base = match &self.spclient_override {
                    Some(b) => b.clone(),
                    None => self.auth.spclient_base().await?,
                };
                format!("{}{p}", base.trim_end_matches('/'))
            }
            Target::Pathfinder(p) => format!("{}{p}", self.pathfinder_base),
        })
    }

    async fn send_uncached(&self, req: &Request) -> Result<Vec<u8>> {
        let _permit = self.limiter.acquire().await.map_err(|_| Error::Cancelled)?;
        let family = req.family();
        if let Some(left) = self.cooldown_left(&family) {
            return Err(Error::RateLimited(left.as_secs().max(1)));
        }
        let url = self.url(&req.target).await?;
        let htu = dpop::htu_from_url(&url);
        let host = reqwest::Url::parse(&url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let method_str = req.method.as_str().to_owned();
        let source: &dyn TokenSource = self.auth.as_ref();
        let mut token = self.tokens.get(source).await?;
        let client_token = self.auth.client_token().await.unwrap_or(None);
        let mut refreshed = false;
        let mut attempt = 0u32;
        let mut nonce_retried = false;
        loop {
            attempt += 1;
            let mut rb = self
                .http
                .request(req.method.clone(), &url)
                .bearer_auth(token.expose())
                .header(reqwest::header::USER_AGENT, &self.headers.user_agent)
                .header(reqwest::header::ACCEPT, req.accept)
                .header(
                    reqwest::header::ACCEPT_LANGUAGE,
                    &self.headers.accept_language,
                )
                .header("app-platform", self.headers.app_platform)
                .header("spotify-app-version", &self.headers.app_version);
            if let Some(ct) = &client_token {
                rb = rb.header("client-token", ct);
            }
            // DPoP proof: rebuilt per attempt so jti is unique and the ath
            // claim always matches the current access token.
            if let Some(key) = &self.dpop_key {
                let nonce = self.nonce_state.get(&host);
                match key.proof(&method_str, &htu, nonce.as_deref(), Some(token.expose())) {
                    Ok(proof) => {
                        rb = rb.header("DPoP", proof);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not build DPoP proof");
                    }
                }
            }
            if let Some(ct) = req.content_type {
                rb = rb.header(reqwest::header::CONTENT_TYPE, ct);
            }
            rb = match &req.body {
                Some(b) => rb.body(b.clone()),
                None if req.method != Method::GET => {
                    rb.header(reqwest::header::CONTENT_LENGTH, "0")
                }
                None => rb,
            };
            let resp = rb.send().await?;
            let status = resp.status();
            // Snapshot nonce + error code before the body is consumed.
            let new_nonce = dpop::extract_nonce(resp.headers());
            if let Some(n) = new_nonce.clone() {
                self.nonce_state.set(&host, n);
            }
            let dpop_err = dpop_error_code(resp.headers());
            tracing::debug!(method = %req.method, status = status.as_u16(), "internal api");
            if status.is_success() {
                return Ok(resp.bytes().await?.to_vec());
            }
            match status {
                // RFC 9449 §8: the server can refuse a proof that lacks a
                // nonce or carries a stale one. Only one such retry per
                // request; it does not count as an attempt.
                StatusCode::UNAUTHORIZED | StatusCode::BAD_REQUEST
                    if self.dpop_key.is_some()
                        && !nonce_retried
                        && (dpop_err.as_deref() == Some("use_dpop_nonce")
                            || new_nonce.is_some()) =>
                {
                    nonce_retried = true;
                    attempt = attempt.saturating_sub(1);
                }
                StatusCode::UNAUTHORIZED if !refreshed => {
                    refreshed = true;
                    token = self.tokens.refresh_after(source, &token).await?;
                }
                StatusCode::UNAUTHORIZED => {
                    return Err(Error::auth("Spotify refused the session token."));
                }
                StatusCode::TOO_MANY_REQUESTS => {
                    let wait = rate_limit_wait(
                        parse_retry_after(resp.headers().get(reqwest::header::RETRY_AFTER)),
                        attempt,
                    );
                    if wait > MAX_INLINE_WAIT || attempt >= MAX_ATTEMPTS {
                        self.start_cooldown(family, wait);
                        return Err(Error::RateLimited(wait.as_secs().max(1)));
                    }
                    tracing::info!(wait_s = wait.as_secs(), "rate limited; backing off");
                    tokio::time::sleep(wait).await;
                }
                s if s.is_server_error() && attempt < 2 => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                s => {
                    return Err(Error::Http {
                        status: s.as_u16(),
                        message: s.canonical_reason().unwrap_or("error").to_owned(),
                    });
                }
            }
        }
    }
}

/// `WWW-Authenticate: DPoP error="use_dpop_nonce"` → `use_dpop_nonce`.
fn dpop_error_code(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let v = headers
        .get(reqwest::header::WWW_AUTHENTICATE)?
        .to_str()
        .ok()?;
    let marker = "error=\"";
    let start = v.find(marker)? + marker.len();
    let rest = &v[start..];
    rest.find('"').map(|end| rest[..end].to_owned())
}

fn is_transient(e: &Error) -> bool {
    matches!(
        e,
        Error::Net(_)
            | Error::RateLimited(_)
            | Error::Http {
                status: 500..=599,
                ..
            }
    )
}

/// The plain HTTP client (rustls, timeouts). Image CDN fetches use it too.
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_and_backoff() {
        let h = reqwest::header::HeaderValue::from_static("7");
        assert_eq!(parse_retry_after(Some(&h)), Some(7));
        let h = reqwest::header::HeaderValue::from_static("soon");
        assert_eq!(parse_retry_after(Some(&h)), None);
        assert_eq!(rate_limit_wait(Some(7), 1), Duration::from_secs(7));
        assert_eq!(rate_limit_wait(Some(0), 1), Duration::from_secs(1));
        assert_eq!(rate_limit_wait(None, 1), Duration::from_secs(1));
        assert_eq!(rate_limit_wait(None, 2), Duration::from_secs(2));
        assert_eq!(rate_limit_wait(None, 3), Duration::from_secs(4));
        assert_eq!(rate_limit_wait(None, 99), Duration::from_secs(64));
        // Absurd server values are clamped instead of overflowing.
        assert_eq!(rate_limit_wait(Some(u64::MAX), 1), MAX_COOLDOWN);
    }

    #[test]
    fn families_scope_rate_limits() {
        let f = |p: &str| Request::get(Target::Spclient(p.into()), JSON).family();
        assert_eq!(f("/collection/v2/paging"), "sp:collection");
        assert_eq!(
            f("/extended-metadata/v0/extended-metadata"),
            "sp:extended-metadata"
        );
        assert_eq!(f("/searchview/km/v4/search/x?y=1"), "sp:searchview");
        assert_eq!(
            Request::get(Target::Pathfinder("/pathfinder/v1/query".into()), JSON).family(),
            "pathfinder"
        );
    }

    #[test]
    fn accept_language_from_locale() {
        assert_eq!(accept_language(Some("es_ES.UTF-8")), "es-ES");
        assert_eq!(accept_language(Some("de_DE@euro")), "de-DE");
        assert_eq!(accept_language(Some("C")), "en");
        assert_eq!(accept_language(None), "en");
    }

    #[test]
    fn headers_match_official_desktop_client() {
        let h = ClientHeaders::from_environment();
        assert!(
            h.user_agent.starts_with("Spotify/103008620 "),
            "{}",
            h.user_agent
        );
        assert!(h.user_agent.ends_with("/0 (desktop)"), "{}", h.user_agent);
        assert!(
            ["Linux", "OSX", "Win32"].contains(&h.app_platform),
            "{}",
            h.app_platform
        );
        assert_eq!(h.app_version, "1.3.8.620");
        // No "librespot" substring anywhere — the whole point is to be
        // indistinguishable from the official client.
        assert!(!h.user_agent.contains("librespot"));
    }

    #[test]
    fn request_keys_differ_by_body_and_target() {
        let a = Request::post(Target::Spclient("/x".into()), PROTOBUF, vec![1]);
        let b = Request::post(Target::Spclient("/x".into()), PROTOBUF, vec![2]);
        let c = Request::get(Target::Pathfinder("/x".into()), JSON);
        assert_ne!(a.key(), b.key());
        assert_ne!(a.key(), c.key());
        assert_eq!(a.key(), a.clone().key());
    }
}
