//! Standalone client-token acquisition, independent of librespot's spclient.
//!
//! The client token is a bearer credential Spotify requires alongside the
//! session access-token on spclient / pathfinder endpoints. The server
//! issues it after a challenge–response exchange at
//! `clienttoken.spotify.com`. The official desktop client handles three
//! challenge types:
//!
//! | Type | Support |
//! |------|---------|
//! | `HASH_CASH` | Fully implemented (same algorithm librespot uses) |
//! | `CLIENT_SECRET_HMAC` | Implemented (requires a shared secret) |
//! | `EVALUATE_JS` | Not supported (would need a JS engine) |

use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use hmac::{Hmac, Mac};
use protobuf::{EnumOrUnknown, Message};
use sha2::Sha256;

use crate::error::{Error, Result};

type HmacSha256 = Hmac<Sha256>;

const CLIENT_TOKEN_URL: &str = "https://clienttoken.spotify.com/v1/clienttoken";

#[derive(Debug, Clone)]
pub struct ClientToken {
    pub token: String,
    pub expires_at: Instant,
}

impl ClientToken {
    pub fn is_fresh(&self) -> bool {
        Instant::now() + Duration::from_secs(60) < self.expires_at
    }
}

/// Acquire a client token, handling hash-cash and HMAC challenges.
pub async fn fetch_client_token(
    http: &reqwest::Client,
    client_id: &str,
    device_id: &str,
    client_version: &str,
    client_secret: Option<&[u8]>,
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
        .map_err(|e| Error::Protocol(format!("client-token request: {e}")))?;

    let mut resp = post_proto(http, &body).await?;

    match resp.response_type.enum_value_or_default() {
        ClientTokenResponseType::RESPONSE_GRANTED_TOKEN_RESPONSE => {
            Ok(granted(resp.take_granted_token()))
        }
        ClientTokenResponseType::RESPONSE_CHALLENGES_RESPONSE => {
            let challenges = resp.take_challenges();
            solve_challenges(http, client_secret, challenges).await
        }
        _ => Err(Error::Protocol(
            "unexpected client-token response type".into(),
        )),
    }
}

async fn solve_challenges(
    http: &reqwest::Client,
    client_secret: Option<&[u8]>,
    challenges: spotify_proto::clienttoken_http::ChallengesResponse,
) -> Result<ClientToken> {
    use spotify_proto::clienttoken_http::*;

    let mut answers: Vec<ChallengeAnswer> = Vec::new();

    for challenge in &challenges.challenges {
        let ctype = challenge.type_.enum_value_or_default();
        match ctype {
            ChallengeType::CHALLENGE_HASH_CASH => {
                let params = challenge.evaluate_hashcash_parameters();
                let suffix = solve_hash_cash(&params.prefix, params.length as u32);
                let mut ans = ChallengeAnswer::new();
                ans.ChallengeType = EnumOrUnknown::new(ChallengeType::CHALLENGE_HASH_CASH);
                let mut hc = HashCashAnswer::new();
                hc.suffix = suffix;
                ans.set_hash_cash(hc);
                answers.push(ans);
            }
            ChallengeType::CHALLENGE_CLIENT_SECRET_HMAC => {
                let secret = client_secret.ok_or_else(|| {
                    Error::Protocol("CLIENT_SECRET_HMAC challenge but no secret configured".into())
                })?;
                let params = challenge.client_secret_parameters();
                let mut mac = HmacSha256::new_from_slice(secret)
                    .map_err(|_| Error::Protocol("invalid HMAC key".into()))?;
                mac.update(params.salt.as_bytes());
                let hmac_result = B64.encode(mac.finalize().into_bytes());
                let mut ans = ChallengeAnswer::new();
                ans.ChallengeType = EnumOrUnknown::new(ChallengeType::CHALLENGE_CLIENT_SECRET_HMAC);
                let mut cs = ClientSecretHMACAnswer::new();
                cs.hmac = hmac_result;
                ans.set_client_secret(cs);
                answers.push(ans);
            }
            ChallengeType::CHALLENGE_EVALUATE_JS => {
                return Err(Error::Protocol(
                    "EVALUATE_JS client-token challenge not supported".into(),
                ));
            }
            _ => {
                return Err(Error::Protocol(format!(
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
        .map_err(|e| Error::Protocol(format!("challenge-answer request: {e}")))?;

    let mut resp = post_proto(http, &body).await?;

    match resp.response_type.enum_value_or_default() {
        ClientTokenResponseType::RESPONSE_GRANTED_TOKEN_RESPONSE => {
            Ok(granted(resp.take_granted_token()))
        }
        _ => Err(Error::Protocol("challenge answer not accepted".into())),
    }
}

fn granted(token: spotify_proto::clienttoken_http::GrantedTokenResponse) -> ClientToken {
    let ttl = Duration::from_secs(token.refresh_after_seconds.max(0) as u64);
    ClientToken {
        token: token.token,
        expires_at: Instant::now() + ttl,
    }
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
        .await?
        .bytes()
        .await?;
    ClientTokenResponse::parse_from_bytes(&resp_bytes)
        .map_err(|e| Error::Protocol(format!("client-token response: {e}")))
}

/// SHA-256 hash-cash solver (the algorithm Spotify uses).
fn solve_hash_cash(prefix: &str, length: u32) -> String {
    use sha2::{Digest, Sha256};
    let target_bits = length as usize;
    let prefix_bytes = prefix.as_bytes();
    let mut counter: u64 = 0;
    loop {
        let suffix = format!("{counter:x}");
        let mut hasher = Sha256::new();
        hasher.update(prefix_bytes);
        hasher.update(suffix.as_bytes());
        let hash = hasher.finalize();
        if leading_zero_bits(&hash) >= target_bits {
            return suffix;
        }
        counter = counter.wrapping_add(1);
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashcash_solver_finds_suffix() {
        // Very small difficulty so the test finishes quickly.
        let suffix = solve_hash_cash("prefix", 6);
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(b"prefix");
        h.update(suffix.as_bytes());
        assert!(leading_zero_bits(&h.finalize()) >= 6);
    }

    #[test]
    fn leading_zero_bits_counts_whole_zero_bytes() {
        assert_eq!(leading_zero_bits(&[0, 0, 0b0001_1111]), 16 + 3);
        assert_eq!(leading_zero_bits(&[0xff]), 0);
        assert_eq!(leading_zero_bits(&[0, 0]), 16);
    }
}
