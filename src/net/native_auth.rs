//! Native authentication engine: login5 tokens with DPoP proof-of-possession.
//!
//! Replaces the librespot-delegated `LiveAuth` with a self-contained
//! implementation that makes FerroTune indistinguishable from the official
//! Spotify desktop client at the protocol level:
//!
//! 1. Generates a P-256 DPoP key pair (persisted in the OS keyring).
//! 2. Refreshes access tokens via `spotify_protocol::Login5` — an
//!    independent HTTP-based token client that talks directly to
//!    `login5.spotify.com`, decoupled from librespot's AP session.
//! 3. Acquires client tokens independently (not through librespot's
//!    spclient, so we know the challenge logic byte-for-byte).
//! 4. Exposes the DPoP key + shared nonce state the `InternalClient` uses
//!    to sign every HTTP request with a DPoP proof JWT.
//!
//! The server sees: the official desktop user-agent (`Spotify/103008620
//! Linux/0 (desktop)`), the official client id, bearer tokens, a client
//! token, AND a DPoP proof — exactly like a real desktop client.
//!
//! The librespot session is still held for spclient endpoint resolution,
//! client-token fallback, and identity queries (username, country, product)
//! until those are also migrated to `spotify_protocol`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::future::BoxFuture;

use super::client::{NonceState, SessionAuth};
use super::client_token::{self, ClientToken};
use super::dpop::{DPOP_KEYRING_KEY, DpopKey};
use super::session::StoredCredentials;
use super::tokens::{AccessToken, TokenSource};
use crate::auth::secrets::{KEY_LIBRESPOT, Secret, SecretStore};
use crate::error::{Error, Result};

/// The client version string we announce — same as the official desktop
/// binary (`1.3.8.620`) so the client-token server has no reason to
/// challenge differently.
pub const SPOTIFY_CLIENT_VERSION: &str = super::client::SPOTIFY_SEMANTIC_VERSION;

/// The native auth engine.
pub struct NativeAuth {
    /// The librespot session (kept for AP connection, Connect, spclient
    /// endpoint resolution, and client-token fallback — until those
    /// subsystems are also migrated to `spotify_protocol`).
    session: RwLock<librespot_core::Session>,
    /// `spotify_protocol::Login5` — independent HTTP token client that
    /// talks to `login5.spotify.com` without going through librespot.
    login5: spotify_protocol::Login5,
    /// DPoP key pair. Shared with the `InternalClient` for per-request
    /// proofs.
    dpop_key: Arc<DpopKey>,
    /// Shared DPoP nonce state (one per host).
    nonce_state: Arc<NonceState>,
    /// Cached client token.
    client_token: tokio::sync::Mutex<Option<ClientToken>>,
    /// Client id (same as librespot's default — the official desktop one).
    client_id: String,
    /// Device id (from the librespot session).
    device_id: String,
    /// HTTP client for client-token requests.
    http: reqwest::Client,
    /// Secret store for persisting the DPoP-bound credential.
    secrets: Arc<dyn SecretStore>,
    /// Set once after the first DPoP-bound credential is persisted, so
    /// the app knows a reconnect with the upgraded credential is worthwhile.
    credential_upgraded: AtomicBool,
}

impl NativeAuth {
    /// Create from an existing librespot session and secret store.
    ///
    /// Loads or generates the DPoP key pair from the keyring. A freshly
    /// generated key is persisted so a later run uses the same `dpop_jkt`
    /// and the tokens it mints stay bound to one identity.
    ///
    /// Also creates a `spotify_protocol::Login5` instance that shares the
    /// same DPoP key — this is the token source for access tokens, decoupled
    /// from librespot's AP channel.
    pub fn new(
        session: librespot_core::Session,
        secrets: Arc<dyn SecretStore>,
        client_id: String,
        http: reqwest::Client,
    ) -> Result<Self> {
        let dpop_key = match secrets.get(DPOP_KEYRING_KEY)? {
            Some(s) => {
                tracing::debug!("loaded existing DPoP key from keyring");
                DpopKey::from_secret(&s)?
            }
            None => {
                tracing::info!("generating new DPoP key pair");
                let key = DpopKey::generate()?;
                if let Err(e) = secrets.set(DPOP_KEYRING_KEY, &key.to_secret()?) {
                    tracing::warn!(error = %e, "could not persist DPoP key to keyring");
                }
                key
            }
        };
        let device_id = session.device_id().to_owned();

        // Build the spotify-protocol DPoP key from the same keyring secret.
        // Both serialisation formats are compatible (JSON with base64url P-256
        // scalar), so the same keyring entry works for both.
        let sp_dpop_key = match secrets.get(DPOP_KEYRING_KEY)? {
            Some(s) => match spotify_protocol::DpopKey::from_secret(s.expose().as_bytes()) {
                Ok(k) => Some(Arc::new(k)),
                Err(e) => {
                    tracing::warn!(error = %e, "could not load DPoP key for spotify-protocol Login5");
                    None
                }
            },
            None => None,
        };
        let login5 = spotify_protocol::Login5::new(
            http.clone(),
            client_id.clone(),
            device_id.clone(),
            sp_dpop_key,
        );

        Ok(Self {
            session: RwLock::new(session),
            login5,
            dpop_key: Arc::new(dpop_key),
            nonce_state: Arc::new(NonceState::default()),
            client_token: tokio::sync::Mutex::new(None),
            client_id,
            device_id,
            http,
            secrets,
            credential_upgraded: AtomicBool::new(false),
        })
    }

