//! Audio key management — request and receive AES-128 decryption keys for
//! encrypted Spotify audio files.
//!
//! # Protocol
//!
//! The client sends a `RequestKey` (0x0c) packet containing the file id and
//! track gid.  The server responds with either an `AesKey` (0x0d) packet
//! carrying the 16-byte key, or an `AesKeyError` (0x0e) with a 2-byte error
//! code.
//!
//! # Wire format
//!
//! Request (sent by client):
//! ```text
//! [20 bytes file_id][16 bytes track_gid][4 bytes seq BE][2 bytes 0x00 0x00]
//! ```
//!
//! Success response (`AesKey`, 0x0d):
//! ```text
//! [4 bytes seq BE][16 bytes AES-128 key]
//! ```
//!
//! Error response (`AesKeyError`, 0x0e):
//! ```text
//! [4 bytes seq BE][2 bytes error_code BE]
//! ```

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use tokio::sync::oneshot;
use tracing::{debug, trace, warn};

use crate::error::{ProtocolError, Result};

// ---------------------------------------------------------------------------
// AudioKey — the 16-byte AES-128 decryption key
// ---------------------------------------------------------------------------

/// A 16-byte AES-128 key used to decrypt Spotify audio files.
#[derive(Clone, PartialEq, Eq)]
pub struct AudioKey(pub [u8; 16]);

impl AudioKey {
    /// Create an `AudioKey` from a byte slice.
    pub fn from_slice(data: &[u8]) -> Result<Self> {
        let arr: [u8; 16] = data
            .try_into()
            .map_err(|_| AudioKeyError::InvalidKeyLength {
                expected: 16,
                got: data.len(),
            })?;
        Ok(Self(arr))
    }

    /// The raw key bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for AudioKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Redact the key value — only show that it exists.
        f.write_str("AudioKey([REDACTED])")
    }
}

// ---------------------------------------------------------------------------
// AudioKeyError
// ---------------------------------------------------------------------------

/// Errors specific to the audio key subsystem.
#[derive(Debug, thiserror::Error)]
pub enum AudioKeyError {
    #[error("audio key: server returned error code {0}")]
    ServerError(u16),

    #[error("audio key: response too short ({got} bytes, expected at least {expected})")]
    ResponseTooShort { expected: usize, got: usize },

    #[error("audio key: invalid key length (expected {expected}, got {got})")]
    InvalidKeyLength { expected: usize, got: usize },

    #[error("audio key: unknown sequence {0}")]
    UnknownSequence(u32),

    #[error("audio key: callback channel was dropped")]
    Channel,

    #[error("audio key: manager is shut down")]
    Shutdown,
}

