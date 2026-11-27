//! DPoP (Demonstrating Proof of Possession, RFC 9449) with P-256 / ES256.
//!
//! The official Spotify desktop client generates an ephemeral EC P-256 key pair,
//! stores it in the OS keyring, and includes a signed DPoP proof JWT with every
//! HTTP request. The access tokens it receives are cryptographically bound to
//! that key pair via the JKT (JSON Key Thumbprint, RFC 7638). This prevents
//! token replay by any party that does not possess the private key.

use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{SigningKey, VerifyingKey};
use p256::elliptic_curve::rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::secrets::Secret;
use crate::error::{Error, Result};

/// A persisted DPoP key pair (P-256).
pub struct DpopKey {
    signing: SigningKey,
    jwk_thumbprint: String,
    jwk_json: String,
}

/// Serialised form stored in the keyring.
#[derive(Serialize, Deserialize)]
struct StoredKey {
    /// PKCS#8 DER, base64-encoded.
    private_key: String,
}

impl DpopKey {
    /// Generate a fresh key pair.
    pub fn generate() -> Result<Self> {
        let signing = SigningKey::random(&mut OsRng);
        Self::from_signing_key(signing)
    }

    /// Restore from a keyring secret.
    pub fn from_secret(s: &Secret) -> Result<Self> {
        let stored: StoredKey =
            serde_json::from_str(s.expose()).map_err(|_| Error::auth("DPoP key is unreadable"))?;
        let der = B64URL
            .decode(stored.private_key.as_bytes())
            .map_err(|_| Error::auth("DPoP key data is corrupt"))?;
        let signing =
            SigningKey::from_slice(&der).map_err(|_| Error::auth("DPoP private key is invalid"))?;
        Self::from_signing_key(signing)
    }

    /// Serialise for the keyring.
    pub fn to_secret(&self) -> Result<Secret> {
        let stored = StoredKey {
            private_key: B64URL.encode(self.signing.to_bytes()),
        };
        Ok(Secret::new(serde_json::to_string(&stored)?))
    }

    fn from_signing_key(signing: SigningKey) -> Result<Self> {
        let verifying = VerifyingKey::from(&signing);
        let point = verifying.to_encoded_point(false);
        let x = B64URL.encode(point.x().ok_or_else(|| Error::auth("bad EC point"))?);
        let y = B64URL.encode(point.y().ok_or_else(|| Error::auth("bad EC point"))?);

        // JWK (RFC 7517) — the public key in the proof header.
        let jwk_json = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);

        // JKT (RFC 7638) — `dpop_jkt` parameter for binding auth codes.
        // Canonical JWK: members sorted alphabetically.
        let canonical = format!(r#"{{"crv":"P-256","kty":"EC","x":"{x}","y":"{y}"}}"#);
        let jwk_thumbprint = B64URL.encode(Sha256::digest(canonical.as_bytes()));

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
    /// * `htm` — HTTP method (e.g. `"POST"`)
    /// * `htu` — target URI *without* query or fragment
    /// * `nonce` — the last `DPoP-Nonce` header the server sent (if any)
    /// * `access_token` — when proving possession for an existing token, the
    ///   `ath` (access token hash) claim is included
    pub fn proof(
        &self,
        htm: &str,
        htu: &str,
        nonce: Option<&str>,
        access_token: Option<&str>,
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
        let jti = B64URL.encode(jti_bytes);

        let mut payload = format!(r#"{{"jti":"{jti}","htm":"{htm}","htu":"{htu}","iat":{iat}"#);

        if let Some(n) = nonce {
            payload.push_str(&format!(r#","nonce":"{n}""#));
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
        let sig_bytes = sig.to_bytes();

        Ok(format!("{signing_input}.{}", B64URL.encode(sig_bytes)))
    }
}

/// Keyring key for the DPoP key pair.
pub const DPOP_KEYRING_KEY: &str = "dpop-key";

impl crate::auth::oauth::DpopProver for DpopKey {
    fn proof(&self, htm: &str, htu: &str, nonce: Option<&str>) -> Result<String> {
        self.proof(htm, htu, nonce, None)
    }
}

/// Load the DPoP key pair from the keyring, or generate a fresh one and
/// persist it. Called before OAuth (to bind the code with `dpop_jkt`) and
/// again when the `NativeAuth` opens a session, so both see the same key.
pub fn load_or_generate(secrets: &dyn crate::auth::secrets::SecretStore) -> Result<DpopKey> {
    if let Some(s) = secrets.get(DPOP_KEYRING_KEY)? {
        return DpopKey::from_secret(&s);
    }
    let key = DpopKey::generate()?;
    if let Err(e) = secrets.set(DPOP_KEYRING_KEY, &key.to_secret()?) {
        tracing::warn!(error = %e, "could not persist DPoP key to keyring");
    }
    Ok(key)
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
    fn roundtrip_keyring() {
        let key = DpopKey::generate().unwrap();
        let secret = key.to_secret().unwrap();
        let restored = DpopKey::from_secret(&secret).unwrap();
        assert_eq!(key.jkt(), restored.jkt());
    }

    #[test]
    fn proof_with_nonce_and_ath() {
        let key = DpopKey::generate().unwrap();
        let proof = key
            .proof(
                "GET",
                "https://spclient.wg.spotify.com/metadata/4/track/abc",
                Some("server-nonce-123"),
                Some("BEhgLSBh6W0QFlN..."),
            )
            .unwrap();
        // Decode and check the payload contains the claims.
        let parts: Vec<&str> = proof.split('.').collect();
        let payload = String::from_utf8(B64URL.decode(parts[1]).unwrap()).unwrap();
        assert!(payload.contains("\"nonce\":\"server-nonce-123\""));
        assert!(payload.contains("\"ath\":"));
    }

    #[test]
    fn htu_strips_query() {
        assert_eq!(
            htu_from_url("https://example.com/path?foo=bar#frag"),
            "https://example.com/path"
        );
    }
}
