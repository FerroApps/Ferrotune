//! The librespot session: the one identity Ferrotune uses for playback,
//! spclient, pathfinder and Spotify Connect.
//!
//! * First login: `Credentials::with_access_token(<browser login token>)`.
//! * Afterwards: librespot's reusable credentials from the keyring
//!   (`AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS`), no browser.
//! * No librespot `Cache` is created, so librespot never writes credentials
//!   or audio to disk; we copy `session.auth_data()` into the keyring.
//! * When enabled, the session is opened through `librespot-connect`'s
//!   `Spirc`, which registers the device with Spotify's connect-state service
//!   over the session's dealer connection. No zeroconf/mDNS discovery and no
//!   extra credentials are involved: the phone sees the device because it is
//!   logged into the same account.

use std::fmt;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use futures::future::BoxFuture;
use librespot_core::authentication::Credentials;
use serde::{Deserialize, Serialize};

use super::client::SessionAuth;
use super::tokens::{AccessToken, TokenSource};
use crate::auth::secrets::Secret;
use crate::error::{Error, Result};

/// librespot's reusable credentials ("stored credentials" blob), kept only
/// in the keyring.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredCredentials {
    pub username: String,
    /// base64 of the opaque auth blob.
    pub auth_data: String,
}

impl fmt::Debug for StoredCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoredCredentials")
            .field("auth_data", &"[REDACTED]")
            .finish_non_exhaustive()
    }
}

impl StoredCredentials {
    pub fn to_secret(&self) -> Result<Secret> {
        Ok(Secret::new(serde_json::to_string(self)?))
    }

    pub fn from_secret(s: &Secret) -> Result<Self> {
        serde_json::from_str(s.expose())
            .map_err(|_| Error::revoked("stored Spotify credentials are unreadable"))
    }
}

pub enum SessionCredentials {
    /// Access token from the browser login (first login only).
    AccessToken(Secret),
    /// Reusable credentials from a previous login.
    Stored(StoredCredentials),
}

impl SessionCredentials {
    fn into_librespot(self) -> Result<Credentials> {
        Ok(match self {
            Self::AccessToken(t) => Credentials::with_access_token(t.expose()),
            Self::Stored(s) => Credentials {
                username: Some(s.username.clone()),
                auth_type: spotify_proto::authentication::AuthenticationType::AUTHENTICATION_STORED_SPOTIFY_CREDENTIALS,
                auth_data: B64
                    .decode(s.auth_data.as_bytes())
                    .map_err(|_| Error::revoked("stored Spotify credentials are corrupt"))?,
            },
        })
    }

    fn is_stored(&self) -> bool {
        matches!(self, Self::Stored(_))
    }
}

/// Options for the Spotify Connect device.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub name: String,
    pub bitrate: super::playback::BitrateKey,
    pub normalisation: bool,
    /// 0.0..=1.0
    pub initial_volume: f32,
}

#[derive(Debug, Clone)]
pub struct SessionOptions {
    pub client_id: String,
    /// Stable device id (persisted by the app); `None` = random per session.
    pub device_id: Option<String>,
    /// Connect autoplay (continue with similar songs when a context ends).
    pub autoplay: bool,
    pub connect: Option<ConnectOptions>,
    /// Without a Connect device (Spotify Soloist plays instead), still open
    /// the dealer connection so the Connect controller sees the devices.
    pub controller: bool,
}

/// The Spotify Connect device: `Spirc` plus its own player.
pub struct ConnectDevice {
    pub spirc: librespot_connect::Spirc,
    events: Mutex<Option<librespot_playback::player::PlayerEventChannel>>,
}

impl ConnectDevice {
    /// The Connect player's event stream (taken once by the engine).
    pub fn take_events(&self) -> Option<librespot_playback::player::PlayerEventChannel> {
        self.events.lock().ok()?.take()
    }
}

