//! Audio data channel management — multiplexed streaming of audio file data
//! over the AP connection.
//!
//! Each channel has a 16-bit ID and passes through two phases:
//!
//! 1. **Header phase**: the server sends header chunks (type + data pairs) until
//!    an end-of-headers marker (header_id 0).
//! 2. **Data phase**: the server sends audio data chunks until an empty data
//!    packet signals end-of-stream.
//!
//! # Wire format
//!
//! Header packet (`StreamChunkRes`, 0x09):
//! ```text
//! [2 bytes channel_id BE][header chunks...]
//! ```
//! Each header chunk:
//! ```text
//! [2 bytes header_id BE][2 bytes data_len BE][data_len bytes data]
//! ```
//! A header chunk with `header_id == 0` signals end-of-headers (its data_len
//! field is still present but the data, if any, is ignored).
//!
//! Data packet (`StreamChunk`, 0x08):
//! ```text
//! [2 bytes channel_id BE][data...]
//! ```
//! An empty data section (packet length == 2, i.e. only the channel id)
//! signals end-of-stream.
//!
//! Channel error packet (`ChannelError`, 0x0a):
//! ```text
//! [2 bytes channel_id BE][2 bytes error_code BE]
//! ```

use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::error::{ProtocolError, Result};

// ---------------------------------------------------------------------------
// ChannelEvent — what a channel consumer receives
// ---------------------------------------------------------------------------

/// Events delivered to a channel consumer.
#[derive(Debug)]
pub enum ChannelEvent {
    /// A header chunk received during the header phase.
    Header {
        /// The header type identifier (nonzero).
        header_id: u8,
        /// The header data.
        data: Vec<u8>,
    },
    /// End of the header phase.
    EndOfHeaders,
    /// A chunk of audio data.
    Data(Vec<u8>),
    /// The channel has been closed (end-of-stream or release).
    Close,
    /// A channel-level error from the server.
    Error(ChannelError),
}

// ---------------------------------------------------------------------------
// ChannelError
// ---------------------------------------------------------------------------

/// Errors specific to the channel subsystem.
#[derive(Debug, thiserror::Error)]
pub enum ChannelError {
    #[error("channel: server error code {0} on channel {1}")]
    ServerError(u16, u16),

    #[error("channel: packet too short ({got} bytes, expected at least {expected})")]
    PacketTooShort { expected: usize, got: usize },

    #[error("channel: unknown channel id {0}")]
    UnknownChannel(u16),

    #[error("channel: no free channel ids")]
    Exhausted,

    #[error("channel: manager is shut down")]
    Shutdown,
}