impl From<AudioKeyError> for ProtocolError {
    fn from(err: AudioKeyError) -> Self {
        ProtocolError::Connection(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Request packet size constants
// ---------------------------------------------------------------------------

/// Size of the encoded request packet.
const REQUEST_PACKET_SIZE: usize = 20 + 16 + 4 + 2; // file_id + gid + seq + padding

/// Minimum size of a success response (seq + key).
const AES_KEY_RESPONSE_MIN: usize = 4 + 16;

/// Minimum size of an error response (seq + error_code).
const AES_KEY_ERROR_RESPONSE_MIN: usize = 4 + 2;

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

struct AudioKeyInner {
    /// Monotonically incrementing sequence counter (wraps at u32::MAX).
    seq_counter: u32,

    /// Pending requests: seq -> oneshot sender for the key (or error).
    pending: HashMap<u32, oneshot::Sender<Result<AudioKey>>>,

    /// Set on shutdown to reject new requests.
    invalid: bool,
}

// ---------------------------------------------------------------------------
// AudioKeyManager
// ---------------------------------------------------------------------------

/// Manages pending audio key requests.
///
/// Thread-safe: protected by a `Mutex` so it can be shared between the
/// session dispatch loop and callers requesting keys.
#[derive(Clone)]
pub struct AudioKeyManager {
    inner: std::sync::Arc<Mutex<AudioKeyInner>>,
}

impl AudioKeyManager {
    /// Create a new, empty manager.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(AudioKeyInner {
                seq_counter: 0,
                pending: HashMap::new(),
                invalid: false,
            })),
        }
    }

    /// Prepare a key request for the given file and track.
    ///
    /// Returns `(encoded_packet_data, Receiver)`.  The caller must send the
    /// packet data with `PacketType::RequestKey` (0x0c) over the AP
    /// connection.  The receiver will yield the key when the server responds.
    pub fn request_key(
        &self,
        file_id: &[u8; 20],
        track_gid: &[u8; 16],
    ) -> Result<(Vec<u8>, oneshot::Receiver<Result<AudioKey>>)> {
        let (tx, rx) = oneshot::channel();

        let seq = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if inner.invalid {
                return Err(AudioKeyError::Shutdown.into());
            }

            let seq = inner.seq_counter;
            inner.seq_counter = seq.wrapping_add(1);
            inner.pending.insert(seq, tx);
            seq
        };

        let packet = encode_request(file_id, track_gid, seq);

        trace!(seq = seq, "audio key: request prepared");

        Ok((packet, rx))
    }

    /// Handle an incoming `AesKey` (0x0d) success response.
    pub fn handle_aes_key(&self, data: &[u8]) -> Result<()> {
        if data.len() < AES_KEY_RESPONSE_MIN {
            return Err(AudioKeyError::ResponseTooShort {
                expected: AES_KEY_RESPONSE_MIN,
                got: data.len(),
            }
            .into());
        }

        let seq = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let key = AudioKey::from_slice(&data[4..20])?;

        trace!(seq = seq, "audio key: received AesKey");

        let sender = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.pending.remove(&seq)
        };

        match sender {
            Some(tx) => {
                let _ = tx.send(Ok(key));
                Ok(())
            }
            None => {
                warn!(seq = seq, "audio key: AesKey for unknown sequence");
                Err(AudioKeyError::UnknownSequence(seq).into())
            }
        }
    }

    /// Handle an incoming `AesKeyError` (0x0e) error response.
    pub fn handle_aes_key_error(&self, data: &[u8]) -> Result<()> {
        if data.len() < AES_KEY_ERROR_RESPONSE_MIN {
            return Err(AudioKeyError::ResponseTooShort {
                expected: AES_KEY_ERROR_RESPONSE_MIN,
                got: data.len(),
            }
            .into());
        }

        let seq = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let error_code = u16::from_be_bytes([data[4], data[5]]);

        debug!(seq = seq, error_code = error_code, "audio key: AesKeyError");

        let sender = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.pending.remove(&seq)
        };

        let err = AudioKeyError::ServerError(error_code);

        match sender {
            Some(tx) => {
                let _ = tx.send(Err(err.into()));
                Ok(())
            }
            None => {
                warn!(seq = seq, "audio key: AesKeyError for unknown sequence");
                Err(AudioKeyError::UnknownSequence(seq).into())
            }
        }
    }

    /// Shut down the manager: reject new requests and drop all pending
    /// callbacks (which causes their receivers to get a `RecvError`).
    pub fn shutdown(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.invalid = true;
        inner.pending.clear();
        debug!("audio key: manager shut down");
    }
}

impl Default for AudioKeyManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Packet encoding
// ---------------------------------------------------------------------------

