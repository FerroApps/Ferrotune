//! Client-token acquisition from `clienttoken.spotify.com`.
//!
//! The client token is a bearer credential Spotify requires alongside the
//! session access-token on spclient / pathfinder endpoints. The server issues
//! it after a challenge-response exchange. The official desktop client handles
//! three challenge types:
//!
//! | Type | Support |
//! |------|---------|
//! | `HASH_CASH` | Fully implemented (same algorithm librespot uses) |
//! | `CLIENT_SECRET_HMAC` | Requires a shared secret from the binary |
//! | `EVALUATE_JS` | Not supported (would need a JS engine) |

use std::time::{Duration, Instant};

use protobuf::{EnumOrUnknown, Message};
use sha2::{Digest, Sha256};

use crate::error::{ProtocolError, Result};

const CLIENT_TOKEN_URL: &str = "https://clienttoken.spotify.com/v1/clienttoken";

// ---------------------------------------------------------------------------
// ClientToken
// ---------------------------------------------------------------------------

/// A cached client token with its expiry.
///
/// Debug intentionally redacts the token value.
#[derive(Clone)]
pub struct ClientToken {
    token: String,
    expires_at: Instant,
}

impl ClientToken {
    /// Create a new `ClientToken` that expires at `expires_at`.
    pub fn new(token: String, expires_at: Instant) -> Self {
        Self { token, expires_at }
    }

    /// Whether the token is still usable (with a 60-second safety margin).
    pub fn is_fresh(&self) -> bool {
        Instant::now() + Duration::from_secs(60) < self.expires_at
    }

    /// The raw token string.
    pub fn token(&self) -> &str {
        &self.token
    }
}

impl std::fmt::Debug for ClientToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientToken")
            .field("token", &"[REDACTED]")
            .field("is_fresh", &self.is_fresh())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// HashCash solver
// ---------------------------------------------------------------------------

/// SHA-256 hash-cash solver (the algorithm Spotify uses).
///
/// Iterates a hex counter until `SHA256(prefix || counter)` has at least
/// `target_bits` leading zero bits.
pub fn solve_hash_cash(prefix: &[u8], target_bits: u32) -> String {
    let mut counter: u64 = 0;
    loop {
        let suffix = format!("{counter:x}");
        let mut hasher = Sha256::new();
        hasher.update(prefix);
        hasher.update(suffix.as_bytes());
        let hash = hasher.finalize();
        if leading_zero_bits(&hash) >= target_bits as usize {
            return suffix;
        }
        counter = counter.wrapping_add(1);
    }
}

/// Count leading zero bits in a byte slice.
fn leading_zero_bits(data: &[u8]) -> usize {
    let mut bits = 0;
    for &byte in data {
        if byte == 0 {
            bits += 8;
        } else {
            bits += byte.leading_zeros() as usize;
            break;
        }
    }
    bits
}

// ---------------------------------------------------------------------------
// Fetch
// ---------------------------------------------------------------------------

