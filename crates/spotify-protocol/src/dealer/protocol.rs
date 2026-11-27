//! Dealer protocol message types and JSON parsing.
//!
//! The dealer sends two kinds of WebSocket frames, both JSON text:
//!
//! * **Messages** (`type: "message"`) — pushed from the server with a URI,
//!   headers, and zero or more payloads (base64-encoded, optionally gzip).
//! * **Requests** (`type: "request"`) — the server asks the client for
//!   something; the client must reply with `{type: "reply", key, payload}`.

use std::collections::HashMap;
use std::io::Read as _;

use base64::Engine as _;
use base64::prelude::BASE64_STANDARD;
use serde::Deserialize;
use tracing::trace;

use crate::error::ProtocolError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A decoded dealer message ready for consumption.
#[derive(Debug, Clone)]
pub struct DealerMessage {
    /// The message URI (e.g., `hm://connect-state/v1/cluster`).
    pub uri: String,
    /// Headers from the message envelope.
    pub headers: HashMap<String, String>,
    /// Decoded payload.  For base64/gzip payloads this is the decoded content
    /// parsed as JSON.  When the raw bytes are not valid JSON the value is a
    /// JSON string of the lossy UTF-8 representation.
    pub payload: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Internal wire types
// ---------------------------------------------------------------------------

/// Raw WebSocket "message" envelope.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct WsEnvelopeMessage {
    #[serde(default)]
    pub headers: HashMap<String, String>,
    pub uri: String,
    #[serde(default)]
    pub payloads: Vec<serde_json::Value>,
}

/// Raw WebSocket "request" envelope.
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)] // fields are part of the wire protocol
pub(crate) struct WsEnvelopeRequest {
    #[serde(rename = "mid")]
    pub message_id: String,
    pub key: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Top-level discriminated union for dealer WebSocket JSON.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WsEnvelope {
    Message(WsEnvelopeMessage),
    Request(WsEnvelopeRequest),
}

// ---------------------------------------------------------------------------
// Payload decoding
// ---------------------------------------------------------------------------

/// Decode a single payload value.
///
/// If the value is a base64 string it is decoded (and gzip-decompressed when
/// the `Transfer-Encoding` header says `gzip`).  Already-structured JSON is
/// returned as-is.
pub(crate) fn decode_payload(
    value: &serde_json::Value,
    headers: &HashMap<String, String>,
) -> Result<serde_json::Value, ProtocolError> {
    match value {
        // Already structured — return as-is.
        serde_json::Value::Object(_)
        | serde_json::Value::Array(_)
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_) => Ok(value.clone()),

        serde_json::Value::Null => Ok(serde_json::Value::Null),

        serde_json::Value::String(s) => {
            let bytes = match BASE64_STANDARD.decode(s) {
                Ok(b) => b,
                Err(_) => {
                    // Not valid base64 — treat as a plain string value.
                    return Ok(value.clone());
                }
            };

            let bytes = decompress_if_gzip(headers, bytes)?;

            // Try to parse as JSON; fall back to a lossy UTF-8 string.
            match serde_json::from_slice::<serde_json::Value>(&bytes) {
                Ok(json) => Ok(json),
                Err(_) => Ok(serde_json::Value::String(
                    String::from_utf8_lossy(&bytes).into_owned(),
                )),
            }
        }
    }
}

/// Decompress `data` when the `Transfer-Encoding` header is `gzip`.
fn decompress_if_gzip(
    headers: &HashMap<String, String>,
    data: Vec<u8>,
) -> Result<Vec<u8>, ProtocolError> {
    let is_gzip = headers
        .get("Transfer-Encoding")
        .is_some_and(|v| v == "gzip");

    if !is_gzip {
        return Ok(data);
    }

    trace!("decompressing gzip payload");
    let mut decoder = flate2::read::GzDecoder::new(&data[..]);
    let mut decompressed = Vec::new();
    decoder
        .read_to_end(&mut decompressed)
        .map_err(|e| ProtocolError::Connection(format!("dealer: gzip decompress failed: {e}")))?;

    Ok(decompressed)
}

