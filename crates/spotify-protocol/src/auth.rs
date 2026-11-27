//! Credentials for Spotify AP authentication.
//!
//! The [`Credentials`] type carries whatever the caller provides — an access
//! token from the browser-based OAuth login, stored reusable credentials from a
//! previous session, or (in theory) a plaintext password.  The session hands
//! these to the AP during authentication and receives back a set of *reusable*
//! credentials that can be persisted in the keyring.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// AuthenticationType — mirrors the protobuf enum without pulling in protobuf
// ---------------------------------------------------------------------------

/// Spotify's `AuthenticationType` values.
///
/// We store the numeric value directly so this crate does not depend on the
/// protobuf definitions (the AP just sends/receives a varint).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[repr(i32)]
pub enum AuthenticationType {
    UserPass = 0,
    #[default]
    StoredSpotifyCredentials = 1,
    StoredFacebookCredentials = 2,
    SpotifyToken = 3,
    FacebookToken = 4,
}

impl AuthenticationType {
    /// Convert from the raw protobuf `i32` value.
    pub fn from_i32(v: i32) -> Option<Self> {
        match v {
            0 => Some(Self::UserPass),
            1 => Some(Self::StoredSpotifyCredentials),
            2 => Some(Self::StoredFacebookCredentials),
            3 => Some(Self::SpotifyToken),
            4 => Some(Self::FacebookToken),
            _ => None,
        }
    }

    /// The raw protobuf value.
    pub fn value(self) -> i32 {
        self as i32
    }
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Credentials used to authenticate with a Spotify access point.
///
/// After a successful login the AP returns *reusable* credentials that the
/// caller should persist (e.g. in the system keyring).  Those reusable
/// credentials can be turned back into a [`Credentials`] via
/// [`Credentials::with_stored`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Credentials {
    /// The Spotify username.  `None` for token-based logins (the AP will
    /// return the canonical username in the welcome message).
    pub username: Option<String>,

    /// Which authentication flow this credential represents.
    pub auth_type: AuthenticationType,

    /// The opaque authentication payload — a password in UTF-8, an access
    /// token, or the stored blob the AP returned on a previous login.
    pub auth_data: Vec<u8>,
}

impl Credentials {
    /// Create credentials from a Spotify access token (browser OAuth flow).
    pub fn with_access_token(token: impl Into<String>) -> Self {
        Self {
            username: None,
            auth_type: AuthenticationType::SpotifyToken,
            auth_data: token.into().into_bytes(),
        }
    }

    /// Create credentials from a username + the reusable blob the AP returned
    /// in a previous `APWelcome`.
    pub fn with_stored(username: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            username: Some(username.into()),
            auth_type: AuthenticationType::StoredSpotifyCredentials,
            auth_data: data,
        }
    }

    /// Create credentials from a username and password (not recommended for
    /// production — Spotify may refuse password logins for some accounts).
    pub fn with_password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: Some(username.into()),
            auth_type: AuthenticationType::UserPass,
            auth_data: password.into().into_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_credentials() {
        let creds = Credentials::with_access_token("my_token");
        assert_eq!(creds.auth_type, AuthenticationType::SpotifyToken);
        assert_eq!(creds.auth_data, b"my_token");
        assert!(creds.username.is_none());
    }

    #[test]
    fn stored_credentials() {
        let creds = Credentials::with_stored("alice", vec![1, 2, 3]);
        assert_eq!(
            creds.auth_type,
            AuthenticationType::StoredSpotifyCredentials
        );
        assert_eq!(creds.auth_data, vec![1, 2, 3]);
        assert_eq!(creds.username.as_deref(), Some("alice"));
    }

    #[test]
    fn password_credentials() {
        let creds = Credentials::with_password("bob", "s3cret");
        assert_eq!(creds.auth_type, AuthenticationType::UserPass);
        assert_eq!(creds.auth_data, b"s3cret");
        assert_eq!(creds.username.as_deref(), Some("bob"));
    }

    #[test]
    fn auth_type_round_trip() {
        for v in 0..=4 {
            let ty = AuthenticationType::from_i32(v);
            assert!(ty.is_some(), "value {v} should be valid");
            assert_eq!(ty.map(|t| t.value()), Some(v));
        }
        assert!(AuthenticationType::from_i32(99).is_none());
    }

    #[test]
    fn serde_round_trip() {
        let creds = Credentials::with_stored("carol", vec![10, 20, 30]);
        let json = serde_json::to_string(&creds).unwrap();
        let back: Credentials = serde_json::from_str(&json).unwrap();
        assert_eq!(creds, back);
    }
}
