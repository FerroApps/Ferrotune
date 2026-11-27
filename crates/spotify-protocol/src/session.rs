//! The Spotify session — the central object through which the rest of the
//! application talks to Spotify.
//!
//! [`Session`] wraps an AP connection and runs a background tokio task that
//! dispatches incoming packets (ping/pong, country code, product info, Mercury,
//! audio-key responses, etc.).
//!
//! # Construction
//!
//! ```ignore
//! let session = Session::connect(config, credentials).await?;
//! println!("logged in as {}", session.username());
//! ```

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Mutex, RwLock, mpsc, watch};
use tracing::{debug, error, info, trace, warn};

use crate::apresolve::{
    APRESOLVE_URL, AccessPoints, ApResolveData, SocketAddress, parse_apresolve_response,
    resolve_to_access_points,
};
use crate::audio_key::AudioKeyManager;
use crate::auth::Credentials;
use crate::channel::ChannelManager;
use crate::config::SessionConfig;
use crate::error::{ProtocolError, Result};
use crate::mercury::MercuryManager;
use crate::packet::PacketType;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of different access points we try before giving up.
const MAX_AP_TRIES: usize = 6;

/// Delay (in seconds) between receiving a Ping and sending the Pong, matching
/// the official client behaviour (~60 s).
const PONG_DELAY_SECS: u64 = 60;

// ---------------------------------------------------------------------------
// AP transport trait  — abstraction over the real AP codec
// ---------------------------------------------------------------------------

/// A boxed future that is `Send`.
type BoxFuture<'a, T> = Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// The sending half of an AP transport.
///
/// The real implementation lives in `ap::ApCodec`; this trait lets the session
/// be testable without a live TCP connection.
pub trait ApTransportSink: Send + Sync + 'static {
    /// Send a `(command, payload)` packet over the AP connection.
    fn send_packet(&mut self, cmd: u8, data: Vec<u8>) -> BoxFuture<'_, Result<()>>;
}

/// Result of receiving one packet: `Some(Ok((cmd, payload)))`, `Some(Err(..))`, or
/// `None` on EOF.
type RecvResult = Option<Result<(u8, Vec<u8>)>>;

/// The receiving half of an AP transport.
pub trait ApTransportStream: Send + 'static {
    /// Receive the next `(command, payload)` packet, or `None` on EOF.
    fn recv_packet(&mut self) -> BoxFuture<'_, RecvResult>;
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

/// Mutable session state protected by fine-grained locks.
#[allow(dead_code)] // packet_tx reserved for future outbound packet queue
struct SessionInner {
    /// Sending half of the AP connection.  `None` before `connect()`.
    sender: Mutex<Option<Box<dyn ApTransportSink>>>,

    /// Canonical username (set after successful authentication).
    username: RwLock<String>,

    /// Two-character country code sent by the AP.
    country: RwLock<String>,

    /// Unique device identifier.
    device_id: String,

    /// Raw authentication blob for re-use by other subsystems.
    auth_data: RwLock<Vec<u8>>,

    /// Connection id (set by the dealer via Connect-State header).
    connection_id: RwLock<String>,

    /// User attributes extracted from the `ProductInfo` XML packet.
    user_attributes: RwLock<HashMap<String, String>>,

    /// Set to `false` when the connection drops or is shut down.
    is_valid: AtomicBool,

    /// OAuth client id.
    client_id: String,

    /// Stored access points for dealer / spclient.
    access_points: RwLock<AccessPoints>,

    /// Signal the dispatch task to exit.
    shutdown_tx: watch::Sender<bool>,

    /// Packet-send channel: the dispatch loop is the only writer to the TCP
    /// stream; callers enqueue packets here.
    packet_tx: mpsc::UnboundedSender<(u8, Vec<u8>)>,

    /// Mercury request/response and pub/sub manager.
    mercury: MercuryManager,

    /// Audio key request/response manager.
    audio_key: AudioKeyManager,