impl WsEnvelopeMessage {
    /// Convert to a public [`DealerMessage`], decoding payloads.
    pub(crate) fn into_dealer_message(self) -> Result<DealerMessage, ProtocolError> {
        let payload = if self.payloads.is_empty() {
            serde_json::Value::Null
        } else if self.payloads.len() == 1 {
            decode_payload(&self.payloads[0], &self.headers)?
        } else {
            let mut decoded = Vec::with_capacity(self.payloads.len());
            for p in &self.payloads {
                decoded.push(decode_payload(p, &self.headers)?);
            }
            serde_json::Value::Array(decoded)
        };

        Ok(DealerMessage {
            uri: self.uri,
            headers: self.headers,
            payload,
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
    fn parse_message_envelope() {
        let json = r#"{
            "type": "message",
            "uri": "hm://connect-state/v1/cluster",
            "headers": {"Content-Type": "application/json"},
            "payloads": []
        }"#;

        let envelope: WsEnvelope = serde_json::from_str(json).expect("parse envelope");
        match envelope {
            WsEnvelope::Message(msg) => {
                assert_eq!(msg.uri, "hm://connect-state/v1/cluster");
                assert_eq!(
                    msg.headers.get("Content-Type").map(String::as_str),
                    Some("application/json")
                );
                assert!(msg.payloads.is_empty());
            }
            _ => panic!("expected Message variant"),
        }
    }

    #[test]
    fn parse_request_envelope() {
        let json = r#"{
            "type": "request",
            "mid": "123",
            "key": "abc",
            "headers": {},
            "payload": {"data": "test"}
        }"#;

        let envelope: WsEnvelope = serde_json::from_str(json).expect("parse envelope");
        match envelope {
            WsEnvelope::Request(req) => {
                assert_eq!(req.message_id, "123");
                assert_eq!(req.key, "abc");
                assert_eq!(req.payload["data"], "test");
            }
            _ => panic!("expected Request variant"),
        }
    }

    #[test]
    fn decode_base64_json_payload() {
        let json_payload = r#"{"status": "ok"}"#;
        let encoded = BASE64_STANDARD.encode(json_payload.as_bytes());
        let value = serde_json::Value::String(encoded);
        let headers = HashMap::new();

        let decoded = decode_payload(&value, &headers).expect("decode");
        assert_eq!(decoded["status"], "ok");
    }

    #[test]
    fn decode_base64_text_payload() {
        // "hello, world!" base64-encoded is not valid JSON, so it falls back
        // to a string.
        let text = "hello, world!";
        let encoded = BASE64_STANDARD.encode(text.as_bytes());
        let value = serde_json::Value::String(encoded);
        let headers = HashMap::new();

        let decoded = decode_payload(&value, &headers).expect("decode");
        assert_eq!(decoded.as_str(), Some(text));
    }

    #[test]
    fn decode_json_passthrough() {
        let value = serde_json::json!({"already": "json"});
        let headers = HashMap::new();

        let decoded = decode_payload(&value, &headers).expect("decode");
        assert_eq!(decoded["already"], "json");
    }

    #[test]
    fn decode_null_passthrough() {
        let decoded = decode_payload(&serde_json::Value::Null, &HashMap::new()).expect("decode");
        assert!(decoded.is_null());
    }

    #[test]
    fn message_with_base64_payload() {
        let payload = serde_json::json!({"track": "spotify:track:123"});
        let encoded = BASE64_STANDARD.encode(payload.to_string().as_bytes());

        let msg = WsEnvelopeMessage {
            headers: HashMap::new(),
            uri: "hm://connect-state/v1/cluster".into(),
            payloads: vec![serde_json::Value::String(encoded)],
        };

        let dealer_msg = msg.into_dealer_message().expect("into_dealer_message");
        assert_eq!(dealer_msg.uri, "hm://connect-state/v1/cluster");
        assert_eq!(dealer_msg.payload["track"], "spotify:track:123");
    }

    #[test]
    fn message_with_empty_payloads() {
        let msg = WsEnvelopeMessage {
            headers: HashMap::new(),
            uri: "hm://test".into(),
            payloads: vec![],
        };

        let dealer_msg = msg.into_dealer_message().expect("into_dealer_message");
        assert!(dealer_msg.payload.is_null());
    }

    #[test]
    fn message_with_multiple_payloads() {
        let p1 = serde_json::json!({"a": 1});
        let p2 = serde_json::json!({"b": 2});

        let msg = WsEnvelopeMessage {
            headers: HashMap::new(),
            uri: "hm://multi".into(),
            payloads: vec![p1.clone(), p2.clone()],
        };

        let dealer_msg = msg.into_dealer_message().expect("into_dealer_message");
        let arr = dealer_msg.payload.as_array().expect("should be array");
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["a"], 1);
        assert_eq!(arr[1]["b"], 2);
    }

    #[test]
    fn decode_gzip_payload() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write as _;

        let json = r#"{"compressed": true}"#;
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(json.as_bytes()).expect("write");
        let compressed = encoder.finish().expect("finish");

        let encoded = BASE64_STANDARD.encode(&compressed);
        let value = serde_json::Value::String(encoded);

        let mut headers = HashMap::new();
        headers.insert("Transfer-Encoding".into(), "gzip".into());

        let decoded = decode_payload(&value, &headers).expect("decode");
        assert_eq!(decoded["compressed"], true);
    }

    #[test]
    fn non_base64_string_returned_as_is() {
        // "not!base64" is not valid base64 — should be returned unchanged.
        let value = serde_json::Value::String("not!base64".into());
        let headers = HashMap::new();

        let decoded = decode_payload(&value, &headers).expect("decode");
        assert_eq!(decoded.as_str(), Some("not!base64"));
    }
}
