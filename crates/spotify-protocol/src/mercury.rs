//! Mercury protocol — request/response + pub/sub multiplexed over the
//! Shannon-encrypted AP connection.
//!
//! Mercury uses packet types 0xb2–0xb5:
//! - `MercuryReq` (0xb2): GET / SEND request-response
//! - `MercurySub` (0xb3): subscribe to a URI pattern
//! - `MercuryUnsub` (0xb4): unsubscribe
//! - `MercuryEvent` (0xb5): server push to subscriptions
//!
//! # Wire format
//!
//! ```text
//! [2-byte seq_len][seq bytes][1-byte flags][2-byte part_count][parts...]
//! ```
//!
//! Each part is `[2-byte BE length][data bytes]`. The first part is always a
//! protobuf `mercury::Header`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use protobuf::Message as _;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, trace, warn};

use crate::error::{ProtocolError, Result};
use crate::packet::PacketType;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Flag: response is complete (final frame).
const FLAG_FINAL: u8 = 0x01;

/// Flag: partial frame — more data to follow for this part.
const FLAG_PARTIAL: u8 = 0x02;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A Mercury method (the string that goes into the protobuf `Header.method`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MercuryMethod {
    Get,
    Send,
    Sub,
    Unsub,
}

impl MercuryMethod {
    /// The AP packet type for this method.
    pub fn packet_type(self) -> PacketType {
        match self {
            Self::Get | Self::Send => PacketType::MercuryReq,
            Self::Sub => PacketType::MercurySub,
            Self::Unsub => PacketType::MercuryUnsub,
        }
    }

    /// The wire-format method string.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Send => "SEND",
            Self::Sub => "SUB",
            Self::Unsub => "UNSUB",
        }
    }
}

impl std::fmt::Display for MercuryMethod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A decoded Mercury response.
#[derive(Debug, Clone)]
pub struct MercuryResponse {
    /// The URI from the response header.
    pub uri: String,
    /// HTTP-style status code from the response header.
    pub status_code: i32,
    /// Content type from the response header, if present.
    pub content_type: Option<String>,
    /// Payload parts (all parts after the header).
    pub payload: Vec<Vec<u8>>,
}

/// Errors specific to the Mercury subsystem.
#[derive(Debug, thiserror::Error)]
pub enum MercuryError {
    #[error("mercury: callback receiver was disconnected")]
    Channel,

    #[error("mercury: unexpected packet command 0x{0:02x}")]
    UnexpectedCommand(u8),

    #[error("mercury: response error (status {status_code}) for {uri}")]
    Response {
        uri: String,
        status_code: i32,
        payload: Vec<Vec<u8>>,
    },

    #[error("mercury: frame too short ({0} bytes)")]
    FrameTooShort(usize),

    #[error("mercury: protobuf decode error: {0}")]
    ProtobufDecode(String),
}

