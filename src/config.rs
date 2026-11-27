//! Runtime configuration: XDG paths, preferences and (advanced) env overrides.
//!
//! Precedence: environment variables > `config.json` > defaults.
//! `config.json` lives at `$XDG_CONFIG_HOME/ferrotune/config.json`; it is
//! optional and nothing in it is required to log in.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "dev.ferro.ferrotune";
pub const APP_NAME: &str = "Ferrotune";
pub const DEFAULT_REDIRECT_PORT: u16 = 8898;
pub const SPOTIFY_ACCOUNTS: &str = "https://accounts.spotify.com";
/// GraphQL "pathfinder" host used by the desktop and web clients.
pub const SPOTIFY_PATHFINDER: &str = "https://api-partner.spotify.com";

/// Scopes requested at the browser login. This is the list the `librespot`
/// binary requests with the same desktop client id; the login token is only
/// used once to open the librespot session, which then issues its own tokens.
pub const SCOPES: &[&str] = &[
    "app-remote-control",
    "playlist-modify",
    "playlist-modify-private",
    "playlist-modify-public",
    "playlist-read",
    "playlist-read-collaborative",
    "playlist-read-private",
    "streaming",
    "ugc-image-upload",
    "user-follow-modify",
    "user-follow-read",
    "user-library-modify",
    "user-library-read",
    "user-modify",
    "user-modify-playback-state",
    "user-modify-private",
    "user-personalized",
    "user-read-birthdate",
    "user-read-currently-playing",
    "user-read-email",
    "user-read-play-history",
    "user-read-playback-position",
    "user-read-playback-state",
    "user-read-private",
    "user-read-recently-played",
    "user-top-read",
];

/// The client id the official desktop client (and librespot) uses.
pub fn desktop_client_id() -> String {
    spotify_protocol::version::CLIENT_ID.to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
pub enum Bitrate {
    #[serde(rename = "96")]
    Low,
    #[serde(rename = "160")]
    #[default]
    Normal,
    #[serde(rename = "320")]
    High,
}

/// Which program plays audio on this computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioEngine {
    /// librespot inside Ferrotune.
    #[default]
    Builtin,
    /// Spotify Soloist, Spotify's official headless player (a separate
    /// process, downloaded from Spotify on first use).
    Soloist,
    /// Native engine: HTTP-based audio delivery (storage-resolve v2 / HLS),
    /// bypassing librespot's AP audio-key channel. Works on accounts where
    /// Spotify denies audio keys.
    Native,
}

/// Values that may be set in `config.json`. All optional.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct FileConfig {
    pub redirect_port: Option<u16>,
    pub theme: Option<ThemeChoice>,
    pub bitrate: Option<Bitrate>,
    /// Expose Ferrotune as a Spotify Connect device (default true).
    pub connect: Option<bool>,
    pub normalisation: Option<bool>,
    pub gapless: Option<bool>,
    pub autoplay: Option<bool>,
    /// Spotify Connect device name (empty = "Ferrotune on <host>").
    pub device_name: Option<String>,
    /// Play on this computer when Spotify allows it (default true).
    pub play_local: Option<bool>,
    pub engine: Option<AudioEngine>,
    /// PipeWire node for Spotify Soloist (empty = default output).
    pub soloist_output: Option<String>,
    /// Spotify Soloist audio cache in MB (default 500, at least 100).
    pub soloist_cache_mb: Option<u32>,
    /// Closing the window keeps Ferrotune running in the tray (default true).
    pub close_to_tray: Option<bool>,
    /// Start with no window, only the tray icon (default false).
    pub start_minimized: Option<bool>,
    pub auto_update_check: Option<bool>,
}