/// librespot session handle (cheap to clone).
#[derive(Clone)]
pub struct SpotifySession {
    pub session: librespot_core::Session,
    pub connect: Option<Arc<ConnectDevice>>,
    /// The dealer runs (through Spirc, or as a controller only).
    pub dealer: bool,
}

impl fmt::Debug for SpotifySession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpotifySession")
            .field("connect", &self.connect.is_some())
            .finish()
    }
}

impl SpotifySession {
    pub fn shutdown(&self) {
        if let Some(c) = &self.connect {
            let _ = c.spirc.shutdown();
        }
        self.session.shutdown();
    }
}

/// Access-point login failure reasons we act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginFailure {
    /// The credentials are no good any more: delete them.
    BadCredentials,
    /// Free account: keep the credentials, explain Premium.
    PremiumRequired,
    /// TryAnotherAP / CouldNotValidateCredentials / ProtocolError /
    /// BadConnectionId: retry later with the same credentials.
    Transient,
    /// Any other refusal (travel restriction, banned app...).
    Other,
}

/// Classify librespot's `AuthenticationError::LoginFailed(code)`.
///
/// librespot maps *every* code to `ErrorKind::PermissionDenied` and its
/// `AuthenticationError` type is private (`mod connection`), so it cannot be
/// downcast from outside the crate; its `Display` text ("Login failed with
/// reason: <reason>", from librespot's `login_error_message`) is stable and
/// is what we match on.
pub fn classify_login_failure(message: &str) -> Option<LoginFailure> {
    let reason = message.split("Login failed with reason: ").nth(1)?;
    Some(if reason.starts_with("Bad credentials") {
        LoginFailure::BadCredentials
    } else if reason.starts_with("Premium account required") {
        LoginFailure::PremiumRequired
    } else if [
        "Try another access point",
        "Could not validate credentials",
        "Protocol error",
        "Bad connection ID",
    ]
    .iter()
    .any(|r| reason.starts_with(r))
    {
        LoginFailure::Transient
    } else {
        LoginFailure::Other
    })
}

pub const PREMIUM_REQUIRED: &str = "Spotify rejected the session (the server returned \"Premium required\", which usually means the client was not recognised). Retrying with updated credentials may fix this.";

fn map_connect_error(e: &librespot_core::Error, stored: bool) -> Error {
    use librespot_core::error::ErrorKind;
    match classify_login_failure(&e.error.to_string()) {
        Some(LoginFailure::BadCredentials) if stored => {
            return Error::revoked(crate::auth::SILENT_LOGIN_REJECTED);
        }
        Some(LoginFailure::BadCredentials) => {
            return Error::auth("Spotify refused the login (bad credentials). Please try again.");
        }
        Some(LoginFailure::PremiumRequired) => return Error::auth(PREMIUM_REQUIRED),
        Some(LoginFailure::Transient) => {
            return Error::Net("Spotify could not complete the login right now".into());
        }
        Some(LoginFailure::Other) => {
            return Error::auth(format!("Spotify refused the login: {}", e.error));
        }
        None => {}
    }
    match e.kind {
        ErrorKind::Unavailable | ErrorKind::DeadlineExceeded => {
            Error::Net("could not reach Spotify's access points".into())
        }
        _ => Error::auth(format!("Spotify login failed: {e}")),
    }
}