impl From<MercuryError> for ProtocolError {
    fn from(err: MercuryError) -> Self {
        ProtocolError::Connection(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Pending request tracking
// ---------------------------------------------------------------------------

/// An in-flight Mercury request or multi-frame response being reassembled.
struct PendingRequest {
    /// Accumulated complete parts.
    parts: Vec<Vec<u8>>,
    /// A partially received part (from a FLAG_PARTIAL frame).
    partial: Option<Vec<u8>>,
    /// The oneshot sender for the eventual response (None for unsolicited
    /// events that happen to match a pending seq, e.g. MercuryEvent).
    callback: Option<oneshot::Sender<Result<MercuryResponse>>>,
}

// ---------------------------------------------------------------------------
// MercuryManager
// ---------------------------------------------------------------------------

/// Manages pending Mercury requests and pub/sub subscriptions.
///
/// Internally protected by a `Mutex` so it can be shared across the session's
/// dispatch loop and any callers sending requests.
#[derive(Clone)]
pub struct MercuryManager {
    inner: Arc<Mutex<MercuryInner>>,
}

struct MercuryInner {
    /// Monotonically incrementing sequence counter.
    seq_counter: u64,

    /// In-flight requests keyed by their sequence ID.
    pending: HashMap<Vec<u8>, PendingRequest>,

    /// Active subscriptions: (URI prefix, sender).
    subscriptions: Vec<(String, mpsc::UnboundedSender<MercuryResponse>)>,

    /// Set to true on shutdown — prevents new registrations.
    invalid: bool,
}

impl MercuryManager {
    /// Create a new, empty manager.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(MercuryInner {
                seq_counter: 0,
                pending: HashMap::new(),
                subscriptions: Vec::new(),
                invalid: false,
            })),
        }
    }

    // -- sequence ID generation ---------------------------------------------

    /// Generate the next 8-byte big-endian sequence ID.
    fn next_seq(&self) -> Vec<u8> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let seq = inner.seq_counter;
        inner.seq_counter = seq.wrapping_add(1);
        seq.to_be_bytes().to_vec()
    }

    // -- sending requests ---------------------------------------------------

    /// Build and register a Mercury request.
    ///
    /// Returns `(packet_type, encoded_frame, response_receiver)`.
    /// The caller is responsible for sending the encoded frame over the AP
    /// connection.
    pub fn prepare_request(
        &self,
        method: MercuryMethod,
        uri: &str,
        content_type: Option<&str>,
        payload: &[Vec<u8>],
    ) -> Result<(
        PacketType,
        Vec<u8>,
        oneshot::Receiver<Result<MercuryResponse>>,
    )> {
        let seq = self.next_seq();
        let (tx, rx) = oneshot::channel();

        let pending = PendingRequest {
            parts: Vec::new(),
            partial: None,
            callback: Some(tx),
        };

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if inner.invalid {
            return Err(ProtocolError::NotConnected);
        }
        inner.pending.insert(seq.clone(), pending);
        drop(inner);

        let frame = Self::encode_request(&seq, method.as_str(), uri, content_type, payload)?;
        Ok((method.packet_type(), frame, rx))
    }

    /// High-level: send a GET request.
    ///
    /// Returns `(packet_type, frame, receiver)`.  Caller sends the frame.
    pub fn get(
        &self,
        uri: &str,
    ) -> Result<(
        PacketType,
        Vec<u8>,
        oneshot::Receiver<Result<MercuryResponse>>,
    )> {
        self.prepare_request(MercuryMethod::Get, uri, None, &[])
    }

    /// High-level: send a SEND request with one payload part.
    pub fn send(
        &self,
        uri: &str,
        data: Vec<u8>,
    ) -> Result<(
        PacketType,
        Vec<u8>,
        oneshot::Receiver<Result<MercuryResponse>>,
    )> {
        self.prepare_request(MercuryMethod::Send, uri, None, &[data])
    }

    /// High-level: prepare a SUB request.
    ///
    /// After the response arrives on the returned receiver, the caller should
    /// call `register_subscription` with the URI (or URIs from the response
    /// payload).
    pub fn prepare_subscribe(
        &self,
        uri: &str,
    ) -> Result<(
        PacketType,
        Vec<u8>,
        oneshot::Receiver<Result<MercuryResponse>>,
    )> {
        self.prepare_request(MercuryMethod::Sub, uri, None, &[])
    }

    /// Register a subscription channel for a URI prefix.
    ///
    /// Any incoming `MercuryEvent` whose URI starts with `prefix` will be
    /// dispatched to this channel.
    pub fn register_subscription(
        &self,
        prefix: String,
    ) -> mpsc::UnboundedReceiver<MercuryResponse> {
        let (tx, rx) = mpsc::unbounded_channel();

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if !inner.invalid {
            debug!(uri = %prefix, "mercury: registered subscription");
            inner.subscriptions.push((prefix, tx));
        }

        rx
    }

    // -- incoming packet handling -------------------------------------------

    /// Handle an incoming Mercury packet from the AP dispatch loop.
    ///
    /// This decodes the frame, reassembles multi-frame responses, and
    /// dispatches completed responses to the right callback or subscription.
    pub fn handle_packet(&self, cmd: u8, data: &[u8]) -> Result<()> {
        let packet_type = PacketType::from_u8(cmd);
        let is_event = packet_type == Some(PacketType::MercuryEvent);

        // Decode the frame header (seq, flags, parts).
        if data.len() < 2 {
            return Err(MercuryError::FrameTooShort(data.len()).into());
        }

        let mut pos = 0;

        let seq_len = read_u16(data, &mut pos)? as usize;
        if data.len() < pos + seq_len {
            return Err(MercuryError::FrameTooShort(data.len()).into());
        }
        let seq = data[pos..pos + seq_len].to_vec();
        pos += seq_len;

        if data.len() < pos + 1 {
            return Err(MercuryError::FrameTooShort(data.len()).into());
        }
        let flags = data[pos];
        pos += 1;

        let count = read_u16(data, &mut pos)? as usize;

        // Look up or create the pending request.
        let mut pending = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            match inner.pending.remove(&seq) {
                Some(p) => p,
                None => {
                    if is_event {
                        // Unsolicited event — create an ad-hoc pending.
                        PendingRequest {
                            parts: Vec::new(),
                            partial: None,
                            callback: None,
                        }
                    } else {
                        warn!(seq = ?seq, cmd = cmd, "mercury: ignoring unknown seq");
                        return Err(MercuryError::UnexpectedCommand(cmd).into());
                    }
                }
            }
        };

        // Parse the parts from the frame.
        for i in 0..count {
            let mut part = read_part(data, &mut pos)?;

            // If we have a partial leftover from a previous frame, prepend it.
            if let Some(mut partial) = pending.partial.take() {
                partial.extend_from_slice(&part);
                part = partial;
            }

            // The last part in a partial frame is itself partial.
            if i == count - 1 && flags == FLAG_PARTIAL {
                pending.partial = Some(part);
            } else {
                pending.parts.push(part);
            }
        }

        if flags == FLAG_FINAL {
            self.complete_request(cmd, pending)?;
        } else {
            // Re-insert for the next frame.
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.pending.insert(seq, pending);
        }

        Ok(())
    }

    /// Finish a fully-reassembled response.
    fn complete_request(&self, cmd: u8, mut pending: PendingRequest) -> Result<()> {
        if pending.parts.is_empty() {
            return Err(MercuryError::FrameTooShort(0).into());
        }

        let header_data = pending.parts.remove(0);
        let header = spotify_proto::mercury::Header::parse_from_bytes(&header_data)
            .map_err(|e| MercuryError::ProtobufDecode(e.to_string()))?;

        let content_type = if header.has_content_type() {
            Some(header.content_type().to_string())
        } else {
            None
        };

        let response = MercuryResponse {
            uri: header.uri().to_string(),
            status_code: header.status_code(),
            content_type,
            payload: pending.parts,
        };

        let status_code = response.status_code;
        let is_event = PacketType::from_u8(cmd) == Some(PacketType::MercuryEvent);

        if status_code >= 400 {
            error!(
                status_code = status_code,
                uri = %response.uri,
                "mercury: error response"
            );

            // Still deliver to the callback so the caller can see the error.
            if let Some(cb) = pending.callback {
                let err = MercuryError::Response {
                    uri: response.uri.clone(),
                    status_code: response.status_code,
                    payload: response.payload.clone(),
                };
                let _ = cb.send(Err(err.into()));
            }

            return Err(MercuryError::Response {
                uri: response.uri,
                status_code: response.status_code,
                payload: response.payload,
            }
            .into());
        }

        if is_event {
            self.dispatch_event(response);
            Ok(())
        } else if let Some(cb) = pending.callback {
            let _ = cb.send(Ok(response));
            Ok(())
        } else {
            trace!(uri = %response.uri, "mercury: no callback for response");
            Ok(())
        }
    }

    /// Match a MercuryEvent response against subscriptions and dispatch.
    fn dispatch_event(&self, response: MercuryResponse) {
        // URL-encode path components for matching (handles UTF-8 usernames).
        let encoded_uri = encode_uri_for_matching(&response.uri);

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut found = false;

        inner.subscriptions.retain(|(prefix, tx)| {
            if encoded_uri.starts_with(prefix.as_str()) {
                found = true;
                // If send fails the subscriber dropped; remove from list.
                tx.send(response.clone()).is_ok()
            } else {
                true
            }
        });

        if !found {
            debug!(uri = %response.uri, "mercury: unmatched subscription event");
        }
    }

    // -- shutdown ------------------------------------------------------------

    /// Shut down: clear all pending requests and subscriptions.
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.invalid = true;
        inner.pending.clear();
        inner.subscriptions.clear();
    }

    // -- static encode / decode helpers (public) ----------------------------

    /// Encode a Mercury request frame.
    ///
    /// ```text
    /// [2-byte seq_len][seq][1-byte flags=FINAL][2-byte part_count][header_part][payload_parts...]
    /// ```
    pub fn encode_request(
        seq: &[u8],
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        payload: &[Vec<u8>],
    ) -> Result<Vec<u8>> {
        let mut header = spotify_proto::mercury::Header::new();
        header.set_uri(uri.to_string());
        header.set_method(method.to_string());
        if let Some(ct) = content_type {
            header.set_content_type(ct.to_string());
        }

        let header_bytes = header
            .write_to_bytes()
            .map_err(|e| ProtocolError::Io(format!("protobuf encode: {e}")))?;

        // part_count = 1 (header) + payload parts
        let part_count = 1 + payload.len();

        // Calculate total size for pre-allocation.
        let total_size = 2 // seq_len
            + seq.len()
            + 1 // flags
            + 2 // part_count
            + 2 + header_bytes.len() // header part
            + payload.iter().map(|p| 2 + p.len()).sum::<usize>();

        let mut frame = Vec::with_capacity(total_size);

        // seq_len + seq
        frame.extend_from_slice(&(seq.len() as u16).to_be_bytes());
        frame.extend_from_slice(seq);

        // flags = FINAL
        frame.push(FLAG_FINAL);

        // part_count
        frame.extend_from_slice(&(part_count as u16).to_be_bytes());

        // header part
        frame.extend_from_slice(&(header_bytes.len() as u16).to_be_bytes());
        frame.extend_from_slice(&header_bytes);

        // payload parts
        for p in payload {
            frame.extend_from_slice(&(p.len() as u16).to_be_bytes());
            frame.extend_from_slice(p);
        }

        Ok(frame)
    }

    /// Decode a Mercury response frame, returning `(seq, response)`.
    ///
    /// This is a convenience for one-shot decoding of a complete (FLAG_FINAL)
    /// frame.  For multi-frame reassembly, use [`handle_packet`](Self::handle_packet).
    pub fn decode_response(data: &[u8]) -> Result<(Vec<u8>, MercuryResponse)> {
        if data.len() < 5 {
            return Err(MercuryError::FrameTooShort(data.len()).into());
        }

        let mut pos = 0;

        let seq_len = read_u16(data, &mut pos)? as usize;
        if data.len() < pos + seq_len {
            return Err(MercuryError::FrameTooShort(data.len()).into());
        }
        let seq = data[pos..pos + seq_len].to_vec();
        pos += seq_len;

        // flags
        if data.len() < pos + 1 {
            return Err(MercuryError::FrameTooShort(data.len()).into());
        }
        let _flags = data[pos];
        pos += 1;

        let count = read_u16(data, &mut pos)? as usize;

        let mut parts = Vec::with_capacity(count);
        for _ in 0..count {
            parts.push(read_part(data, &mut pos)?);
        }

        if parts.is_empty() {
            return Err(MercuryError::FrameTooShort(data.len()).into());
        }

        let header_data = parts.remove(0);
        let header = spotify_proto::mercury::Header::parse_from_bytes(&header_data)
            .map_err(|e| MercuryError::ProtobufDecode(e.to_string()))?;

        let content_type = if header.has_content_type() {
            Some(header.content_type().to_string())
        } else {
            None
        };

        let response = MercuryResponse {
            uri: header.uri().to_string(),
            status_code: header.status_code(),
            content_type,
            payload: parts,
        };

        Ok((seq, response))
    }
}