    /// Audio data channel manager.
    channel: ChannelManager,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

/// A shared, cloneable handle to a Spotify session.
///
/// Internally backed by `Arc<SessionInner>`, so cloning is cheap and all
/// clones share the same connection and state.
#[derive(Clone)]
pub struct Session {
    inner: Arc<SessionInner>,
}

impl Session {
    // -- construction -------------------------------------------------------

    /// Connect to Spotify, authenticate, and start the background dispatch
    /// task.
    ///
    /// This is the main entry point.  It resolves access points, connects via
    /// TCP, performs the Shannon handshake, authenticates, and spawns the
    /// packet dispatch loop.
    ///
    /// Because the AP transport module (`ap/`) may not be ready yet, this
    /// function takes two closures that produce the transport halves.  When
    /// the AP module is finished these will be replaced with concrete types.
    pub async fn connect_with_transport<S, R, CF>(
        config: SessionConfig,
        credentials: Credentials,
        access_points: AccessPoints,
        connect_fn: CF,
    ) -> Result<Self>
    where
        S: ApTransportSink,
        R: ApTransportStream,
        CF: AsyncConnectFn<S, R>,
    {
        let mut last_err = ProtocolError::Connection("no access points available".into());

        // Try access points in order.
        let aps: Vec<SocketAddress> = access_points.accesspoint.into_iter().collect();
        for (tried, ap) in aps.iter().enumerate() {
            if tried >= MAX_AP_TRIES {
                break;
            }

            info!(host = %ap.0, port = ap.1, "connecting to AP");

            match connect_fn.connect(ap, &config, &credentials).await {
                Ok((sender, receiver, welcome)) => {
                    let session = Self::from_parts(config, welcome, sender);
                    session.start_dispatch(receiver);
                    return Ok(session);
                }
                Err(e) => {
                    warn!(host = %ap.0, port = ap.1, error = %e, "AP connection failed");
                    if matches!(&e, ProtocolError::AuthFailed(msg) if msg.contains("TryAnotherAP"))
                    {
                        // Explicitly instructed to try the next one.
                        last_err = e;
                        continue;
                    }
                    // On hard auth failures, don't retry.
                    if matches!(&e, ProtocolError::AuthFailed(_)) {
                        return Err(e);
                    }
                    last_err = e;
                }
            }
        }

        Err(last_err)
    }

    /// Build a Session from already-authenticated parts.  This is the core
    /// constructor used both by `connect_with_transport` and by tests.
    pub fn from_parts<S: ApTransportSink>(
        config: SessionConfig,
        welcome: WelcomeData,
        sender: S,
    ) -> Self {
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let (packet_tx, _packet_rx) = mpsc::unbounded_channel();

        let inner = SessionInner {
            sender: Mutex::new(Some(Box::new(sender))),
            username: RwLock::new(welcome.username),
            country: RwLock::new(String::new()),
            device_id: config.device_id.clone(),
            auth_data: RwLock::new(welcome.auth_data),
            connection_id: RwLock::new(String::new()),
            user_attributes: RwLock::new(HashMap::new()),
            is_valid: AtomicBool::new(true),
            client_id: config.client_id.clone(),
            access_points: RwLock::new(AccessPoints::default()),
            shutdown_tx,
            packet_tx,
            mercury: MercuryManager::new(),
            audio_key: AudioKeyManager::new(),
            channel: ChannelManager::new(),
        };

        Self {
            inner: Arc::new(inner),
        }
    }

    // -- dispatch -----------------------------------------------------------

    /// Spawn the background packet dispatch task.
    fn start_dispatch<R: ApTransportStream>(&self, receiver: R) {
        let session = self.clone();
        let mut shutdown_rx = self.inner.shutdown_tx.subscribe();

        tokio::spawn(async move {
            dispatch_loop(session, receiver, &mut shutdown_rx).await;
        });
    }

    // -- packet sending -----------------------------------------------------