/// Acquire a client token, handling hash-cash challenges.
///
/// The protocol:
/// 1. POST to `/v1/clienttoken` with protobuf `ClientTokenRequest`
/// 2. Server may respond with challenges (HashCash, etc.)
/// 3. Solve challenge, re-POST with solution
/// 4. Server returns `granted_token`
pub async fn fetch_client_token(
    http: &reqwest::Client,
    client_id: &str,
    device_id: &str,
    client_version: &str,
) -> Result<ClientToken> {
    use spotify_proto::clienttoken_http::*;
    use spotify_proto::connectivity::*;

    let mut platform_data = PlatformSpecificData::new();
    let mut linux = NativeDesktopLinuxData::new();
    if let Ok(info) = sys_info() {
        linux.system_name = info.sysname;
        linux.system_release = info.release;
        linux.system_version = info.version;
        linux.hardware = info.machine;
    }
    platform_data.set_desktop_linux(linux);

    let mut connectivity = ConnectivitySdkData::new();
    connectivity.platform_specific_data = Some(platform_data).into();
    connectivity.device_id = device_id.to_owned();

    let mut data = ClientDataRequest::new();
    data.client_version = client_version.to_owned();
    data.client_id = client_id.to_owned();
    data.set_connectivity_sdk_data(connectivity);

    let mut wrapper = ClientTokenRequest::new();
    wrapper.request_type = EnumOrUnknown::new(ClientTokenRequestType::REQUEST_CLIENT_DATA_REQUEST);
    wrapper.set_client_data(data);

    let body = wrapper
        .write_to_bytes()
        .map_err(|e| ProtocolError::Token(format!("client-token request encode: {e}")))?;

    let mut resp = post_proto(http, &body).await?;

    match resp.response_type.enum_value_or_default() {
        ClientTokenResponseType::RESPONSE_GRANTED_TOKEN_RESPONSE => {
            Ok(granted(resp.take_granted_token()))
        }
        ClientTokenResponseType::RESPONSE_CHALLENGES_RESPONSE => {
            let challenges = resp.take_challenges();
            solve_challenges(http, challenges).await
        }
        _ => Err(ProtocolError::Token(
            "unexpected client-token response type".into(),
        )),
    }
}

async fn solve_challenges(
    http: &reqwest::Client,
    challenges: spotify_proto::clienttoken_http::ChallengesResponse,
) -> Result<ClientToken> {
    use spotify_proto::clienttoken_http::*;

    let mut answers: Vec<ChallengeAnswer> = Vec::new();

    for challenge in &challenges.challenges {
        let ctype = challenge.type_.enum_value_or_default();
        match ctype {
            ChallengeType::CHALLENGE_HASH_CASH => {
                let params = challenge.evaluate_hashcash_parameters();
                tracing::debug!(
                    prefix_len = params.prefix.len(),
                    difficulty = params.length,
                    "solving hash-cash challenge"
                );
                let suffix = solve_hash_cash(params.prefix.as_bytes(), params.length as u32);
                let mut ans = ChallengeAnswer::new();
                ans.ChallengeType = EnumOrUnknown::new(ChallengeType::CHALLENGE_HASH_CASH);
                let mut hc = HashCashAnswer::new();
                hc.suffix = suffix;
                ans.set_hash_cash(hc);
                answers.push(ans);
            }
            ChallengeType::CHALLENGE_EVALUATE_JS => {
                return Err(ProtocolError::Token(
                    "EVALUATE_JS client-token challenge not supported".into(),
                ));
            }
            ChallengeType::CHALLENGE_CLIENT_SECRET_HMAC => {
                return Err(ProtocolError::Token(
                    "CLIENT_SECRET_HMAC challenge requires a shared secret".into(),
                ));
            }
            _ => {
                return Err(ProtocolError::Token(format!(
                    "unknown client-token challenge type: {ctype:?}"
                )));
            }
        }
    }

    let mut answers_msg = ChallengeAnswersRequest::new();
    answers_msg.state = challenges.state.clone();
    answers_msg.answers = answers;

    let mut wrapper = ClientTokenRequest::new();
    wrapper.request_type =
        EnumOrUnknown::new(ClientTokenRequestType::REQUEST_CHALLENGE_ANSWERS_REQUEST);
    wrapper.set_challenge_answers(answers_msg);

    let body = wrapper
        .write_to_bytes()
        .map_err(|e| ProtocolError::Token(format!("challenge-answer encode: {e}")))?;

    let mut resp = post_proto(http, &body).await?;

    match resp.response_type.enum_value_or_default() {
        ClientTokenResponseType::RESPONSE_GRANTED_TOKEN_RESPONSE => {
            Ok(granted(resp.take_granted_token()))
        }
        _ => Err(ProtocolError::Token("challenge answer not accepted".into())),
    }
}

fn granted(token: spotify_proto::clienttoken_http::GrantedTokenResponse) -> ClientToken {
    let ttl = Duration::from_secs(token.refresh_after_seconds.max(0) as u64);
    ClientToken::new(token.token, Instant::now() + ttl)
}

