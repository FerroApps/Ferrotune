/// Full version string from the official Spotify desktop binary.
pub const VERSION_STRING: &str = "1.3.8.620.g39b276d5d7ac";

/// Semantic portion: `major.minor.patch.build`.
pub const SEMANTIC_VERSION: &str = "1.3.8.620";

/// Numeric encoding: `major*100_000_000 + minor*1_000_000 + patch*1_000 + build`.
pub const NUMERIC_VERSION: u32 = 103_008_620;

/// User-Agent header sent by the official client.
pub const USER_AGENT: &str = "Spotify/103008620 Linux/0 (desktop)";

/// OAuth client id used by the official desktop client.
pub const CLIENT_ID: &str = "65b708073fc0480ea92a077233ca87bd";

/// Platform identifier sent in device registration.
pub const PLATFORM: &str = "linux-x86_64";