    /// Send a packet over the AP connection.
    pub async fn send_packet(&self, cmd: PacketType, data: Vec<u8>) -> Result<()> {
        if !self.is_valid() {
            return Err(ProtocolError::NotConnected);
        }

        let mut guard = self.inner.sender.lock().await;
        match guard.as_mut() {
            Some(sender) => sender.send_packet(cmd as u8, data).await,
            None => Err(ProtocolError::NotConnected),
        }
    }

    // -- shutdown / validity ------------------------------------------------

    /// Shut down the session: marks it invalid, signals the dispatch task to
    /// exit, and drops the sender.
    pub fn shutdown(&self) {
        debug!("session shutdown");
        self.inner.is_valid.store(false, Ordering::Release);
        self.inner.mercury.shutdown();
        self.inner.audio_key.shutdown();
        self.inner.channel.shutdown();
        // Signal the dispatch task; ignore error (already shut down).
        let _ = self.inner.shutdown_tx.send(true);
    }

    /// Whether this session has been invalidated (connection dropped or
    /// explicit shutdown).
    pub fn is_invalid(&self) -> bool {
        !self.is_valid()
    }

    fn is_valid(&self) -> bool {
        self.inner.is_valid.load(Ordering::Acquire)
    }

    // -- identity getters ---------------------------------------------------

    pub fn username(&self) -> String {
        self.inner.username.blocking_read().clone()
    }

    pub fn country(&self) -> String {
        self.inner.country.blocking_read().clone()
    }

    pub fn device_id(&self) -> String {
        self.inner.device_id.clone()
    }

    pub fn auth_data(&self) -> Vec<u8> {
        self.inner.auth_data.blocking_read().clone()
    }

    pub fn connection_id(&self) -> String {
        self.inner.connection_id.blocking_read().clone()
    }

    pub fn set_connection_id(&self, id: String) {
        *self.inner.connection_id.blocking_write() = id;
    }

    pub fn get_user_attribute(&self, key: &str) -> Option<String> {
        self.inner.user_attributes.blocking_read().get(key).cloned()
    }

    pub fn client_id(&self) -> String {
        self.inner.client_id.clone()
    }

    /// Access the Mercury manager for request/response and pub/sub.
    pub fn mercury(&self) -> &MercuryManager {
        &self.inner.mercury
    }

    /// Access the audio key manager for requesting track decryption keys.
    pub fn audio_key(&self) -> &AudioKeyManager {
        &self.inner.audio_key
    }

    /// Access the channel manager for streaming audio data.
    pub fn channel(&self) -> &ChannelManager {
        &self.inner.channel
    }

    // -- async getters (for use from async contexts) -------------------------

    pub async fn username_async(&self) -> String {
        self.inner.username.read().await.clone()
    }

    pub async fn country_async(&self) -> String {
        self.inner.country.read().await.clone()
    }

    pub async fn auth_data_async(&self) -> Vec<u8> {
        self.inner.auth_data.read().await.clone()
    }

    pub async fn connection_id_async(&self) -> String {
        self.inner.connection_id.read().await.clone()
    }

    pub async fn set_connection_id_async(&self, id: String) {
        *self.inner.connection_id.write().await = id;
    }

    pub async fn get_user_attribute_async(&self, key: &str) -> Option<String> {
        self.inner.user_attributes.read().await.get(key).cloned()
    }

    // -- access points (for dealer/spclient) --------------------------------

    /// Get a copy of the stored access points.
    pub async fn access_points(&self) -> AccessPoints {
        let guard = self.inner.access_points.read().await;
        AccessPoints {
            accesspoint: guard.accesspoint.clone(),
            dealer: guard.dealer.clone(),
            spclient: guard.spclient.clone(),
        }
    }

