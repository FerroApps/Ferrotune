//! Dealer WebSocket module — real-time push messages from Spotify.
//!
//! The dealer is Spotify's WebSocket connection for Connect state changes,
//! player commands, and other real-time events.  It lives at
//! `wss://dealer.spotify.com:443/` (the URL comes from AP resolution) and
//! carries two kinds of frame:
//!
//! * **Messages** — pushed from the server, matched to subscribers by URI
//!   prefix.
//! * **Requests** — the server asks the client to acknowledge something; the
//!   client replies with `{type: "reply", key, payload: {success: true}}`.
//!
//! # Usage
//!
//! ```ignore
//! let dealer = DealerManager::connect(
//!     "wss://dealer.spotify.com/?access_token=..."
//! ).await?;
//!
//! println!("connection id: {}", dealer.connection_id());
//!
//! let mut rx = dealer.listen_for("hm://connect-state");
//! dealer.start().await?;
//!
//! while let Some(msg) = rx.recv().await {
//!     println!("{}: {:?}", msg.uri, msg.payload);
//! }
//! ```

pub mod protocol;

use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, watch};
use tracing::{debug, error, info, trace, warn};

pub use protocol::DealerMessage;
use protocol::WsEnvelope;

use crate::error::ProtocolError;

// ---------------------------------------------------------------------------
// Type aliases
// ---------------------------------------------------------------------------

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsMessage = tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const PING_INTERVAL: Duration = Duration::from_secs(30);
const PING_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// Subscriber
// ---------------------------------------------------------------------------

/// A subscriber registered via [`DealerManager::listen_for`].
struct Subscriber {
    prefix: String,
    tx: mpsc::UnboundedSender<DealerMessage>,
}

// ---------------------------------------------------------------------------
// DealerManager
// ---------------------------------------------------------------------------

/// Manages the dealer WebSocket connection and message dispatch.
///
/// Create one with [`connect`](Self::connect), register URI prefix
/// subscriptions with [`listen_for`](Self::listen_for), then call
/// [`start`](Self::start) to begin processing messages in the background.
pub struct DealerManager {
    inner: Arc<DealerInner>,
    /// Handle to the background task (set by `start()`).
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

struct DealerInner {
    /// The dealer WebSocket URL (kept for reconnection).
    url: String,
    /// Connection ID from the `Spotify-Connection-Id` response header.
    connection_id: RwLock<String>,
    /// Registered message subscribers.
    subscribers: Mutex<Vec<Subscriber>>,
    /// Shutdown signal.
    shutdown_tx: watch::Sender<bool>,
    /// The initial WebSocket stream, taken by [`DealerManager::start`].
    initial_stream: Mutex<Option<WsStream>>,
}

impl DealerManager {
    /// Connect to the dealer WebSocket at the given URL.
    ///
    /// The URL should include the access token as a query parameter, e.g.:
    /// `wss://dealer.spotify.com:443/?access_token=<token>`
    ///
    /// This performs the WebSocket handshake and extracts the
    /// `Spotify-Connection-Id` header from the response.  Call
    /// [`listen_for`](Self::listen_for) to register subscriptions, then
    /// [`start`](Self::start) to begin processing messages.
    pub async fn connect(url: &str) -> Result<Self, ProtocolError> {
        let (stream, response) = tokio_tungstenite::connect_async(url).await.map_err(|e| {
            ProtocolError::Connection(format!("dealer: websocket connect failed: {e}"))
        })?;

        // Extract the Spotify-Connection-Id from the upgrade response.
        let connection_id = response
            .headers()
            .get("Spotify-Connection-Id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned();

        if connection_id.is_empty() {
            warn!("dealer: response missing Spotify-Connection-Id header");
        } else {
            debug!(connection_id = %connection_id, "dealer: connection established");
        }

        let (shutdown_tx, _) = watch::channel(false);

        let inner = Arc::new(DealerInner {
            url: url.to_owned(),
            connection_id: RwLock::new(connection_id),
            subscribers: Mutex::new(Vec::new()),
            shutdown_tx,
            initial_stream: Mutex::new(Some(stream)),
        });

        Ok(Self {
            inner,
            handle: Mutex::new(None),
        })
    }

    /// The connection ID received from the dealer via the
    /// `Spotify-Connection-Id` response header.
    ///
    /// Returns an empty string if the header was not present.
    pub fn connection_id(&self) -> String {
        self.inner
            .connection_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Subscribe to messages whose URI starts with the given prefix.
    ///
    /// Returns an unbounded receiver that delivers matching
    /// [`DealerMessage`]s once [`start`](Self::start) is called.
    ///
    /// Subscriptions may be registered both before and after `start()`.
    /// Dropping the receiver automatically unregisters the subscription on
    /// the next dispatch.
    pub fn listen_for(&self, uri_prefix: &str) -> mpsc::UnboundedReceiver<DealerMessage> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner
            .subscribers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Subscriber {
                prefix: uri_prefix.to_owned(),
                tx,
            });
        rx
    }

