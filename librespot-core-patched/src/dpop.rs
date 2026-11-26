//! DPoP (Demonstrating Proof of Possession, RFC 9449) with P-256 / ES256.
//!
//! The official Spotify desktop client generates an ephemeral EC P-256 key
//! pair and includes a signed DPoP proof JWT with every HTTP request to the
//! login5 and spclient backends. The access tokens it receives are
//! cryptographically bound to the key via the JKT (RFC 7638) in the proof's
//! public JWK, so a token stolen in transit cannot be replayed without the
//! matching private key.
//!
//! This module provides the minimum the `Login5Manager` needs: one key per
//! session (generated on first use), per-host nonces picked up from the
//! server's `DPoP-Nonce` response header, and a proof builder that produces
//! a compact JWS (`header.payload.signature`) ready for the `DPoP` header.

use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::elliptic_curve::rand_core::{OsRng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Error;

#[derive(Serialize, Deserialize)]
struct StoredKey {
    private_key: String,
}

/// P-256 DPoP key pair with interior mutability for the server nonce.
pub struct DpopKey {
    signing: SigningKey,
    jwk_json: String,
    jwk_thumbprint: String,
    nonce: Mutex<Option<String>>,
}

impl DpopKey {
    /// Generate a fresh key pair.
    pub fn new() -> Result<Self, Error> {
        let signing = SigningKey::random(&mut OsRng);
        Self::from_signing_key(signing)
    }

    fn from_signing_key(signing: SigningKey) -> Result<Self, Error> {
        let verifying = VerifyingKey::from(&signing);
        let point = verifying.to_encoded_point(false);
        let x = B64URL.encode(
            point
                .x()
                .ok_or_else(|| Error::internal("DPoP: bad EC point"))?,
        );
        let y = B64URL.encode(
            point
                .y()
                .ok_or_else(|| Error::internal("DPoP: bad EC point"))?,
        );
        // JWK + canonical JKT (RFC 7638: members sorted alphabetically).
        let jwk_json = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let jwk_thumbprint = B64URL.encode(Sha256::digest(canonical.as_bytes()));
        Ok(Self {
            signing,
            jwk_json,
            jwk_thumbprint,
            nonce: Mutex::new(None),
        })
    }

    /// JSON Key Thumbprint (`dpop_jkt` query parameter).
    pub fn jkt(&self) -> &str {
        &self.jwk_thumbprint
    }

    /// Store the `DPoP-Nonce` header the server last sent (RFC 9449 §8).
    pub fn set_nonce(&self, nonce: String) {
        if let Ok(mut n) = self.nonce.lock() {
            *n = Some(nonce);
        }
    }

    /// Build a DPoP proof JWT for one request.
    ///
    /// * `method` — HTTP method (`"POST"`, `"GET"`, ...)
    /// * `url` — full target URL; the query and fragment are stripped as
    ///   required by the `htu` claim
    /// * `access_token` — when proving possession of an existing bearer
    ///   token, the `ath` (access-token hash) claim is included
    pub fn create_proof(
        &self,
        method: &str,
        url: &str,
        access_token: Option<&str>,
    ) -> Result<String, Error> {
        let htu = htu_from_url(url);
        let header = format!(
            r#"{{"typ":"dpop+jwt","alg":"ES256","jwk":{}}}"#,
            self.jwk_json
        );
        let iat = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut jti_bytes = [0u8; 16];
        OsRng.fill_bytes(&mut jti_bytes);
        let jti = B64URL.encode(jti_bytes);
        let mut payload =
            format!(r#"{{"jti":"{jti}","htm":"{method}","htu":"{htu}","iat":{iat}"#);
        if let Ok(guard) = self.nonce.lock() {
            if let Some(n) = guard.as_ref() {
                payload.push_str(&format!(r#","nonce":"{n}""#));
            }
        }
        if let Some(at) = access_token {
            let ath = B64URL.encode(Sha256::digest(at.as_bytes()));
            payload.push_str(&format!(r#","ath":"{ath}""#));
        }
        payload.push('}');

        let signing_input = format!(
            "{}.{}",
            B64URL.encode(header.as_bytes()),
            B64URL.encode(payload.as_bytes())
        );
        let sig: p256::ecdsa::Signature = self.signing.sign(signing_input.as_bytes());
        Ok(format!("{signing_input}.{}", B64URL.encode(sig.to_bytes())))
    }

    /// Serialise the private key for the caller to persist.
    pub fn to_private_key_b64(&self) -> String {
        B64URL.encode(self.signing.to_bytes())
    }

    /// Restore a key pair from a base64 private-key blob (as returned by
    /// `to_private_key_b64`).
    pub fn from_private_key_b64(s: &str) -> Result<Self, Error> {
        let bytes = B64URL
            .decode(s.as_bytes())
            .map_err(|e| Error::failed_precondition(format!("DPoP key decode: {e}")))?;
        let signing = SigningKey::from_slice(&bytes)
            .map_err(|e| Error::failed_precondition(format!("DPoP key invalid: {e}")))?;
        Self::from_signing_key(signing)
    }
}

/// Strip query and fragment from a URL (required by the `htu` claim).
pub fn htu_from_url(url: &str) -> String {
    if let Ok(mut u) = url::Url::parse(url) {
        u.set_query(None);
        u.set_fragment(None);
        u.to_string()
    } else {
        url.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_proof_shape() {
        let key = DpopKey::new().unwrap();
        assert!(!key.jkt().is_empty());
        let proof = key
            .create_proof("POST", "https://login5.spotify.com/v3/login", None)
            .unwrap();
        assert_eq!(proof.matches('.').count(), 2, "three JWT parts");
    }

    #[test]
    fn nonce_appears_in_payload() {
        let key = DpopKey::new().unwrap();
        key.set_nonce("server-nonce-xyz".into());
        let proof = key
            .create_proof("POST", "https://login5.spotify.com/v3/login", None)
            .unwrap();
        let payload = String::from_utf8(
            B64URL
                .decode(proof.split('.').nth(1).unwrap().as_bytes())
                .unwrap(),
        )
        .unwrap();
        assert!(payload.contains("\"nonce\":\"server-nonce-xyz\""));
    }

    #[test]
    fn htu_strips_query() {
        assert_eq!(
            htu_from_url("https://example.com/path?x=1#y"),
            "https://example.com/path"
        );
    }
}
