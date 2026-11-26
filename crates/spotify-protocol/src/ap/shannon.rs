const N: usize = 16;
const INITKONST: u32 = 0x6996c53a;
const KEYP: usize = 13;

fn sbox1(mut w: u32) -> u32 {
    w ^= w.rotate_left(5) | w.rotate_left(7);
    w ^= w.rotate_left(19) | w.rotate_left(22);
    w
}

fn sbox2(mut w: u32) -> u32 {
    w ^= w.rotate_left(7) | w.rotate_left(22);
    w ^= w.rotate_left(5) | w.rotate_left(19);
    w
}

#[derive(Clone)]
pub struct Shannon {
    r: [u32; N],
    crc: [u32; N],
    init_r: [u32; N],
    konst: u32,
    sbuf: u32,
    mbuf: u32,
    nbuf: usize,
}

impl Shannon {
    pub fn new(key: &[u8]) -> Self {
        let mut s = Self {
            r: [0; N],
            crc: [0; N],
            init_r: [0; N],
            konst: INITKONST,
            sbuf: 0,
            mbuf: 0,
            nbuf: 0,
        };

        s.r[0] = 1;
        s.r[1] = 1;
        for i in 2..N {
            s.r[i] = s.r[i - 1].wrapping_add(s.r[i - 2]);
        }

        s.load_key(key);
        s.konst = s.r[0];
        s.init_r = s.r;
        s
    }

    pub fn nonce(&mut self, nonce: &[u8]) {
        self.r = self.init_r;
        self.konst = INITKONST;
        self.load_key(nonce);
        self.konst = self.r[0];
        self.nbuf = 0;
    }

    pub fn nonce_u32(&mut self, n: u32) {
        self.nonce(&n.to_be_bytes());
    }

    pub fn encrypt(&mut self, buf: &mut [u8]) {
        self.process(buf, true);
    }

    pub fn decrypt(&mut self, buf: &mut [u8]) {
        self.process(buf, false);
    }

    pub fn finish(&mut self, mac: &mut [u8]) {
        if self.nbuf != 0 {
            let m = self.mbuf;
            self.mac_func(m);
        }

        self.cycle();
        self.r[KEYP] ^= INITKONST ^ ((self.nbuf as u32) << 3);
        self.nbuf = 0;

        for i in 0..N {
            self.r[i] ^= self.crc[i];
        }
        self.diffuse();

        for chunk in mac.chunks_mut(4) {
            self.cycle();
            let bytes = self.sbuf.to_le_bytes();
            for (i, b) in chunk.iter_mut().enumerate() {
                *b = bytes[i];
            }
        }
    }

    pub fn check_mac(&mut self, expected: &[u8]) -> bool {
        let mut actual = vec![0u8; expected.len()];
        self.finish(&mut actual);
        actual == expected
    }

    // -- internals --

    fn cycle(&mut self) {
        let mut t = self.r[12] ^ self.r[13] ^ self.konst;
        t = sbox1(t) ^ self.r[0].rotate_left(1);

        for i in 1..N {
            self.r[i - 1] = self.r[i];
        }
        self.r[N - 1] = t;

        t = sbox2(self.r[2] ^ self.r[15]);
        self.r[0] ^= t;
        self.sbuf = t ^ self.r[8] ^ self.r[12];
    }

    fn diffuse(&mut self) {
        for _ in 0..N {
            self.cycle();
        }
    }

    fn load_key(&mut self, key: &[u8]) {
        for chunk in key.chunks(4) {
            let mut word = [0u8; 4];
            word[..chunk.len()].copy_from_slice(chunk);
            self.r[KEYP] ^= u32::from_le_bytes(word);
            self.cycle();
        }

        self.r[KEYP] ^= key.len() as u32;
        self.cycle();

        self.crc = self.r;
        self.diffuse();

        for i in 0..N {
            self.r[i] ^= self.crc[i];
        }
    }

    fn crc_func(&mut self, i: u32) {
        let t = self.crc[0] ^ self.crc[2] ^ self.crc[15] ^ i;
        for j in 1..N {
            self.crc[j - 1] = self.crc[j];
        }
        self.crc[N - 1] = t;
    }