async fn post_proto(
    http: &reqwest::Client,
    body: &[u8],
) -> Result<spotify_proto::clienttoken_http::ClientTokenResponse> {
    use spotify_proto::clienttoken_http::*;

    let resp_bytes = http
        .post(CLIENT_TOKEN_URL)
        .header("Accept", "application/x-protobuf")
        .header("Content-Type", "application/x-protobuf")
        .body(body.to_vec())
        .send()
        .await
        .map_err(|e| ProtocolError::Http(format!("client-token request: {e}")))?
        .bytes()
        .await
        .map_err(|e| ProtocolError::Http(format!("client-token response body: {e}")))?;

    ClientTokenResponse::parse_from_bytes(&resp_bytes)
        .map_err(|e| ProtocolError::Token(format!("client-token response decode: {e}")))
}

// ---------------------------------------------------------------------------
// System info (for ConnectivitySdkData)
// ---------------------------------------------------------------------------

struct SysInfo {
    sysname: String,
    release: String,
    version: String,
    machine: String,
}

fn sys_info() -> std::result::Result<SysInfo, ()> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;
        unsafe {
            let mut buf: libc::utsname = std::mem::zeroed();
            if libc::uname(&mut buf) != 0 {
                return Err(());
            }
            Ok(SysInfo {
                sysname: CStr::from_ptr(buf.sysname.as_ptr())
                    .to_string_lossy()
                    .into(),
                release: CStr::from_ptr(buf.release.as_ptr())
                    .to_string_lossy()
                    .into(),
                version: CStr::from_ptr(buf.version.as_ptr())
                    .to_string_lossy()
                    .into(),
                machine: CStr::from_ptr(buf.machine.as_ptr())
                    .to_string_lossy()
                    .into(),
            })
        }
    }
    #[cfg(not(unix))]
    {
        Ok(SysInfo {
            sysname: "Windows".into(),
            release: "10".into(),
            version: String::new(),
            machine: std::env::consts::ARCH.into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashcash_solver_finds_suffix() {
        // Very small difficulty so the test finishes quickly.
        let suffix = solve_hash_cash(b"prefix", 6);
        let mut h = Sha256::new();
        h.update(b"prefix");
        h.update(suffix.as_bytes());
        assert!(leading_zero_bits(&h.finalize()) >= 6);
    }

    #[test]
    fn hashcash_solver_difficulty_8() {
        let suffix = solve_hash_cash(b"test-prefix-abc", 8);
        let mut h = Sha256::new();
        h.update(b"test-prefix-abc");
        h.update(suffix.as_bytes());
        assert!(leading_zero_bits(&h.finalize()) >= 8);
    }

    #[test]
    fn leading_zero_bits_counts_correctly() {
        assert_eq!(leading_zero_bits(&[0, 0, 0b0001_1111]), 16 + 3);
        assert_eq!(leading_zero_bits(&[0xff]), 0);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
        assert_eq!(leading_zero_bits(&[0b0000_0001]), 7);
        assert_eq!(leading_zero_bits(&[0b0100_0000]), 1);
        assert_eq!(leading_zero_bits(&[]), 0);
    }

    #[test]
    fn client_token_freshness() {
        let fresh = ClientToken::new("tok".into(), Instant::now() + Duration::from_secs(300));
        assert!(fresh.is_fresh());
        assert_eq!(fresh.token(), "tok");

        let expired = ClientToken::new("tok".into(), Instant::now() - Duration::from_secs(1));
        assert!(!expired.is_fresh());

        // Expires within the 60-second safety margin.
        let borderline = ClientToken::new("tok".into(), Instant::now() + Duration::from_secs(30));
        assert!(!borderline.is_fresh());
    }

    #[test]
    fn client_token_debug_redacts() {
        let ct = ClientToken::new(
            "secret-value".into(),
            Instant::now() + Duration::from_secs(60),
        );
        let dbg = format!("{ct:?}");
        assert!(dbg.contains("REDACTED"));
        assert!(!dbg.contains("secret-value"));
    }
}