async fn start_connect(
    session: librespot_core::Session,
    credentials: Credentials,
    opts: &ConnectOptions,
) -> std::result::Result<ConnectDevice, librespot_core::Error> {
    use librespot_connect::{ConnectConfig, Spirc};
    use librespot_core::config::DeviceType;
    use librespot_playback::audio_backend;
    use librespot_playback::config::AudioFormat;
    use librespot_playback::mixer::softmixer::SoftMixer;
    use librespot_playback::mixer::{Mixer, MixerConfig};
    use librespot_playback::player::Player;

    let mixer = Arc::new(SoftMixer::open(MixerConfig::default())?);
    let backend = audio_backend::find(None)
        .ok_or_else(|| librespot_core::Error::unavailable("no audio backend"))?;
    let player = Player::new(
        super::playback::player_config(opts.bitrate, opts.normalisation),
        session.clone(),
        mixer.get_soft_volume(),
        move || backend(None, AudioFormat::default()),
    );
    let events = player.get_player_event_channel();
    let config = ConnectConfig {
        name: opts.name.clone(),
        device_type: DeviceType::Computer,
        initial_volume: super::playback::volume_to_u16(opts.initial_volume),
        ..ConnectConfig::default()
    };
    let (spirc, task) = Spirc::new(config, session, credentials, player, mixer).await?;
    tokio::spawn(task);
    Ok(ConnectDevice {
        spirc,
        events: Mutex::new(Some(events)),
    })
}

/// Open the dealer websocket without registering a playable device, the
/// way the web player observes Connect: the connection id that the
/// connect-state observer needs comes from `hm://pusher/v1/connections/`.
async fn start_controller_dealer(session: &librespot_core::Session) -> bool {
    use futures::StreamExt;
    let ids = session
        .dealer()
        .listen_for("hm://pusher/v1/connections/", |msg| {
            msg.headers
                .get("Spotify-Connection-Id")
                .cloned()
                .ok_or_else(|| librespot_core::Error::failed_precondition("no connection id"))
        });
    let mut ids = match ids {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "no dealer for the Connect controller");
            return false;
        }
    };
    if let Err(e) = session.dealer().start().await {
        tracing::warn!(error = %e, "could not open the dealer for the Connect controller");
        return false;
    }
    let s = session.clone();
    tokio::spawn(async move {
        while let Some(id) = ids.next().await {
            match id {
                Ok(id) => s.set_connection_id(&id),
                Err(e) => tracing::debug!(error = %e, "bad connection id push"),
            }
        }
    });
    true
}

/// Open the session (and the Connect device when enabled). Returns fresh
/// reusable credentials for the keyring.
pub async fn connect(
    creds: SessionCredentials,
    opts: &SessionOptions,
) -> Result<(SpotifySession, StoredCredentials)> {
    let stored = creds.is_stored();
    let credentials = creds.into_librespot()?;
    let mut config = librespot_core::SessionConfig {
        client_id: opts.client_id.clone(),
        autoplay: Some(opts.autoplay),
        ..librespot_core::SessionConfig::default()
    };
    if let Some(id) = &opts.device_id {
        config.device_id = id.clone();
    }
    let mut device = None;
    let mut session = librespot_core::Session::new(config.clone(), None);
    if let Some(c) = &opts.connect {
        // Spirc opens the session itself after registering its listeners.
        match start_connect(session.clone(), credentials.clone(), c).await {
            Ok(d) => device = Some(Arc::new(d)),
            Err(e) => {
                let failure = classify_login_failure(&e.error.to_string());
                // "Premium required" from Connect may just mean the AP
                // blocks third-party Connect registrations — the plain
                // session (no Spirc device) can still work, and the DPoP
                // credential upgrade that runs after the first token fetch
                // may fix it for the next session.
                let fatal = matches!(
                    failure,
                    Some(LoginFailure::BadCredentials | LoginFailure::Other)
                );
                if fatal {
                    return Err(map_connect_error(&e, stored));
                }
                tracing::warn!(error = %e, "Spotify Connect unavailable; continuing without it");
                session.shutdown();
                session = librespot_core::Session::new(config, None);
            }
        }
    }
    let mut dealer = device.is_some();
    if device.is_none() {
        session
            .connect(credentials, false)
            .await
            .map_err(|e| map_connect_error(&e, stored))?;
        if opts.controller {
            dealer = start_controller_dealer(&session).await;
        }
    }
    let creds = StoredCredentials {
        username: session.username(),
        auth_data: B64.encode(session.auth_data()),
    };
    Ok((
        SpotifySession {
            session,
            connect: device,
            dealer,
        },
        creds,
    ))
}