    /// Store resolved access points (from an AP resolve call).
    pub async fn set_access_points(&self, aps: AccessPoints) {
        *self.inner.access_points.write().await = aps;
    }
}

// ---------------------------------------------------------------------------
// WelcomeData — the bits from APWelcome that the Session stores
// ---------------------------------------------------------------------------

/// Data extracted from the AP's `APWelcome` response after successful
/// authentication.
#[derive(Debug, Clone)]
pub struct WelcomeData {
    /// Canonical username.
    pub username: String,
    /// The reusable auth blob that can be persisted in the keyring.
    pub auth_data: Vec<u8>,
}

// ---------------------------------------------------------------------------
// AsyncConnectFn — trait for the connection factory
// ---------------------------------------------------------------------------

/// Trait for async connection factory functions.
///
/// Takes an access point address, config and credentials, and returns the
/// transport halves plus the welcome data.
pub trait AsyncConnectFn<S, R>: Send + Sync {
    fn connect<'a>(
        &'a self,
        ap: &'a SocketAddress,
        config: &'a SessionConfig,
        credentials: &'a Credentials,
    ) -> BoxFuture<'a, Result<(S, R, WelcomeData)>>;
}

/// Blanket impl: any matching async closure is an `AsyncConnectFn`.
impl<F, S, R> AsyncConnectFn<S, R> for F
where
    F: for<'a> Fn(
            &'a SocketAddress,
            &'a SessionConfig,
            &'a Credentials,
        ) -> BoxFuture<'a, Result<(S, R, WelcomeData)>>
        + Send
        + Sync,
{
    fn connect<'a>(
        &'a self,
        ap: &'a SocketAddress,
        config: &'a SessionConfig,
        credentials: &'a Credentials,
    ) -> BoxFuture<'a, Result<(S, R, WelcomeData)>> {
        (self)(ap, config, credentials)
    }
}

// ---------------------------------------------------------------------------
// Dispatch loop
// ---------------------------------------------------------------------------

async fn dispatch_loop<R: ApTransportStream>(
    session: Session,
    mut receiver: R,
    shutdown_rx: &mut watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            biased;

            _ = shutdown_rx.changed() => {
                debug!("dispatch: shutdown signal received");
                break;
            }

            packet = receiver.recv_packet() => {
                match packet {
                    Some(Ok((cmd, data))) => {
                        if let Err(e) = dispatch_packet(&session, cmd, data).await {
                            debug!(error = %e, cmd = cmd, "dispatch: could not handle packet");
                        }
                    }
                    Some(Err(e)) => {
                        error!(error = %e, "dispatch: AP connection error");
                        session.shutdown();
                        break;
                    }
                    None => {
                        warn!("dispatch: AP connection closed");
                        session.shutdown();
                        break;
                    }
                }
            }
        }
    }

    debug!("dispatch task exiting");
}

