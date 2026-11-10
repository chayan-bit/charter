//! `eth_subscribe newHeads` against a local WebSocket server that speaks the node's wire
//! format: the subscription reply, then one notification per block.

use charter_store::heads::{WsConfig, subscribe, ws_url};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

/// Serves one connection: checks the access key and the request, answers `reply`, then
/// sends `then` and closes. Returns the server's URL.
#[allow(clippy::result_large_err)] // the handshake callback's error type is tungstenite's
async fn server(reply: Value, then: Vec<Message>) -> String {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}", l.local_addr().expect("addr"));
    tokio::spawn(async move {
        let (s, _) = l.accept().await.expect("accept");
        let check = |req: &Request, resp: Response| {
            assert_eq!(req.headers()["x-api-key"], "k");
            Ok(resp)
        };
        let mut ws = tokio_tungstenite::accept_hdr_async(s, check)
            .await
            .expect("handshake");
        let Some(Ok(Message::Text(t))) = ws.next().await else {
            panic!("no request");
        };
        let req: Value = serde_json::from_str(&t).expect("json");
        assert_eq!(
            (&req["method"], &req["params"]),
            (&json!("eth_subscribe"), &json!(["newHeads"]))
        );
        ws.send(Message::text(reply.to_string()))
            .await
            .expect("reply");
        for m in then {
            ws.send(m).await.expect("send");
        }
        let _ = ws.close(None).await;
    });
    url
}

fn head(n: &str) -> Message {
    let v = json!({"jsonrpc": "2.0", "method": "eth_subscription",
        "params": {"subscription": "0xab", "result": {"number": n, "hash": "0x00"}}});
    Message::text(v.to_string())
}

fn cfg(url: String) -> WsConfig {
    WsConfig {
        url,
        access_key: Some("k".into()),
    }
}

#[tokio::test]
async fn yields_each_block_number_until_the_server_closes() {
    let ok = json!({"jsonrpc": "2.0", "id": 1, "result": "0xab"});
    let then = vec![head("0x10"), Message::Ping(vec![1].into()), head("0x11")];
    let mut h = subscribe(&cfg(server(ok, then).await))
        .await
        .expect("subscribe");
    assert_eq!(h.next().await.expect("head"), Some(16));
    assert_eq!(h.next().await.expect("head"), Some(17));
    assert_eq!(h.next().await.expect("closed"), None);
}

#[tokio::test]
async fn a_refused_subscription_is_an_error() {
    let no = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "no pubsub"}});
    let e = subscribe(&cfg(server(no, vec![]).await))
        .await
        .err()
        .expect("refused");
    assert!(e.to_string().contains("no pubsub"), "{e}");
}

#[test]
fn the_socket_lives_on_the_rpc_host() {
    assert_eq!(ws_url("https://rpc.example/x"), "wss://rpc.example/x");
    assert_eq!(ws_url("http://127.0.0.1:8545"), "ws://127.0.0.1:8545");
}