impl From<ChannelError> for ProtocolError {
    fn from(err: ChannelError) -> Self {
        ProtocolError::Connection(err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

struct ChannelInner {
    /// Next channel ID to try when allocating.
    next_id: u16,

    /// Active channels: id -> sender.
    channels: HashMap<u16, mpsc::UnboundedSender<ChannelEvent>>,

    /// Set on shutdown.
    invalid: bool,
}

// ---------------------------------------------------------------------------
// ChannelManager
// ---------------------------------------------------------------------------

/// Manages multiplexed audio data channels.
///
/// Thread-safe: protected by a `Mutex` for sharing between the session
/// dispatch loop and callers allocating channels.
#[derive(Clone)]
pub struct ChannelManager {
    inner: std::sync::Arc<Mutex<ChannelInner>>,
}

impl ChannelManager {
    /// Create a new, empty manager.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(Mutex::new(ChannelInner {
                next_id: 0,
                channels: HashMap::new(),
                invalid: false,
            })),
        }
    }

    /// Allocate a new channel.
    ///
    /// Returns `(channel_id, Receiver)`.  The channel id must be included in
    /// the file request sent to the server.  Audio data and header events
    /// will arrive on the receiver.
    pub fn allocate(&self) -> Result<(u16, mpsc::UnboundedReceiver<ChannelEvent>)> {
        let (tx, rx) = mpsc::unbounded_channel();

        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());

        if inner.invalid {
            return Err(ChannelError::Shutdown.into());
        }

        // Find a free channel ID.  We start from `next_id` and wrap around,
        // giving up after 65536 attempts (every possible u16).
        let start = inner.next_id;
        let mut id = start;
        loop {
            if let std::collections::hash_map::Entry::Vacant(e) = inner.channels.entry(id) {
                e.insert(tx);
                inner.next_id = id.wrapping_add(1);
                trace!(channel_id = id, "channel: allocated");
                return Ok((id, rx));
            }

            id = id.wrapping_add(1);
            if id == start {
                return Err(ChannelError::Exhausted.into());
            }
        }
    }

    /// Handle an incoming header packet (`StreamChunkRes`, 0x09).
    ///
    /// Parses the header chunks and dispatches `ChannelEvent::Header` and
    /// `ChannelEvent::EndOfHeaders` events.
    pub fn handle_header(&self, data: &[u8]) -> Result<()> {
        if data.len() < 2 {
            return Err(ChannelError::PacketTooShort {
                expected: 2,
                got: data.len(),
            }
            .into());
        }

        let channel_id = u16::from_be_bytes([data[0], data[1]]);
        let mut pos = 2;

        let sender = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.channels.get(&channel_id).cloned()
        };

        let sender = match sender {
            Some(s) => s,
            None => {
                warn!(
                    channel_id = channel_id,
                    "channel: header for unknown channel"
                );
                return Err(ChannelError::UnknownChannel(channel_id).into());
            }
        };

        // Parse header chunks until we hit header_id 0 or run out of data.
        while pos + 2 <= data.len() {
            let header_id = u16::from_be_bytes([data[pos], data[pos + 1]]);
            pos += 2;

            if header_id == 0 {
                // End-of-headers marker.  The data_len field is still present
                // in the wire format; skip past it.
                if pos + 2 <= data.len() {
                    let skip_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                    // Advance past the length field and any trailing data.
                    let _ = pos + 2 + skip_len;
                }
                let _ = sender.send(ChannelEvent::EndOfHeaders);
                trace!(channel_id = channel_id, "channel: end of headers");
                break;
            }

            // Read the data length.
            if pos + 2 > data.len() {
                warn!(
                    channel_id = channel_id,
                    "channel: header chunk truncated (no data_len)"
                );
                break;
            }
            let chunk_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;

            if pos + chunk_len > data.len() {
                warn!(
                    channel_id = channel_id,
                    header_id = header_id,
                    chunk_len = chunk_len,
                    remaining = data.len() - pos,
                    "channel: header chunk data truncated"
                );
                break;
            }

            let chunk_data = data[pos..pos + chunk_len].to_vec();
            pos += chunk_len;

            // header_id fits in u8 for the event (Spotify uses small values).
            let header_id_u8 = if header_id <= u8::MAX as u16 {
                header_id as u8
            } else {
                // Unlikely but safe: truncate with a warning.
                warn!(
                    channel_id = channel_id,
                    header_id = header_id,
                    "channel: header_id exceeds u8, truncating"
                );
                (header_id & 0xFF) as u8
            };

            let _ = sender.send(ChannelEvent::Header {
                header_id: header_id_u8,
                data: chunk_data,
            });

            trace!(
                channel_id = channel_id,
                header_id = header_id_u8,
                "channel: header chunk"
            );
        }

        Ok(())
    }

    /// Handle an incoming data packet (`StreamChunk`, 0x08).
    ///
    /// An empty data section (packet is exactly 2 bytes: just the channel id)
    /// signals end-of-stream.
    pub fn handle_data(&self, data: &[u8]) -> Result<()> {
        if data.len() < 2 {
            return Err(ChannelError::PacketTooShort {
                expected: 2,
                got: data.len(),
            }
            .into());
        }

        let channel_id = u16::from_be_bytes([data[0], data[1]]);

        let sender = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.channels.get(&channel_id).cloned()
        };

        let sender = match sender {
            Some(s) => s,
            None => {
                warn!(channel_id = channel_id, "channel: data for unknown channel");
                return Err(ChannelError::UnknownChannel(channel_id).into());
            }
        };

        if data.len() == 2 {
            // End-of-stream.
            trace!(channel_id = channel_id, "channel: end of stream");
            let _ = sender.send(ChannelEvent::Close);
        } else {
            let chunk = data[2..].to_vec();
            trace!(
                channel_id = channel_id,
                len = chunk.len(),
                "channel: data chunk"
            );
            let _ = sender.send(ChannelEvent::Data(chunk));
        }

        Ok(())
    }

    /// Handle an incoming channel error packet (`ChannelError`, 0x0a).
    pub fn handle_channel_error(&self, data: &[u8]) -> Result<()> {
        if data.len() < 4 {
            return Err(ChannelError::PacketTooShort {
                expected: 4,
                got: data.len(),
            }
            .into());
        }

        let channel_id = u16::from_be_bytes([data[0], data[1]]);
        let error_code = u16::from_be_bytes([data[2], data[3]]);

        debug!(
            channel_id = channel_id,
            error_code = error_code,
            "channel: server error"
        );

        let sender = {
            let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.channels.get(&channel_id).cloned()
        };

        if let Some(sender) = sender {
            let _ = sender.send(ChannelEvent::Error(ChannelError::ServerError(
                error_code, channel_id,
            )));
        } else {
            warn!(
                channel_id = channel_id,
                "channel: error for unknown channel"
            );
        }

        Ok(())
    }

    /// Release a channel, removing it from the active set and sending a
    /// `Close` event to any listener.
    pub fn release(&self, channel_id: u16) {
        let sender = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.channels.remove(&channel_id)
        };

        if let Some(tx) = sender {
            let _ = tx.send(ChannelEvent::Close);
            trace!(channel_id = channel_id, "channel: released");
        }
    }

    /// Shut down the manager: close all active channels and reject new
    /// allocations.
    pub fn shutdown(&self) {
        let senders: Vec<mpsc::UnboundedSender<ChannelEvent>> = {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            inner.invalid = true;
            inner.channels.drain().map(|(_, tx)| tx).collect()
        };

        for tx in senders {
            let _ = tx.send(ChannelEvent::Close);
        }

        debug!("channel: manager shut down");
    }
}

