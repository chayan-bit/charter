//! `RpcArkiv` against a scripted local HTTP server: chain guard, access key, 429 handling,
//! wire parsing and error mapping. No network.

use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use alloy_primitives::B256;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{SolError, SolValue};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::{Pred, TxParams, attr};

type Reply = (u16, Vec<(&'static str, String)>, Json);
type Seen = Arc<Mutex<Vec<(String, Json)>>>;

fn ok(result: Json) -> Reply {
    (
        200,
        vec![],
        json!({ "jsonrpc": "2.0", "id": 1, "result": result }),
    )
}

fn fail(code: i64, message: &str, data: Option<String>) -> Reply {
    (
        200,
        vec![],
        json!({ "jsonrpc": "2.0", "id": 1, "error": { "code": code, "message": message, "data": data } }),
    )
}

const RABDI_HEX: &str = "0x76edaf";

/// Serve `h` on a local port; returns the URL and every (request head, body) seen.
async fn mock(h: impl Fn(&Json) -> Reply + Send + Sync + 'static) -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("http://{}", listener.local_addr().expect("addr"));
    let seen: Seen = Arc::default();
    let (h, log) = (Arc::new(h), seen.clone());
    tokio::spawn(async move {
        while let Ok((conn, _)) = listener.accept().await {
            let (h, log) = (h.clone(), log.clone());
            tokio::spawn(async move { serve(conn, &*h, &log).await });
        }
    });
    (url, seen)
}

async fn serve(mut conn: TcpStream, h: &(dyn Fn(&Json) -> Reply + Send + Sync), log: &Seen) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let (head, body) = loop {
        let Ok(n) = conn.read(&mut tmp).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&tmp[..n]);
        let text = String::from_utf8_lossy(&buf).to_string();
        let Some(split) = text.find("\r\n\r\n") else {
            continue;
        };
        let head = text[..split].to_lowercase();
        let len = head
            .lines()
            .find_map(|l| {
                l.strip_prefix("content-length:")
                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
            })
            .unwrap_or(0);
        if buf.len() >= split + 4 + len {
            break (
                head,
                serde_json::from_slice::<Json>(&buf[split + 4..split + 4 + len])
                    .unwrap_or_default(),
            );
        }
    };
    let (status, headers, reply) = h(&body);
    log.lock()
        .unwrap_or_else(|p| p.into_inner())
        .push((head, body));
    if status == 0 {
        return; // Scripted connection drop: close without a response.
    }
    let payload = reply.to_string();
    let extra: String = headers
        .iter()
        .map(|(k, v)| format!("{k}: {v}\r\n"))
        .collect();
    let resp = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra}\r\n{payload}",
        payload.len()
    );
    let _ = conn.write_all(resp.as_bytes()).await;
}

fn cfg(url: &str) -> RpcConfig {
    RpcConfig {
        url: url.into(),
        max_rate_wait: Duration::from_secs(5),
        ..RpcConfig::rabdi(Some("sekret".into()))
    }
}

async fn client(h: impl Fn(&Json) -> Reply + Send + Sync + 'static) -> (RpcArkiv, Seen) {
    let (url, seen) = mock(move |b| {
        if b["method"] == "eth_chainId" {
            ok(json!(RABDI_HEX))
        } else {
            h(b)
        }
    })
    .await;
    (RpcArkiv::connect(cfg(&url)).await.expect("connect"), seen)
}

fn signed(chain_id: u64, input: Bytes) -> SignedTx {
    let p = TxParams {
        nonce: 0,
        to: ARKIV_ADDRESS,
        value: U256::ZERO,
        input,
        gas_limit: 100_000,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 0,
    };
    SignedTx::sign(&PrivateKeySigner::random(), chain_id, p).expect("sign")
}

#[tokio::test]
async fn connect_checks_the_chain_and_sends_the_access_key() {
    let (rpc, seen) = client(|_| ok(json!(null))).await;
    let head = seen.lock().expect("log")[0].0.clone();
    assert!(head.contains("x-api-key: sekret"), "{head}");
    assert!(
        !format!("{rpc:?}").contains("sekret"),
        "the key never reaches Debug output"
    );
    let (url, _) = mock(|_| ok(json!("0x1"))).await;
    let err = RpcArkiv::connect(cfg(&url)).await.expect_err("wrong chain");
    assert_eq!(
        err,
        StoreError::WrongChain {
            expected: RABDI_CHAIN_ID,
            got: 1
        }
    );
    assert_eq!(RpcConfig::rabdi(Some(String::new())).access_key, None);
}

