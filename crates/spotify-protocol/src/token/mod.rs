//! Token management: DPoP proofs, client tokens, and cached access tokens.
//!
//! This module handles all token-related authentication that happens after the
//! initial AP connection:
//!
//! - **DPoP** (RFC 9449): Proof-of-possession JWTs bound to access tokens.
//! - **Client tokens**: Separate credential from `clienttoken.spotify.com` with
//!   HashCash challenge-response.
//! - **Token caching**: Thread-safe storage of access and client tokens with
//!   automatic expiry.

pub mod client_token;
pub mod dpop;

use std::time::{Duration, Instant};

use tokio::sync::Mutex;

pub use client_token::ClientToken;
pub use dpop::DpopKey;

// ---------------------------------------------------------------------------
// CachedToken (access token with expiry)
// ---------------------------------------------------------------------------

/// A cached access token with its expiry time.
///
/// Debug is intentionally redacted to avoid leaking token values in logs.
struct CachedToken {
    token: String,
    expires_at: Instant,
}

impl CachedToken {
    fn is_fresh(&self) -> bool {
        // 60-second safety margin, same as ClientToken.
        Instant::now() + Duration::from_secs(60) < self.expires_at
    }
}

// ---------------------------------------------------------------------------
// TokenManager
// ---------------------------------------------------------------------------

/// Central token store for a Spotify session.
///
/// Holds the optional DPoP key pair and caches the access and client tokens
/// with thread-safe interior mutability (via [`tokio::sync::Mutex`]).
pub struct TokenManager {
    dpop_key: Option<DpopKey>,
    access_token: Mutex<Option<CachedToken>>,
    client_token: Mutex<Option<ClientToken>>,
}

impl TokenManager {
    /// Create a new `TokenManager`.
    ///
    /// Pass `Some(dpop_key)` to enable DPoP proof generation; `None` for
    /// sessions that do not use proof-of-possession.
    pub fn new(dpop_key: Option<DpopKey>) -> Self {
        Self {
            dpop_key,
            access_token: Mutex::new(None),
            client_token: Mutex::new(None),
        }
    }

    /// The DPoP key (if available).
    pub fn dpop_key(&self) -> Option<&DpopKey> {
        self.dpop_key.as_ref()
    }

    // --- Access token ---

    /// Cache an access token with the given TTL.
    pub async fn set_access_token(&self, token: String, expires_in: Duration) {
        let mut guard = self.access_token.lock().await;
        *guard = Some(CachedToken {
            token,
            expires_at: Instant::now() + expires_in,
        });
    }

    /// Return the cached access token if it is still fresh, or `None`.
    pub async fn access_token(&self) -> Option<String> {
        let guard = self.access_token.lock().await;
        guard
            .as_ref()
            .filter(|ct| ct.is_fresh())
            .map(|ct| ct.token.clone())
    }

    /// Clear the cached access token (e.g. on logout or session reset).
    pub async fn clear_access_token(&self) {
        let mut guard = self.access_token.lock().await;
        *guard = None;
    }

    // --- Client token ---

    /// Cache a client token.
    pub async fn set_client_token(&self, token: ClientToken) {
        let mut guard = self.client_token.lock().await;
        *guard = Some(token);
    }

    /// Return the cached client token string if it is still fresh, or `None`.
    pub async fn client_token(&self) -> Option<String> {
        let guard = self.client_token.lock().await;
        guard
            .as_ref()
            .filter(|ct| ct.is_fresh())
            .map(|ct| ct.token().to_owned())
    }

    /// Clear the cached client token.
    pub async fn clear_client_token(&self) {
        let mut guard = self.client_token.lock().await;
        *guard = None;
    }
}

impl std::fmt::Debug for TokenManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenManager")
            .field("has_dpop_key", &self.dpop_key.is_some())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn access_token_caching() {
        let mgr = TokenManager::new(None);

        // Initially empty.
        assert!(mgr.access_token().await.is_none());

        // Set a token with 5-minute TTL.
        mgr.set_access_token("tok-123".into(), Duration::from_secs(300))
            .await;
        assert_eq!(mgr.access_token().await.as_deref(), Some("tok-123"));

        // Clear it.
        mgr.clear_access_token().await;
        assert!(mgr.access_token().await.is_none());
    }

    #[tokio::test]
    async fn access_token_expiry() {
        let mgr = TokenManager::new(None);

        // Set a token that is already expired (0 TTL, within the 60s margin).
        mgr.set_access_token("expired".into(), Duration::from_secs(0))
            .await;
        assert!(
            mgr.access_token().await.is_none(),
            "expired token should not be returned"
        );
    }

    #[tokio::test]
    async fn client_token_caching() {
        let mgr = TokenManager::new(None);

        assert!(mgr.client_token().await.is_none());

        let ct = ClientToken::new(
            "client-tok-456".into(),
            Instant::now() + Duration::from_secs(300),
        );
        mgr.set_client_token(ct).await;
        assert_eq!(mgr.client_token().await.as_deref(), Some("client-tok-456"));

        mgr.clear_client_token().await;
        assert!(mgr.client_token().await.is_none());
    }

    #[tokio::test]
    async fn client_token_expiry() {
        let mgr = TokenManager::new(None);

        let ct = ClientToken::new("old".into(), Instant::now() - Duration::from_secs(1));
        mgr.set_client_token(ct).await;
        assert!(
            mgr.client_token().await.is_none(),
            "expired client token should not be returned"
        );
    }

    #[test]
    fn token_manager_with_dpop_key() {
        let key = DpopKey::generate().unwrap();
        let jkt = key.jkt().to_owned();
        let mgr = TokenManager::new(Some(key));

        assert!(mgr.dpop_key().is_some());
        assert_eq!(mgr.dpop_key().unwrap().jkt(), jkt);
    }

    #[test]
    fn token_manager_without_dpop_key() {
        let mgr = TokenManager::new(None);
        assert!(mgr.dpop_key().is_none());
    }

    #[test]
    fn debug_does_not_leak() {
        let mgr = TokenManager::new(None);
        let dbg = format!("{mgr:?}");
        assert!(dbg.contains("TokenManager"));
        assert!(dbg.contains("has_dpop_key"));
    }
}
