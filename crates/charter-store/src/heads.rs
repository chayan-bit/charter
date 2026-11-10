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
        loop {
            let Some(v) = self.message().await? else {
                return Ok(None);
            };
            if v["method"] != "eth_subscription" {
                continue;
            }
            let n = v["params"]["result"]["number"].as_str().unwrap_or("");
            return u64::from_str_radix(n.trim_start_matches("0x"), 16)
                .map(Some)
                .map_err(|_| ws_err(format!("bad block number {n:?}")));
        }
    }
}
