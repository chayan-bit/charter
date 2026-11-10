//! `RpcArkiv`: JSON-RPC over HTTP to an Arkiv node (Rabdi by default).
//!
//! The chain id is checked on connect and on every send. HTTP 429 (`ANON_COST_LIMITED`)
//! becomes `E_RATE_LIMITED`; the client waits out `Retry-After` only within
//! `max_rate_wait`, and every request has a timeout, so a call never hangs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alloy_primitives::{Address, Bytes, TxHash, U64, U128, U256, hex};
use alloy_sol_types::SolCall;
use arkiv_bindings::{IEntityRegistry, Operation};
use arkiv_rpc_types::QueryResponse;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value as Json, json};

use crate::{
    ARKIV_ADDRESS, Arkiv, MAX_RAW_TX_BYTES, MinedTx, Query, QueryOpts, QueryPage, Quota,
    RABDI_CHAIN_ID, RABDI_RPC, Receipt, SignedTx, StoreError, TxInfo, decode_revert,
    execute_calldata,
};

/// Header carrying the optional access key.
pub const ACCESS_KEY_HEADER: &str = "X-API-KEY";

/// Extra attempts for a call whose connection drops before a full response arrives.
/// Rabdi was measured dropping about 1 in 10 requests (2026-11-09), so a 19-call
/// clone without retries succeeded only about 13% of the time.
const TRANSPORT_RETRIES: u32 = 3;

#[derive(Clone)]
pub struct RpcConfig {
    pub url: String,
    /// The only chain this client will talk to.
    pub chain_id: u64,
    /// Arkiv access key; raises the anonymous query budget. Never logged.
    pub access_key: Option<String>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// Longest total time one call may spend waiting out `Retry-After`.
    pub max_rate_wait: Duration,
}

impl RpcConfig {
    /// The `newHeads` socket on the same host, with the same access key.
    pub fn ws(&self) -> crate::heads::WsConfig {
        crate::heads::WsConfig {
            url: crate::heads::ws_url(&self.url),
            access_key: self.access_key.clone(),
        }
    }

    pub fn rabdi(access_key: Option<String>) -> Self {
        Self {
            url: RABDI_RPC.into(),
            chain_id: RABDI_CHAIN_ID,
            access_key: access_key.filter(|k| !k.is_empty()),
            timeout: Duration::from_secs(30),
            max_rate_wait: Duration::from_secs(60),
        }
    }
}

impl std::fmt::Debug for RpcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RpcConfig")
            .field("url", &self.url)
            .field("chain_id", &self.chain_id)
            .field(
                "access_key",
                &self.access_key.as_ref().map(|_| "<redacted>"),
            )
            .field("timeout", &self.timeout)
            .field("max_rate_wait", &self.max_rate_wait)
            .finish()
    }
}