/// Encode a `RequestKey` packet.
///
/// ```text
/// [20 bytes file_id][16 bytes track_gid][4 bytes seq BE][2 bytes 0x00 0x00]
/// ```
fn encode_request(file_id: &[u8; 20], track_gid: &[u8; 16], seq: u32) -> Vec<u8> {
    let mut packet = Vec::with_capacity(REQUEST_PACKET_SIZE);
    packet.extend_from_slice(file_id);
    packet.extend_from_slice(track_gid);
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x00]);
    packet
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_file_id() -> [u8; 20] {
        [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
            0x0f, 0x10, 0x11, 0x12, 0x13, 0x14,
        ]
    }

    fn sample_track_gid() -> [u8; 16] {
        [
            0xa0, 0xa1, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7, 0xa8, 0xa9, 0xaa, 0xab, 0xac, 0xad,
            0xae, 0xaf,
        ]
    }

    // -- encoding tests -------------------------------------------------------

    #[test]
    fn request_encoding_format() {
        let file_id = sample_file_id();
        let track_gid = sample_track_gid();
        let seq: u32 = 42;

        let packet = encode_request(&file_id, &track_gid, seq);

        assert_eq!(packet.len(), REQUEST_PACKET_SIZE);

        // file_id at offset 0..20
        assert_eq!(&packet[0..20], &file_id);
        // track_gid at offset 20..36
        assert_eq!(&packet[20..36], &track_gid);
        // seq at offset 36..40 (big-endian)
        assert_eq!(&packet[36..40], &42u32.to_be_bytes());
        // padding at offset 40..42
        assert_eq!(&packet[40..42], &[0x00, 0x00]);
    }

    #[test]
    fn request_encoding_seq_zero() {
        let packet = encode_request(&[0u8; 20], &[0u8; 16], 0);
        assert_eq!(packet.len(), REQUEST_PACKET_SIZE);
        assert_eq!(&packet[36..40], &[0, 0, 0, 0]);
    }

    #[test]
    fn request_encoding_seq_max() {
        let packet = encode_request(&[0u8; 20], &[0u8; 16], u32::MAX);
        assert_eq!(&packet[36..40], &[0xFF, 0xFF, 0xFF, 0xFF]);
    }

    // -- response decoding tests -----------------------------------------------

    #[test]
    fn handle_aes_key_success() {
        let mgr = AudioKeyManager::new();
        let file_id = sample_file_id();
        let track_gid = sample_track_gid();

        let (packet, mut rx) = mgr.request_key(&file_id, &track_gid).unwrap();

        // Extract the seq from the encoded packet.
        let seq_bytes = &packet[36..40];

        // Build a success response: [seq][16 bytes key]
        let key_bytes = [
            0x10, 0x20, 0x30, 0x40, 0x50, 0x60, 0x70, 0x80, 0x90, 0xa0, 0xb0, 0xc0, 0xd0, 0xe0,
            0xf0, 0x00,
        ];
        let mut response = Vec::new();
        response.extend_from_slice(seq_bytes);
        response.extend_from_slice(&key_bytes);

        mgr.handle_aes_key(&response).unwrap();

        let result = rx.try_recv().unwrap().unwrap();
        assert_eq!(result.0, key_bytes);
    }

    #[test]
    fn handle_aes_key_error_response() {
        let mgr = AudioKeyManager::new();
        let file_id = sample_file_id();
        let track_gid = sample_track_gid();

        let (packet, mut rx) = mgr.request_key(&file_id, &track_gid).unwrap();

        let seq_bytes = &packet[36..40];

        // Build an error response: [seq][2 bytes error_code]
        let error_code: u16 = 1; // Typically: playback denied
        let mut response = Vec::new();
        response.extend_from_slice(seq_bytes);
        response.extend_from_slice(&error_code.to_be_bytes());

        mgr.handle_aes_key_error(&response).unwrap();

        let result = rx.try_recv().unwrap();
        assert!(result.is_err());
    }

    #[test]
    fn handle_aes_key_unknown_seq() {
        let mgr = AudioKeyManager::new();

        // Response for a seq that was never registered.
        let mut response = Vec::new();
        response.extend_from_slice(&999u32.to_be_bytes());
        response.extend_from_slice(&[0u8; 16]);

        let result = mgr.handle_aes_key(&response);
        assert!(result.is_err());
    }

    #[test]
    fn handle_aes_key_error_unknown_seq() {
        let mgr = AudioKeyManager::new();

        let mut response = Vec::new();
        response.extend_from_slice(&999u32.to_be_bytes());
        response.extend_from_slice(&0u16.to_be_bytes());

        let result = mgr.handle_aes_key_error(&response);
        assert!(result.is_err());
    }

    #[test]
    fn handle_aes_key_too_short() {
        let mgr = AudioKeyManager::new();

        // Less than 20 bytes.
        let result = mgr.handle_aes_key(&[0u8; 10]);
        assert!(result.is_err());

        // Empty.
        let result = mgr.handle_aes_key(&[]);
        assert!(result.is_err());
    }

    #[test]
    fn handle_aes_key_error_too_short() {
        let mgr = AudioKeyManager::new();

        // Less than 6 bytes.
        let result = mgr.handle_aes_key_error(&[0u8; 3]);
        assert!(result.is_err());
    }

    // -- sequence counter tests ------------------------------------------------

    #[test]
    fn sequence_counter_increments() {
        let mgr = AudioKeyManager::new();
        let file_id = sample_file_id();
        let track_gid = sample_track_gid();

        let (pkt1, _rx1) = mgr.request_key(&file_id, &track_gid).unwrap();
        let (pkt2, _rx2) = mgr.request_key(&file_id, &track_gid).unwrap();

        let seq1 = u32::from_be_bytes(pkt1[36..40].try_into().unwrap());
        let seq2 = u32::from_be_bytes(pkt2[36..40].try_into().unwrap());

        assert_eq!(seq1, 0);
        assert_eq!(seq2, 1);
    }

    // -- shutdown tests --------------------------------------------------------

    #[test]
    fn shutdown_clears_pending() {
        let mgr = AudioKeyManager::new();
        let file_id = sample_file_id();
        let track_gid = sample_track_gid();

        let (_pkt, _rx) = mgr.request_key(&file_id, &track_gid).unwrap();

        mgr.shutdown();

        let inner = mgr.inner.lock().unwrap();
        assert!(inner.invalid);
        assert!(inner.pending.is_empty());
    }

    #[test]
    fn request_after_shutdown_fails() {
        let mgr = AudioKeyManager::new();
        mgr.shutdown();

        let result = mgr.request_key(&[0u8; 20], &[0u8; 16]);
        assert!(result.is_err());
    }

    // -- AudioKey tests --------------------------------------------------------

    #[test]
    fn audio_key_from_slice_ok() {
        let data = [1u8; 16];
        let key = AudioKey::from_slice(&data).unwrap();
        assert_eq!(key.as_bytes(), &data);
    }

    #[test]
    fn audio_key_from_slice_wrong_len() {
        assert!(AudioKey::from_slice(&[0u8; 15]).is_err());
        assert!(AudioKey::from_slice(&[0u8; 17]).is_err());
        assert!(AudioKey::from_slice(&[]).is_err());
    }

    #[test]
    fn audio_key_debug_redacted() {
        let key = AudioKey([0u8; 16]);
        let debug_str = format!("{key:?}");
        assert!(debug_str.contains("REDACTED"));
        assert!(!debug_str.contains("0, 0, 0"));
    }

    #[test]
    fn audio_key_clone_eq() {
        let key1 = AudioKey([42u8; 16]);
        let key2 = key1.clone();
        assert_eq!(key1, key2);
    }

    // -- manager clone test ----------------------------------------------------

    #[test]
    fn manager_clone_shares_state() {
        let mgr1 = AudioKeyManager::new();
        let mgr2 = mgr1.clone();

        let (pkt, mut rx) = mgr1.request_key(&[0u8; 20], &[0u8; 16]).unwrap();

        // Handle the response on the clone.
        let seq_bytes = &pkt[36..40];
        let mut response = Vec::new();
        response.extend_from_slice(seq_bytes);
        response.extend_from_slice(&[0xABu8; 16]);

        mgr2.handle_aes_key(&response).unwrap();

        let key = rx.try_recv().unwrap().unwrap();
        assert_eq!(key.0, [0xABu8; 16]);
    }
}