/// Preferences edited on the Settings screen. Nothing secret: they are
/// stored in `config.json` next to the hand-edited options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub bitrate: Bitrate,
    pub normalisation: bool,
    pub gapless: bool,
    pub autoplay: bool,
    /// Custom Connect device name; empty = the default.
    pub device_name: String,
    pub theme: ThemeChoice,
    pub play_local: bool,
    pub connect: bool,
    pub engine: AudioEngine,
    /// PipeWire node name or id for Spotify Soloist; empty = default.
    pub soloist_output: String,
    /// Spotify Soloist `--cache-size` in MB (never unlimited).
    pub soloist_cache_mb: u32,
    /// Closing the window hides it to the tray while a tray exists.
    pub close_to_tray: bool,
    /// Launch with only the tray icon, no window.
    pub start_minimized: bool,
    /// Look for new Ferrotune releases at start and every 6 hours.
    pub auto_update_check: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            bitrate: Bitrate::Normal,
            normalisation: false,
            gapless: true,
            autoplay: true,
            device_name: String::new(),
            theme: ThemeChoice::Dark,
            play_local: true,
            connect: true,
            engine: AudioEngine::Builtin,
            soloist_output: String::new(),
            soloist_cache_mb: crate::net::soloist::process::DEFAULT_CACHE_MB,
            close_to_tray: true,
            start_minimized: false,
            auto_update_check: true,
        }
    }
}

impl Settings {
    /// Write the settings into `config.json`, keeping every other key
    /// (unknown or hand-edited) as it is. Written atomically.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut root = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .filter(serde_json::Value::is_object)
            .unwrap_or_else(|| serde_json::json!({}));
        let v = |x: serde_json::Result<serde_json::Value>| x.unwrap_or_default();
        if let Some(m) = root.as_object_mut() {
            m.insert("bitrate".into(), v(serde_json::to_value(self.bitrate)));
            m.insert("normalisation".into(), self.normalisation.into());
            m.insert("gapless".into(), self.gapless.into());
            m.insert("autoplay".into(), self.autoplay.into());
            m.insert("device_name".into(), self.device_name.clone().into());
            m.insert("theme".into(), v(serde_json::to_value(self.theme)));
            m.insert("play_local".into(), self.play_local.into());
            m.insert("connect".into(), self.connect.into());
            m.insert("engine".into(), v(serde_json::to_value(self.engine)));
            m.insert("soloist_output".into(), self.soloist_output.clone().into());
            m.insert("soloist_cache_mb".into(), self.soloist_cache_mb.into());
            m.insert("close_to_tray".into(), self.close_to_tray.into());
            m.insert("start_minimized".into(), self.start_minimized.into());
            m.insert("auto_update_check".into(), self.auto_update_check.into());
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(&root).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    /// Client id for login and the librespot session: the desktop client id
    /// unless the hidden `FERROTUNE_CLIENT_ID` override is set.
    pub client_id: String,
    pub redirect_port: u16,
    /// Announce a Spotify Connect device ("Ferrotune on <host>").
    pub connect: bool,
    pub device_name: String,
    pub theme: ThemeChoice,
    pub bitrate: Bitrate,
    /// Settings-screen values as loaded at start.
    pub settings: Settings,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub state_dir: PathBuf,
    pub config_dir: PathBuf,
    pub accounts_base: String,
    /// spclient base override (tests / advanced). `None` = resolved by the
    /// librespot session's access-point resolver.
    pub spclient_base: Option<String>,
    pub pathfinder_base: String,
    /// Spotify Soloist download host (`FERROTUNE_SOLOIST_CDN` for tests).
    pub soloist_cdn: String,
    /// `--mock`: no network, mock data, in-memory secrets.
    pub mock: bool,
    /// Log `first_frame_ms` (FERRO_TRACE_STARTUP=1).
    pub trace_startup: bool,
}

impl Config {
    /// Load configuration from the environment and the config file.
    pub fn load(mock: bool) -> Self {
        let (data_dir, cache_dir, state_dir, config_dir) = dirs();
        let file = read_file_config(&config_dir.join("config.json"));
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        Self::resolve(
            file,
            Env {
                client_id: env("FERROTUNE_CLIENT_ID"),
                port: env("FERROTUNE_REDIRECT_PORT").and_then(|p| p.parse().ok()),
                theme: env("FERRO_THEME"),
                connect: env("FERROTUNE_CONNECT").map(|v| v != "0"),
                spclient: env("FERROTUNE_SPCLIENT_URL"),
                pathfinder: env("FERROTUNE_PATHFINDER_URL"),
                soloist_cdn: env("FERROTUNE_SOLOIST_CDN"),
            },
            Paths {
                data_dir,
                cache_dir,
                state_dir,
                config_dir,
            },
            mock,
            env("FERRO_TRACE_STARTUP").is_some_and(|v| v == "1"),
        )
    }

