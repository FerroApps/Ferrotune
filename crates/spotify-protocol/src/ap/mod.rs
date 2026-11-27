//! Spotify Access Point (AP) binary protocol.
//!
//! This module implements the full AP connection lifecycle:
//! 1. **Resolve** an access-point address via `apresolve.spotify.com`
//! 2. **Handshake** (DH key exchange + RSA verification)
//! 3. **Authenticate** with Spotify credentials
//! 4. **Codec** for Shannon-encrypted packet framing

mod codec;
mod handshake;
mod packet;
mod shannon;

pub use codec::ApCodec;
pub use handshake::handshake;
pub use packet::PacketType;
pub use shannon::Shannon;

use std::{io, time::Duration};

use futures_util::{SinkExt, StreamExt};
use num_traits::FromPrimitive;
use protobuf::Message;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_util::codec::Framed;
use tracing::debug;

use crate::version;

use spotify_proto::authentication::{APWelcome, ClientResponseEncrypted, CpuFamily, Os};
use spotify_proto::keyexchange::{APLoginFailed, ErrorCode};

/// Framed TCP transport using the Shannon-encrypted AP codec.
pub type Transport = Framed<TcpStream, ApCodec>;

// ---------------------------------------------------------------------------
// AP resolver
// ---------------------------------------------------------------------------

const AP_RESOLVE_URL: &str =
    "https://apresolve.spotify.com/?type=accesspoint&type=dealer&type=spclient";

#[derive(Debug, Deserialize)]
struct ApResolveResponse {
    #[serde(default)]
    accesspoint: Vec<String>,
    #[serde(default)]
    dealer: Vec<String>,
    #[serde(default)]
    spclient: Vec<String>,
}

/// Resolved access-point, dealer, and spclient endpoints.
#[derive(Debug, Clone)]
pub struct ApEndpoints {
    pub accesspoint: Vec<String>,
    pub dealer: Vec<String>,
    pub spclient: Vec<String>,
}

/// Resolve Spotify AP endpoints via `apresolve.spotify.com`.
pub async fn resolve() -> Result<ApEndpoints, ApError> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| ApError::Resolve(e.to_string()))?;

    let resp: ApResolveResponse = client
        .get(AP_RESOLVE_URL)
        .send()
        .await
        .map_err(|e| ApError::Resolve(e.to_string()))?
        .json()
        .await
        .map_err(|e| ApError::Resolve(e.to_string()))?;

    if resp.accesspoint.is_empty() {
        return Err(ApError::Resolve("no access points returned".to_string()));
    }

    Ok(ApEndpoints {
        accesspoint: resp.accesspoint,
        dealer: resp.dealer,
        spclient: resp.spclient,
    })
}

// ---------------------------------------------------------------------------
// Connect (TCP + handshake)
// ---------------------------------------------------------------------------

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Connect to a Spotify access point, performing the DH/RSA handshake.
///
/// `host` and `port` are from the resolver (or hard-coded fallback).
pub async fn connect(host: &str, port: u16) -> Result<Transport, ApError> {
    let addr = format!("{host}:{port}");
    let transport = tokio::time::timeout(CONNECT_TIMEOUT, async {
        let socket = TcpStream::connect(&addr).await?;
        debug!("TCP connection to AP {addr} established");
        handshake(socket).await
    })
    .await
    .map_err(|_| ApError::Timeout)??;

    Ok(transport)
}