impl Default for ChannelManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- allocation tests ------------------------------------------------------

    #[test]
    fn allocate_returns_sequential_ids() {
        let mgr = ChannelManager::new();

        let (id1, _rx1) = mgr.allocate().unwrap();
        let (id2, _rx2) = mgr.allocate().unwrap();
        let (id3, _rx3) = mgr.allocate().unwrap();

        assert_eq!(id1, 0);
        assert_eq!(id2, 1);
        assert_eq!(id3, 2);
    }

    #[test]
    fn allocate_reuses_released_id() {
        let mgr = ChannelManager::new();

        let (id1, _rx1) = mgr.allocate().unwrap();
        let (id2, _rx2) = mgr.allocate().unwrap();
        assert_eq!(id1, 0);
        assert_eq!(id2, 1);

        // Release channel 0.
        mgr.release(id1);

        // Next allocation should skip to 2 (next_id continues), but then 0 is
        // also free.  The allocator starts from next_id which is 2 here.
        let (id3, _rx3) = mgr.allocate().unwrap();
        assert_eq!(id3, 2);

        // Release 1, then allocate — should get 3 (next_id).
        mgr.release(id2);
        let (id4, _rx4) = mgr.allocate().unwrap();
        assert_eq!(id4, 3);
    }

    #[test]
    fn allocate_after_shutdown_fails() {
        let mgr = ChannelManager::new();
        mgr.shutdown();

        let result = mgr.allocate();
        assert!(result.is_err());
    }

    // -- header parsing tests --------------------------------------------------

    #[test]
    fn handle_header_single_chunk_then_end() {
        let mgr = ChannelManager::new();
        let (id, mut rx) = mgr.allocate().unwrap();

        // Build a header packet:
        // [channel_id][header_id=3][data_len=4][data "test"][header_id=0][data_len=0]
        let mut packet = Vec::new();
        packet.extend_from_slice(&id.to_be_bytes());
        // Header chunk: id=3, len=4, data="test"
        packet.extend_from_slice(&3u16.to_be_bytes());
        packet.extend_from_slice(&4u16.to_be_bytes());
        packet.extend_from_slice(b"test");
        // End-of-headers: id=0, len=0
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes());

        mgr.handle_header(&packet).unwrap();

        // Should receive Header then EndOfHeaders.
        match rx.try_recv().unwrap() {
            ChannelEvent::Header { header_id, data } => {
                assert_eq!(header_id, 3);
                assert_eq!(data, b"test");
            }
            other => panic!("expected Header, got {other:?}"),
        }

        match rx.try_recv().unwrap() {
            ChannelEvent::EndOfHeaders => {}
            other => panic!("expected EndOfHeaders, got {other:?}"),
        }
    }

    #[test]
    fn handle_header_multiple_chunks() {
        let mgr = ChannelManager::new();
        let (id, mut rx) = mgr.allocate().unwrap();

        let mut packet = Vec::new();
        packet.extend_from_slice(&id.to_be_bytes());
        // Chunk 1: id=1, len=2, data="ab"
        packet.extend_from_slice(&1u16.to_be_bytes());
        packet.extend_from_slice(&2u16.to_be_bytes());
        packet.extend_from_slice(b"ab");
        // Chunk 2: id=2, len=3, data="cde"
        packet.extend_from_slice(&2u16.to_be_bytes());
        packet.extend_from_slice(&3u16.to_be_bytes());
        packet.extend_from_slice(b"cde");
        // End: id=0, len=0
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes());

        mgr.handle_header(&packet).unwrap();

        match rx.try_recv().unwrap() {
            ChannelEvent::Header { header_id, data } => {
                assert_eq!(header_id, 1);
                assert_eq!(data, b"ab");
            }
            other => panic!("expected Header(1), got {other:?}"),
        }

        match rx.try_recv().unwrap() {
            ChannelEvent::Header { header_id, data } => {
                assert_eq!(header_id, 2);
                assert_eq!(data, b"cde");
            }
            other => panic!("expected Header(2), got {other:?}"),
        }

        match rx.try_recv().unwrap() {
            ChannelEvent::EndOfHeaders => {}
            other => panic!("expected EndOfHeaders, got {other:?}"),
        }
    }

    #[test]
    fn handle_header_unknown_channel() {
        let mgr = ChannelManager::new();

        let mut packet = Vec::new();
        packet.extend_from_slice(&999u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes());
        packet.extend_from_slice(&0u16.to_be_bytes());

        let result = mgr.handle_header(&packet);
        assert!(result.is_err());
    }

    #[test]
    fn handle_header_too_short() {
        let mgr = ChannelManager::new();

        let result = mgr.handle_header(&[0]);
        assert!(result.is_err());

        let result = mgr.handle_header(&[]);
        assert!(result.is_err());
    }

    // -- data handling tests ---------------------------------------------------

    #[test]
    fn handle_data_delivers_chunk() {
        let mgr = ChannelManager::new();
        let (id, mut rx) = mgr.allocate().unwrap();

        let mut packet = Vec::new();
        packet.extend_from_slice(&id.to_be_bytes());
        packet.extend_from_slice(b"audio data here");

        mgr.handle_data(&packet).unwrap();

        match rx.try_recv().unwrap() {
            ChannelEvent::Data(data) => {
                assert_eq!(data, b"audio data here");
            }
            other => panic!("expected Data, got {other:?}"),
        }
    }

    #[test]
    fn handle_data_multiple_chunks() {
        let mgr = ChannelManager::new();
        let (id, mut rx) = mgr.allocate().unwrap();

        for i in 0..3 {
            let mut packet = Vec::new();
            packet.extend_from_slice(&id.to_be_bytes());
            packet.extend_from_slice(&[i; 100]);
            mgr.handle_data(&packet).unwrap();
        }

        for i in 0..3 {
            match rx.try_recv().unwrap() {
                ChannelEvent::Data(data) => {
                    assert_eq!(data.len(), 100);
                    assert!(data.iter().all(|&b| b == i));
                }
                other => panic!("expected Data, got {other:?}"),
            }
        }
    }

    #[test]
    fn handle_data_end_of_stream() {
        let mgr = ChannelManager::new();
        let (id, mut rx) = mgr.allocate().unwrap();

        // Send some data first.
        let mut data_packet = Vec::new();
        data_packet.extend_from_slice(&id.to_be_bytes());
        data_packet.extend_from_slice(b"chunk");
        mgr.handle_data(&data_packet).unwrap();

        // End-of-stream: just the channel id, no data.
        let eos_packet = id.to_be_bytes().to_vec();
        mgr.handle_data(&eos_packet).unwrap();

        match rx.try_recv().unwrap() {
            ChannelEvent::Data(d) => assert_eq!(d, b"chunk"),
            other => panic!("expected Data, got {other:?}"),
        }

        match rx.try_recv().unwrap() {
            ChannelEvent::Close => {}
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn handle_data_unknown_channel() {
        let mgr = ChannelManager::new();

        let mut packet = Vec::new();
        packet.extend_from_slice(&999u16.to_be_bytes());
        packet.extend_from_slice(b"data");

        let result = mgr.handle_data(&packet);
        assert!(result.is_err());
    }

    #[test]
    fn handle_data_too_short() {
        let mgr = ChannelManager::new();

        let result = mgr.handle_data(&[0]);
        assert!(result.is_err());

        let result = mgr.handle_data(&[]);
        assert!(result.is_err());
    }

    // -- channel error tests ---------------------------------------------------

    #[test]
    fn handle_channel_error_delivers_event() {
        let mgr = ChannelManager::new();
        let (id, mut rx) = mgr.allocate().unwrap();

        let mut packet = Vec::new();
        packet.extend_from_slice(&id.to_be_bytes());
        packet.extend_from_slice(&42u16.to_be_bytes());

        mgr.handle_channel_error(&packet).unwrap();

        match rx.try_recv().unwrap() {
            ChannelEvent::Error(ChannelError::ServerError(code, ch)) => {
                assert_eq!(code, 42);
                assert_eq!(ch, id);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn handle_channel_error_too_short() {
        let mgr = ChannelManager::new();

        let result = mgr.handle_channel_error(&[0, 0, 0]);
        assert!(result.is_err());
    }

    // -- release tests ---------------------------------------------------------

    #[test]
    fn release_sends_close() {
        let mgr = ChannelManager::new();
        let (id, mut rx) = mgr.allocate().unwrap();

        mgr.release(id);

        match rx.try_recv().unwrap() {
            ChannelEvent::Close => {}
            other => panic!("expected Close, got {other:?}"),
        }
    }

    #[test]
    fn release_nonexistent_is_noop() {
        let mgr = ChannelManager::new();
        // Should not panic.
        mgr.release(999);
    }

    // -- shutdown tests --------------------------------------------------------

    #[test]
    fn shutdown_closes_all_channels() {
        let mgr = ChannelManager::new();
        let (_id1, mut rx1) = mgr.allocate().unwrap();
        let (_id2, mut rx2) = mgr.allocate().unwrap();

        mgr.shutdown();

        match rx1.try_recv().unwrap() {
            ChannelEvent::Close => {}
            other => panic!("expected Close on rx1, got {other:?}"),
        }

        match rx2.try_recv().unwrap() {
            ChannelEvent::Close => {}
            other => panic!("expected Close on rx2, got {other:?}"),
        }
    }

    #[test]
    fn shutdown_rejects_new_allocations() {
        let mgr = ChannelManager::new();
        mgr.shutdown();

        let result = mgr.allocate();
        assert!(result.is_err());
    }

    // -- clone sharing test ----------------------------------------------------

    #[test]
    fn manager_clone_shares_state() {
        let mgr1 = ChannelManager::new();
        let mgr2 = mgr1.clone();

        let (id, mut rx) = mgr1.allocate().unwrap();

        // Handle data on the clone.
        let mut packet = Vec::new();
        packet.extend_from_slice(&id.to_be_bytes());
        packet.extend_from_slice(b"shared");

        mgr2.handle_data(&packet).unwrap();

        match rx.try_recv().unwrap() {
            ChannelEvent::Data(data) => assert_eq!(data, b"shared"),
            other => panic!("expected Data, got {other:?}"),
        }
    }

    // -- full flow test --------------------------------------------------------

    #[test]
    fn full_header_then_data_flow() {
        let mgr = ChannelManager::new();
        let (id, mut rx) = mgr.allocate().unwrap();

        // 1. Header packet with file size header (id=3) + end-of-headers.
        let mut header_pkt = Vec::new();
        header_pkt.extend_from_slice(&id.to_be_bytes());
        // File size header: id=3, len=4, data=[0,0,0x10,0x00] (4096 bytes)
        header_pkt.extend_from_slice(&3u16.to_be_bytes());
        header_pkt.extend_from_slice(&4u16.to_be_bytes());
        header_pkt.extend_from_slice(&[0x00, 0x00, 0x10, 0x00]);
        // End of headers.
        header_pkt.extend_from_slice(&0u16.to_be_bytes());
        header_pkt.extend_from_slice(&0u16.to_be_bytes());

        mgr.handle_header(&header_pkt).unwrap();

        // 2. Data packets.
        let mut data_pkt1 = Vec::new();
        data_pkt1.extend_from_slice(&id.to_be_bytes());
        data_pkt1.extend_from_slice(&[0xAA; 2048]);
        mgr.handle_data(&data_pkt1).unwrap();

        let mut data_pkt2 = Vec::new();
        data_pkt2.extend_from_slice(&id.to_be_bytes());
        data_pkt2.extend_from_slice(&[0xBB; 2048]);
        mgr.handle_data(&data_pkt2).unwrap();

        // 3. End-of-stream.
        mgr.handle_data(id.to_be_bytes().as_ref()).unwrap();

        // Verify the event sequence.
        match rx.try_recv().unwrap() {
            ChannelEvent::Header { header_id, data } => {
                assert_eq!(header_id, 3);
                assert_eq!(data, [0x00, 0x00, 0x10, 0x00]);
            }
            other => panic!("expected Header, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            ChannelEvent::EndOfHeaders => {}
            other => panic!("expected EndOfHeaders, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            ChannelEvent::Data(d) => {
                assert_eq!(d.len(), 2048);
                assert!(d.iter().all(|&b| b == 0xAA));
            }
            other => panic!("expected Data, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            ChannelEvent::Data(d) => {
                assert_eq!(d.len(), 2048);
                assert!(d.iter().all(|&b| b == 0xBB));
            }
            other => panic!("expected Data, got {other:?}"),
        }
        match rx.try_recv().unwrap() {
            ChannelEvent::Close => {}
            other => panic!("expected Close, got {other:?}"),
        }
    }
}
