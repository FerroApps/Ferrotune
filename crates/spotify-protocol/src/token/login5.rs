//! Login5 token refresh via `login5.spotify.com`.
//!
//! Login5 is Spotify's HTTP-based token service. It exchanges stored
//! credentials (from AP authentication) for short-lived access tokens.
//! The server may issue HashCash challenges before granting a token.
//!
//! When a DPoP key is provided, the proof JWT is included in the request
//! and the returned stored credential is bound to that key — using it on
//! the next AP session makes the server treat the connection as an
//! official desktop client.

use std::sync::Arc;
use std::time::{Duration, Instant};

use protobuf::{Message, MessageField, UnknownValue};
use tokio::sync::Mutex;
use uuid::Uuid;

use super::client_token::solve_hash_cash;
use super::dpop::DpopKey;
use crate::error::{ProtocolError, Result};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// The official desktop client hits v4; the proto schema we have is v3 but
/// the wire format is compatible — the server just also accepts a `flow_id`
/// (field 4) that the v3 schema doesn't declare.
const LOGIN5_URL: &str = "https://login5.spotify.com/v4/login";

const MAX_RETRIES: u8 = 3;
const RETRY_DELAY: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------
// Login5Token
// ---------------------------------------------------------------------------

/// A token obtained from Login5.
///
/// Debug intentionally redacts the access token.
pub struct Login5Token {
    access_token: String,
    /// How long the token is valid for (from the moment it was issued).
    pub expires_in: Duration,
    /// DPoP-bound stored credential for the next session.
    /// Empty when the server did not return one (e.g. no DPoP key was sent).
    pub stored_credential: Vec<u8>,
    /// When this token was received.
    pub obtained_at: Instant,
}

impl Login5Token {
    /// The raw access token string.
    pub fn access_token(&self) -> &str {
        &self.access_token
    }

    /// Whether the token has expired (with a 60-second safety margin).
    pub fn is_expired(&self) -> bool {
        let age = self.obtained_at.elapsed();
        age + Duration::from_secs(60) >= self.expires_in
    }
}

impl std::fmt::Debug for Login5Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login5Token")
            .field("access_token", &"[REDACTED]")
            .field("expires_in", &self.expires_in)
            .field("has_stored_credential", &!self.stored_credential.is_empty())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Login5Error
// ---------------------------------------------------------------------------

/// Errors specific to the Login5 flow.
#[derive(Debug, thiserror::Error)]
pub enum Login5Error {
    #[error("login5 request denied: {0:?}")]
    Denied(String),

    #[error("login5 code challenge not supported")]
    CodeChallenge,

    #[error("login5 authentication failed after {0} retries")]
    RetriesFailed(u8),
}

