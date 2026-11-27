use std::{env::consts::ARCH, io, sync::LazyLock};

use hmac::{Hmac, Mac};
use num_bigint::BigUint;
use num_integer::Integer;
use num_traits::{One, Zero};
use protobuf::Message;
use rand::RngCore;
use sha1::{Digest, Sha1};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::codec::{Decoder, Framed};

use super::codec::ApCodec;
use crate::version;

use spotify_proto::keyexchange::{
    APResponseMessage, ClientHello, ClientResponsePlaintext, Cryptosuite, Platform, Product,
    ProductFlags,
};

// ---------------------------------------------------------------------------
// DH parameters (768-bit MODP group)
// ---------------------------------------------------------------------------

static DH_GENERATOR: LazyLock<BigUint> = LazyLock::new(|| BigUint::from_bytes_be(&[0x02]));

static DH_PRIME: LazyLock<BigUint> = LazyLock::new(|| {
    BigUint::from_bytes_be(&[
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xc9, 0x0f, 0xda, 0xa2, 0x21, 0x68, 0xc2,
        0x34, 0xc4, 0xc6, 0x62, 0x8b, 0x80, 0xdc, 0x1c, 0xd1, 0x29, 0x02, 0x4e, 0x08, 0x8a, 0x67,
        0xcc, 0x74, 0x02, 0x0b, 0xbe, 0xa6, 0x3b, 0x13, 0x9b, 0x22, 0x51, 0x4a, 0x08, 0x79, 0x8e,
        0x34, 0x04, 0xdd, 0xef, 0x95, 0x19, 0xb3, 0xcd, 0x3a, 0x43, 0x1b, 0x30, 0x2b, 0x0a, 0x6d,
        0xf2, 0x5f, 0x14, 0x37, 0x4f, 0xe1, 0x35, 0x6d, 0x6d, 0x51, 0xc2, 0x45, 0xe4, 0x85, 0xb5,
        0x76, 0x62, 0x5e, 0x7e, 0xc6, 0xf4, 0x4c, 0x42, 0xe9, 0xa6, 0x3a, 0x36, 0x20, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
    ])
});

/// Spotify server RSA public key (modulus, 2048-bit).
/// The exponent is the standard 65537 (0x10001).
const SERVER_KEY: [u8; 256] = [
    0xac, 0xe0, 0x46, 0x0b, 0xff, 0xc2, 0x30, 0xaf, 0xf4, 0x6b, 0xfe, 0xc3, 0xbf, 0xbf, 0x86, 0x3d,
    0xa1, 0x91, 0xc6, 0xcc, 0x33, 0x6c, 0x93, 0xa1, 0x4f, 0xb3, 0xb0, 0x16, 0x12, 0xac, 0xac, 0x6a,
    0xf1, 0x80, 0xe7, 0xf6, 0x14, 0xd9, 0x42, 0x9d, 0xbe, 0x2e, 0x34, 0x66, 0x43, 0xe3, 0x62, 0xd2,
    0x32, 0x7a, 0x1a, 0x0d, 0x92, 0x3b, 0xae, 0xdd, 0x14, 0x02, 0xb1, 0x81, 0x55, 0x05, 0x61, 0x04,
    0xd5, 0x2c, 0x96, 0xa4, 0x4c, 0x1e, 0xcc, 0x02, 0x4a, 0xd4, 0xb2, 0x0c, 0x00, 0x1f, 0x17, 0xed,
    0xc2, 0x2f, 0xc4, 0x35, 0x21, 0xc8, 0xf0, 0xcb, 0xae, 0xd2, 0xad, 0xd7, 0x2b, 0x0f, 0x9d, 0xb3,
    0xc5, 0x32, 0x1a, 0x2a, 0xfe, 0x59, 0xf3, 0x5a, 0x0d, 0xac, 0x68, 0xf1, 0xfa, 0x62, 0x1e, 0xfb,
    0x2c, 0x8d, 0x0c, 0xb7, 0x39, 0x2d, 0x92, 0x47, 0xe3, 0xd7, 0x35, 0x1a, 0x6d, 0xbd, 0x24, 0xc2,
    0xae, 0x25, 0x5b, 0x88, 0xff, 0xab, 0x73, 0x29, 0x8a, 0x0b, 0xcc, 0xcd, 0x0c, 0x58, 0x67, 0x31,
    0x89, 0xe8, 0xbd, 0x34, 0x80, 0x78, 0x4a, 0x5f, 0xc9, 0x6b, 0x89, 0x9d, 0x95, 0x6b, 0xfc, 0x86,
    0xd7, 0x4f, 0x33, 0xa6, 0x78, 0x17, 0x96, 0xc9, 0xc3, 0x2d, 0x0d, 0x32, 0xa5, 0xab, 0xcd, 0x05,
    0x27, 0xe2, 0xf7, 0x10, 0xa3, 0x96, 0x13, 0xc4, 0x2f, 0x99, 0xc0, 0x27, 0xbf, 0xed, 0x04, 0x9c,
    0x3c, 0x27, 0x58, 0x04, 0xb6, 0xb2, 0x19, 0xf9, 0xc1, 0x2f, 0x02, 0xe9, 0x48, 0x63, 0xec, 0xa1,
    0xb6, 0x42, 0xa0, 0x9d, 0x48, 0x25, 0xf8, 0xb3, 0x9d, 0xd0, 0xe8, 0x6a, 0xf9, 0x48, 0x4d, 0xa1,
    0xc2, 0xba, 0x86, 0x30, 0x42, 0xea, 0x9d, 0xb3, 0x08, 0x6c, 0x19, 0x0e, 0x48, 0xb3, 0x9d, 0x66,
    0xeb, 0x00, 0x06, 0xa2, 0x5a, 0xee, 0xa1, 0x1b, 0x13, 0x87, 0x3c, 0xd7, 0x19, 0xe6, 0x55, 0xbd,
];