    /// The DPoP key (shared with the `InternalClient` for per-request proofs).
    pub fn dpop_key(&self) -> Arc<DpopKey> {
        self.dpop_key.clone()
    }

    /// The DPoP nonce state (shared with the `InternalClient`).
    pub fn nonce_state(&self) -> Arc<NonceState> {
        self.nonce_state.clone()
    }

    /// The DPoP JKT for OAuth binding (`dpop_jkt` query parameter).
    pub fn dpop_jkt(&self) -> String {
        self.dpop_key.jkt().to_owned()
    }

    pub fn replace(&self, session: librespot_core::Session) {
        if let Ok(mut s) = self.session.write() {
            *s = session;
        }
        self.credential_upgraded.store(false, Ordering::Relaxed);
    }

    /// Whether the first token fetch returned a DPoP-bound stored
    /// credential that was persisted. The app should reconnect the session
    /// with the upgraded credential so the AP grants audio keys.
    pub fn credential_upgraded(&self) -> bool {
        self.credential_upgraded.load(Ordering::Relaxed)
    }

    /// Save a DPoP-bound stored credential from the login5 response.
    fn persist_upgraded_credential(&self, username: &str, auth_data: &[u8]) {
        let creds = StoredCredentials {
            username: username.to_owned(),
            auth_data: B64.encode(auth_data),
        };
        match serde_json::to_string(&creds) {
            Ok(json) => {
                if let Err(e) = self.secrets.set(KEY_LIBRESPOT, &Secret::new(json)) {
                    tracing::warn!(error = %e, "could not persist DPoP-bound credential");
                } else {
                    tracing::info!("persisted DPoP-bound credential for next session");
                    self.credential_upgraded.store(true, Ordering::Relaxed);
                }
            }
            Err(e) => tracing::warn!(error = %e, "could not serialise credential"),
        }
    }

    fn current(&self) -> Result<librespot_core::Session> {
        self.session
            .read()
            .map(|s| s.clone())
            .map_err(|_| Error::auth("session lock poisoned"))
    }

    async fn fetch_client_token_inner(&self) -> Result<String> {
        let mut cached = self.client_token.lock().await;
        if let Some(ct) = cached.as_ref().filter(|ct| ct.is_fresh()) {
            return Ok(ct.token.clone());
        }
        let ct = client_token::fetch_client_token(
            &self.http,
            &self.client_id,
            &self.device_id,
            SPOTIFY_CLIENT_VERSION,
            // The CLIENT_SECRET_HMAC shared secret could not be extracted
            // from the stripped binary. The desktop client almost always
            // gets a HASH_CASH challenge, which we solve without it.
            None,
        )
        .await?;
        let token = ct.token.clone();
        *cached = Some(ct);
        Ok(token)
    }
}

impl TokenSource for NativeAuth {
    fn fetch(&self) -> BoxFuture<'_, Result<AccessToken>> {
        Box::pin(async move {
            let session = self.current()?;
            if session.is_invalid() {
                return Err(Error::Net("the Spotify session is reconnecting".into()));
            }

            // Get the stored credential and username from the librespot
            // session — these are the AP authentication blob that login5
            // exchanges for access tokens.
            let auth_data = session.auth_data();
            let username = session.username();

            // Use spotify_protocol::Login5 for the token exchange instead
            // of librespot's session.login5(). This decouples token refresh
            // from the librespot AP channel — the first step toward removing
            // the librespot dependency entirely.
            let t = self
                .login5
                .auth_token_and_credential(&auth_data, &username)
                .await
                .map_err(|e| Error::auth(format!("could not get a session token: {e}")))?;

            // The login5 v4 response with DPoP returns a stored credential
            // that is bound to the DPoP key. Persisting it means the next
            // AP session authenticates as an official desktop client, which
            // unlocks audio-key access on accounts that deny librespot.
            if !t.stored_credential.is_empty() && !self.credential_upgraded.load(Ordering::Relaxed)
            {
                self.persist_upgraded_credential(&username, &t.stored_credential);
            }

            let left = if t.is_expired() {
                Duration::from_secs(1)
            } else {
                t.expires_in
                    .saturating_sub(t.obtained_at.elapsed())
                    .max(Duration::from_secs(1))
            };
            Ok(AccessToken {
                secret: Secret::new(t.access_token().to_owned()),
                expires_at: Instant::now() + left,
            })
        })
    }
}

impl SessionAuth for NativeAuth {
    fn client_token(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move {
            match self.fetch_client_token_inner().await {
                Ok(t) => Ok(Some(t)),
                Err(e) => {
                    tracing::warn!(error = %e, "native client token failed; falling back to librespot");
                    // Fallback: librespot's spclient fetches its own client
                    // token through the AP, so we always have one when the
                    // session is up even if our own HTTP solver fails.
                    let session = self.current()?;
                    Ok(session.spclient().client_token().await.ok())
                }
            }
        })
    }

    fn spclient_base(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let session = self.current()?;
            session
                .spclient()
                .base_url()
                .await
                .map_err(|e| Error::Net(format!("no spclient endpoint: {e}")))
        })
    }

    fn username(&self) -> String {
        self.current().map(|s| s.username()).unwrap_or_default()
    }

    fn country(&self) -> String {
        self.current().map(|s| s.country()).unwrap_or_default()
    }

    fn product(&self) -> Option<String> {
        self.current().ok()?.get_user_attribute("type")
    }
}