impl From<Login5Error> for ProtocolError {
    fn from(err: Login5Error) -> Self {
        ProtocolError::Token(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Login5
// ---------------------------------------------------------------------------

/// Login5 client.
///
/// Holds the HTTP client, identity, and optional DPoP key needed to exchange
/// stored credentials for access tokens via `login5.spotify.com`.
pub struct Login5 {
    http: reqwest::Client,
    client_id: String,
    device_id: String,
    dpop_key: Option<Arc<DpopKey>>,
    last_dpop_nonce: Mutex<Option<String>>,
}

impl Login5 {
    /// Create a new Login5 client.
    pub fn new(
        http: reqwest::Client,
        client_id: String,
        device_id: String,
        dpop_key: Option<Arc<DpopKey>>,
    ) -> Self {
        Self {
            http,
            client_id,
            device_id,
            dpop_key,
            last_dpop_nonce: Mutex::new(None),
        }
    }

    /// Refresh an access token using stored credentials from AP authentication.
    ///
    /// Returns only the access token (the stored credential from the response
    /// is discarded).
    pub async fn auth_token(
        &self,
        stored_credential: &[u8],
        username: &str,
    ) -> Result<Login5Token> {
        self.login5_request(stored_credential, username).await
    }

    /// Same as [`auth_token`](Self::auth_token) — the returned [`Login5Token`]
    /// always carries `stored_credential` when the server provides one (which
    /// it does when a DPoP proof was included in the request).
    pub async fn auth_token_and_credential(
        &self,
        stored_credential: &[u8],
        username: &str,
    ) -> Result<Login5Token> {
        self.login5_request(stored_credential, username).await
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Build and POST a LoginRequest, handling challenges and retries.
    async fn login5_request(
        &self,
        stored_credential: &[u8],
        username: &str,
    ) -> Result<Login5Token> {
        use spotify_proto::credentials::StoredCredential as ProtoStoredCredential;
        use spotify_proto::login5::login_request::Login_method;

        let method = Login_method::StoredCredential(ProtoStoredCredential {
            username: username.to_owned(),
            data: stored_credential.to_vec(),
            ..Default::default()
        });

        let mut request = self.build_login_request(Some(method));
        let mut response_bytes = self.post_request(&request).await?;
        let mut attempts: u8 = 0;

        loop {
            attempts += 1;

            let response = spotify_proto::login5::LoginResponse::parse_from_bytes(&response_bytes)
                .map_err(|e| ProtocolError::Token(format!("login5 response decode: {e}")))?;

            // Success.
            if response.has_ok() {
                let ok = response.ok();
                let expires_in = Duration::from_secs(ok.access_token_expires_in.max(0) as u64);
                return Ok(Login5Token {
                    access_token: ok.access_token.clone(),
                    expires_in,
                    stored_credential: ok.stored_credential.clone(),
                    obtained_at: Instant::now(),
                });
            }

            // Error.
            if response.has_error() {
                use spotify_proto::login5::LoginError;
                match response.error() {
                    LoginError::TIMEOUT | LoginError::TOO_MANY_ATTEMPTS => {
                        tracing::warn!(
                            error = ?response.error(),
                            "login5: transient error, retrying after delay"
                        );
                        tokio::time::sleep(RETRY_DELAY).await;
                    }
                    other => {
                        return Err(Login5Error::Denied(format!("{other:?}")).into());
                    }
                }
            }

            // HashCash challenge.
            if response.has_challenges() {
                Self::solve_challenges(&mut request, &response)?;
            }

            if attempts >= MAX_RETRIES {
                return Err(Login5Error::RetriesFailed(MAX_RETRIES).into());
            }

            response_bytes = self.post_request(&request).await?;
        }
    }

    /// Build a `LoginRequest` protobuf message.
    fn build_login_request(
        &self,
        login_method: Option<spotify_proto::login5::login_request::Login_method>,
    ) -> spotify_proto::login5::LoginRequest {
        use spotify_proto::client_info::ClientInfo;

        let mut req = spotify_proto::login5::LoginRequest {
            client_info: MessageField::some(ClientInfo {
                client_id: self.client_id.clone(),
                device_id: self.device_id.clone(),
                ..Default::default()
            }),
            login_method,
            ..Default::default()
        };

        // The official desktop client includes a flow_id (field 4, UUID string)
        // in every v4 LoginRequest. The v3 proto schema doesn't declare it, so
        // we inject it as an unknown field — the server accepts it either way.
        let flow_id = Uuid::new_v4().to_string();
        req.special_fields
            .mut_unknown_fields()
            .add_value(4, UnknownValue::LengthDelimited(flow_id.into_bytes()));

        req
    }

    /// POST a serialised LoginRequest and return the raw response bytes.
    async fn post_request(&self, request: &spotify_proto::login5::LoginRequest) -> Result<Vec<u8>> {
        let body = request
            .write_to_bytes()
            .map_err(|e| ProtocolError::Token(format!("login5 request encode: {e}")))?;

        let mut http_req = self
            .http
            .post(LOGIN5_URL)
            .header("Accept", "application/x-protobuf")
            .header("Content-Type", "application/x-protobuf")
            .header("User-Agent", crate::version::USER_AGENT)
            .body(body);

        // Include DPoP proof if a key is available, echoing the last nonce.
        if let Some(ref dpop_key) = self.dpop_key {
            let last_nonce = self.last_dpop_nonce.lock().await;
            let proof = dpop_key.proof("POST", LOGIN5_URL, None, last_nonce.as_deref())?;
            drop(last_nonce);
            tracing::debug!("login5: including DPoP proof JWT");
            http_req = http_req.header("dpop", proof);
        }

        let resp = http_req
            .send()
            .await
            .map_err(|e| ProtocolError::Http(format!("login5 request: {e}")))?;

        // Persist the DPoP-Nonce so subsequent requests echo it back.
        if let Some(nonce) = super::dpop::extract_nonce(resp.headers()) {
            tracing::debug!(nonce = %nonce, "login5: persisting DPoP-Nonce");
            *self.last_dpop_nonce.lock().await = Some(nonce);
        }

        let bytes = resp
            .bytes()
            .await
            .map_err(|e| ProtocolError::Http(format!("login5 response body: {e}")))?;

        Ok(bytes.to_vec())
    }

    /// Solve HashCash challenges from a login response and attach solutions
    /// to the request for the next attempt.
    fn solve_challenges(
        request: &mut spotify_proto::login5::LoginRequest,
        response: &spotify_proto::login5::LoginResponse,
    ) -> Result<()> {
        let challenges = response.challenges();
        tracing::debug!(
            count = challenges.challenges.len(),
            "login5: solving challenges"
        );

        let mut solutions = spotify_proto::login5::ChallengeSolutions::new();

        for challenge in &challenges.challenges {
            if challenge.has_code() {
                return Err(Login5Error::CodeChallenge.into());
            }

            if !challenge.has_hashcash() {
                tracing::debug!("login5: empty challenge, skipping");
                continue;
            }

            let hc = challenge.hashcash();
            let started = Instant::now();

            // The login5 HashCash uses context-aware solving: the prefix is
            // the concatenation of login_context + challenge prefix, and the
            // solution suffix is 16 raw bytes. However, the client_token
            // HashCash solver uses a hex-counter approach against just the
            // prefix. For login5, we use the same solver but feed it the
            // challenge prefix directly — the server accepts both formats.
            let suffix = solve_hash_cash(&hc.prefix, hc.length as u32);

            let elapsed = started.elapsed();
            tracing::debug!(
                difficulty = hc.length,
                elapsed_ms = elapsed.as_millis(),
                "login5: solved hashcash challenge"
            );

            let duration_proto = protobuf::well_known_types::duration::Duration {
                seconds: elapsed.as_secs() as i64,
                nanos: elapsed.subsec_nanos() as i32,
                ..Default::default()
            };

            let mut hc_solution = spotify_proto::hashcash::HashcashSolution::new();
            hc_solution.suffix = suffix.into_bytes();
            hc_solution.duration = MessageField::some(duration_proto);

            let mut solution = spotify_proto::login5::ChallengeSolution::new();
            solution.set_hashcash(hc_solution);
            solutions.solutions.push(solution);
        }

        request.challenge_solutions = MessageField::some(solutions);
        request.login_context = response.login_context.clone();

        Ok(())
    }
}

impl std::fmt::Debug for Login5 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Login5")
            .field("client_id", &self.client_id)
            .field("device_id", &self.device_id)
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
    use protobuf::Message;
    use spotify_proto::login5::login_request::Login_method;

    /// Helper: build a Login5 client with no DPoP key.
    fn test_client() -> Login5 {
        Login5::new(
            reqwest::Client::new(),
            "test-client-id".into(),
            "test-device-id".into(),
            None,
        )
    }

    /// Helper: build a Login5 client with a DPoP key.
    fn test_client_with_dpop() -> Login5 {
        let key = DpopKey::generate().unwrap();
        Login5::new(
            reqwest::Client::new(),
            "test-client-id".into(),
            "test-device-id".into(),
            Some(Arc::new(key)),
        )
    }

    // -----------------------------------------------------------------------
    // Request construction
    // -----------------------------------------------------------------------

    #[test]
    fn request_has_client_info() {
        let client = test_client();
        let method = Login_method::StoredCredential(spotify_proto::credentials::StoredCredential {
            username: "alice".into(),
            data: vec![1, 2, 3],
            ..Default::default()
        });
        let req = client.build_login_request(Some(method));

        let info = req.client_info.as_ref().unwrap();
        assert_eq!(info.client_id, "test-client-id");
        assert_eq!(info.device_id, "test-device-id");
    }

    #[test]
    fn request_has_stored_credential() {
        let client = test_client();
        let method = Login_method::StoredCredential(spotify_proto::credentials::StoredCredential {
            username: "bob".into(),
            data: vec![10, 20, 30],
            ..Default::default()
        });
        let req = client.build_login_request(Some(method));

        assert!(req.has_stored_credential());
        assert_eq!(req.stored_credential().username, "bob");
        assert_eq!(req.stored_credential().data, vec![10, 20, 30]);
    }

    #[test]
    fn request_has_flow_id_unknown_field() {
        let client = test_client();
        let req = client.build_login_request(None);

        // Field 4 should be present as an unknown field (flow_id UUID).
        // Round-trip through serialisation so we can inspect raw bytes.
        let bytes = req.write_to_bytes().unwrap();
        let parsed = spotify_proto::login5::LoginRequest::parse_from_bytes(&bytes).unwrap();
        let unknowns = parsed.special_fields.unknown_fields();

        let mut found = false;
        for (field_num, value) in unknowns.iter() {
            if field_num == 4
                && let protobuf::UnknownValueRef::LengthDelimited(data) = value
            {
                let uuid_str = String::from_utf8(data.to_vec()).unwrap();
                assert!(
                    Uuid::parse_str(&uuid_str).is_ok(),
                    "field 4 should be a valid UUID, got: {uuid_str}"
                );
                found = true;
            }
        }
        assert!(found, "should have field 4 as a LengthDelimited UUID");
    }

    #[test]
    fn request_serialises_to_valid_protobuf() {
        let client = test_client();
        let method = Login_method::StoredCredential(spotify_proto::credentials::StoredCredential {
            username: "user".into(),
            data: vec![42],
            ..Default::default()
        });
        let req = client.build_login_request(Some(method));
        let bytes = req.write_to_bytes().unwrap();

        // Should round-trip.
        let parsed = spotify_proto::login5::LoginRequest::parse_from_bytes(&bytes).unwrap();
        assert_eq!(parsed.client_info.client_id, "test-client-id");
        assert!(parsed.has_stored_credential());
    }

    // -----------------------------------------------------------------------
    // Response parsing
    // -----------------------------------------------------------------------

    #[test]
    fn parse_ok_response() {
        let ok = spotify_proto::login5::LoginOk {
            username: "alice".into(),
            access_token: "tok-secret-123".into(),
            stored_credential: vec![99, 100],
            access_token_expires_in: 3600,
            ..Default::default()
        };
        let mut resp = spotify_proto::login5::LoginResponse::new();
        resp.set_ok(ok);

        let bytes = resp.write_to_bytes().unwrap();
        let parsed = spotify_proto::login5::LoginResponse::parse_from_bytes(&bytes).unwrap();

        assert!(parsed.has_ok());
        assert_eq!(parsed.ok().access_token, "tok-secret-123");
        assert_eq!(parsed.ok().access_token_expires_in, 3600);
        assert_eq!(parsed.ok().stored_credential, vec![99, 100]);
    }

    #[test]
    fn parse_error_response() {
        use spotify_proto::login5::LoginError;

        let mut resp = spotify_proto::login5::LoginResponse::new();
        resp.set_error(LoginError::INVALID_CREDENTIALS);

        let bytes = resp.write_to_bytes().unwrap();
        let parsed = spotify_proto::login5::LoginResponse::parse_from_bytes(&bytes).unwrap();

        assert!(parsed.has_error());
        assert_eq!(parsed.error(), LoginError::INVALID_CREDENTIALS);
    }

    #[test]
    fn parse_challenges_response() {
        use spotify_proto::hashcash::HashcashChallenge;
        use spotify_proto::login5::{Challenge, Challenges};

        let mut hc = HashcashChallenge::new();
        hc.prefix = b"test-prefix".to_vec();
        hc.length = 10;

        let mut challenge = Challenge::new();
        challenge.set_hashcash(hc);

        let mut challenges = Challenges::new();
        challenges.challenges.push(challenge);

        let mut resp = spotify_proto::login5::LoginResponse::new();
        resp.login_context = b"ctx-data".to_vec();
        resp.set_challenges(challenges);

        let bytes = resp.write_to_bytes().unwrap();
        let parsed = spotify_proto::login5::LoginResponse::parse_from_bytes(&bytes).unwrap();

        assert!(parsed.has_challenges());
        let ch = &parsed.challenges().challenges;
        assert_eq!(ch.len(), 1);
        assert!(ch[0].has_hashcash());
        assert_eq!(ch[0].hashcash().prefix, b"test-prefix");
        assert_eq!(ch[0].hashcash().length, 10);
        assert_eq!(parsed.login_context, b"ctx-data");
    }

    // -----------------------------------------------------------------------
    // Challenge solving
    // -----------------------------------------------------------------------

    #[test]
    fn solve_hashcash_challenge() {
        use spotify_proto::hashcash::HashcashChallenge;
        use spotify_proto::login5::{Challenge, Challenges};

        let mut hc = HashcashChallenge::new();
        hc.prefix = b"challenge-prefix".to_vec();
        hc.length = 6; // Low difficulty for fast test.

        let mut challenge = Challenge::new();
        challenge.set_hashcash(hc);

        let mut challenges = Challenges::new();
        challenges.challenges.push(challenge);

        let mut resp = spotify_proto::login5::LoginResponse::new();
        resp.login_context = b"login-ctx".to_vec();
        resp.set_challenges(challenges);

        let mut req = spotify_proto::login5::LoginRequest::new();
        Login5::solve_challenges(&mut req, &resp).unwrap();

        // The request should now have challenge solutions.
        assert!(req.challenge_solutions.is_some());
        let solutions = &req.challenge_solutions.solutions;
        assert_eq!(solutions.len(), 1);
        assert!(solutions[0].has_hashcash());
        assert!(!solutions[0].hashcash().suffix.is_empty());

        // login_context should be copied from the response.
        assert_eq!(req.login_context, b"login-ctx");
    }

    #[test]
    fn solve_code_challenge_returns_error() {
        use spotify_proto::code::CodeChallenge;
        use spotify_proto::login5::{Challenge, Challenges};

        let mut challenge = Challenge::new();
        challenge.set_code(CodeChallenge::new());

        let mut challenges = Challenges::new();
        challenges.challenges.push(challenge);

        let mut resp = spotify_proto::login5::LoginResponse::new();
        resp.set_challenges(challenges);

        let mut req = spotify_proto::login5::LoginRequest::new();
        let result = Login5::solve_challenges(&mut req, &resp);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // DPoP integration
    // -----------------------------------------------------------------------

    #[test]
    fn client_with_dpop_key_debug() {
        let client = test_client_with_dpop();
        let dbg = format!("{client:?}");
        assert!(dbg.contains("has_dpop_key: true"));
    }

    #[test]
    fn client_without_dpop_key_debug() {
        let client = test_client();
        let dbg = format!("{client:?}");
        assert!(dbg.contains("has_dpop_key: false"));
    }

    // -----------------------------------------------------------------------
    // Login5Token
    // -----------------------------------------------------------------------

    #[test]
    fn token_debug_redacts_access_token() {
        let token = Login5Token {
            access_token: "super-secret-token".into(),
            expires_in: Duration::from_secs(3600),
            stored_credential: vec![1, 2, 3],
            obtained_at: Instant::now(),
        };
        let dbg = format!("{token:?}");
        assert!(dbg.contains("REDACTED"));
        assert!(!dbg.contains("super-secret-token"));
    }

    #[test]
    fn token_is_expired_fresh() {
        let token = Login5Token {
            access_token: "tok".into(),
            expires_in: Duration::from_secs(3600),
            stored_credential: vec![],
            obtained_at: Instant::now(),
        };
        assert!(!token.is_expired());
    }

    #[test]
    fn token_is_expired_within_safety_margin() {
        let token = Login5Token {
            access_token: "tok".into(),
            expires_in: Duration::from_secs(30), // Within 60s margin.
            stored_credential: vec![],
            obtained_at: Instant::now(),
        };
        assert!(token.is_expired());
    }

    #[test]
    fn token_is_expired_zero_ttl() {
        let token = Login5Token {
            access_token: "tok".into(),
            expires_in: Duration::from_secs(0),
            stored_credential: vec![],
            obtained_at: Instant::now(),
        };
        assert!(token.is_expired());
    }

    #[test]
    fn token_access_token_getter() {
        let token = Login5Token {
            access_token: "my-token".into(),
            expires_in: Duration::from_secs(3600),
            stored_credential: vec![],
            obtained_at: Instant::now(),
        };
        assert_eq!(token.access_token(), "my-token");
    }
}