// ---------------------------------------------------------------------------
// DH key pair
// ---------------------------------------------------------------------------

/// Modular exponentiation: `base^exp mod modulus`.
fn powm(base: &BigUint, exp: &BigUint, modulus: &BigUint) -> BigUint {
    let mut base = base.clone();
    let mut exp = exp.clone();
    let mut result: BigUint = One::one();

    while !exp.is_zero() {
        if exp.is_odd() {
            result = (result * &base) % modulus;
        }
        exp >>= 1;
        base = (&base * &base) % modulus;
    }

    result
}

struct DhLocalKeys {
    private_key: BigUint,
    public_key: BigUint,
}

impl DhLocalKeys {
    fn random() -> Self {
        let mut bytes = [0u8; 95];
        rand::rng().fill_bytes(&mut bytes);
        let private_key = BigUint::from_bytes_le(&bytes);
        let public_key = powm(&DH_GENERATOR, &private_key, &DH_PRIME);
        Self {
            private_key,
            public_key,
        }
    }

    fn public_key_bytes(&self) -> Vec<u8> {
        self.public_key.to_bytes_be()
    }

    fn shared_secret(&self, remote_key: &[u8]) -> Vec<u8> {
        let remote = BigUint::from_bytes_be(remote_key);
        powm(&remote, &self.private_key, &DH_PRIME).to_bytes_be()
    }
}

// ---------------------------------------------------------------------------
// Handshake
// ---------------------------------------------------------------------------

/// Perform the Spotify AP DH + RSA key-exchange handshake over `connection`,
/// returning a `Framed` transport with the negotiated Shannon cipher keys.
pub async fn handshake<T: AsyncRead + AsyncWrite + Unpin>(
    mut connection: T,
) -> io::Result<Framed<T, ApCodec>> {
    let local_keys = DhLocalKeys::random();
    let gc = local_keys.public_key_bytes();

    let mut accumulator = send_client_hello(&mut connection, gc).await?;
    let ap_response: APResponseMessage = recv_packet(&mut connection, &mut accumulator).await?;

    let challenge = ap_response.challenge.get_or_default();
    let dh_challenge = challenge
        .login_crypto_challenge
        .get_or_default()
        .diffie_hellman
        .get_or_default();

    let remote_key = dh_challenge.gs().to_owned();
    let remote_signature = dh_challenge.gs_signature().to_owned();

    // Verify the server's RSA signature over the DH public key to prevent
    // man-in-the-middle attacks.
    verify_server_key(&remote_key, &remote_signature)?;

    let shared_secret = local_keys.shared_secret(&remote_key);
    let (challenge_response, send_key, recv_key) = compute_keys(&shared_secret, &accumulator)?;

    let codec = ApCodec::new(&send_key, &recv_key);

    send_client_response(&mut connection, challenge_response).await?;

    Ok(codec.framed(connection))
}