    fn resolve(file: FileConfig, env: Env, paths: Paths, mock: bool, trace_startup: bool) -> Self {
        let theme = match env.theme.as_deref() {
            Some("light") => ThemeChoice::Light,
            Some("dark") => ThemeChoice::Dark,
            _ => file.theme.unwrap_or_default(),
        };
        let settings = Settings {
            bitrate: file.bitrate.unwrap_or_default(),
            normalisation: file.normalisation.unwrap_or(false),
            gapless: file.gapless.unwrap_or(true),
            autoplay: file.autoplay.unwrap_or(true),
            device_name: file
                .device_name
                .clone()
                .map(|n| n.trim().chars().take(64).collect())
                .unwrap_or_default(),
            theme,
            play_local: file.play_local.unwrap_or(true),
            connect: env.connect.or(file.connect).unwrap_or(true),
            engine: file.engine.unwrap_or_default(),
            soloist_output: file
                .soloist_output
                .clone()
                .map(|n| {
                    n.trim()
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(128)
                        .collect()
                })
                .unwrap_or_default(),
            soloist_cache_mb: file
                .soloist_cache_mb
                .unwrap_or(crate::net::soloist::process::DEFAULT_CACHE_MB)
                .clamp(crate::net::soloist::process::MIN_CACHE_MB, 100_000),
            close_to_tray: file.close_to_tray.unwrap_or(true),
            start_minimized: file.start_minimized.unwrap_or(false),
            auto_update_check: file.auto_update_check.unwrap_or(true),
        };
        let device_name = if settings.device_name.is_empty() {
            device_name(hostname().as_deref())
        } else {
            settings.device_name.clone()
        };
        Self {
            settings,
            client_id: env
                .client_id
                .map(|c| c.trim().to_owned())
                .unwrap_or_else(desktop_client_id),
            redirect_port: env
                .port
                .or(file.redirect_port)
                .unwrap_or(DEFAULT_REDIRECT_PORT),
            connect: env.connect.or(file.connect).unwrap_or(true),
            device_name,
            theme,
            bitrate: file.bitrate.unwrap_or_default(),
            data_dir: paths.data_dir,
            cache_dir: paths.cache_dir,
            state_dir: paths.state_dir,
            config_dir: paths.config_dir,
            accounts_base: SPOTIFY_ACCOUNTS.into(),
            spclient_base: env
                .spclient
                .and_then(|u| safe_endpoint("FERROTUNE_SPCLIENT_URL", &u)),
            pathfinder_base: env
                .pathfinder
                .and_then(|u| safe_endpoint("FERROTUNE_PATHFINDER_URL", &u))
                .unwrap_or_else(|| SPOTIFY_PATHFINDER.into()),
            soloist_cdn: env
                .soloist_cdn
                .and_then(|u| safe_endpoint("FERROTUNE_SOLOIST_CDN", &u))
                .unwrap_or_else(|| crate::net::soloist::install::CDN_BASE.into()),
            mock,
            trace_startup,
        }
    }

    /// The loopback redirect URI. The desktop client id accepts
    /// `http://127.0.0.1:<port>/login` (librespot uses the same form);
    /// Spotify requires the literal `127.0.0.1`, not `localhost`.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/login", self.redirect_port)
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("cache.sqlite3")
    }

