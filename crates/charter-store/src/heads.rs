//! `eth_subscribe newHeads` over WebSocket: block numbers as the node announces them
//! (docs/p5/speed-and-cost.md 3.13 measured each block about 0.3 s after its timestamp).
//! Read-only and best effort: callers keep polling as the fallback when it drops.

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::StoreError;
use crate::rpc::ACCESS_KEY_HEADER;

/// The process's shared `newHeads` subscription, as every pipeline sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// No shared socket (never started, or it dropped): pipelines open their own or poll.
    Down,
    /// Started, not yet subscribed.
    Connecting,
    /// The latest announced block, its base fee and when it arrived.
    Tip {
        number: u64,
        base_fee: u128,
        at: std::time::Instant,
    },
}

static FEED: std::sync::OnceLock<tokio::sync::watch::Sender<Feed>> = std::sync::OnceLock::new();

fn feed_tx() -> &'static tokio::sync::watch::Sender<Feed> {
    FEED.get_or_init(|| tokio::sync::watch::channel(Feed::Down).0)
}

/// A view of the shared feed; `changed()` wakes on the next block after the last one
/// this receiver marked seen.
pub fn feed() -> tokio::sync::watch::Receiver<Feed> {
    feed_tx().subscribe()
}

/// The reader of one shared `newHeads` socket for this process, for the caller to spawn
/// (this crate has no runtime of its own); `None` while one is already running. Opened at
/// command start, the 1.4 s handshake (MEASURED on Rabdi) overlaps the reads before a
/// write instead of delaying its first inclusion wait.
pub fn start_feed(cfg: WsConfig) -> Option<impl Future<Output = ()> + Send + 'static> {
    let tx = feed_tx();
    let started = tx.send_if_modified(|f| match f {
        Feed::Down => {
            *f = Feed::Connecting;
            true
        }
        _ => false,
    });
    started.then_some(async move {
        if let Ok(mut h) = subscribe(&cfg).await {
            while let Ok(Some((number, base_fee))) = h.next_tip().await {
                let at = std::time::Instant::now();
                tx.send_replace(Feed::Tip {
                    number,
                    base_fee,
                    at,
                });
            }
        }
        tx.send_replace(Feed::Down);
    })
}

/// Where the socket is and the optional access key, sent as on HTTP.
#[derive(Clone)]
pub struct WsConfig {
    pub url: String,
    pub access_key: Option<String>,
}

impl std::fmt::Debug for WsConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConfig")
            .field("url", &self.url)
            .field(
                "access_key",
                &self.access_key.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The node serves WebSocket on the JSON-RPC host: `https://h/p` is `wss://h/p`.
pub fn ws_url(http: &str) -> String {
    match (http.strip_prefix("https://"), http.strip_prefix("http://")) {
        (Some(rest), _) => format!("wss://{rest}"),
        (_, Some(rest)) => format!("ws://{rest}"),
        _ => http.to_owned(),
    }
}

fn ws_err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Rpc(format!("websocket: {e}"))
}

/// A live `newHeads` subscription.
pub struct Heads {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

/// Connects and subscribes; returns once the node has accepted the subscription.
pub async fn subscribe(cfg: &WsConfig) -> Result<Heads, StoreError> {
    let mut req = cfg.url.as_str().into_client_request().map_err(ws_err)?;
    if let Some(k) = &cfg.access_key {
        let v = HeaderValue::from_str(k).map_err(|_| ws_err("access key is not a header value"))?;
        req.headers_mut().insert(ACCESS_KEY_HEADER, v);
    }
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(ws_err)?;
    let mut h = Heads { ws };
    let sub = json!({"jsonrpc": "2.0", "id": 1, "method": "eth_subscribe", "params": ["newHeads"]});
    h.ws.send(Message::text(sub.to_string()))
        .await
        .map_err(ws_err)?;
    let Some(reply) = h.message().await? else {
        return Err(ws_err("closed before the subscription reply"));
    };
    match (&reply["result"], &reply["error"]) {
        (Value::String(_), _) => Ok(h),
        (_, Value::Null) => Err(ws_err(format!("unexpected reply {reply}"))),
        (_, e) => Err(StoreError::Rpc(format!(
            "eth_subscribe: {}",
            e["message"].as_str().unwrap_or("refused")
        ))),
    }
}

impl Heads {
    /// The next JSON text frame; `None` once the socket closes. Pings are answered by
    /// tungstenite while it reads.
    async fn message(&mut self) -> Result<Option<Value>, StoreError> {
        while let Some(m) = self.ws.next().await {
            match m.map_err(ws_err)? {
                Message::Text(t) => return serde_json::from_str(&t).map(Some).map_err(ws_err),
                Message::Close(_) => return Ok(None),
                _ => {}
            }
        }
        Ok(None)
    }

    /// The number of the next announced block; `None` once the node closes the socket.
    pub async fn next(&mut self) -> Result<Option<u64>, StoreError> {
        Ok(self.next_tip().await?.map(|(n, _)| n))
    }

    /// The next announced block and its base fee (0 when the header has none).
    pub async fn next_tip(&mut self) -> Result<Option<(u64, u128)>, StoreError> {
        loop {
            let Some(v) = self.message().await? else {
                return Ok(None);
            };
            if v["method"] != "eth_subscription" {
                continue;
            }
            let head = &v["params"]["result"];
            let n = head["number"].as_str().unwrap_or("");
            let number = u64::from_str_radix(n.trim_start_matches("0x"), 16)
                .map_err(|_| ws_err(format!("bad block number {n:?}")))?;
            let fee = head["baseFeePerGas"].as_str().unwrap_or("0x0");
            let base_fee = u128::from_str_radix(fee.trim_start_matches("0x"), 16)
                .map_err(|_| ws_err(format!("bad base fee {fee:?}")))?;
            return Ok(Some((number, base_fee)));
        }
    }
}