/// Verify the server's RSA signature (PKCS#1 v1.5 / SHA-1) over the DH
/// public key `gs`.
fn verify_server_key(gs: &[u8], signature: &[u8]) -> io::Result<()> {
    use rsa::{BigUint as RsaBigUint, Pkcs1v15Sign, RsaPublicKey};

    let n = RsaBigUint::from_bytes_be(&SERVER_KEY);
    let e = RsaBigUint::new(vec![65537]);
    let public_key = RsaPublicKey::new(n, e).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid RSA server key: {e}"),
        )
    })?;

    let hash = Sha1::digest(gs);
    let padding = Pkcs1v15Sign::new::<Sha1>();
    public_key.verify(padding, &hash, signature).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "server DH key RSA signature verification failed",
        )
    })
}

// ---------------------------------------------------------------------------
// ClientHello
// ---------------------------------------------------------------------------

fn detect_platform() -> Platform {
    match std::env::consts::OS {
        "freebsd" | "netbsd" | "openbsd" => match ARCH {
            "x86_64" => Platform::PLATFORM_FREEBSD_X86_64,
            _ => Platform::PLATFORM_FREEBSD_X86,
        },
        "ios" => match ARCH {
            "aarch64" => Platform::PLATFORM_IPHONE_ARM64,
            _ => Platform::PLATFORM_IPHONE_ARM,
        },
        "linux" | "android" => match ARCH {
            "arm" | "aarch64" => Platform::PLATFORM_LINUX_ARM,
            "blackfin" => Platform::PLATFORM_LINUX_BLACKFIN,
            "mips" => Platform::PLATFORM_LINUX_MIPS,
            "sh" => Platform::PLATFORM_LINUX_SH,
            "x86_64" => Platform::PLATFORM_LINUX_X86_64,
            _ => Platform::PLATFORM_LINUX_X86,
        },
        "macos" => match ARCH {
            "ppc" | "ppc64" => Platform::PLATFORM_OSX_PPC,
            "x86_64" => Platform::PLATFORM_OSX_X86_64,
            _ => Platform::PLATFORM_OSX_X86,
        },
        "windows" => match ARCH {
            "arm" | "aarch64" => Platform::PLATFORM_WINDOWS_CE_ARM,
            "x86_64" => Platform::PLATFORM_WIN32_X86_64,
            _ => Platform::PLATFORM_WIN32_X86,
        },
        _ => Platform::PLATFORM_LINUX_X86,
    }
}

async fn send_client_hello<T: AsyncWrite + Unpin>(
    connection: &mut T,
    gc: Vec<u8>,
) -> io::Result<Vec<u8>> {
    let mut client_nonce = vec![0u8; 0x10];
    rand::rng().fill_bytes(&mut client_nonce);

    let platform = detect_platform();

    #[cfg(debug_assertions)]
    const PRODUCT_FLAGS: ProductFlags = ProductFlags::PRODUCT_FLAG_DEV_BUILD;
    #[cfg(not(debug_assertions))]
    const PRODUCT_FLAGS: ProductFlags = ProductFlags::PRODUCT_FLAG_NONE;

    let mut packet = ClientHello::new();
    packet
        .build_info
        .mut_or_insert_default()
        .set_product(Product::PRODUCT_CLIENT);
    packet
        .build_info
        .mut_or_insert_default()
        .product_flags
        .push(PRODUCT_FLAGS.into());
    packet
        .build_info
        .mut_or_insert_default()
        .set_platform(platform);
    packet
        .build_info
        .mut_or_insert_default()
        .set_version(version::NUMERIC_VERSION as u64);
    packet
        .cryptosuites_supported
        .push(Cryptosuite::CRYPTO_SUITE_SHANNON.into());
    packet
        .login_crypto_hello
        .mut_or_insert_default()
        .diffie_hellman
        .mut_or_insert_default()
        .set_gc(gc);
    packet
        .login_crypto_hello
        .mut_or_insert_default()
        .diffie_hellman
        .mut_or_insert_default()
        .set_server_keys_known(1);
    packet.set_client_nonce(client_nonce);
    packet.set_padding(vec![0x1e]);

    // Wire format: [0x00, 0x04] magic + 4-byte BE size + protobuf
    let mut buffer = vec![0, 4];
    let size: u32 = 2 + 4 + packet.compute_size() as u32;
    buffer.extend_from_slice(&size.to_be_bytes());
    packet
        .write_to_vec(&mut buffer)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    connection.write_all(&buffer).await?;
    Ok(buffer)
}