/// [`SessionAuth`] over a live librespot session. The session can be swapped
/// after a reconnect without rebuilding the backends.
pub struct LiveAuth {
    session: RwLock<librespot_core::Session>,
}

impl LiveAuth {
    pub fn new(session: librespot_core::Session) -> Self {
        Self {
            session: RwLock::new(session),
        }
    }

    pub fn replace(&self, session: librespot_core::Session) {
        if let Ok(mut s) = self.session.write() {
            *s = session;
        }
    }

    fn current(&self) -> Result<librespot_core::Session> {
        self.session
            .read()
            .map(|s| s.clone())
            .map_err(|_| Error::auth("session lock poisoned"))
    }
}

impl TokenSource for LiveAuth {
    fn fetch(&self) -> BoxFuture<'_, Result<AccessToken>> {
        Box::pin(async move {
            let session = self.current()?;
            if session.is_invalid() {
                return Err(Error::Net("the Spotify session is reconnecting".into()));
            }
            // login5 with the session's stored credentials: the same token
            // source librespot's spclient and dealer use.
            let t = session
                .login5()
                .auth_token()
                .await
                .map_err(|e| Error::auth(format!("could not get a session token: {e}")))?;
            let age = t.timestamp.elapsed().unwrap_or_default();
            let left = t.expires_in.saturating_sub(age);
            Ok(AccessToken {
                secret: Secret::new(t.access_token),
                expires_at: Instant::now() + left.max(Duration::from_secs(1)),
            })
        })
    }
}

impl SessionAuth for LiveAuth {
    fn client_token(&self) -> BoxFuture<'_, Result<Option<String>>> {
        Box::pin(async move {
            let session = self.current()?;
            Ok(session.spclient().client_token().await.ok())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_credentials_roundtrip_and_redaction() {
        let c = StoredCredentials {
            username: "user".into(),
            auth_data: "c2VjcmV0".into(),
        };
        assert_eq!(
            StoredCredentials::from_secret(&c.to_secret().unwrap()).unwrap(),
            c
        );
        assert!(!format!("{c:?}").contains("c2VjcmV0"));
        assert!(
            StoredCredentials::from_secret(&Secret::new("{"))
                .unwrap_err()
                .is_revoked()
        );
    }

    #[test]
    fn credentials_map_to_librespot() {
        let c = SessionCredentials::Stored(StoredCredentials {
            username: "u".into(),
            auth_data: B64.encode(b"blob"),
        })
        .into_librespot()
        .unwrap();
        assert_eq!(c.auth_data, b"blob");
        assert_eq!(c.username.as_deref(), Some("u"));
        let t = SessionCredentials::AccessToken(Secret::new("tok"))
            .into_librespot()
            .unwrap();
        assert_eq!(t.auth_data, b"tok");
    }

    #[test]
    fn login_errors_classify() {
        let failed = |reason: &str| {
            librespot_core::Error::permission_denied(std::io::Error::other(format!(
                "Login failed with reason: {reason}"
            )))
        };
        // Only bad credentials revoke the stored login.
        assert!(map_connect_error(&failed("Bad credentials"), true).is_revoked());
        assert!(!map_connect_error(&failed("Bad credentials"), false).is_revoked());
        for transient in [
            "Try another access point",
            "Could not validate credentials",
            "Protocol error",
        ] {
            let e = map_connect_error(&failed(transient), true);
            assert!(matches!(e, Error::Net(_)), "{transient}: {e:?}");
        }
        let premium = map_connect_error(&failed("Premium account required"), true);
        assert!(!premium.is_revoked());
        assert_eq!(premium.user_message(), PREMIUM_REQUIRED);
        assert!(!map_connect_error(&failed("Travel restriction"), true).is_revoked());
        let down = librespot_core::Error::unavailable("x");
        assert!(matches!(map_connect_error(&down, true), Error::Net(_)));
        assert_eq!(classify_login_failure("timeout"), None);
    }
}