/// Handle a single incoming packet.
async fn dispatch_packet(session: &Session, cmd: u8, data: Vec<u8>) -> Result<()> {
    let packet_type = PacketType::from_u8(cmd);

    match packet_type {
        Some(PacketType::Ping) => {
            trace!("received Ping");
            handle_ping(session, &data).await
        }

        Some(PacketType::PongAck) => {
            trace!("received PongAck");
            Ok(())
        }

        Some(PacketType::CountryCode) => {
            let country = String::from_utf8(data)
                .map_err(|e| ProtocolError::Connection(format!("invalid country code: {e}")))?;
            info!(country = %country, "country code received");
            *session.inner.country.write().await = country;
            Ok(())
        }

        Some(PacketType::ProductInfo) => handle_product_info(session, &data).await,

        // Mercury responses — routed to the MercuryManager.
        Some(PacketType::MercuryReq)
        | Some(PacketType::MercurySub)
        | Some(PacketType::MercuryUnsub)
        | Some(PacketType::MercuryEvent) => {
            trace!(cmd = cmd, len = data.len(), "mercury packet");
            if let Err(e) = session.mercury().handle_packet(cmd, &data) {
                debug!(error = %e, cmd = cmd, "mercury: packet handling error");
            }
            Ok(())
        }

        // Audio key responses — routed to the AudioKeyManager.
        Some(PacketType::AesKey) => {
            trace!(cmd = cmd, len = data.len(), "audio key success packet");
            if let Err(e) = session.audio_key().handle_aes_key(&data) {
                debug!(error = %e, "audio key: AesKey handling error");
            }
            Ok(())
        }
        Some(PacketType::AesKeyError) => {
            trace!(cmd = cmd, len = data.len(), "audio key error packet");
            if let Err(e) = session.audio_key().handle_aes_key_error(&data) {
                debug!(error = %e, "audio key: AesKeyError handling error");
            }
            Ok(())
        }

        // Stream / channel — routed to the ChannelManager.
        Some(PacketType::StreamChunk) => {
            trace!(cmd = cmd, len = data.len(), "channel data packet");
            if let Err(e) = session.channel().handle_data(&data) {
                debug!(error = %e, "channel: data handling error");
            }
            Ok(())
        }
        Some(PacketType::StreamChunkRes) => {
            trace!(cmd = cmd, len = data.len(), "channel header packet");
            if let Err(e) = session.channel().handle_header(&data) {
                debug!(error = %e, "channel: header handling error");
            }
            Ok(())
        }
        Some(PacketType::ChannelError) => {
            trace!(cmd = cmd, len = data.len(), "channel error packet");
            if let Err(e) = session.channel().handle_channel_error(&data) {
                debug!(error = %e, "channel: error handling error");
            }
            Ok(())
        }

        // Benign packets we can safely ignore.
        Some(PacketType::SecretBlock)
        | Some(PacketType::LegacyWelcome)
        | Some(PacketType::LicenseVersion)
        | Some(PacketType::PreferredLocale) => Ok(()),

        _ => {
            trace!(cmd = cmd, "ignoring unknown packet");
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Ping / Pong
// ---------------------------------------------------------------------------

/// Handle an incoming Ping: schedule a Pong to be sent after a ~60 s delay
/// (spawned in a separate task so the dispatch loop is not blocked).
async fn handle_ping(session: &Session, _data: &[u8]) -> Result<()> {
    let session = session.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(PONG_DELAY_SECS)).await;

        if session.is_valid() {
            let pong_data = vec![0u8; 4];
            if let Err(e) = session.send_packet(PacketType::Pong, pong_data).await {
                warn!(error = %e, "failed to send Pong");
            } else {
                trace!("sent Pong");
            }
        }
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// ProductInfo XML parsing
// ---------------------------------------------------------------------------

/// Parse the ProductInfo XML packet and store attributes in the session.
///
/// The XML looks like:
/// ```xml
/// <products>
///   <product>
///     <type>premium</type>
///     <expiry>2099-12-31</expiry>
///     ...
///   </product>
/// </products>
/// ```
async fn handle_product_info(session: &Session, data: &[u8]) -> Result<()> {
    let xml = std::str::from_utf8(data)
        .map_err(|e| ProtocolError::XmlParse(format!("invalid UTF-8 in ProductInfo: {e}")))?;

    let mut reader = quick_xml::Reader::from_str(xml);
    let mut buf = Vec::new();
    let mut current_element = String::new();
    let mut attributes: HashMap<String, String> = HashMap::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                current_element = String::from_utf8_lossy(e.name().as_ref()).into_owned();
            }
            Ok(quick_xml::events::Event::End(_)) => {
                current_element.clear();
            }
            Ok(quick_xml::events::Event::Text(ref e)) => {
                if !current_element.is_empty()
                    && let Ok(text) = e.unescape()
                {
                    attributes.insert(current_element.clone(), text.into_owned());
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                warn!(
                    position = reader.buffer_position(),
                    error = %e,
                    "error parsing ProductInfo XML"
                );
                // Keep going — partial parse is better than none.
            }
        }
        buf.clear();
    }

    trace!(attributes = ?attributes, "product info received");

    if let Some(account_type) = attributes.get("type")
        && account_type != "premium"
    {
        warn!(account_type = %account_type, "non-premium account detected");
    }

    *session.inner.user_attributes.write().await = attributes;
    Ok(())
}

// ---------------------------------------------------------------------------
// AP resolve helper
// ---------------------------------------------------------------------------

/// Resolve access points by fetching from `apresolve.spotify.com`.
///
/// The caller provides a function to perform the HTTP GET (so this crate
/// does not depend on a specific HTTP library).
pub async fn resolve_access_points<F, Fut>(
    fetch_fn: F,
    filter_port: Option<u16>,
) -> Result<AccessPoints>
where
    F: FnOnce(&str) -> Fut,
    Fut: std::future::Future<Output = std::result::Result<Vec<u8>, String>>,
{
    let body = fetch_fn(APRESOLVE_URL)
        .await
        .map_err(ProtocolError::ApResolve)?;

    let data = parse_apresolve_response(&body)?;
    let mut aps = resolve_to_access_points(data, filter_port);

    // Fill in fallbacks if any list is empty.
    if aps.is_any_empty() {
        warn!("some AP endpoints missing, using fallbacks");
        let fallback = resolve_to_access_points(ApResolveData::fallback(), filter_port);
        if aps.accesspoint.is_empty() {
            aps.accesspoint = fallback.accesspoint;
        }
        if aps.dealer.is_empty() {
            aps.dealer = fallback.dealer;
        }
        if aps.spclient.is_empty() {
            aps.spclient = fallback.spclient;
        }
    }

    Ok(aps)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    type PacketLog = Arc<tokio::sync::Mutex<Vec<(u8, Vec<u8>)>>>;

    /// Minimal in-memory AP transport for testing.
    struct MockSender {
        sent: PacketLog,
    }

    impl ApTransportSink for MockSender {
        fn send_packet(&mut self, cmd: u8, data: Vec<u8>) -> BoxFuture<'_, Result<()>> {
            let sent = self.sent.clone();
            Box::pin(async move {
                sent.lock().await.push((cmd, data));
                Ok(())
            })
        }
    }

    struct MockReceiver {
        packets: Vec<Result<(u8, Vec<u8>)>>,
    }

    impl ApTransportStream for MockReceiver {
        fn recv_packet(&mut self) -> BoxFuture<'_, RecvResult> {
            Box::pin(async {
                if self.packets.is_empty() {
                    return None;
                }
                Some(self.packets.remove(0))
            })
        }
    }

    fn test_welcome() -> WelcomeData {
        WelcomeData {
            username: "testuser".into(),
            auth_data: vec![1, 2, 3, 4],
        }
    }

    fn test_config() -> SessionConfig {
        SessionConfig {
            client_id: "test_client".into(),
            device_id: "abcd1234".into(),
            proxy: None,
        }
    }

    #[test]
    fn session_basic_accessors() {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sender = MockSender { sent: sent.clone() };
        let session = Session::from_parts(test_config(), test_welcome(), sender);

        assert_eq!(session.username(), "testuser");
        assert_eq!(session.device_id(), "abcd1234");
        assert_eq!(session.client_id(), "test_client");
        assert_eq!(session.auth_data(), vec![1, 2, 3, 4]);
        assert!(session.country().is_empty());
        assert!(session.connection_id().is_empty());
        assert!(!session.is_invalid());
    }

    #[test]
    fn session_clone_shares_state() {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sender = MockSender { sent: sent.clone() };
        let session1 = Session::from_parts(test_config(), test_welcome(), sender);
        let session2 = session1.clone();

        session1.set_connection_id("conn-123".into());
        assert_eq!(session2.connection_id(), "conn-123");
    }

    #[test]
    fn session_shutdown() {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sender = MockSender { sent: sent.clone() };
        let session = Session::from_parts(test_config(), test_welcome(), sender);

        assert!(!session.is_invalid());
        session.shutdown();
        assert!(session.is_invalid());
    }

    #[tokio::test]
    async fn dispatch_country_code() {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sender = MockSender { sent: sent.clone() };
        let session = Session::from_parts(test_config(), test_welcome(), sender);

        // Simulate a CountryCode packet.
        let result = dispatch_packet(&session, PacketType::CountryCode as u8, b"SE".to_vec()).await;
        assert!(result.is_ok());
        assert_eq!(session.country_async().await, "SE");
    }

    #[tokio::test]
    async fn dispatch_product_info() {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sender = MockSender { sent: sent.clone() };
        let session = Session::from_parts(test_config(), test_welcome(), sender);

        let xml = br#"<products><product><type>premium</type><expiry>2099-12-31</expiry></product></products>"#;
        let result = dispatch_packet(&session, PacketType::ProductInfo as u8, xml.to_vec()).await;
        assert!(result.is_ok());
        assert_eq!(
            session.get_user_attribute_async("type").await,
            Some("premium".into())
        );
        assert_eq!(
            session.get_user_attribute_async("expiry").await,
            Some("2099-12-31".into())
        );
    }

    #[tokio::test]
    async fn dispatch_unknown_packet_does_not_crash() {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sender = MockSender { sent: sent.clone() };
        let session = Session::from_parts(test_config(), test_welcome(), sender);

        // 0xFF is not a known packet type.
        let result = dispatch_packet(&session, 0xFF, vec![]).await;
        // Should succeed silently.
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn dispatch_loop_exits_on_eof() {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sender = MockSender { sent: sent.clone() };
        let session = Session::from_parts(test_config(), test_welcome(), sender);

        let receiver = MockReceiver {
            packets: vec![
                Ok((PacketType::CountryCode as u8, b"US".to_vec())),
                // Then EOF (empty)
            ],
        };

        let mut shutdown_rx = session.inner.shutdown_tx.subscribe();
        dispatch_loop(session.clone(), receiver, &mut shutdown_rx).await;

        // Session should be invalid after EOF.
        assert!(session.is_invalid());
        assert_eq!(session.country_async().await, "US");
    }

    #[tokio::test]
    async fn dispatch_loop_exits_on_shutdown() {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let sender = MockSender { sent: sent.clone() };
        let session = Session::from_parts(test_config(), test_welcome(), sender);

        // Receiver that blocks indefinitely.
        struct HangingReceiver;
        impl ApTransportStream for HangingReceiver {
            fn recv_packet(&mut self) -> BoxFuture<'_, RecvResult> {
                // Never returns — simulates a connection that is idle.
                Box::pin(std::future::pending())
            }
        }

        let session2 = session.clone();
        let handle = tokio::spawn(async move {
            let mut shutdown_rx = session2.inner.shutdown_tx.subscribe();
            dispatch_loop(session2, HangingReceiver, &mut shutdown_rx).await;
        });

        // Give the dispatch loop a moment to start.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Shut down.
        session.shutdown();

        // The dispatch task should exit quickly.
        tokio::time::timeout(std::time::Duration::from_secs(2), handle)
            .await
            .expect("dispatch should exit on shutdown")
            .expect("dispatch task should not panic");
    }

    #[tokio::test]
    async fn resolve_access_points_ok() {
        let result = resolve_access_points(
            |_url| async {
                Ok(
                    br#"{"accesspoint":["ap:4070"],"dealer":["d:443"],"spclient":["s:443"]}"#
                        .to_vec(),
                )
            },
            None,
        )
        .await;

        let aps = result.expect("should succeed");
        assert_eq!(aps.accesspoint.len(), 1);
        assert_eq!(aps.dealer.len(), 1);
        assert_eq!(aps.spclient.len(), 1);
    }

    #[tokio::test]
    async fn resolve_access_points_fallback_on_empty() {
        let result = resolve_access_points(
            |_url| async {
                // AP returns empty lists.
                Ok(br#"{"accesspoint":[],"dealer":[],"spclient":[]}"#.to_vec())
            },
            None,
        )
        .await;

        let aps = result.expect("should succeed with fallbacks");
        assert!(!aps.accesspoint.is_empty());
        assert!(!aps.dealer.is_empty());
        assert!(!aps.spclient.is_empty());
    }

    #[tokio::test]
    async fn resolve_access_points_http_error() {
        let result = resolve_access_points(
            |_url| async { Err::<Vec<u8>, String>("network error".into()) },
            None,
        )
        .await;

        assert!(result.is_err());
    }
}
