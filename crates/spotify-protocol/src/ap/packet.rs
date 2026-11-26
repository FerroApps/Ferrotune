use num_derive::{FromPrimitive, ToPrimitive};

/// Spotify AP protocol packet command byte.
///
/// Values taken from the official wire format; keep in sync with the Spotify
/// desktop client and librespot.
#[derive(Debug, Copy, Clone, PartialEq, Eq, FromPrimitive, ToPrimitive)]
#[repr(u8)]
pub enum PacketType {
    SecretBlock = 0x02,
    Ping = 0x04,
    StreamChunk = 0x08,
    StreamChunkRes = 0x09,
    ChannelError = 0x0a,
    ChannelAbort = 0x0b,
    RequestKey = 0x0c,
    AesKey = 0x0d,
    AesKeyError = 0x0e,
    Image = 0x19,
    CountryCode = 0x1b,
    Pong = 0x49,
    PongAck = 0x4a,
    Pause = 0x4b,
    ProductInfo = 0x50,
    LegacyWelcome = 0x69,
    PreferredLocale = 0x74,
    LicenseVersion = 0x76,
    TrackEndedTime = 0x82,
    Login = 0xab,
    APWelcome = 0xac,
    AuthFailure = 0xad,
    MercuryReq = 0xb2,
    MercurySub = 0xb3,
    MercuryUnsub = 0xb4,
    MercuryEvent = 0xb5,
}

impl std::fmt::Display for PacketType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_traits::FromPrimitive;

    #[test]
    fn from_primitive_roundtrip() {
        let cases: &[(u8, PacketType)] = &[
            (0x02, PacketType::SecretBlock),
            (0x04, PacketType::Ping),
            (0x08, PacketType::StreamChunk),
            (0x09, PacketType::StreamChunkRes),
            (0x0a, PacketType::ChannelError),
            (0x0b, PacketType::ChannelAbort),
            (0x0c, PacketType::RequestKey),
            (0x0d, PacketType::AesKey),
            (0x0e, PacketType::AesKeyError),
            (0x19, PacketType::Image),
            (0x1b, PacketType::CountryCode),
            (0x49, PacketType::Pong),
            (0x4a, PacketType::PongAck),
            (0x4b, PacketType::Pause),
            (0x50, PacketType::ProductInfo),
            (0x69, PacketType::LegacyWelcome),
            (0x74, PacketType::PreferredLocale),
            (0x76, PacketType::LicenseVersion),
            (0x82, PacketType::TrackEndedTime),
            (0xab, PacketType::Login),
            (0xac, PacketType::APWelcome),
            (0xad, PacketType::AuthFailure),
            (0xb2, PacketType::MercuryReq),
            (0xb3, PacketType::MercurySub),
            (0xb4, PacketType::MercuryUnsub),
            (0xb5, PacketType::MercuryEvent),
        ];

        for &(byte, expected) in cases {
            let pt = PacketType::from_u8(byte)
                .unwrap_or_else(|| panic!("from_u8({byte:#04x}) should succeed"));
            assert_eq!(pt, expected, "mismatch for byte {byte:#04x}");
            assert_eq!(
                byte, expected as u8,
                "discriminant mismatch for {expected:?}"
            );
        }
    }

    #[test]
    fn unknown_byte_returns_none() {
        assert!(PacketType::from_u8(0xFF).is_none());
        assert!(PacketType::from_u8(0x00).is_none());
        assert!(PacketType::from_u8(0x01).is_none());
    }
}