    /// Start the background receive loop.
    ///
    /// This spawns a tokio task that reads messages from the WebSocket,
    /// dispatches them to subscribers registered via
    /// [`listen_for`](Self::listen_for), handles WebSocket ping/pong
    /// keep-alive (ping every 30 s, pong timeout 3 s), and reconnects
    /// every 10 seconds on failure.
    pub async fn start(&self) -> Result<(), ProtocolError> {
        let stream = self
            .inner
            .initial_stream
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| {
                ProtocolError::Connection("dealer: already started or no stream available".into())
            })?;

        let inner = Arc::clone(&self.inner);
        let handle = tokio::spawn(async move {
            run_loop(inner, Some(stream)).await;
        });

        *self.handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        Ok(())
    }

    /// Shut down the dealer connection and stop all background tasks.
    pub fn shutdown(&self) {
        debug!("dealer: shutdown requested");
        let _ = self.inner.shutdown_tx.send(true);
        if let Some(handle) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }
}

impl Drop for DealerManager {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------------------
// Background tasks
// ---------------------------------------------------------------------------

/// Main background loop: processes one WebSocket connection, then reconnects
/// on failure every [`RECONNECT_INTERVAL`].
async fn run_loop(inner: Arc<DealerInner>, initial_stream: Option<WsStream>) {
    let mut stream = initial_stream;
    let mut shutdown_rx = inner.shutdown_tx.subscribe();

    loop {
        if *inner.shutdown_tx.borrow() {
            break;
        }

        let ws = match stream.take() {
            Some(s) => s,
            None => {
                // Reconnect.
                debug!(url = %inner.url, "dealer: reconnecting");
                match tokio_tungstenite::connect_async(&inner.url).await {
                    Ok((s, response)) => {
                        // Update connection ID on reconnect.
                        if let Some(id) = response
                            .headers()
                            .get("Spotify-Connection-Id")
                            .and_then(|v| v.to_str().ok())
                        {
                            if let Ok(mut guard) = inner.connection_id.write() {
                                *guard = id.to_owned();
                            }
                            debug!(connection_id = %id, "dealer: reconnected");
                        }
                        s
                    }
                    Err(e) => {
                        error!(
                            error = %e,
                            "dealer: reconnection failed, retrying in {}s",
                            RECONNECT_INTERVAL.as_secs()
                        );
                        tokio::select! {
                            () = tokio::time::sleep(RECONNECT_INTERVAL) => {},
                            _ = shutdown_rx.changed() => break,
                        }
                        continue;
                    }
                }
            }
        };

        // Process this connection until it drops or shutdown is requested.
        process_connection(&inner, ws).await;

        if *inner.shutdown_tx.borrow() {
            break;
        }

        info!(
            "dealer: connection lost, reconnecting in {}s",
            RECONNECT_INTERVAL.as_secs()
        );
        tokio::select! {
            () = tokio::time::sleep(RECONNECT_INTERVAL) => {},
            _ = shutdown_rx.changed() => break,
        }
    }

    debug!("dealer: run loop exiting");
}

/// Process a single WebSocket connection: receive messages, dispatch to
/// subscribers, and handle ping/pong keep-alive.
async fn process_connection(inner: &DealerInner, ws: WsStream) {
    let (mut ws_tx, mut ws_rx) = ws.split();
    let pong_received = Arc::new(AtomicBool::new(true));

    // Channel for outbound frames (ping, request replies).
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<WsMessage>();
    let out_tx_ping = out_tx.clone();
    let pong_for_ping = Arc::clone(&pong_received);

    // Send task: forward outbound messages to the WebSocket.
    let send_task = async {
        while let Some(msg) = out_rx.recv().await {
            if let Err(e) = ws_tx.send(msg).await {
                warn!(error = %e, "dealer: send error");
                break;
            }
        }
        let _ = ws_tx.close().await;
    };

    // Receive task: read messages and dispatch to subscribers.
    let receive_task = async {
        loop {
            match ws_rx.next().await {
                Some(Ok(WsMessage::Text(text))) => {
                    handle_text_message(inner, &text, &out_tx);
                }
                Some(Ok(WsMessage::Pong(_))) => {
                    trace!("dealer: pong received");
                    pong_received.store(true, Ordering::Release);
                }
                Some(Ok(WsMessage::Close(_))) => {
                    debug!("dealer: closed by server");
                    break;
                }
                Some(Ok(_)) => {
                    // Binary, Ping (auto-ponged by tungstenite), Frame.
                }
                Some(Err(e)) => {
                    warn!(error = %e, "dealer: websocket error");
                    break;
                }
                None => {
                    debug!("dealer: stream ended");
                    break;
                }
            }
        }
    };

    // Ping task: send pings every 30 s, expect pong within 3 s.
    let ping_task = async {
        let mut interval = tokio::time::interval(PING_INTERVAL);
        loop {
            interval.tick().await;

            pong_for_ping.store(false, Ordering::Release);
            if out_tx_ping
                .send(WsMessage::Ping(bytes::Bytes::new()))
                .is_err()
            {
                // Send channel closed — connection is gone.
                break;
            }
            trace!("dealer: ping sent");

            tokio::time::sleep(PING_TIMEOUT).await;

            if !pong_for_ping.load(Ordering::Acquire) {
                warn!("dealer: no pong received within timeout");
                break;
            }
        }
    };

    let mut shutdown_rx = inner.shutdown_tx.subscribe();

    tokio::select! {
        () = send_task => debug!("dealer: send task ended"),
        () = receive_task => debug!("dealer: receive task ended"),
        () = ping_task => debug!("dealer: ping task ended"),
        _ = shutdown_rx.changed() => debug!("dealer: shutdown signal"),
    }
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

/// Parse and dispatch a text frame from the WebSocket.
fn handle_text_message(inner: &DealerInner, text: &str, out_tx: &mpsc::UnboundedSender<WsMessage>) {
    let envelope: WsEnvelope = match serde_json::from_str(text) {
        Ok(e) => e,
        Err(e) => {
            warn!(error = %e, "dealer: failed to parse message");
            trace!(text = %text, "dealer: unparseable message");
            return;
        }
    };

    match envelope {
        WsEnvelope::Message(msg) => match msg.into_dealer_message() {
            Ok(dealer_msg) => dispatch_message(inner, dealer_msg),
            Err(e) => warn!(error = %e, "dealer: failed to decode payload"),
        },
        WsEnvelope::Request(req) => handle_request(req, out_tx),
    }
}

/// Dispatch a decoded message to matching subscribers (URI prefix match).
///
/// Dead subscribers (whose receiver has been dropped) are pruned
/// automatically.
fn dispatch_message(inner: &DealerInner, msg: DealerMessage) {
    let mut subscribers = inner.subscribers.lock().unwrap_or_else(|e| e.into_inner());

    let mut handled = false;
    subscribers.retain(|sub| {
        if msg.uri.starts_with(&sub.prefix) {
            if sub.tx.send(msg.clone()).is_ok() {
                handled = true;
                true
            } else {
                // Receiver dropped — remove this subscriber.
                false
            }
        } else {
            true
        }
    });

    if !handled {
        debug!(uri = %msg.uri, "dealer: no subscriber for message");
    }
}

/// Handle a dealer request by replying with `{success: true}`.
fn handle_request(req: protocol::WsEnvelopeRequest, out_tx: &mpsc::UnboundedSender<WsMessage>) {
    trace!(key = %req.key, mid = %req.message_id, "dealer: request received");

    let reply = serde_json::json!({
        "type": "reply",
        "key": req.key,
        "payload": {
            "success": true,
        }
    });

    if let Err(e) = out_tx.send(WsMessage::Text(reply.to_string().into())) {
        warn!(error = %e, "dealer: failed to send request reply");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // -- unit tests (no WebSocket) ------------------------------------------

    #[test]
    fn dispatch_to_matching_subscriber() {
        let (shutdown_tx, _) = watch::channel(false);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let inner = DealerInner {
            url: String::new(),
            connection_id: RwLock::new(String::new()),
            subscribers: Mutex::new(vec![Subscriber {
                prefix: "hm://connect-state".into(),
                tx,
            }]),
            shutdown_tx,
            initial_stream: Mutex::new(None),
        };

        let msg = DealerMessage {
            uri: "hm://connect-state/v1/cluster".into(),
            headers: HashMap::new(),
            payload: serde_json::json!({"test": true}),
        };

        dispatch_message(&inner, msg);

        let received = rx.try_recv().expect("should receive message");
        assert_eq!(received.uri, "hm://connect-state/v1/cluster");
        assert_eq!(received.payload["test"], true);
    }

    #[test]
    fn dispatch_no_match() {
        let (shutdown_tx, _) = watch::channel(false);
        let (tx, mut rx) = mpsc::unbounded_channel();

        let inner = DealerInner {
            url: String::new(),
            connection_id: RwLock::new(String::new()),
            subscribers: Mutex::new(vec![Subscriber {
                prefix: "hm://connect-state".into(),
                tx,
            }]),
            shutdown_tx,
            initial_stream: Mutex::new(None),
        };

        let msg = DealerMessage {
            uri: "hm://playlist/v1/changes".into(),
            headers: HashMap::new(),
            payload: serde_json::Value::Null,
        };

        dispatch_message(&inner, msg);
        assert!(
            rx.try_recv().is_err(),
            "should not receive non-matching message"
        );
    }

    #[test]
    fn dispatch_multiple_subscribers() {
        let (shutdown_tx, _) = watch::channel(false);
        let (tx1, mut rx1) = mpsc::unbounded_channel();
        let (tx2, mut rx2) = mpsc::unbounded_channel();

        let inner = DealerInner {
            url: String::new(),
            connection_id: RwLock::new(String::new()),
            subscribers: Mutex::new(vec![
                Subscriber {
                    prefix: "hm://connect-state".into(),
                    tx: tx1,
                },
                Subscriber {
                    prefix: "hm://connect-state/v1".into(),
                    tx: tx2,
                },
            ]),
            shutdown_tx,
            initial_stream: Mutex::new(None),
        };

        let msg = DealerMessage {
            uri: "hm://connect-state/v1/cluster".into(),
            headers: HashMap::new(),
            payload: serde_json::Value::Null,
        };

        dispatch_message(&inner, msg);

        // Both subscribers should match.
        assert!(rx1.try_recv().is_ok());
        assert!(rx2.try_recv().is_ok());
    }

    #[test]
    fn dispatch_removes_dead_subscribers() {
        let (shutdown_tx, _) = watch::channel(false);
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx); // Drop receiver — sends will fail.

        let inner = DealerInner {
            url: String::new(),
            connection_id: RwLock::new(String::new()),
            subscribers: Mutex::new(vec![Subscriber {
                prefix: "hm://".into(),
                tx,
            }]),
            shutdown_tx,
            initial_stream: Mutex::new(None),
        };

        let msg = DealerMessage {
            uri: "hm://test".into(),
            headers: HashMap::new(),
            payload: serde_json::Value::Null,
        };

        dispatch_message(&inner, msg);

        let subs = inner.subscribers.lock().expect("lock");
        assert!(subs.is_empty(), "dead subscriber should be removed");
    }

    #[test]
    fn handle_text_message_dispatches_message() {
        let (shutdown_tx, _) = watch::channel(false);
        let (sub_tx, mut sub_rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        let inner = DealerInner {
            url: String::new(),
            connection_id: RwLock::new(String::new()),
            subscribers: Mutex::new(vec![Subscriber {
                prefix: "hm://connect-state".into(),
                tx: sub_tx,
            }]),
            shutdown_tx,
            initial_stream: Mutex::new(None),
        };

        let text = r#"{"type":"message","uri":"hm://connect-state/v1/cluster","headers":{},"payloads":[]}"#;
        handle_text_message(&inner, text, &out_tx);

        let msg = sub_rx.try_recv().expect("should receive message");
        assert_eq!(msg.uri, "hm://connect-state/v1/cluster");
    }

    #[test]
    fn handle_text_message_replies_to_request() {
        let (shutdown_tx, _) = watch::channel(false);

        let inner = DealerInner {
            url: String::new(),
            connection_id: RwLock::new(String::new()),
            subscribers: Mutex::new(vec![]),
            shutdown_tx,
            initial_stream: Mutex::new(None),
        };

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        let text = r#"{"type":"request","mid":"42","key":"req-key","headers":{},"payload":{}}"#;
        handle_text_message(&inner, text, &out_tx);

        let reply = out_rx.try_recv().expect("should send reply");
        if let WsMessage::Text(reply_text) = reply {
            let parsed: serde_json::Value = serde_json::from_str(&reply_text).expect("parse reply");
            assert_eq!(parsed["type"], "reply");
            assert_eq!(parsed["key"], "req-key");
            assert_eq!(parsed["payload"]["success"], true);
        } else {
            panic!("expected text reply");
        }
    }

    #[test]
    fn handle_text_message_ignores_garbage() {
        let (shutdown_tx, _) = watch::channel(false);

        let inner = DealerInner {
            url: String::new(),
            connection_id: RwLock::new(String::new()),
            subscribers: Mutex::new(vec![]),
            shutdown_tx,
            initial_stream: Mutex::new(None),
        };

        let (out_tx, mut out_rx) = mpsc::unbounded_channel();
        handle_text_message(&inner, "not json at all", &out_tx);

        // No reply should be sent.
        assert!(out_rx.try_recv().is_err());
    }

    // -- integration tests (local WebSocket) --------------------------------

    #[tokio::test]
    async fn connect_and_receive_message() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("ws://127.0.0.1:{}", addr.port());

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept_async");

            let msg = serde_json::json!({
                "type": "message",
                "uri": "hm://connect-state/v1/cluster",
                "headers": {},
                "payloads": []
            });
            ws.send(WsMessage::Text(msg.to_string().into()))
                .await
                .expect("send");

            // Keep connection alive while the client reads.
            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let dealer = DealerManager::connect(&url).await.expect("connect");
        let mut rx = dealer.listen_for("hm://connect-state");
        dealer.start().await.expect("start");

        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out waiting for message")
            .expect("channel closed");

        assert_eq!(msg.uri, "hm://connect-state/v1/cluster");

        dealer.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn connect_and_receive_base64_payload() {
        use base64::Engine as _;
        use base64::prelude::BASE64_STANDARD;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("ws://127.0.0.1:{}", addr.port());

        let payload_json = serde_json::json!({"track": "spotify:track:abc"});
        let encoded = BASE64_STANDARD.encode(payload_json.to_string().as_bytes());

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept_async");

            let msg = serde_json::json!({
                "type": "message",
                "uri": "hm://connect-state/v1/cluster",
                "headers": {},
                "payloads": [encoded]
            });
            ws.send(WsMessage::Text(msg.to_string().into()))
                .await
                .expect("send");

            tokio::time::sleep(Duration::from_secs(2)).await;
        });

        let dealer = DealerManager::connect(&url).await.expect("connect");
        let mut rx = dealer.listen_for("hm://connect-state");
        dealer.start().await.expect("start");

        let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");

        assert_eq!(msg.payload["track"], "spotify:track:abc");

        dealer.shutdown();
        server.abort();
    }

    #[tokio::test]
    async fn dealer_replies_to_request() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("ws://127.0.0.1:{}", addr.port());

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let mut ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept_async");

            // Send a request.
            let req = serde_json::json!({
                "type": "request",
                "mid": "99",
                "key": "test-key",
                "headers": {},
                "payload": {}
            });
            ws.send(WsMessage::Text(req.to_string().into()))
                .await
                .expect("send request");

            // Read the reply (Ping frames are auto-handled by tungstenite).
            let reply = tokio::time::timeout(Duration::from_secs(5), ws.next())
                .await
                .expect("timed out waiting for reply")
                .expect("stream ended")
                .expect("websocket error");

            if let WsMessage::Text(text) = reply {
                let parsed: serde_json::Value = serde_json::from_str(&text).expect("parse reply");
                assert_eq!(parsed["type"], "reply");
                assert_eq!(parsed["key"], "test-key");
                assert_eq!(parsed["payload"]["success"], true);
            } else {
                panic!("expected text reply, got {reply:?}");
            }
        });

        let dealer = DealerManager::connect(&url).await.expect("connect");
        dealer.start().await.expect("start");

        // Wait for the server to complete its assertions.
        server.await.expect("server task panicked");

        dealer.shutdown();
    }

    #[tokio::test]
    async fn shutdown_stops_background_task() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("ws://127.0.0.1:{}", addr.port());

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept_async");
            // Keep the connection alive.
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let dealer = DealerManager::connect(&url).await.expect("connect");
        dealer.start().await.expect("start");

        // Let the background task start.
        tokio::time::sleep(Duration::from_millis(50)).await;

        dealer.shutdown();

        // Handle should have been cleared.
        assert!(
            dealer.handle.lock().expect("lock").is_none(),
            "handle should be cleared after shutdown"
        );

        server.abort();
    }

    #[tokio::test]
    async fn start_twice_returns_error() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("local_addr");
        let url = format!("ws://127.0.0.1:{}", addr.port());

        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("accept_async");
            tokio::time::sleep(Duration::from_secs(60)).await;
        });

        let dealer = DealerManager::connect(&url).await.expect("connect");
        dealer.start().await.expect("start");

        // Second start should fail.
        let result = dealer.start().await;
        assert!(result.is_err());

        dealer.shutdown();
        server.abort();
    }
}