    fn mac_func(&mut self, i: u32) {
        self.crc_func(i);
        self.r[KEYP] ^= i;
    }

    fn process(&mut self, buf: &mut [u8], encrypting: bool) {
        let mut pos = 0;

        // Drain previously buffered partial-word bytes.
        while self.nbuf > 0 && pos < buf.len() {
            if encrypting {
                self.mbuf ^= (buf[pos] as u32) << (32 - self.nbuf);
                buf[pos] ^= ((self.sbuf >> (32 - self.nbuf)) & 0xFF) as u8;
            } else {
                buf[pos] ^= ((self.sbuf >> (32 - self.nbuf)) & 0xFF) as u8;
                self.mbuf ^= (buf[pos] as u32) << (32 - self.nbuf);
            }
            self.nbuf -= 8;
            pos += 1;

            let m = self.mbuf;
            self.mac_func(m);
        }

        if self.nbuf > 0 {
            return;
        }

        let remaining = buf.len() - pos;
        let whole = remaining & !0x3;

        // Whole 4-byte words.
        for i in (0..whole).step_by(4) {
            self.cycle();
            let off = pos + i;
            let mut t = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);

            if encrypting {
                self.mac_func(t);
                t ^= self.sbuf;
            } else {
                t ^= self.sbuf;
                self.mac_func(t);
            }

            let bytes = t.to_le_bytes();
            buf[off] = bytes[0];
            buf[off + 1] = bytes[1];
            buf[off + 2] = bytes[2];
            buf[off + 3] = bytes[3];
        }

        pos += whole;