#[tokio::test]
async fn a_short_retry_after_is_waited_out() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (rpc, _) = client(move |_| {
        if c.fetch_add(1, Ordering::SeqCst) == 0 {
            (
                429,
                vec![("retry-after", "1".into())],
                json!({ "error": "ANON_COST_LIMITED" }),
            )
        } else {
            ok(json!("0x10"))
        }
    })
    .await;
    let t = Instant::now();
    assert_eq!(rpc.head().await.expect("head"), 16);
    assert!(t.elapsed() >= Duration::from_secs(1));
}

#[tokio::test]
async fn dropped_connections_are_retried_for_reads_but_not_for_sends() {
    let calls = Arc::new(AtomicUsize::new(0));
    let c = calls.clone();
    let (rpc, seen) = client(move |b| {
        let n = c.fetch_add(1, Ordering::SeqCst);
        match b["method"].as_str() {
            Some("eth_blockNumber") if n < 2 => (0, vec![], json!(null)),
            Some("eth_blockNumber") => ok(json!("0x10")),
            _ => (0, vec![], json!(null)),
        }
    })
    .await;
    assert_eq!(rpc.head().await.expect("head after two drops"), 16);
    let before = seen.lock().expect("log").len();
    let err = rpc
        .send(signed(RABDI_CHAIN_ID, Bytes::new()))
        .await
        .expect_err("a dropped send surfaces");
    assert!(matches!(err, StoreError::Rpc(_)), "{err:?}");
    assert_eq!(seen.lock().expect("log").len(), before + 1, "sent once");
    calls.store(0, Ordering::SeqCst);
    let (url, _) = mock(|b| {
        if b["method"] == "eth_chainId" {
            ok(json!(RABDI_HEX))
        } else {
            (0, vec![], json!(null))
        }
    })
    .await;
    let rpc = RpcArkiv::connect(cfg(&url)).await.expect("connect");
    assert!(
        matches!(rpc.head().await, Err(StoreError::Rpc(_))),
        "gives up after the retries"
    );
}

#[tokio::test]
async fn a_long_or_missing_retry_after_surfaces_e_rate_limited_at_once() {
    let (rpc, _) = client(|b| {
        if b["method"] == "eth_blockNumber" {
            (429, vec![("retry-after", "391".into())], json!({}))
        } else {
            (429, vec![], json!({}))
        }
    })
    .await;
    let t = Instant::now();
    let err = rpc.head().await.expect_err("limited");
    assert_eq!(
        err,
        StoreError::RateLimited {
            retry_after: Some(Duration::from_secs(391))
        }
    );
    assert_eq!(err.code(), "E_RATE_LIMITED");
    assert_eq!(
        rpc.balance(Address::ZERO).await,
        Err(StoreError::RateLimited { retry_after: None })
    );
    assert!(t.elapsed() < Duration::from_secs(3), "never hangs");
}