impl Default for MercuryManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Wire-format helpers (private)
// ---------------------------------------------------------------------------

/// Read a big-endian u16 from `data` at `pos`, advancing `pos`.
fn read_u16(data: &[u8], pos: &mut usize) -> Result<u16> {
    if data.len() < *pos + 2 {
        return Err(MercuryError::FrameTooShort(data.len()).into());
    }
    let val = u16::from_be_bytes([data[*pos], data[*pos + 1]]);
    *pos += 2;
    Ok(val)
}

/// Read a length-prefixed part from `data` at `pos`, advancing `pos`.
fn read_part(data: &[u8], pos: &mut usize) -> Result<Vec<u8>> {
    let len = read_u16(data, pos)? as usize;
    if data.len() < *pos + len {
        return Err(MercuryError::FrameTooShort(data.len()).into());
    }
    let part = data[*pos..*pos + len].to_vec();
    *pos += len;
    Ok(part)
}

/// URL-encode each path component for subscription matching.
///
/// This handles UTF-8 usernames that may appear in Mercury URIs. The first
/// component (scheme like `hm:`) is left unencoded.
fn encode_uri_for_matching(uri: &str) -> String {
    let mut split = uri.split('/');
    let mut result = String::with_capacity(uri.len() + 32);

    // First component (before the first `/`) is unencoded.
    if let Some(first) = split.next() {
        result.push_str(first);
    }

    for component in split {
        result.push('/');
        for chunk in form_urlencoded::byte_serialize(component.as_bytes()) {
            result.push_str(chunk);
        }
    }

    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let seq = 42u64.to_be_bytes().to_vec();
        let payload = vec![b"hello".to_vec(), b"world".to_vec()];

        let frame = MercuryManager::encode_request(
            &seq,
            "GET",
            "hm://metadata/track/abc123",
            Some("application/protobuf"),
            &payload,
        )
        .unwrap();

        let (dec_seq, resp) = MercuryManager::decode_response(&frame).unwrap();
        assert_eq!(dec_seq, seq);
        assert_eq!(resp.uri, "hm://metadata/track/abc123");
        assert_eq!(resp.status_code, 0); // Not set in request header
        assert_eq!(resp.content_type.as_deref(), Some("application/protobuf"));
        assert_eq!(resp.payload.len(), 2);
        assert_eq!(resp.payload[0], b"hello");
        assert_eq!(resp.payload[1], b"world");
    }

    #[test]
    fn encode_no_payload_no_content_type() {
        let seq = 1u64.to_be_bytes().to_vec();
        let frame = MercuryManager::encode_request(&seq, "SUB", "hm://events", None, &[]).unwrap();

        let (dec_seq, resp) = MercuryManager::decode_response(&frame).unwrap();
        assert_eq!(dec_seq, seq);
        assert_eq!(resp.uri, "hm://events");
        assert!(resp.content_type.is_none());
        assert!(resp.payload.is_empty());
    }

    #[test]
    fn decode_too_short() {
        assert!(MercuryManager::decode_response(&[]).is_err());
        assert!(MercuryManager::decode_response(&[0, 1]).is_err());
    }

    #[test]
    fn handle_packet_final_response() {
        let mgr = MercuryManager::new();

        // Prepare a request to register the seq.
        let (pkt_type, _frame, mut rx) = mgr
            .prepare_request(MercuryMethod::Get, "hm://test", None, &[])
            .unwrap();
        assert_eq!(pkt_type, PacketType::MercuryReq);

        // Extract the seq that was registered.
        let seq = {
            let inner = mgr.inner.lock().unwrap();
            inner.pending.keys().next().unwrap().clone()
        };

        // Build a response frame with that seq.
        let mut header = spotify_proto::mercury::Header::new();
        header.set_uri("hm://test".to_string());
        header.set_method("GET".to_string());
        header.set_status_code(200);
        let header_bytes = header.write_to_bytes().unwrap();

        let payload_data = b"response-body";
        let part_count: u16 = 2; // header + 1 payload

        let mut frame = Vec::new();
        frame.extend_from_slice(&(seq.len() as u16).to_be_bytes());
        frame.extend_from_slice(&seq);
        frame.push(FLAG_FINAL);
        frame.extend_from_slice(&part_count.to_be_bytes());
        // header part
        frame.extend_from_slice(&(header_bytes.len() as u16).to_be_bytes());
        frame.extend_from_slice(&header_bytes);
        // payload part
        frame.extend_from_slice(&(payload_data.len() as u16).to_be_bytes());
        frame.extend_from_slice(payload_data);

        mgr.handle_packet(PacketType::MercuryReq as u8, &frame)
            .unwrap();

        // The response should arrive on the oneshot.
        let resp = rx.try_recv().unwrap().unwrap();
        assert_eq!(resp.uri, "hm://test");
        assert_eq!(resp.status_code, 200);
        assert_eq!(resp.payload.len(), 1);
        assert_eq!(resp.payload[0], payload_data);
    }

    #[test]
    fn handle_packet_partial_then_final() {
        let mgr = MercuryManager::new();

        let (_pkt_type, _frame, mut rx) = mgr
            .prepare_request(MercuryMethod::Get, "hm://partial", None, &[])
            .unwrap();

        let seq = {
            let inner = mgr.inner.lock().unwrap();
            inner.pending.keys().next().unwrap().clone()
        };

        // Build a header for the response.
        let mut header = spotify_proto::mercury::Header::new();
        header.set_uri("hm://partial".to_string());
        header.set_status_code(200);
        let header_bytes = header.write_to_bytes().unwrap();

        // Frame 1: FLAG_PARTIAL with the header part and start of payload.
        let partial_data = b"first-half-";
        {
            let mut frame = Vec::new();
            frame.extend_from_slice(&(seq.len() as u16).to_be_bytes());
            frame.extend_from_slice(&seq);
            frame.push(FLAG_PARTIAL); // partial
            frame.extend_from_slice(&2u16.to_be_bytes()); // 2 parts: header + partial payload
            // header part (complete)
            frame.extend_from_slice(&(header_bytes.len() as u16).to_be_bytes());
            frame.extend_from_slice(&header_bytes);
            // partial payload
            frame.extend_from_slice(&(partial_data.len() as u16).to_be_bytes());
            frame.extend_from_slice(partial_data);

            mgr.handle_packet(PacketType::MercuryReq as u8, &frame)
                .unwrap();
        }

        // Response should not be ready yet.
        assert!(rx.try_recv().is_err());

        // Frame 2: FLAG_FINAL with the rest of the payload.
        let rest_data = b"second-half";
        {
            let mut frame = Vec::new();
            frame.extend_from_slice(&(seq.len() as u16).to_be_bytes());
            frame.extend_from_slice(&seq);
            frame.push(FLAG_FINAL);
            frame.extend_from_slice(&1u16.to_be_bytes()); // 1 part: rest of payload
            frame.extend_from_slice(&(rest_data.len() as u16).to_be_bytes());
            frame.extend_from_slice(rest_data);

            mgr.handle_packet(PacketType::MercuryReq as u8, &frame)
                .unwrap();
        }

        let resp = rx.try_recv().unwrap().unwrap();
        assert_eq!(resp.uri, "hm://partial");
        assert_eq!(resp.payload.len(), 1);
        assert_eq!(resp.payload[0], b"first-half-second-half");
    }

    #[test]
    fn subscription_dispatch() {
        let mgr = MercuryManager::new();

        // Register a subscription.
        let mut rx = mgr.register_subscription("hm://collection/".to_string());

        // Build a MercuryEvent frame.
        let seq = 99u64.to_be_bytes().to_vec();
        let mut header = spotify_proto::mercury::Header::new();
        header.set_uri("hm://collection/artist/changed".to_string());
        header.set_status_code(200);
        let header_bytes = header.write_to_bytes().unwrap();

        let event_payload = b"event-data";

        let mut frame = Vec::new();
        frame.extend_from_slice(&(seq.len() as u16).to_be_bytes());
        frame.extend_from_slice(&seq);
        frame.push(FLAG_FINAL);
        frame.extend_from_slice(&2u16.to_be_bytes());
        frame.extend_from_slice(&(header_bytes.len() as u16).to_be_bytes());
        frame.extend_from_slice(&header_bytes);
        frame.extend_from_slice(&(event_payload.len() as u16).to_be_bytes());
        frame.extend_from_slice(event_payload);

        mgr.handle_packet(PacketType::MercuryEvent as u8, &frame)
            .unwrap();

        // The event should arrive on the subscription channel.
        let resp = rx.try_recv().unwrap();
        assert_eq!(resp.uri, "hm://collection/artist/changed");
        assert_eq!(resp.payload[0], event_payload);
    }

    #[test]
    fn subscription_no_match() {
        let mgr = MercuryManager::new();

        let _rx = mgr.register_subscription("hm://other/".to_string());

        let seq = 100u64.to_be_bytes().to_vec();
        let mut header = spotify_proto::mercury::Header::new();
        header.set_uri("hm://collection/changed".to_string());
        header.set_status_code(200);
        let header_bytes = header.write_to_bytes().unwrap();

        let mut frame = Vec::new();
        frame.extend_from_slice(&(seq.len() as u16).to_be_bytes());
        frame.extend_from_slice(&seq);
        frame.push(FLAG_FINAL);
        frame.extend_from_slice(&1u16.to_be_bytes()); // just header
        frame.extend_from_slice(&(header_bytes.len() as u16).to_be_bytes());
        frame.extend_from_slice(&header_bytes);

        // Should not error — just log and continue.
        mgr.handle_packet(PacketType::MercuryEvent as u8, &frame)
            .unwrap();
    }

    #[test]
    fn shutdown_clears_state() {
        let mgr = MercuryManager::new();
        let _rx = mgr.register_subscription("hm://test/".to_string());
        let _ = mgr.prepare_request(MercuryMethod::Get, "hm://test", None, &[]);

        mgr.shutdown();

        let inner = mgr.inner.lock().unwrap();
        assert!(inner.invalid);
        assert!(inner.pending.is_empty());
        assert!(inner.subscriptions.is_empty());
    }

    #[test]
    fn prepare_request_after_shutdown_fails() {
        let mgr = MercuryManager::new();
        mgr.shutdown();

        let result = mgr.prepare_request(MercuryMethod::Get, "hm://test", None, &[]);
        assert!(result.is_err());
    }

    #[test]
    fn error_response_delivered_to_callback() {
        let mgr = MercuryManager::new();

        let (_pkt_type, _frame, mut rx) = mgr
            .prepare_request(MercuryMethod::Get, "hm://error-test", None, &[])
            .unwrap();

        let seq = {
            let inner = mgr.inner.lock().unwrap();
            inner.pending.keys().next().unwrap().clone()
        };

        let mut header = spotify_proto::mercury::Header::new();
        header.set_uri("hm://error-test".to_string());
        header.set_status_code(404);
        let header_bytes = header.write_to_bytes().unwrap();

        let mut frame = Vec::new();
        frame.extend_from_slice(&(seq.len() as u16).to_be_bytes());
        frame.extend_from_slice(&seq);
        frame.push(FLAG_FINAL);
        frame.extend_from_slice(&1u16.to_be_bytes());
        frame.extend_from_slice(&(header_bytes.len() as u16).to_be_bytes());
        frame.extend_from_slice(&header_bytes);

        // handle_packet returns an error for status >= 400.
        let result = mgr.handle_packet(PacketType::MercuryReq as u8, &frame);
        assert!(result.is_err());

        // The callback should also receive the error.
        let resp_result = rx.try_recv().unwrap();
        assert!(resp_result.is_err());
    }

    #[test]
    fn encode_uri_for_matching_basic() {
        assert_eq!(
            encode_uri_for_matching("hm://collection/artist/abc"),
            "hm://collection/artist/abc"
        );
    }

    #[test]
    fn encode_uri_for_matching_utf8() {
        // A username with special characters should be percent-encoded.
        let encoded = encode_uri_for_matching("hm://user/j\u{00f6}rg/collection");
        assert!(encoded.contains("j%C3%B6rg"));
    }

    #[test]
    fn method_display() {
        assert_eq!(MercuryMethod::Get.to_string(), "GET");
        assert_eq!(MercuryMethod::Send.to_string(), "SEND");
        assert_eq!(MercuryMethod::Sub.to_string(), "SUB");
        assert_eq!(MercuryMethod::Unsub.to_string(), "UNSUB");
    }

    #[test]
    fn method_packet_type() {
        assert_eq!(MercuryMethod::Get.packet_type(), PacketType::MercuryReq);
        assert_eq!(MercuryMethod::Send.packet_type(), PacketType::MercuryReq);
        assert_eq!(MercuryMethod::Sub.packet_type(), PacketType::MercurySub);
        assert_eq!(MercuryMethod::Unsub.packet_type(), PacketType::MercuryUnsub);
    }
}
