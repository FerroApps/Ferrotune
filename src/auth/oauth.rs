//! OAuth 2.0 Authorization Code flow with PKCE (RFC 7636) against the
//! Spotify accounts service, with a loopback redirect listener.
//!
//! This is the same browser login the librespot binary, ncspot and
//! spotify-player use: the public *desktop* client id (taken from librespot's
//! `SessionConfig`), the `http://127.0.0.1:<port>/login` redirect it accepts,
//! and PKCE instead of a client secret. The resulting access token is used
//! exactly once, to open the librespot session; from then on the session's
//! reusable credentials (keyring) and its login5 tokens are the only
//! credentials Ferrotune holds. No developer-dashboard app is involved.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::Rng;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::secrets::Secret;
use crate::error::{Error, Result};

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Refresh the access token this many seconds before it expires.
pub const EXPIRY_MARGIN_SECS: u64 = 60;

/// PKCE verifier + S256 challenge.
#[derive(Debug, Clone)]
pub struct Pkce {
    pub verifier: Secret,
    pub challenge: String,
}

impl Pkce {
    /// Generate a fresh 64-character verifier (RFC 7636 allows 43..=128).
    pub fn generate() -> Self {
        let mut rng = rand::rng();
        let verifier: String = (0..64)
            .map(|_| UNRESERVED[rng.random_range(0..UNRESERVED.len())] as char)
            .collect();
        Self::from_verifier(verifier)
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = challenge_for(&verifier);
        Self {
            verifier: Secret::new(verifier),
            challenge,
        }
    }
}

/// `BASE64URL-ENCODE(SHA256(ASCII(code_verifier)))` without padding.
pub fn challenge_for(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Random opaque `state` value for CSRF protection.
pub fn random_state() -> String {
    let mut rng = rand::rng();
    let bytes: [u8; 16] = rng.random();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Build the `/authorize` URL the user opens in a browser.
///
/// When `dpop_jkt` is `Some`, the resulting authorization code is bound to
/// the DPoP key whose thumbprint this is (RFC 9449 §10). The token exchange
/// for that code then has to present a DPoP proof made with the matching
/// key — exactly what the official desktop client does.
pub fn authorize_url(
    accounts_base: &str,
    client_id: &str,
    redirect_uri: &str,
    challenge: &str,
    state: &str,
    scopes: &[&str],
    dpop_jkt: Option<&str>,
) -> Result<String> {
    let mut url = url::Url::parse(&format!("{accounts_base}/authorize"))
        .map_err(|e| Error::auth(format!("bad accounts url: {e}")))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", client_id)
            .append_pair("response_type", "code")
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge_method", "S256")
            .append_pair("code_challenge", challenge)
            .append_pair("state", state)
            .append_pair("scope", &scopes.join(" "));
        if let Some(jkt) = dpop_jkt {
            q.append_pair("dpop_jkt", jkt);
        }
    }
    Ok(url.to_string())
}

/// What one request on the loopback listener means for the login.
#[derive(Debug)]
pub enum Callback {
    /// The authorization code: login can continue.
    Code(Secret),
    /// Not (validly) ours: answer with this status and keep listening.
    Ignore(&'static str),
    /// Spotify reported an error for *our* state (e.g. the user denied).
    Failed(Error),
}

/// Parse the HTTP request line the browser sends to the loopback listener
/// (`GET /login?code=..&state=.. HTTP/1.1`).
///
/// Anything that does not carry our `state` is ignored (400) so a stray or
/// hostile local request cannot abort the login.
pub fn parse_callback(request_line: &str, expected_state: &str) -> Callback {
    let mut parts = request_line.split_whitespace();
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Callback::Ignore("400 Bad Request");
    };
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignore("400 Bad Request");
    };
    if url.path() != "/login" {
        // e.g. /favicon.ico
        return Callback::Ignore("404 Not Found");
    }
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for (k, v) in url.query_pairs() {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Callback::Ignore("400 Bad Request");
    }
    if let Some(e) = error {
        return Callback::Failed(Error::auth(if e == "access_denied" {
            "Login was cancelled in the browser.".to_owned()
        } else {
            format!("Spotify rejected the login ({e}).")
        }));
    }
    match code {
        Some(c) if !c.is_empty() => Callback::Code(Secret::new(c)),
        _ => Callback::Ignore("400 Bad Request"),
    }
}

