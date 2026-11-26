//! DPoP (Demonstrating Proof of Possession, RFC 9449) with P-256 / ES256.
//!
//! The official Spotify desktop client generates an ephemeral EC P-256 key pair,
//! stores it in the OS keyring, and includes a signed DPoP proof JWT with every
//! HTTP request. The access tokens it receives are cryptographically bound to
//! that key pair via the JKT (JSON Key Thumbprint, RFC 7638). This prevents
//! token replay by any party that does not possess the private key.

use std::time::{SystemTime, UNIX_EPOCH};

use base64ct::{Base64UrlUnpadded, Encoding};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::elliptic_curve::rand_core::OsRng;
use sha2::{Digest, Sha256};

use crate::error::{ProtocolError, Result};

/// A persisted DPoP key pair (P-256).
///
/// Debug is intentionally omitted to prevent accidentally leaking key material.
pub struct DpopKey {
    signing: SigningKey,
    jwk_thumbprint: String,
    jwk_json: String,
}

/// Serialised form stored in the keyring (JSON with the raw private key bytes).
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredKey {
    /// Raw scalar bytes of the P-256 private key, base64url-encoded.
    private_key: String,
}

impl DpopKey {
    /// Generate a fresh random P-256 key pair.
    pub fn generate() -> Result<Self> {
        let signing = SigningKey::random(&mut OsRng);
        Self::from_signing_key(signing)
    }

    /// Restore from a serialised secret (as produced by [`to_secret`]).
    pub fn from_secret(secret: &[u8]) -> Result<Self> {
        let stored: StoredKey = serde_json::from_slice(secret)
            .map_err(|_| ProtocolError::Token("DPoP key is unreadable".into()))?;
        let der = Base64UrlUnpadded::decode_vec(&stored.private_key)
            .map_err(|_| ProtocolError::Token("DPoP key data is corrupt".into()))?;
        let signing = SigningKey::from_slice(&der)
            .map_err(|_| ProtocolError::Token("DPoP private key is invalid".into()))?;
        Self::from_signing_key(signing)
    }

    /// Serialise the private key for secure storage.
    pub fn to_secret(&self) -> Result<Vec<u8>> {
        let stored = StoredKey {
            private_key: Base64UrlUnpadded::encode_string(&self.signing.to_bytes()),
        };
        serde_json::to_vec(&stored)
            .map_err(|e| ProtocolError::Token(format!("DPoP key serialise: {e}")))
    }

    fn from_signing_key(signing: SigningKey) -> Result<Self> {
        let verifying = VerifyingKey::from(&signing);
        let point = verifying.to_encoded_point(false);
        let x = Base64UrlUnpadded::encode_string(
            point
                .x()
                .ok_or_else(|| ProtocolError::Token("bad EC point".into()))?,
        );
        let y = Base64UrlUnpadded::encode_string(
            point
                .y()
                .ok_or_else(|| ProtocolError::Token("bad EC point".into()))?,
        );

        // JWK (RFC 7517) — the public key embedded in the proof header.
        let jwk_json = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);

        // JKT (RFC 7638) — `dpop_jkt` parameter for binding auth codes.
        // Canonical JWK: members sorted alphabetically (crv < kty < x < y).
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let jwk_thumbprint =
            Base64UrlUnpadded::encode_string(&Sha256::digest(canonical.as_bytes()));