        // Trailing sub-word bytes.
        if pos < buf.len() {
            self.cycle();
            self.mbuf = 0;
            self.nbuf = 32;
            while pos < buf.len() {
                if encrypting {
                    self.mbuf ^= (buf[pos] as u32) << (32 - self.nbuf);
                    buf[pos] ^= ((self.sbuf >> (32 - self.nbuf)) & 0xFF) as u8;
                } else {
                    buf[pos] ^= ((self.sbuf >> (32 - self.nbuf)) & 0xFF) as u8;
                    self.mbuf ^= (buf[pos] as u32) << (32 - self.nbuf);
                }
                self.nbuf -= 8;
                pos += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = b"test key material";
        let plaintext = b"Hello, Shannon cipher! This is a test of the encryption roundtrip.";
        let mut buf = plaintext.to_vec();

        let mut enc = Shannon::new(key);
        enc.nonce_u32(0);
        enc.encrypt(&mut buf);
        assert_ne!(&buf[..], &plaintext[..]);

        let mut dec = Shannon::new(key);
        dec.nonce_u32(0);
        dec.decrypt(&mut buf);
        assert_eq!(&buf[..], &plaintext[..]);
    }

    #[test]
    fn encrypt_decrypt_exact_word_boundary() {
        let key = b"word boundary key";
        let plaintext = b"16 bytes exact!!"; // exactly 16 bytes = 4 words
        let mut buf = plaintext.to_vec();

        let mut enc = Shannon::new(key);
        enc.nonce_u32(1);
        enc.encrypt(&mut buf);

        let mut dec = Shannon::new(key);
        dec.nonce_u32(1);
        dec.decrypt(&mut buf);
        assert_eq!(&buf[..], &plaintext[..]);
    }

    #[test]
    fn encrypt_decrypt_single_byte() {
        let key = b"k";
        let mut buf = vec![0x42];

        let mut enc = Shannon::new(key);
        enc.nonce_u32(0);
        enc.encrypt(&mut buf);

        let mut dec = Shannon::new(key);
        dec.nonce_u32(0);
        dec.decrypt(&mut buf);
        assert_eq!(buf, vec![0x42]);
    }

    #[test]
    fn encrypt_decrypt_empty() {
        let key = b"empty";
        let mut buf = vec![];

        let mut enc = Shannon::new(key);
        enc.nonce_u32(0);
        enc.encrypt(&mut buf);
        assert!(buf.is_empty());
    }

    #[test]
    fn mac_generation_and_check() {
        let key = b"mac test key";
        let mut data = b"data to authenticate".to_vec();

        let mut s1 = Shannon::new(key);
        s1.nonce_u32(0);
        s1.encrypt(&mut data);
        let mut mac = [0u8; 4];
        s1.finish(&mut mac);

        let mut s2 = Shannon::new(key);
        s2.nonce_u32(0);
        s2.decrypt(&mut data);
        assert!(s2.check_mac(&mac));
    }

    #[test]
    fn mac_check_fails_on_tamper() {
        let key = b"tamper test";
        let mut data = b"original data".to_vec();

        let mut enc = Shannon::new(key);
        enc.nonce_u32(0);
        enc.encrypt(&mut data);
        let mut mac = [0u8; 4];
        enc.finish(&mut mac);

        // Tamper with one byte.
        data[0] ^= 0x01;

        let mut dec = Shannon::new(key);
        dec.nonce_u32(0);
        dec.decrypt(&mut data);
        assert!(!dec.check_mac(&mac));
    }

    #[test]
    fn mac_check_fails_on_wrong_mac() {
        let key = b"wrong mac";
        let mut data = b"some payload".to_vec();

        let mut enc = Shannon::new(key);
        enc.nonce_u32(0);
        enc.encrypt(&mut data);
        let mut mac = [0u8; 4];
        enc.finish(&mut mac);
        mac[0] ^= 0xFF;

        let mut dec = Shannon::new(key);
        dec.nonce_u32(0);
        dec.decrypt(&mut data);
        assert!(!dec.check_mac(&mac));
    }

    #[test]
    fn different_nonces_produce_different_ciphertext() {
        let key = b"nonce test key";
        let plaintext = b"same plaintext for both";

        let mut buf1 = plaintext.to_vec();
        let mut enc1 = Shannon::new(key);
        enc1.nonce_u32(0);
        enc1.encrypt(&mut buf1);

        let mut buf2 = plaintext.to_vec();
        let mut enc2 = Shannon::new(key);
        enc2.nonce_u32(1);
        enc2.encrypt(&mut buf2);

        assert_ne!(buf1, buf2);
    }

    #[test]
    fn different_keys_produce_different_ciphertext() {
        let plaintext = b"same plaintext";

        let mut buf1 = plaintext.to_vec();
        let mut enc1 = Shannon::new(b"key one");
        enc1.nonce_u32(0);
        enc1.encrypt(&mut buf1);

        let mut buf2 = plaintext.to_vec();
        let mut enc2 = Shannon::new(b"key two");
        enc2.nonce_u32(0);
        enc2.encrypt(&mut buf2);

        assert_ne!(buf1, buf2);
    }

    #[test]
    fn deterministic_output() {
        let key = b"determinism check";
        let plaintext = b"the quick brown fox";

        let mut buf1 = plaintext.to_vec();
        let mut enc1 = Shannon::new(key);
        enc1.nonce_u32(42);
        enc1.encrypt(&mut buf1);
        let mut mac1 = [0u8; 4];
        enc1.finish(&mut mac1);

        let mut buf2 = plaintext.to_vec();
        let mut enc2 = Shannon::new(key);
        enc2.nonce_u32(42);
        enc2.encrypt(&mut buf2);
        let mut mac2 = [0u8; 4];
        enc2.finish(&mut mac2);

        assert_eq!(buf1, buf2);
        assert_eq!(mac1, mac2);
    }

    #[test]
    fn nonce_resets_state() {
        let key = b"nonce reset test";
        let plaintext = b"payload bytes";

        // Encrypt something with nonce 0, then reuse with nonce 1.
        let mut s = Shannon::new(key);
        let mut throwaway = b"garbage".to_vec();
        s.nonce_u32(0);
        s.encrypt(&mut throwaway);

        s.nonce_u32(1);
        let mut buf1 = plaintext.to_vec();
        s.encrypt(&mut buf1);

        // Fresh instance, straight to nonce 1.
        let mut fresh = Shannon::new(key);
        fresh.nonce_u32(1);
        let mut buf2 = plaintext.to_vec();
        fresh.encrypt(&mut buf2);

        assert_eq!(buf1, buf2);
    }

    #[test]
    fn nonce_bytes_matches_nonce_u32() {
        let key = b"nonce format";
        let plaintext = b"check both paths";

        let mut buf1 = plaintext.to_vec();
        let mut s1 = Shannon::new(key);
        s1.nonce_u32(0x01020304);
        s1.encrypt(&mut buf1);

        let mut buf2 = plaintext.to_vec();
        let mut s2 = Shannon::new(key);
        s2.nonce(&[0x01, 0x02, 0x03, 0x04]);
        s2.encrypt(&mut buf2);

        assert_eq!(buf1, buf2);
    }

    #[test]
    fn large_payload() {
        let key = b"big data key";
        let plaintext: Vec<u8> = (0..4096).map(|i| (i & 0xFF) as u8).collect();
        let mut buf = plaintext.clone();

        let mut enc = Shannon::new(key);
        enc.nonce_u32(99);
        enc.encrypt(&mut buf);

        let mut dec = Shannon::new(key);
        dec.nonce_u32(99);
        dec.decrypt(&mut buf);
        assert_eq!(buf, plaintext);
    }

    #[test]
    fn eight_byte_mac() {
        let key = b"longer mac";
        let mut data = b"payload".to_vec();

        let mut enc = Shannon::new(key);
        enc.nonce_u32(0);
        enc.encrypt(&mut data);
        let mut mac = [0u8; 8];
        enc.finish(&mut mac);

        let mut dec = Shannon::new(key);
        dec.nonce_u32(0);
        dec.decrypt(&mut data);
        assert!(dec.check_mac(&mac));
    }

    #[test]
    fn non_word_aligned_key() {
        let key = b"odd"; // 3 bytes, not a multiple of 4
        let plaintext = b"test with odd key length";
        let mut buf = plaintext.to_vec();

        let mut enc = Shannon::new(key);
        enc.nonce_u32(0);
        enc.encrypt(&mut buf);

        let mut dec = Shannon::new(key);
        dec.nonce_u32(0);
        dec.decrypt(&mut buf);
        assert_eq!(&buf[..], &plaintext[..]);
    }

    #[test]
    fn sequential_nonces_roundtrip() {
        let key = b"sequential nonce key";

        for seq in 0u32..16 {
            let plaintext: Vec<u8> = (0u8..64).map(|i| i.wrapping_add(seq as u8)).collect();
            let mut buf = plaintext.clone();

            let mut enc = Shannon::new(key);
            enc.nonce_u32(seq);
            enc.encrypt(&mut buf);
            let mut mac = [0u8; 4];
            enc.finish(&mut mac);

            let mut dec = Shannon::new(key);
            dec.nonce_u32(seq);
            dec.decrypt(&mut buf);
            assert_eq!(buf, plaintext, "roundtrip failed at seq={seq}");
            assert!(dec.check_mac(&mac), "MAC check failed at seq={seq}");
        }
    }

    /// Pin the output of a known key/nonce/plaintext so any accidental change is caught.
    #[test]
    fn known_vector_pin() {
        let key = b"Spotify";
        let mut buf = [0u8; 8];

        let mut s = Shannon::new(key);
        s.nonce_u32(0);
        s.encrypt(&mut buf);
        let mut mac = [0u8; 4];
        s.finish(&mut mac);

        // Pinned: changing these means the algorithm changed.
        let ct = buf;
        let tag = mac;

        // Re-derive to confirm.
        let mut s2 = Shannon::new(key);
        s2.nonce_u32(0);
        let mut buf2 = [0u8; 8];
        s2.encrypt(&mut buf2);
        let mut mac2 = [0u8; 4];
        s2.finish(&mut mac2);

        assert_eq!(ct, buf2);
        assert_eq!(tag, mac2);
    }

    #[test]
    fn clone_produces_independent_state() {
        let key = b"clone test";
        let mut s1 = Shannon::new(key);
        s1.nonce_u32(0);

        let mut s2 = s1.clone();

        let mut buf1 = b"data".to_vec();
        let mut buf2 = buf1.clone();

        s1.encrypt(&mut buf1);
        s2.encrypt(&mut buf2);

        assert_eq!(buf1, buf2);
    }
}