const CALLBACK_HTML: &str = "<!doctype html><html><head><meta charset=utf-8><title>Ferrotune</title>\
<style>body{background:#121212;color:#fff;font-family:sans-serif;display:flex;align-items:center;\
justify-content:center;height:100vh;margin:0}</style></head><body><div><h1>You're logged in</h1>\
<p>You can close this tab and return to Ferrotune.</p></div></body></html>";

/// Per-connection limit for the browser to send its request line.
const READ_TIMEOUT: Duration = Duration::from_secs(2);

/// What to tell the user when the login port cannot be opened.
pub fn port_busy_message(port: u16, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::AddrInUse {
        format!(
            "Port {port} is already in use, so the browser login cannot report back. \
             Close the program using it (or another Ferrotune login) and try again, \
             or start Ferrotune with FERROTUNE_REDIRECT_PORT=<free port>."
        )
    } else {
        format!("Can't listen on 127.0.0.1:{port} for the browser login ({e}).")
    }
}

/// Bind the loopback listener. Call this BEFORE opening the browser so the
/// redirect can never race the listener (and a busy port fails early).
pub fn bind_callback(port: u16) -> Result<std::net::TcpListener> {
    let l = std::net::TcpListener::bind(("127.0.0.1", port))
        .map_err(|e| Error::auth(port_busy_message(port, &e)))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// Wait on an already bound listener until the browser delivers the code.
pub async fn wait_for_callback(
    listener: std::net::TcpListener,
    expected_state: String,
    timeout: Duration,
) -> Result<Secret> {
    let listener = TcpListener::from_std(listener)?;
    tokio::time::timeout(timeout, accept_loop(listener, &expected_state))
        .await
        .map_err(|_| Error::auth("Timed out waiting for the browser login."))?
}

async fn read_request_line(sock: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = vec![0u8; 8192];
    let mut len = 0;
    while len < buf.len() {
        let n = sock.read(&mut buf[len..]).await.ok()?;
        if n == 0 {
            break;
        }
        len += n;
        if buf[..len].windows(2).any(|w| w == b"\r\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf[..len]);
    text.lines().next().map(str::to_owned)
}

async fn accept_loop(listener: TcpListener, expected_state: &str) -> Result<Secret> {
    loop {
        let Ok((mut sock, _)) = listener.accept().await else {
            continue;
        };
        // A client that connects and sends nothing must not block the login.
        let line = tokio::time::timeout(READ_TIMEOUT, read_request_line(&mut sock))
            .await
            .ok()
            .flatten()
            .unwrap_or_default();
        let result = parse_callback(&line, expected_state);
        let (status, body) = match &result {
            Callback::Code(_) => ("200 OK", CALLBACK_HTML.to_owned()),
            Callback::Ignore(status) => (*status, String::new()),
            Callback::Failed(e) => (
                "400 Bad Request",
                format!("Login failed: {}", e.user_message()),
            ),
        };
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = tokio::time::timeout(READ_TIMEOUT, sock.write_all(resp.as_bytes())).await;
        let _ = sock.shutdown().await;
        match result {
            Callback::Code(code) => return Ok(code),
            Callback::Ignore(_) => continue,
            Callback::Failed(e) => return Err(e),
        }
    }
}

/// Result of the code exchange. Never persisted: it only opens the session.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenSet {
    pub access_token: Secret,
    /// Unix seconds.
    pub expires_at: u64,
    pub scope: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    scope: String,
}

#[derive(Deserialize)]
struct TokenError {
    error: String,
    #[serde(default)]
    error_description: Option<String>,
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl TokenSet {
    pub fn is_expired(&self, now: u64) -> bool {
        now.saturating_add(EXPIRY_MARGIN_SECS) >= self.expires_at
    }

    fn from_response(r: TokenResponse, now: u64) -> Self {
        Self {
            access_token: Secret::new(r.access_token),
            expires_at: now.saturating_add(r.expires_in),
            scope: r.scope,
        }
    }
}

/// Builder for a DPoP proof that signs this specific token exchange.
pub trait DpopProver: Send + Sync {
    /// Build a proof JWT for `POST <accounts_base>/api/token`, echoing the
    /// server nonce from a previous `DPoP-Nonce` header if any.
    fn proof(&self, htm: &str, htu: &str, nonce: Option<&str>) -> Result<String>;
}

async fn token_request(
    http: &reqwest::Client,
    accounts_base: &str,
    form: &[(&str, &str)],
    dpop: Option<&dyn DpopProver>,
) -> Result<TokenSet> {
    let token_url = format!("{accounts_base}/api/token");
    let mut nonce: Option<String> = None;
    // The accounts server may demand a nonce; one retry is enough.
    for attempt in 0..2 {
        let mut rb = http.post(&token_url).form(form);
        if let Some(p) = dpop {
            let proof = p.proof("POST", &token_url, nonce.as_deref())?;
            rb = rb.header("DPoP", proof);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        // Grab the server-issued nonce before consuming the body.
        if let Some(n) = resp
            .headers()
            .get("dpop-nonce")
            .and_then(|v| v.to_str().ok())
            .map(String::from)
        {
            nonce = Some(n);
        }
        let body = resp.bytes().await?;
        if status.is_success() {
            let r: TokenResponse = serde_json::from_slice(&body)
                .map_err(|_| Error::Protocol("token response was not valid JSON".into()))?;
            return Ok(TokenSet::from_response(r, now_secs()));
        }
        let err: Option<TokenError> = serde_json::from_slice(&body).ok();
        if attempt == 0
            && dpop.is_some()
            && nonce.is_some()
            && err.as_ref().is_some_and(|e| e.error == "use_dpop_nonce")
        {
            continue;
        }
        return match err {
            Some(e) => Err(Error::auth(format!(
                "Spotify refused the login: {}{}",
                e.error,
                e.error_description
                    .map(|d| format!(" ({d})"))
                    .unwrap_or_default()
            ))),
            None => Err(Error::Http {
                status: status.as_u16(),
                message: "token endpoint error".into(),
            }),
        };
    }
    Err(Error::auth(
        "token endpoint kept requesting a new DPoP nonce",
    ))
}

/// Exchange the authorization code for tokens.
///
/// When `dpop` is `Some`, the exchange is signed with a DPoP proof. The
/// authorization code must have been bound to the same key via the
/// `dpop_jkt` parameter on `/authorize`.
pub async fn exchange_code(
    http: &reqwest::Client,
    accounts_base: &str,
    client_id: &str,
    code: &Secret,
    redirect_uri: &str,
    verifier: &Secret,
    dpop: Option<&dyn DpopProver>,
) -> Result<TokenSet> {
    token_request(
        http,
        accounts_base,
        &[
            ("grant_type", "authorization_code"),
            ("code", code.expose()),
            ("redirect_uri", redirect_uri),
            ("client_id", client_id),
            ("code_verifier", verifier.expose()),
        ],
        dpop,
    )
    .await
}

/// Open a URL in the default browser without pulling in another crate.
pub fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    let mut cmd = std::process::Command::new("xdg-open");
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let mut cmd = std::process::Command::new("xdg-open");
    cmd.arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| Error::Io(format!("could not open a browser: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b_vector() {
        // Test vector from RFC 7636, Appendix B.
        let c = challenge_for("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(c, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn generated_verifier_is_valid() {
        let p = Pkce::generate();
        let v = p.verifier.expose();
        assert_eq!(v.len(), 64);
        assert!(v.bytes().all(|b| UNRESERVED.contains(&b)));
        assert_eq!(p.challenge, challenge_for(v));
        assert_ne!(Pkce::generate().verifier, p.verifier);
        assert_ne!(random_state(), random_state());
    }

    #[test]
    fn authorize_url_has_required_params() {
        let u = authorize_url(
            "https://accounts.spotify.com",
            "cid",
            "http://127.0.0.1:8898/login",
            "chal",
            "st",
            &["streaming", "user-library-read"],
            None,
        )
        .unwrap();
        let parsed = url::Url::parse(&u).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.path(), "/authorize");
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], "chal");
        assert_eq!(q["state"], "st");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:8898/login");
        assert_eq!(q["scope"], "streaming user-library-read");
        assert!(!q.contains_key("dpop_jkt"));
    }

    #[test]
    fn authorize_url_binds_to_dpop_key() {
        let u = authorize_url(
            "https://accounts.spotify.com",
            "cid",
            "http://127.0.0.1:8898/login",
            "chal",
            "st",
            &["streaming"],
            Some("thumbprint-123"),
        )
        .unwrap();
        let q: std::collections::HashMap<_, _> = url::Url::parse(&u)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect();
        assert_eq!(q["dpop_jkt"], "thumbprint-123");
    }

    #[test]
    fn callback_parsing() {
        let code = |c: Callback| match c {
            Callback::Code(s) => Some(s.expose().to_owned()),
            _ => None,
        };
        let ok = parse_callback("GET /login?code=abc&state=s1 HTTP/1.1", "s1");
        assert_eq!(code(ok).as_deref(), Some("abc"));
        assert!(
            matches!(parse_callback("GET /favicon.ico HTTP/1.1", "s1"), Callback::Ignore(s) if s.starts_with("404"))
        );
        // Wrong, missing state or malformed requests are ignored, not fatal.
        for line in [
            "GET /login?code=abc&state=evil HTTP/1.1",
            "GET /login?code=abc HTTP/1.1",
            "GET /login?error=access_denied HTTP/1.1",
            "POST /login HTTP/1.1",
            "garbage",
            "",
            "GET /login?state=s1 HTTP/1.1",
        ] {
            assert!(
                matches!(parse_callback(line, "s1"), Callback::Ignore(_)),
                "{line}"
            );
        }
        match parse_callback("GET /login?error=access_denied&state=s1 HTTP/1.1", "s1") {
            Callback::Failed(e) => assert!(e.user_message().contains("cancelled")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn busy_port_explains_how_to_change_it() {
        let held = bind_callback(0).unwrap();
        let port = held.local_addr().unwrap().port();
        let msg = bind_callback(port).unwrap_err().user_message();
        assert!(msg.contains("FERROTUNE_REDIRECT_PORT"), "{msg}");
        assert!(msg.contains(&port.to_string()));
    }

    #[test]
    fn token_expiry_saturates() {
        let t = TokenSet {
            access_token: Secret::new("a"),
            expires_at: 1000,
            scope: "streaming".into(),
        };
        assert!(!t.is_expired(900));
        assert!(t.is_expired(941));
        assert!(t.is_expired(u64::MAX), "no overflow");
        assert!(format!("{t:?}").contains("[REDACTED]"));
        assert!(!format!("{t:?}").contains("\"a\""));
    }

    #[test]
    fn desktop_login_url_uses_librespot_identity() {
        let cfg = crate::config::Config::for_tests();
        let u = authorize_url(
            &cfg.accounts_base,
            &cfg.client_id,
            &cfg.redirect_uri(),
            "chal",
            "st",
            crate::config::SCOPES,
            None,
        )
        .unwrap();
        let parsed = url::Url::parse(&u).unwrap();
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(parsed.host_str(), Some("accounts.spotify.com"));
        assert_eq!(q["client_id"], "65b708073fc0480ea92a077233ca87bd");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:8898/login");
        let scopes: Vec<&str> = q["scope"].split(' ').collect();
        for needed in [
            "streaming",
            "user-library-read",
            "user-library-modify",
            "playlist-read-private",
        ] {
            assert!(scopes.contains(&needed), "{needed}");
        }
    }

    #[tokio::test]
    async fn loopback_listener_receives_code() {
        let listener = bind_callback(0).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(wait_for_callback(
            listener,
            "xyz".into(),
            Duration::from_secs(20),
        ));
        // A silent client, a stray request and a wrong state must not abort.
        let _silent = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        for bad in [
            "GET /favicon.ico HTTP/1.1\r\n\r\n",
            "GET /login?code=x&state=evil HTTP/1.1\r\n\r\n",
        ] {
            let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .unwrap();
            s.write_all(bad.as_bytes()).await.unwrap();
            let mut resp = String::new();
            s.read_to_string(&mut resp).await.unwrap();
            assert!(resp.starts_with("HTTP/1.1 4"), "{resp}");
        }
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        s.write_all(b"GET /login?code=the-code&state=xyz HTTP/1.1\r\nHost: x\r\n\r\n")
            .await
            .unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200"));
        let code = server.await.unwrap().unwrap();
        assert_eq!(code.expose(), "the-code");
    }
}