    pub fn config_path(&self) -> PathBuf {
        self.config_dir.join("config.json")
    }

    pub fn log_path(&self) -> PathBuf {
        self.state_dir.join("log")
    }

    /// True when the hidden `FERROTUNE_CLIENT_ID` override is in use.
    pub fn custom_client_id(&self) -> bool {
        self.client_id != desktop_client_id()
    }

    /// Where Spotify Soloist is installed and keeps its data.
    pub fn soloist_paths(&self) -> crate::net::soloist::install::Paths {
        crate::net::soloist::install::Paths::new(&self.data_dir, &self.cache_dir)
    }

    /// Optional overrides for GraphQL persisted-query hashes.
    pub fn pathfinder_overrides_path(&self) -> PathBuf {
        self.config_dir.join("pathfinder.json")
    }

    /// Configuration for tests: temp-free, mock, never touches real dirs.
    pub fn for_tests() -> Self {
        let tmp = std::env::temp_dir().join("ferrotune-tests");
        Self::resolve(
            FileConfig::default(),
            Env::default(),
            Paths {
                data_dir: tmp.clone(),
                cache_dir: tmp.clone(),
                state_dir: tmp.clone(),
                config_dir: tmp,
            },
            true,
            false,
        )
    }
}

#[derive(Default)]
struct Env {
    client_id: Option<String>,
    port: Option<u16>,
    theme: Option<String>,
    connect: Option<bool>,
    spclient: Option<String>,
    pathfinder: Option<String>,
    soloist_cdn: Option<String>,
}

/// Endpoint overrides receive the session token, so only `https://` is
/// accepted, plus plain `http://` to a loopback address (local test
/// servers). Anything else is ignored with a warning.
pub fn safe_endpoint(var: &str, raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    let ok = url::Url::parse(trimmed)
        .ok()
        .is_some_and(|u| match u.scheme() {
            "https" => u.host().is_some(),
            "http" => match u.host() {
                Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
                Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
                Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
                None => false,
            },
            _ => false,
        });
    if ok {
        Some(trimmed.to_owned())
    } else {
        tracing::warn!(
            var,
            "ignoring endpoint override: must be https:// (or http:// on loopback)"
        );
        None
    }
}

/// Name shown in the Spotify Connect device picker.
pub fn device_name(host: Option<&str>) -> String {
    match host.map(str::trim).filter(|h| !h.is_empty()) {
        Some(h) => format!("{APP_NAME} on {h}"),
        None => APP_NAME.to_owned(),
    }
}

/// The machine's host name, without pulling in a crate for it.
fn hostname() -> Option<String> {
    if let Ok(h) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        return Some(h.trim().to_owned());
    }
    if let Ok(h) = std::fs::read_to_string("/etc/hostname") {
        return Some(h.trim().to_owned());
    }
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
}

struct Paths {
    data_dir: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
    config_dir: PathBuf,
}

fn dirs() -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    match directories::ProjectDirs::from("dev", "ferro", "ferrotune") {
        Some(p) => {
            let state = p
                .state_dir()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| p.data_local_dir().to_path_buf());
            (
                p.data_dir().to_path_buf(),
                p.cache_dir().to_path_buf(),
                state,
                p.config_dir().to_path_buf(),
            )
        }
        None => {
            let base = std::env::temp_dir().join("ferrotune");
            (base.clone(), base.clone(), base.clone(), base)
        }
    }
}

