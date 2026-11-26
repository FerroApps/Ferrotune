use std::io;

use bytes::{BufMut, Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};

use super::Shannon;

const HEADER_SIZE: usize = 3;
const MAC_SIZE: usize = 4;

/// Decode state machine: we read the 3-byte header first, then wait for
/// the payload + 4-byte MAC.
#[derive(Debug)]
enum DecodeState {
    Header,
    Payload(u8, usize),
}

/// Shannon-encrypted packet codec for the Spotify AP binary protocol.
///
/// Each frame on the wire is:
///   encrypted(1-byte cmd | 2-byte BE payload len) | encrypted(payload) | 4-byte MAC
///
/// Nonces are sequential u32 counters, one for each direction.
pub struct ApCodec {
    encode_cipher: Shannon,
    encode_nonce: u32,

    decode_cipher: Shannon,
    decode_nonce: u32,
    decode_state: DecodeState,
}

impl ApCodec {
    /// Create a new codec from the send and receive Shannon keys produced by
    /// the DH handshake.
    pub fn new(send_key: &[u8], recv_key: &[u8]) -> Self {
        Self {
            encode_cipher: Shannon::new(send_key),
            encode_nonce: 0,

            decode_cipher: Shannon::new(recv_key),
            decode_nonce: 0,
            decode_state: DecodeState::Header,
        }
    }
}

impl Encoder<(u8, Vec<u8>)> for ApCodec {
    type Error = io::Error;

    fn encode(&mut self, item: (u8, Vec<u8>), buf: &mut BytesMut) -> io::Result<()> {
        let (cmd, payload) = item;
        let offset = buf.len();

        buf.reserve(HEADER_SIZE + payload.len() + MAC_SIZE);
        buf.put_u8(cmd);
        buf.put_u16(payload.len() as u16);
        buf.extend_from_slice(&payload);

        self.encode_cipher.nonce_u32(self.encode_nonce);
        self.encode_nonce += 1;

        self.encode_cipher.encrypt(&mut buf[offset..]);

        let mut mac = [0u8; MAC_SIZE];
        self.encode_cipher.finish(&mut mac);
        buf.extend_from_slice(&mac);

        Ok(())
    }
}

impl Decoder for ApCodec {
    type Item = (u8, Bytes);
    type Error = io::Error;

    fn decode(&mut self, buf: &mut BytesMut) -> io::Result<Option<(u8, Bytes)>> {
        if let DecodeState::Header = self.decode_state
            && buf.len() >= HEADER_SIZE
        {
            let mut header = [0u8; HEADER_SIZE];
            header.copy_from_slice(buf.split_to(HEADER_SIZE).as_ref());

            self.decode_cipher.nonce_u32(self.decode_nonce);
            self.decode_nonce += 1;

            self.decode_cipher.decrypt(&mut header);

            let cmd = header[0];
            let size = u16::from_be_bytes([header[1], header[2]]) as usize;
            self.decode_state = DecodeState::Payload(cmd, size);
        }

        if let DecodeState::Payload(cmd, size) = self.decode_state
            && buf.len() >= size + MAC_SIZE
        {
            self.decode_state = DecodeState::Header;

            let mut payload = buf.split_to(size + MAC_SIZE);

            self.decode_cipher
                .decrypt(payload.get_mut(..size).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "payload was malformed")
                })?);
            let mac = payload.split_off(size);

            if !self.decode_cipher.check_mac(mac.as_ref()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "MAC verification failed",
                ));
            }

            return Ok(Some((cmd, payload.freeze())));
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode then decode must recover the original command and payload.
    #[test]
    fn roundtrip() {
        let send_key = b"send key material for test";
        let recv_key = b"recv key material for test";

        let mut encoder = ApCodec::new(send_key, recv_key);
        let mut decoder = ApCodec::new(recv_key, send_key);

        let cmd: u8 = 0xab; // Login
        let payload = b"hello, Spotify!".to_vec();

        let mut buf = BytesMut::new();
        encoder.encode((cmd, payload.clone()), &mut buf).ok();

        let decoded = decoder.decode(&mut buf).ok().flatten();
        assert!(decoded.is_some(), "should decode a full frame");

        let (dec_cmd, dec_payload) = decoded.unwrap();
        assert_eq!(dec_cmd, cmd);
        assert_eq!(dec_payload.as_ref(), payload.as_slice());
    }

    /// Multiple sequential packets must each decode correctly (nonces advance).
    #[test]
    fn sequential_packets() {
        let send_key = b"sequential send";
        let recv_key = b"sequential recv";

        let mut encoder = ApCodec::new(send_key, recv_key);
        let mut decoder = ApCodec::new(recv_key, send_key);

        for i in 0u8..10 {
            let cmd = i;
            let payload = vec![i; (i as usize) * 7 + 1];

            let mut buf = BytesMut::new();
            encoder.encode((cmd, payload.clone()), &mut buf).ok();

            let decoded = decoder.decode(&mut buf).ok().flatten();
            assert!(decoded.is_some(), "packet {i} should decode");

            let (dec_cmd, dec_payload) = decoded.unwrap();
            assert_eq!(dec_cmd, cmd);
            assert_eq!(dec_payload.as_ref(), payload.as_slice());
        }
    }

    /// Partial buffer returns Ok(None) instead of an error.
    #[test]
    fn partial_header() {
        let mut decoder = ApCodec::new(b"k1", b"k2");
        let mut buf = BytesMut::from(&[0u8, 1][..]);
        let result = decoder.decode(&mut buf);
        assert!(result.is_ok());
        assert!(result.ok().flatten().is_none());
    }

    /// Empty payload encodes/decodes correctly.
    #[test]
    fn empty_payload() {
        let mut encoder = ApCodec::new(b"ek", b"dk");
        let mut decoder = ApCodec::new(b"dk", b"ek");

        let mut buf = BytesMut::new();
        encoder.encode((0x04, vec![]), &mut buf).ok();

        let decoded = decoder.decode(&mut buf).ok().flatten();
        assert!(decoded.is_some());
        let (cmd, data) = decoded.unwrap();
        assert_eq!(cmd, 0x04);
        assert!(data.is_empty());
    }

    /// Tampered ciphertext must fail MAC check.
    #[test]
    fn tampered_payload_fails_mac() {
        let mut encoder = ApCodec::new(b"ek", b"dk");
        let mut decoder = ApCodec::new(b"dk", b"ek");

        let mut buf = BytesMut::new();
        encoder
            .encode((0xab, b"secret data".to_vec()), &mut buf)
            .ok();

        // Tamper with one byte in the encrypted payload (after the header area).
        if buf.len() > 4 {
            buf[3] ^= 0xFF;
        }

        let result = decoder.decode(&mut buf);
        assert!(result.is_err(), "tampered data should produce an error");
    }
}