// ---------------------------------------------------------------------------
// ClientResponsePlaintext
// ---------------------------------------------------------------------------

async fn send_client_response<T: AsyncWrite + Unpin>(
    connection: &mut T,
    challenge: Vec<u8>,
) -> io::Result<()> {
    let mut packet = ClientResponsePlaintext::new();
    packet
        .login_crypto_response
        .mut_or_insert_default()
        .diffie_hellman
        .mut_or_insert_default()
        .set_hmac(challenge);
    packet.pow_response.mut_or_insert_default();
    packet.crypto_response.mut_or_insert_default();

    let mut buffer = Vec::new();
    let size: u32 = 4 + packet.compute_size() as u32;
    buffer.extend_from_slice(&size.to_be_bytes());
    packet
        .write_to_vec(&mut buffer)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    connection.write_all(&buffer).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Receive a protobuf packet (4-byte BE length prefix)
// ---------------------------------------------------------------------------

async fn recv_packet<T, M>(connection: &mut T, acc: &mut Vec<u8>) -> io::Result<M>
where
    T: AsyncRead + Unpin,
    M: Message,
{
    let header = read_into_accumulator(connection, 4, acc).await?;
    let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as usize;
    if size < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "AP response packet size too small",
        ));
    }
    let data = read_into_accumulator(connection, size - 4, acc).await?;
    M::parse_from_bytes(data).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

async fn read_into_accumulator<'b, T: AsyncRead + Unpin>(
    connection: &mut T,
    size: usize,
    acc: &'b mut Vec<u8>,
) -> io::Result<&'b mut [u8]> {
    let offset = acc.len();
    acc.resize(offset + size, 0);
    connection.read_exact(&mut acc[offset..]).await?;
    Ok(&mut acc[offset..])
}

// ---------------------------------------------------------------------------
// Key derivation: 5 rounds of HMAC-SHA1 → 100 bytes
// ---------------------------------------------------------------------------

type HmacSha1 = Hmac<Sha1>;

fn compute_keys(shared_secret: &[u8], packets: &[u8]) -> io::Result<(Vec<u8>, Vec<u8>, Vec<u8>)> {
    let mut data = Vec::with_capacity(0x64);
    for i in 1u8..6 {
        let mut mac = HmacSha1::new_from_slice(shared_secret)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        mac.update(packets);
        mac.update(&[i]);
        data.extend_from_slice(&mac.finalize().into_bytes());
    }

    let mut mac = HmacSha1::new_from_slice(&data[..0x14])
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    mac.update(packets);

    Ok((
        mac.finalize().into_bytes().to_vec(), // challenge HMAC
        data[0x14..0x34].to_vec(),            // Shannon send key
        data[0x34..0x54].to_vec(),            // Shannon recv key
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dh_key_generation() {
        let keys = DhLocalKeys::random();
        let pub_bytes = keys.public_key_bytes();
        // The 768-bit prime gives a public key of at most 96 bytes.
        assert!(!pub_bytes.is_empty());
        assert!(pub_bytes.len() <= 96);
    }

    #[test]
    fn dh_shared_secret_matches() {
        let alice = DhLocalKeys::random();
        let bob = DhLocalKeys::random();

        let secret_ab = alice.shared_secret(&bob.public_key_bytes());
        let secret_ba = bob.shared_secret(&alice.public_key_bytes());
        assert_eq!(secret_ab, secret_ba);
    }

    #[test]
    fn compute_keys_produces_correct_lengths() {
        let shared = vec![0x42u8; 32];
        let packets = b"fake accumulator data";
        let (challenge, send_key, recv_key) =
            compute_keys(&shared, packets).expect("compute_keys should succeed");
        // HMAC-SHA1 output is 20 bytes.
        assert_eq!(challenge.len(), 20);
        // Shannon keys are 32 bytes each (0x34 - 0x14 = 0x20).
        assert_eq!(send_key.len(), 32);
        assert_eq!(recv_key.len(), 32);
    }

    #[test]
    fn powm_basic() {
        // 2^10 mod 1000 = 1024 mod 1000 = 24
        let result = powm(
            &BigUint::from(2u32),
            &BigUint::from(10u32),
            &BigUint::from(1000u32),
        );
        assert_eq!(result, BigUint::from(24u32));
    }

    #[test]
    fn detect_platform_returns_value() {
        // Just check it does not panic.
        let _ = detect_platform();
    }
}
