//! AP (access-point) resolution — discover the Spotify AP, dealer and spclient
//! endpoints via `apresolve.spotify.com`.

use std::collections::VecDeque;

use serde::Deserialize;

use crate::error::ProtocolError;

/// A `(host, port)` pair for an AP endpoint.
pub type SocketAddress = (String, u16);

/// The resolved endpoints for all three endpoint families.
#[derive(Debug, Default)]
pub struct AccessPoints {
    pub accesspoint: VecDeque<SocketAddress>,
    pub dealer: VecDeque<SocketAddress>,
    pub spclient: VecDeque<SocketAddress>,
}

impl AccessPoints {
    /// True when any of the three endpoint lists is empty.
    pub fn is_any_empty(&self) -> bool {
        self.accesspoint.is_empty() || self.dealer.is_empty() || self.spclient.is_empty()
    }
}

/// Raw JSON payload returned by `apresolve.spotify.com`.
#[derive(Deserialize, Default, Debug)]
pub struct ApResolveData {
    #[serde(default)]
    pub accesspoint: Vec<String>,
    #[serde(default)]
    pub dealer: Vec<String>,
    #[serde(default)]
    pub spclient: Vec<String>,
}

impl ApResolveData {
    /// Hard-coded fallback addresses used when AP resolution fails.
    pub fn fallback() -> Self {
        Self {
            accesspoint: vec!["ap.spotify.com:443".into()],
            dealer: vec!["dealer.spotify.com:443".into()],
            spclient: vec!["spclient.wg.spotify.com:443".into()],
        }
    }
}

/// The URL used to resolve access points.
pub const APRESOLVE_URL: &str =
    "https://apresolve.spotify.com/?type=accesspoint&type=dealer&type=spclient";

/// Parse a `"host:port"` string into a [`SocketAddress`].
pub fn parse_ap_string(s: &str) -> Option<SocketAddress> {
    let mut split = s.rsplitn(2, ':');
    let port: u16 = split.next()?.parse().ok()?;
    let host = split.next()?.to_owned();
    Some((host, port))
}

/// Convert a list of `"host:port"` strings, optionally filtering to a
/// specific port.
pub fn parse_ap_list(list: Vec<String>, filter_port: Option<u16>) -> VecDeque<SocketAddress> {
    list.into_iter()
        .filter_map(|s| {
            let addr = parse_ap_string(&s)?;
            match filter_port {
                Some(fp) if fp != addr.1 => None,
                _ => Some(addr),
            }
        })
        .collect()
}

/// Convert raw resolve data to [`AccessPoints`], optionally filtering to a
/// specific port.
pub fn resolve_to_access_points(data: ApResolveData, filter_port: Option<u16>) -> AccessPoints {
    AccessPoints {
        accesspoint: parse_ap_list(data.accesspoint, filter_port),
        dealer: parse_ap_list(data.dealer, filter_port),
        spclient: parse_ap_list(data.spclient, filter_port),
    }
}

/// Resolve access points by making an HTTP GET to `apresolve.spotify.com`.
///
/// The caller is responsible for the HTTP transport (reqwest, hyper, etc.).
/// This function takes the raw JSON response bytes and returns the parsed
/// data.
pub fn parse_apresolve_response(body: &[u8]) -> Result<ApResolveData, ProtocolError> {
    serde_json::from_slice(body).map_err(|e| ProtocolError::ApResolve(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ap_string_valid() {
        let addr = parse_ap_string("ap-gew4.spotify.com:4070");
        assert_eq!(addr, Some(("ap-gew4.spotify.com".to_owned(), 4070)));
    }

    #[test]
    fn parse_ap_string_443() {
        let addr = parse_ap_string("ap.spotify.com:443");
        assert_eq!(addr, Some(("ap.spotify.com".to_owned(), 443)));
    }

    #[test]
    fn parse_ap_string_invalid() {
        assert!(parse_ap_string("no-port").is_none());
        assert!(parse_ap_string("host:notaport").is_none());
    }

    #[test]
    fn parse_ap_list_with_filter() {
        let list = vec![
            "a.spotify.com:4070".into(),
            "b.spotify.com:443".into(),
            "c.spotify.com:80".into(),
        ];
        let result = parse_ap_list(list, Some(443));
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].1, 443);
    }

    #[test]
    fn parse_apresolve_json() {
        let json = br#"{
            "accesspoint": ["ap1.spotify.com:4070", "ap2.spotify.com:443"],
            "dealer": ["dealer1.spotify.com:443"],
            "spclient": ["spclient1.wg.spotify.com:443"]
        }"#;
        let data = parse_apresolve_response(json).unwrap();
        assert_eq!(data.accesspoint.len(), 2);
        assert_eq!(data.dealer.len(), 1);
        assert_eq!(data.spclient.len(), 1);
    }

    #[test]
    fn fallback_addresses() {
        let fb = ApResolveData::fallback();
        assert!(!fb.accesspoint.is_empty());
        assert!(!fb.dealer.is_empty());
        assert!(!fb.spclient.is_empty());
    }

    #[test]
    fn resolve_to_access_points_no_filter() {
        let data = ApResolveData {
            accesspoint: vec!["a:4070".into(), "b:443".into()],
            dealer: vec!["d:443".into()],
            spclient: vec!["s:443".into()],
        };
        let aps = resolve_to_access_points(data, None);
        assert_eq!(aps.accesspoint.len(), 2);
        assert!(!aps.is_any_empty());
    }
}