        Ok(Self {
            signing,
            jwk_thumbprint,
            jwk_json,
        })
    }

    /// The JKT value used in `dpop_jkt` query parameters.
    pub fn jkt(&self) -> &str {
        &self.jwk_thumbprint
    }

    /// Build a DPoP proof JWT for a request.
    ///
    /// * `method` — HTTP method (e.g. `"POST"`)
    /// * `url` — target URI *without* query or fragment
    /// * `access_token` — when proving possession for an existing token, the
    ///   `ath` (access token hash) claim is included
    /// * `nonce` — the last `DPoP-Nonce` header the server sent (if any)
    pub fn proof(
        &self,
        method: &str,
        url: &str,
        access_token: Option<&str>,
        nonce: Option<&str>,
    ) -> Result<String> {
        let header = format!(
            r#"{{"typ":"dpop+jwt","alg":"ES256","jwk":{}}}"#,
            self.jwk_json
        );

        let iat = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let mut jti_bytes = [0u8; 16];
        p256::elliptic_curve::rand_core::RngCore::fill_bytes(&mut OsRng, &mut jti_bytes);
        let jti = Base64UrlUnpadded::encode_string(&jti_bytes);

        let mut payload = format!(r#"{{"jti":"{jti}","htm":"{method}","htu":"{url}","iat":{iat}"#);

        if let Some(n) = nonce {
            payload.push_str(&format!(r#","nonce":"{n}""#));
        }
        if let Some(at) = access_token {
            let ath = Base64UrlUnpadded::encode_string(&Sha256::digest(at.as_bytes()));
            payload.push_str(&format!(r#","ath":"{ath}""#));
        }
        payload.push('}');

        let signing_input = format!(
            "{}.{}",
            Base64UrlUnpadded::encode_string(header.as_bytes()),
            Base64UrlUnpadded::encode_string(payload.as_bytes()),
        );

        let sig: p256::ecdsa::Signature = self.signing.sign(signing_input.as_bytes());
        let sig_bytes = sig.to_bytes();

        Ok(format!(
            "{signing_input}.{}",
            Base64UrlUnpadded::encode_string(&sig_bytes)
        ))
    }
}

/// Extract the `DPoP-Nonce` header from an HTTP response.
pub fn extract_nonce(headers: &reqwest::header::HeaderMap) -> Option<String> {
    headers
        .get("dpop-nonce")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
}

/// Strip query and fragment from a URL, as required by the `htu` claim.
pub fn htu_from_url(url: &str) -> String {
    if let Ok(mut u) = url::Url::parse(url) {
        u.set_query(None);
        u.set_fragment(None);
        u.to_string()
    } else {
        url.to_owned()
    }
}

// Custom Debug that never leaks the private key.
impl std::fmt::Debug for DpopKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DpopKey")
            .field("jkt", &self.jwk_thumbprint)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_and_proof() {
        let key = DpopKey::generate().unwrap();
        assert!(!key.jkt().is_empty());
        let proof = key
            .proof("POST", "https://accounts.spotify.com/api/token", None, None)
            .unwrap();
        assert_eq!(proof.matches('.').count(), 2, "JWT has three parts");
    }

    #[test]
    fn roundtrip_secret() {
        let key = DpopKey::generate().unwrap();
        let secret = key.to_secret().unwrap();
        let restored = DpopKey::from_secret(&secret).unwrap();
        assert_eq!(key.jkt(), restored.jkt());
    }

    #[test]
    fn proof_header_has_correct_typ_and_alg() {
        let key = DpopKey::generate().unwrap();
        let proof = key
            .proof("GET", "https://example.com/resource", None, None)
            .unwrap();
        let parts: Vec<&str> = proof.split('.').collect();
        assert_eq!(parts.len(), 3);

        let header_json =
            String::from_utf8(Base64UrlUnpadded::decode_vec(parts[0]).unwrap()).unwrap();
        assert!(header_json.contains(r#""typ":"dpop+jwt""#));
        assert!(header_json.contains(r#""alg":"ES256""#));
        assert!(header_json.contains(r#""jwk":"#));
    }

    #[test]
    fn proof_payload_has_required_claims() {
        let key = DpopKey::generate().unwrap();
        let proof = key
            .proof(
                "POST",
                "https://spclient.wg.spotify.com/metadata/4/track/abc",
                None,
                None,
            )
            .unwrap();
        let parts: Vec<&str> = proof.split('.').collect();
        let payload_json =
            String::from_utf8(Base64UrlUnpadded::decode_vec(parts[1]).unwrap()).unwrap();

        assert!(payload_json.contains(r#""htm":"POST""#));
        assert!(
            payload_json
                .contains(r#""htu":"https://spclient.wg.spotify.com/metadata/4/track/abc""#)
        );
        assert!(payload_json.contains(r#""jti":"#));
        assert!(payload_json.contains(r#""iat":"#));
        // No nonce or ath when not provided.
        assert!(!payload_json.contains("\"nonce\""));
        assert!(!payload_json.contains("\"ath\""));
    }

    #[test]
    fn proof_with_nonce_and_ath() {
        let key = DpopKey::generate().unwrap();
        let proof = key
            .proof(
                "GET",
                "https://spclient.wg.spotify.com/metadata/4/track/abc",
                Some("BEhgLSBh6W0QFlN..."),
                Some("server-nonce-123"),
            )
            .unwrap();
        let parts: Vec<&str> = proof.split('.').collect();
        let payload = String::from_utf8(Base64UrlUnpadded::decode_vec(parts[1]).unwrap()).unwrap();
        assert!(payload.contains("\"nonce\":\"server-nonce-123\""));
        assert!(payload.contains("\"ath\":"));
    }

    #[test]
    fn jkt_is_base64url_sha256() {
        let key = DpopKey::generate().unwrap();
        let jkt = key.jkt();
        // SHA-256 is 32 bytes; base64url-unpadded of 32 bytes is 43 characters.
        assert_eq!(jkt.len(), 43, "JKT should be 43 base64url chars");
        // Should be decodable.
        assert!(Base64UrlUnpadded::decode_vec(jkt).is_ok());
    }

    #[test]
    fn htu_strips_query_and_fragment() {
        assert_eq!(
            htu_from_url("https://example.com/path?foo=bar#frag"),
            "https://example.com/path"
        );
    }

    #[test]
    fn htu_preserves_clean_url() {
        assert_eq!(
            htu_from_url("https://example.com/path"),
            "https://example.com/path"
        );
    }

    #[test]
    fn from_secret_rejects_garbage() {
        assert!(DpopKey::from_secret(b"not json").is_err());
        assert!(DpopKey::from_secret(br#"{"private_key":"!!!"}"#).is_err());
    }

    #[test]
    fn debug_does_not_leak_key() {
        let key = DpopKey::generate().unwrap();
        let dbg = format!("{key:?}");
        assert!(dbg.contains("DpopKey"));
        assert!(dbg.contains("jkt"));
        // Should not contain the raw private key bytes.
        assert!(!dbg.contains("signing"));
    }
}
