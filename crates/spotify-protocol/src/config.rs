//! Session configuration.

use url::Url;

/// Configuration for a [`Session`](crate::session::Session).
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// OAuth client id.  Defaults to the official desktop client id.
    pub client_id: String,
    /// Unique device identifier.  Defaults to a random 20-byte hex string
    /// matching Spotify's format.
    pub device_id: String,
    /// Optional HTTPS proxy for all outbound connections.
    pub proxy: Option<Url>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            client_id: crate::version::CLIENT_ID.to_owned(),
            device_id: generate_device_id(),
            proxy: None,
        }
    }
}

/// Generate a random 20-byte hex-encoded device id (40 hex chars), matching
/// the format Spotify clients use.
pub fn generate_device_id() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 20];
    rand::rng().fill_bytes(&mut buf);
    hex::encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_format() {
        let id = generate_device_id();
        assert_eq!(id.len(), 40);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn device_id_unique() {
        let a = generate_device_id();
        let b = generate_device_id();
        assert_ne!(a, b);
    }

    #[test]
    fn default_config() {
        let cfg = SessionConfig::default();
        assert_eq!(cfg.client_id, crate::version::CLIENT_ID);
        assert_eq!(cfg.device_id.len(), 40);
        assert!(cfg.proxy.is_none());
    }
}