#[derive(Debug)]
pub struct RpcArkiv {
    http: reqwest::Client,
    cfg: RpcConfig,
    id: AtomicU64,
    /// The chain id `connect` verified; 0 until then. Saves one round trip per command.
    verified_chain: AtomicU64,
    /// `CHARTER_TRACE` set: one stderr line per call with its method, time and size.
    trace: bool,
    /// The latest quota reports: `[arkiv_query, every other method]` (separate budgets),
    /// shared with every [`RpcArkiv::with_max_rate_wait`] twin.
    quota: Arc<Mutex<[Option<Quota>; 2]>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReceipt {
    transaction_hash: TxHash,
    block_number: U64,
    status: U64,
    gas_used: U64,
    effective_gas_price: U128,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTx {
    hash: TxHash,
    from: Address,
    to: Option<Address>,
    value: U256,
    block_number: Option<U64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMinedTx {
    hash: TxHash,
    from: Address,
    to: Option<Address>,
    block_number: Option<U64>,
    transaction_index: Option<U64>,
    input: Bytes,
}

impl RawMinedTx {
    fn mined(self) -> Option<MinedTx> {
        Some(MinedTx {
            hash: self.hash,
            from: self.from,
            to: self.to,
            block: u64_of(self.block_number?),
            index: u32::try_from(u64_of(self.transaction_index?)).ok()?,
            input: self.input,
        })
    }
}

#[derive(Deserialize)]
struct RawFullBlock {
    transactions: Vec<RawMinedTx>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawBlock {
    number: U64,
    base_fee_per_gas: Option<U128>,
}

impl RpcArkiv {
    pub fn config(&self) -> &RpcConfig {
        &self.cfg
    }

    /// A twin that waits out at most `d` of `Retry-After` per call (zero for a caller that
    /// reschedules itself, as `chart ui`'s sync loop does), sharing this client's
    /// connections and quota reports. Writes stay on the original: a multi-tx write cut
    /// off by a 429 mid-sequence leaves partial state.
    pub fn with_max_rate_wait(&self, d: Duration) -> Self {
        Self {
            http: self.http.clone(),
            cfg: RpcConfig {
                max_rate_wait: d,
                ..self.cfg.clone()
            },
            id: AtomicU64::new(1),
            verified_chain: AtomicU64::new(self.verified_chain.load(Ordering::Relaxed)),
            trace: self.trace,
            quota: self.quota.clone(),
        }
    }

    fn note_quota(&self, method: &str, q: Option<Quota>) {
        if let (Some(q), Ok(mut all)) = (q, self.quota.lock()) {
            all[usize::from(method != "arkiv_query")] = Some(q);
        }
    }

    /// Build the client and refuse to continue unless the RPC serves `cfg.chain_id`.
    pub async fn connect(cfg: RpcConfig) -> Result<Self, StoreError> {
        let http = reqwest::Client::builder()
            .timeout(cfg.timeout)
            .build()
            .map_err(|e| StoreError::Rpc(format!("http client: {e}")))?;
        let me = Self {
            http,
            cfg,
            id: AtomicU64::new(1),
            verified_chain: AtomicU64::new(0),
            trace: std::env::var_os("CHARTER_TRACE").is_some_and(|v| !v.is_empty()),
            quota: Arc::default(),
        };
        let got = me.chain_id().await?;
        if got != me.cfg.chain_id {
            return Err(StoreError::WrongChain {
                expected: me.cfg.chain_id,
                got,
            });
        }
        me.verified_chain.store(got, Ordering::Relaxed);
        Ok(me)
    }

    async fn call<T: DeserializeOwned>(&self, method: &str, params: Json) -> Result<T, StoreError> {
        let id = self.id.fetch_add(1, Ordering::Relaxed);
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let mut waited = Duration::ZERO;
        let started = std::time::Instant::now();
        // Sends are not retried here: the tx pipeline owns resends and nonce recovery.
        let mut retries = if method == "eth_sendRawTransaction" {
            0
        } else {
            TRANSPORT_RETRIES
        };
        loop {
            let mut req = self.http.post(&self.cfg.url).json(&body);
            if let Some(k) = &self.cfg.access_key {
                req = req.header(ACCESS_KEY_HEADER, k);
            }
            let resp = match req.send().await {
                Ok(resp) => resp,
                Err(e) if retries > 0 => {
                    retries -= 1;
                    self.transport_backoff(method, &e, retries).await;
                    continue;
                }
                Err(e) => return Err(StoreError::Rpc(format!("{method}: {e}"))),
            };
            let status = resp.status().as_u16();
            let version = resp.version();
            let now = std::time::Instant::now();
            self.note_quota(method, Quota::from_response(status, resp.headers(), now));
            if status == 429 {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok()?.trim().parse().ok())
                    .map(Duration::from_secs);
                match retry_after {
                    Some(d) if waited + d <= self.cfg.max_rate_wait => {
                        // stderr only: stdout belongs to the git remote-helper protocol.
                        eprintln!("chart: rate limited on {method}, retrying in {d:?}");
                        tokio::time::sleep(d).await;
                        waited += d;
                        continue;
                    }
                    _ => return Err(StoreError::RateLimited { retry_after }),
                }
            }
            let text = match resp.text().await {
                Ok(text) => text,
                Err(e) if retries > 0 => {
                    retries -= 1;
                    self.transport_backoff(method, &e, retries).await;
                    continue;
                }
                Err(e) => return Err(StoreError::Rpc(format!("{method}: {e}"))),
            };
            if !(200..300).contains(&status) {
                return Err(StoreError::Http {
                    status,
                    body: text.chars().take(300).collect(),
                });
            }
            if self.trace {
                eprintln!(
                    "chart-trace: {method} {} ms {} B {version:?}",
                    started.elapsed().as_millis(),
                    text.len()
                );
            }
            let mut j: Json = serde_json::from_str(&text)
                .map_err(|e| StoreError::Rpc(format!("{method}: {e}")))?;
            if let Some(err) = j.get("error") {
                return Err(rpc_error(err));
            }
            return serde_json::from_value(j["result"].take())
                .map_err(|e| StoreError::Rpc(format!("{method}: {e}")));
        }
    }

    /// Waits 200, 400, then 800 ms before re-sending a call whose connection dropped.
    async fn transport_backoff(&self, method: &str, e: &reqwest::Error, left: u32) {
        let wait = Duration::from_millis(200 << (TRANSPORT_RETRIES - 1 - left));
        if self.trace {
            eprintln!("chart-trace: {method} dropped ({e}), retrying in {wait:?}");
        }
        tokio::time::sleep(wait).await;
    }
}

/// Map a JSON-RPC error object onto a `StoreError`.
fn rpc_error(err: &Json) -> StoreError {
    let code = err["code"].as_i64().unwrap_or(0);
    let msg = err["message"].as_str().unwrap_or("").to_string();
    let lower = msg.to_lowercase();
    if lower.contains("nonce too low") {
        return StoreError::NonceTooLow(msg);
    }
    if code == 3 || lower.contains("revert") {
        let data = err["data"].as_str().and_then(|d| hex::decode(d).ok());
        let reason = data.as_deref().and_then(decode_revert).unwrap_or(msg);
        return StoreError::Reverted { tx: None, reason };
    }
    if (-32005..=-32001).contains(&code) {
        return StoreError::InvalidQuery(msg);
    }
    StoreError::Rpc(format!("{code}: {msg}"))
}

fn u64_of(v: U64) -> u64 {
    v.to::<u64>()
}

impl Arkiv for RpcArkiv {
    /// The worse of the query and the plain budget, among reports not yet past their reset.
    fn quota(&self) -> Option<Quota> {
        let now = std::time::Instant::now();
        let all = self.quota.lock().ok()?.clone();
        all.into_iter()
            .flatten()
            .filter(|q| now < q.until())
            .max_by(|a, b| a.used().unwrap_or(0.0).total_cmp(&b.used().unwrap_or(0.0)))
    }

    async fn chain_id(&self) -> Result<u64, StoreError> {
        match self.verified_chain.load(Ordering::Relaxed) {
            0 => self.call::<U64>("eth_chainId", json!([])).await.map(u64_of),
            known => Ok(known),
        }
    }

    async fn head(&self) -> Result<u64, StoreError> {
        self.call::<U64>("eth_blockNumber", json!([]))
            .await
            .map(u64_of)
    }

    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.head_fee().await.map(|(_, fee)| fee)
    }

    async fn head_fee(&self) -> Result<(u64, u128), StoreError> {
        let b: RawBlock = self
            .call("eth_getBlockByNumber", json!(["latest", false]))
            .await?;
        let fee = b.base_fee_per_gas.map(|f| f.to::<u128>()).unwrap_or(0);
        Ok((u64_of(b.number), fee))
    }

    fn heads_ws(&self) -> Option<crate::heads::WsConfig> {
        // Only real endpoints serve a socket; tests point at plain HTTP mocks.
        self.cfg.url.starts_with("https://").then(|| self.cfg.ws())
    }

    async fn query(&self, q: &Query, opts: &QueryOpts) -> Result<QueryPage, StoreError> {
        let r: QueryResponse = self
            .call("arkiv_query", json!([q.render()?, opts.to_json()?]))
            .await?;
        Ok(QueryPage {
            block_number: r.block_number,
            count: r.count,
            rows: r.data,
        })
    }

    async fn entity_nonce(&self, creator: Address) -> Result<u64, StoreError> {
        let data = Bytes::from(IEntityRegistry::entityNonceCall { owner: creator }.abi_encode());
        let out: Bytes = self
            .call(
                "eth_call",
                json!([{ "to": ARKIV_ADDRESS, "data": data }, "latest"]),
            )
            .await?;
        IEntityRegistry::entityNonceCall::abi_decode_returns(&out)
            .map_err(|e| StoreError::Rpc(format!("entityNonce: {e}")))
    }

    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.call("eth_getBalance", json!([a, "latest"])).await
    }

    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.call::<U64>("eth_getTransactionCount", json!([a, "pending"]))
            .await
            .map(u64_of)
    }

    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.call::<U64>("eth_getTransactionCount", json!([a, "latest"]))
            .await
            .map(u64_of)
    }

    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        let tx = json!({ "from": from, "to": ARKIV_ADDRESS, "data": execute_calldata(ops) });
        self.call::<U64>("eth_estimateGas", json!([tx]))
            .await
            .map(u64_of)
    }

    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        if tx.chain_id() != self.cfg.chain_id {
            return Err(StoreError::WrongChain {
                expected: self.cfg.chain_id,
                got: tx.chain_id(),
            });
        }
        if tx.raw().len() > MAX_RAW_TX_BYTES {
            return Err(StoreError::TxTooLarge {
                size: tx.raw().len(),
                cap: MAX_RAW_TX_BYTES,
            });
        }
        match self
            .call::<TxHash>("eth_sendRawTransaction", json!([tx.raw()]))
            .await
        {
            Err(StoreError::Rpc(m)) if m.contains("already known") => Ok(tx.hash()),
            Err(StoreError::Rpc(m)) if m.to_lowercase().contains("insufficient funds") => {
                let have = self.balance(tx.from()).await.unwrap_or_default();
                Err(StoreError::InsufficientFunds {
                    address: tx.from(),
                    need: tx.max_cost(),
                    have,
                })
            }
            other => other,
        }
    }

    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        let r: Option<RawReceipt> = self.call("eth_getTransactionReceipt", json!([h])).await?;
        Ok(r.map(|r| Receipt {
            tx_hash: r.transaction_hash,
            block_number: u64_of(r.block_number),
            success: u64_of(r.status) == 1,
            gas_used: u64_of(r.gas_used),
            effective_gas_price: r.effective_gas_price.to::<u128>(),
        }))
    }

    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        let t: Option<RawTx> = self.call("eth_getTransactionByHash", json!([h])).await?;
        Ok(t.map(|t| TxInfo {
            hash: t.hash,
            from: t.from,
            to: t.to,
            value: t.value,
            block_number: t.block_number.map(u64_of),
        }))
    }

    async fn mined_tx(&self, h: TxHash) -> Result<Option<MinedTx>, StoreError> {
        let t: Option<RawMinedTx> = self.call("eth_getTransactionByHash", json!([h])).await?;
        Ok(t.and_then(RawMinedTx::mined))
    }

    async fn block_txs(&self, n: u64) -> Result<Vec<MinedTx>, StoreError> {
        let b: Option<RawFullBlock> = self
            .call("eth_getBlockByNumber", json!([format!("{n:#x}"), true]))
            .await?;
        Ok(b.map(|b| {
            b.transactions
                .into_iter()
                .filter_map(RawMinedTx::mined)
                .collect()
        })
        .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests;
