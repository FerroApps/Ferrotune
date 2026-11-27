//! Spotify AP packet type identifiers.

/// Known packet command bytes exchanged over the AP (Shannon-encrypted)
/// connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    Login = 0xab,
    APWelcome = 0xac,
    AuthFailure = 0xad,
    MercuryReq = 0xb2,
    MercurySub = 0xb3,
    MercuryUnsub = 0xb4,
    MercuryEvent = 0xb5,
}

impl PacketType {
    /// Try to map a raw command byte to a known packet type.
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x02 => Some(Self::SecretBlock),
            0x04 => Some(Self::Ping),
            0x08 => Some(Self::StreamChunk),
            0x09 => Some(Self::StreamChunkRes),
            0x0a => Some(Self::ChannelError),
            0x0b => Some(Self::ChannelAbort),
            0x0c => Some(Self::RequestKey),
            0x0d => Some(Self::AesKey),
            0x0e => Some(Self::AesKeyError),
            0x19 => Some(Self::Image),
            0x1b => Some(Self::CountryCode),
            0x49 => Some(Self::Pong),
            0x4a => Some(Self::PongAck),
            0x4b => Some(Self::Pause),
            0x50 => Some(Self::ProductInfo),
            0x69 => Some(Self::LegacyWelcome),
            0x74 => Some(Self::PreferredLocale),
            0x76 => Some(Self::LicenseVersion),
            0xab => Some(Self::Login),
            0xac => Some(Self::APWelcome),
            0xad => Some(Self::AuthFailure),
            0xb2 => Some(Self::MercuryReq),
            0xb3 => Some(Self::MercurySub),
            0xb4 => Some(Self::MercuryUnsub),
            0xb5 => Some(Self::MercuryEvent),
            _ => None,
        }
    }
}