#[tokio::test]
async fn reads_parse_the_wire_types() {
    let receipt = json!({
        "transactionHash": B256::repeat_byte(1), "blockNumber": "0x2a", "status": "0x1",
        "gasUsed": "0x5208", "effectiveGasPrice": "0x3b9aca00"
    });
    let (rpc, seen) = client(move |b| match b["method"].as_str().unwrap_or("") {
        "eth_blockNumber" => ok(json!("0x10")),
        "eth_getBlockByNumber" => ok(json!({ "baseFeePerGas": "0x3b9aca00" })),
        "eth_getBalance" => ok(json!("0x64")),
        "eth_getTransactionCount" => ok(json!("0x5")),
        "eth_estimateGas" => ok(json!("0x5208")),
        "eth_call" => ok(json!(Bytes::from(314u64.abi_encode()))),
        "arkiv_query" => {
            ok(json!({ "blockNumber": "0x9", "count": "0x2", "data": [{ "key": B256::ZERO }] }))
        }
        "eth_getTransactionReceipt" if b["params"][0] == json!(B256::repeat_byte(1)) => {
            ok(receipt.clone())
        }
        _ => ok(json!(null)),
    })
    .await;
    assert_eq!(rpc.head().await, Ok(16));
    assert_eq!(rpc.base_fee().await, Ok(1_000_000_000));
    assert_eq!(rpc.balance(Address::ZERO).await, Ok(U256::from(100)));
    assert_eq!(rpc.tx_count(Address::ZERO).await, Ok(5));
    assert_eq!(rpc.estimate(Address::ZERO, &[]).await, Ok(21_000));
    assert_eq!(rpc.entity_nonce(Address::ZERO).await, Ok(314));
    let q = Query::from(Pred::eq(attr("ag.kind"), "anchor"));
    let page = rpc
        .query(&q, &QueryOpts::count_only())
        .await
        .expect("query");
    assert_eq!(
        (page.block_number, page.count, page.rows.len()),
        (9, Some(2), 1)
    );
    let r = rpc
        .receipt(TxHash::repeat_byte(1))
        .await
        .expect("r")
        .expect("some");
    assert_eq!(
        (r.block_number, r.success, r.gas_used, r.effective_gas_price),
        (42, true, 21_000, 1_000_000_000)
    );
    assert_eq!(rpc.receipt(TxHash::repeat_byte(2)).await, Ok(None));
    let log = seen.lock().expect("log");
    let sent = log
        .iter()
        .find(|(_, b)| b["method"] == "arkiv_query")
        .expect("query sent");
    assert_eq!(
        sent.1["params"],
        json!(["ag.kind = str('anchor')", { "select": { "count": true }, "limit": "0x1" }])
    );
    let call = log
        .iter()
        .find(|(_, b)| b["method"] == "eth_call")
        .expect("call sent");
    assert_eq!(call.1["params"][0]["to"], json!(ARKIV_ADDRESS));
    assert!(
        call.1["params"][0]["data"]
            .as_str()
            .expect("hex")
            .starts_with("0x36917bfd"),
        "entityNonce selector"
    );
    let count = log
        .iter()
        .find(|(_, b)| b["method"] == "eth_getTransactionCount")
        .expect("sent");
    assert_eq!(count.1["params"][1], "pending");
}

#[tokio::test]
async fn transaction_by_hash_and_mined_nonce_parse() {
    // Shape of a plain Rabdi transfer from eth_getTransactionByHash.
    let t = json!({
        "hash": B256::repeat_byte(1), "from": Address::repeat_byte(2),
        "to": Address::repeat_byte(3), "value": "0x853a0d2313c0000",
        "blockNumber": "0x2a", "type": "0x2", "nonce": "0x0"
    });
    let (rpc, seen) = client(move |b| match b["method"].as_str().unwrap_or("") {
        "eth_getTransactionCount" => ok(json!("0x4")),
        "eth_getTransactionByHash" if b["params"][0] == json!(B256::repeat_byte(1)) => {
            ok(t.clone())
        }
        _ => ok(json!(null)),
    })
    .await;
    let got = rpc
        .transaction(TxHash::repeat_byte(1))
        .await
        .expect("t")
        .expect("some");
    assert_eq!(
        got,
        TxInfo {
            hash: TxHash::repeat_byte(1),
            from: Address::repeat_byte(2),
            to: Some(Address::repeat_byte(3)),
            value: U256::from(0x853a0d2313c0000u64),
            block_number: Some(42),
        }
    );
    assert_eq!(rpc.transaction(TxHash::repeat_byte(2)).await, Ok(None));
    assert_eq!(rpc.tx_count_mined(Address::ZERO).await, Ok(4));
    let log = seen.lock().expect("log");
    let count = log
        .iter()
        .find(|(_, b)| b["method"] == "eth_getTransactionCount")
        .expect("sent");
    assert_eq!(count.1["params"][1], "latest");
}

#[tokio::test]
async fn node_errors_map_to_store_errors() {
    let not_owner = IEntityRegistry::NotOwner {
        entityKey: B256::ZERO,
        caller: Address::ZERO,
        owner: Address::ZERO,
    };
    let data = hex::encode_prefixed(not_owner.abi_encode());
    let (rpc, _) = client(move |b| match b["method"].as_str().unwrap_or("") {
        "eth_estimateGas" => fail(3, "execution reverted", Some(data.clone())),
        "eth_getBalance" => fail(-32000, "execution reverted", None),
        "arkiv_query" => fail(-32001, "query error at byte 0", None),
        "eth_getTransactionCount" => (413, vec![], json!("request body too large")),
        "eth_blockNumber" => fail(-32603, "internal", None),
        _ => ok(json!(null)),
    })
    .await;
    let reason = |e| match e {
        Err(StoreError::Reverted { reason, .. }) => reason,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        reason(rpc.estimate(Address::ZERO, &[]).await.map(|_| ())),
        "NotOwner"
    );
    assert_eq!(
        reason(rpc.balance(Address::ZERO).await.map(|_| ())),
        "execution reverted"
    );
    let q = Query::from(Pred::All);
    assert!(matches!(
        rpc.query(&q, &QueryOpts::default()).await,
        Err(StoreError::InvalidQuery(_))
    ));
    assert!(matches!(
        rpc.tx_count(Address::ZERO).await,
        Err(StoreError::Http { status: 413, .. })
    ));
    assert!(matches!(rpc.head().await, Err(StoreError::Rpc(m)) if m.contains("-32603")));
}