/// Connect with retries, trying up to `max_retries` additional attempts.
pub async fn connect_with_retry(
    host: &str,
    port: u16,
    max_retries: u8,
) -> Result<Transport, ApError> {
    let mut last_err = None;
    for attempt in 0..=max_retries {
        match connect(host, port).await {
            Ok(t) => return Ok(t),
            Err(e) => {
                debug!("AP connection attempt {attempt} to {host}:{port} failed: {e}");
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or(ApError::Timeout))
}

// ---------------------------------------------------------------------------
// Authenticate
// ---------------------------------------------------------------------------

/// Credentials for Spotify authentication (matches the protobuf
/// `AuthenticationType` enum values).
#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: Option<String>,
    pub auth_type: spotify_proto::authentication::AuthenticationType,
    pub auth_data: Vec<u8>,
}

/// Reusable credentials returned after successful authentication.
#[derive(Debug, Clone)]
pub struct ReusableCredentials {
    pub username: String,
    pub auth_type: spotify_proto::authentication::AuthenticationType,
    pub auth_data: Vec<u8>,
}

/// Authenticate with the AP, returning reusable credentials on success.
pub async fn authenticate(
    transport: &mut Transport,
    credentials: Credentials,
    device_id: &str,
) -> Result<ReusableCredentials, ApError> {
    let cpu_family = match std::env::consts::ARCH {
        "blackfin" => CpuFamily::CPU_BLACKFIN,
        "arm" | "aarch64" => CpuFamily::CPU_ARM,
        "ia64" => CpuFamily::CPU_IA64,
        "mips" => CpuFamily::CPU_MIPS,
        "ppc" => CpuFamily::CPU_PPC,
        "ppc64" => CpuFamily::CPU_PPC_64,
        "sh" => CpuFamily::CPU_SH,
        "x86" => CpuFamily::CPU_X86,
        "x86_64" => CpuFamily::CPU_X86_64,
        _ => CpuFamily::CPU_UNKNOWN,
    };

    let os = match std::env::consts::OS {
        "android" => Os::OS_ANDROID,
        "freebsd" | "netbsd" | "openbsd" => Os::OS_FREEBSD,
        "ios" => Os::OS_IPHONE,
        "linux" => Os::OS_LINUX,
        "macos" => Os::OS_OSX,
        "windows" => Os::OS_WINDOWS,
        _ => Os::OS_UNKNOWN,
    };

    let mut packet = ClientResponseEncrypted::new();
    if let Some(ref username) = credentials.username {
        packet
            .login_credentials
            .mut_or_insert_default()
            .set_username(username.clone());
    }
    packet
        .login_credentials
        .mut_or_insert_default()
        .set_typ(credentials.auth_type);
    packet
        .login_credentials
        .mut_or_insert_default()
        .set_auth_data(credentials.auth_data);
    packet
        .system_info
        .mut_or_insert_default()
        .set_cpu_family(cpu_family);
    packet.system_info.mut_or_insert_default().set_os(os);
    packet
        .system_info
        .mut_or_insert_default()
        .set_system_information_string(format!(
            "spotify_client_{}_{}",
            version::PLATFORM,
            version::NUMERIC_VERSION
        ));
    packet
        .system_info
        .mut_or_insert_default()
        .set_device_id(device_id.to_string());
    packet.set_version_string(version::USER_AGENT.to_string());

    let cmd = PacketType::Login;
    let data = packet
        .write_to_bytes()
        .map_err(|e| ApError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;

    debug!("Authenticating with AP using {:?}", credentials.auth_type);
    transport
        .send((cmd as u8, data))
        .await
        .map_err(ApError::Io)?;

    let (cmd, data) = transport
        .next()
        .await
        .ok_or(ApError::NoResponse)?
        .map_err(ApError::Io)?;

    match PacketType::from_u8(cmd) {
        Some(PacketType::APWelcome) => {
            let welcome = APWelcome::parse_from_bytes(data.as_ref())
                .map_err(|e| ApError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
            Ok(ReusableCredentials {
                username: welcome.canonical_username().to_owned(),
                auth_type: welcome.reusable_auth_credentials_type(),
                auth_data: welcome.reusable_auth_credentials().to_owned(),
            })
        }
        Some(PacketType::AuthFailure) => {
            let failure = APLoginFailed::parse_from_bytes(data.as_ref())
                .map_err(|e| ApError::Io(io::Error::new(io::ErrorKind::InvalidData, e)))?;
            Err(ApError::AuthFailed(failure.error_code()))
        }
        _ => Err(ApError::UnexpectedPacket(cmd)),
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from AP connection, handshake, and authentication.
#[derive(Debug, thiserror::Error)]
pub enum ApError {
    #[error("AP resolve failed: {0}")]
    Resolve(String),

    #[error("connection timed out")]
    Timeout,

    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("authentication failed: {}", login_error_message(.0))]
    AuthFailed(ErrorCode),

    #[error("unexpected packet type {0:#04x}")]
    UnexpectedPacket(u8),

    #[error("transport returned no data")]
    NoResponse,
}

fn login_error_message(code: &ErrorCode) -> &'static str {
    match code {
        ErrorCode::ProtocolError => "Protocol error",
        ErrorCode::TryAnotherAP => "Try another access point",
        ErrorCode::BadConnectionId => "Bad connection ID",
        ErrorCode::TravelRestriction => "Travel restriction",
        ErrorCode::PremiumAccountRequired => "Premium account required",
        ErrorCode::BadCredentials => "Bad credentials",
        ErrorCode::CouldNotValidateCredentials => "Could not validate credentials",
        ErrorCode::AccountExists => "Account exists",
        ErrorCode::ExtraVerificationRequired => "Extra verification required",
        ErrorCode::InvalidAppKey => "Invalid app key",
        ErrorCode::ApplicationBanned => "Application banned",
    }
}

// ---------------------------------------------------------------------------
// Helpers: parse "host:port" from resolver
// ---------------------------------------------------------------------------

/// Parse a `"host:port"` string into its components.
pub fn parse_ap_address(addr: &str) -> Option<(&str, u16)> {
    let (host, port_str) = addr.rsplit_once(':')?;
    let port = port_str.parse().ok()?;
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ap_address_valid() {
        let (host, port) = parse_ap_address("ap-gew4.spotify.com:4070").unwrap();
        assert_eq!(host, "ap-gew4.spotify.com");
        assert_eq!(port, 4070);
    }

    #[test]
    fn parse_ap_address_invalid() {
        assert!(parse_ap_address("no-port-here").is_none());
        assert!(parse_ap_address("host:notanumber").is_none());
        assert!(parse_ap_address("").is_none());
    }

    #[test]
    fn login_error_messages_are_non_empty() {
        // Verify all error codes produce a non-empty message.
        let codes = [
            ErrorCode::ProtocolError,
            ErrorCode::TryAnotherAP,
            ErrorCode::BadConnectionId,
            ErrorCode::TravelRestriction,
            ErrorCode::PremiumAccountRequired,
            ErrorCode::BadCredentials,
            ErrorCode::CouldNotValidateCredentials,
            ErrorCode::AccountExists,
            ErrorCode::ExtraVerificationRequired,
            ErrorCode::InvalidAppKey,
            ErrorCode::ApplicationBanned,
        ];
        for code in codes {
            assert!(!login_error_message(&code).is_empty());
        }
    }
}