fn read_file_config(path: &Path) -> FileConfig {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "ignoring invalid config.json");
            FileConfig::default()
        }),
        Err(_) => FileConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> Paths {
        Paths {
            data_dir: "/d".into(),
            cache_dir: "/c".into(),
            state_dir: "/s".into(),
            config_dir: "/cfg".into(),
        }
    }

    #[test]
    fn env_overrides_file() {
        let file: FileConfig = serde_json::from_str(
            r#"{"client_id":"ignored","redirect_port":9000,"theme":"light","connect":false}"#,
        )
        .unwrap();
        let c = Config::resolve(
            file,
            Env {
                client_id: Some(" env-id ".into()),
                ..Env::default()
            },
            paths(),
            false,
            false,
        );
        assert_eq!(c.client_id, "env-id");
        assert!(c.custom_client_id());
        assert_eq!(c.redirect_port, 9000);
        assert_eq!(c.theme, ThemeChoice::Light);
        assert!(!c.connect);
        assert_eq!(c.redirect_uri(), "http://127.0.0.1:9000/login");
    }

    #[test]
    fn defaults_need_no_configuration() {
        let c = Config::resolve(
            FileConfig::default(),
            Env {
                theme: Some("dark".into()),
                ..Env::default()
            },
            paths(),
            true,
            false,
        );
        // Zero configuration: the desktop client id from librespot.
        assert_eq!(c.client_id, "65b708073fc0480ea92a077233ca87bd");
        assert!(!c.custom_client_id());
        assert_eq!(c.redirect_uri(), "http://127.0.0.1:8898/login");
        assert!(c.connect);
        assert_eq!(c.spclient_base, None);
        assert_eq!(c.pathfinder_base, SPOTIFY_PATHFINDER);
        assert_eq!(c.redirect_port, DEFAULT_REDIRECT_PORT);
        assert_eq!(c.theme, ThemeChoice::Dark);
        assert_eq!(c.bitrate, Bitrate::Normal);
        assert_eq!(c.db_path(), PathBuf::from("/d/cache.sqlite3"));
    }

    #[test]
    fn endpoint_overrides_must_be_https_or_loopback() {
        let ok = |u: &str| safe_endpoint("X", u);
        assert_eq!(
            ok("https://spclient.example/").as_deref(),
            Some("https://spclient.example")
        );
        assert!(ok("http://127.0.0.1:9999").is_some());
        assert!(ok("http://localhost:1").is_some());
        assert!(ok("http://[::1]:1").is_some());
        assert!(ok("http://evil.example").is_none());
        assert!(ok("http://10.0.0.1").is_none());
        assert!(ok("ftp://x").is_none());
        assert!(ok("garbage").is_none());
    }

    #[test]
    fn settings_load_and_save_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ferrotune-settings-{}", std::process::id()));
        let path = dir.join("config.json");
        std::fs::create_dir_all(&dir).unwrap();
        // Hand-edited keys survive a save.
        std::fs::write(&path, r#"{"redirect_port": 9001, "theme": "light"}"#).unwrap();
        let s = Settings {
            bitrate: Bitrate::High,
            normalisation: true,
            gapless: false,
            autoplay: false,
            device_name: "Desk".into(),
            theme: ThemeChoice::Dark,
            play_local: false,
            connect: true,
            engine: AudioEngine::Soloist,
            soloist_output: "alsa_output.usb".into(),
            soloist_cache_mb: 1000,
            close_to_tray: false,
            start_minimized: true,
            auto_update_check: false,
        };
        s.save(&path).unwrap();
        let file = read_file_config(&path);
        assert_eq!(file.redirect_port, Some(9001));
        let c = Config::resolve(file, Env::default(), paths(), false, false);
        assert_eq!(c.settings, s);
        assert_eq!(c.bitrate, Bitrate::High);
        assert_eq!(c.device_name, "Desk");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"bitrate\": \"320\""), "{raw}");
        assert!(raw.contains("\"engine\": \"soloist\""), "{raw}");
        // Garbage files are replaced, not fatal.
        std::fs::write(&path, "not json").unwrap();
        s.save(&path).unwrap();
        assert_eq!(read_file_config(&path).normalisation, Some(true));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn device_names() {
        assert_eq!(device_name(Some("desk\n")), "Ferrotune on desk");
        assert_eq!(device_name(Some("desk")), "Ferrotune on desk");
        assert_eq!(device_name(Some("  ")), "Ferrotune");
        assert_eq!(device_name(None), "Ferrotune");
    }
}