async fn send_with(message: &'static str) -> (SignedTx, Result<TxHash, StoreError>) {
    let (rpc, _) = client(move |b| match b["method"].as_str().unwrap_or("") {
        "eth_getBalance" => ok(json!("0x7")),
        _ => fail(-32000, message, None),
    })
    .await;
    let tx = signed(RABDI_CHAIN_ID, Bytes::new());
    let got = rpc.send(tx.clone()).await;
    (tx, got)
}

#[tokio::test]
async fn send_errors_map_to_store_errors() {
    let (_, got) = send_with("nonce too low: next nonce 5, tx nonce 0").await;
    assert!(matches!(got, Err(StoreError::NonceTooLow(_))), "{got:?}");
    let (tx, got) = send_with("already known").await;
    assert_eq!(
        got,
        Ok(tx.hash()),
        "a resend the pool already holds is success"
    );
    let (tx, got) = send_with("insufficient funds for gas * price + value").await;
    let want = StoreError::InsufficientFunds {
        address: tx.from(),
        need: tx.max_cost(),
        have: U256::from(7),
    };
    assert_eq!(got, Err(want));
    assert_eq!(got.map_err(|e| e.code()), Err("E_INSUFFICIENT_FUNDS"));
}

#[tokio::test]
async fn send_guards_chain_id_and_size_before_any_request() {
    let (rpc, seen) = client(|_| ok(json!(B256::ZERO))).await;
    let other = signed(1, Bytes::new());
    assert_eq!(
        rpc.send(other).await,
        Err(StoreError::WrongChain {
            expected: RABDI_CHAIN_ID,
            got: 1
        })
    );
    let big = signed(RABDI_CHAIN_ID, vec![1u8; MAX_RAW_TX_BYTES].into());
    assert_eq!(
        rpc.send(big).await.map_err(|e| e.code()),
        Err("E_TX_TOO_LARGE")
    );
    assert_eq!(
        seen.lock().expect("log").len(),
        1,
        "only the connect probe reached the node"
    );
}

#[tokio::test]
async fn the_quota_headers_of_the_latest_responses_are_kept_and_a_long_wait_bails() {
    let (mut rpc, _) = client(|b| match b["method"].as_str() {
        Some("arkiv_query") => (
            429,
            vec![
                ("ratelimit", "limit=10000, remaining=0, reset=2035".into()),
                ("retry-after", "2".into()),
            ],
            json!({ "error": "ANON_COST_LIMITED" }),
        ),
        _ => {
            let (s, _, j) = ok(json!("0x10"));
            (s, vec![("arkiv-quota-used-percent", "12".into())], j)
        }
    })
    .await;
    assert_eq!(rpc.quota(), None, "nothing reported yet");
    rpc.head().await.expect("head");
    assert_eq!(rpc.quota().and_then(|q| q.used()), Some(12.0));
    // A caller that reschedules itself (chart ui) waits out no Retry-After at all.
    rpc.set_max_rate_wait(Duration::ZERO);
    let t = Instant::now();
    let q = Query::from(Pred::eq(attr("ag.kind"), "cob"));
    let err = rpc
        .query(&q, &QueryOpts::count_only())
        .await
        .expect_err("limited");
    assert_eq!(
        err,
        StoreError::RateLimited {
            retry_after: Some(Duration::from_secs(2))
        }
    );
    assert!(
        t.elapsed() < Duration::from_secs(1),
        "no sleep inside the call"
    );
    let worst = rpc.quota().expect("quota");
    assert_eq!(
        (worst.used(), worst.remaining, worst.retry_after),
        (Some(100.0), Some(0), Some(2))
    );
    rpc.head().await.expect("head");
    assert_eq!(
        rpc.quota().and_then(|q| q.used()),
        Some(100.0),
        "the query budget stays the worst"
    );
}
