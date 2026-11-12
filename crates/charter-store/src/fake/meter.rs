//! `Meter`: wraps a backend and counts what one operation costs on the wire, for the
//! performance gate (docs/p5/speed-and-cost.md section 7). Every count is exact on the
//! fake, so a gate can compare them with zero margin:
//!
//! - `rounds`: sequential round trips; calls that overlap share one (each call yields once,
//!   as a network call would), a call starting with none in flight opens one;
//! - `calls` and `queries`: RPC calls and `arkiv_query` calls;
//! - `txs` and `bytes`: transactions sent and their calldata bytes;
//! - `gas`: gas of the landed txs with every calldata byte priced as non-zero. Signatures
//!   are hedged and salts random, so the zero bytes of calldata, and with them the exact
//!   `gasUsed`, move by a few dozen gas between runs; the ceiling does not.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};

use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, TxHash, U256};
use arkiv_bindings::Operation;
use serde::{Deserialize, Serialize};

use super::exec::intrinsic_gas;
use crate::{
    Arkiv, EntityData, MinedTx, Query, QueryOpts, QueryPage, Quota, Receipt, SignedTx, StoreError,
    TxInfo,
};

/// What one operation cost; see the module docs for each field.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Metrics {
    pub rounds: u64,
    pub calls: u64,
    pub queries: u64,
    pub txs: u64,
    pub bytes: u64,
    pub gas: u64,
}

impl Metrics {
    /// `(name, value)` of every field, in a fixed order.
    pub fn fields(&self) -> [(&'static str, u64); 6] {
        [
            ("rounds", self.rounds),
            ("calls", self.calls),
            ("queries", self.queries),
            ("txs", self.txs),
            ("bytes", self.bytes),
            ("gas", self.gas),
        ]
    }

    /// The smaller of each field (timing slack only ever adds a read).
    pub fn min(&self, o: &Self) -> Self {
        Self {
            rounds: self.rounds.min(o.rounds),
            calls: self.calls.min(o.calls),
            queries: self.queries.min(o.queries),
            txs: self.txs.min(o.txs),
            bytes: self.bytes.min(o.bytes),
            gas: self.gas.min(o.gas),
        }
    }
}

/// A backend that counts round trips, calls, sent bytes and landed gas (see the module docs).
pub struct Meter<'a, A> {
    inner: &'a A,
    calls: Mutex<BTreeMap<&'static str, u64>>,
    inflight: AtomicU64,
    rounds: AtomicU64,
    /// Calldata gas ceiling minus the intrinsic gas, by sent tx not yet seen landed.
    sent: Mutex<HashMap<TxHash, i128>>,
    txs: AtomicU64,
    bytes: AtomicU64,
    gas: AtomicU64,
}

impl<'a, A: Arkiv> Meter<'a, A> {
    pub fn new(inner: &'a A) -> Self {
        Self {
            inner,
            calls: Mutex::default(),
            inflight: AtomicU64::new(0),
            rounds: AtomicU64::new(0),
            sent: Mutex::default(),
            txs: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            gas: AtomicU64::new(0),
        }
    }

    /// Forgets everything counted so far.
    pub fn reset(&self) {
        self.calls.lock().expect("lock").clear();
        self.sent.lock().expect("lock").clear();
        for c in [&self.rounds, &self.txs, &self.bytes, &self.gas] {
            c.store(0, SeqCst);
        }
    }

    /// Calls of method `m` so far.
    pub fn count(&self, m: &str) -> u64 {
        self.calls
            .lock()
            .expect("lock")
            .get(m)
            .copied()
            .unwrap_or(0)
    }

    pub fn metrics(&self) -> Metrics {
        let calls = self.calls.lock().expect("lock");
        Metrics {
            rounds: self.rounds.load(SeqCst),
            calls: calls.values().sum(),
            queries: calls.get("query").copied().unwrap_or(0),
            txs: self.txs.load(SeqCst),
            bytes: self.bytes.load(SeqCst),
            gas: self.gas.load(SeqCst),
        }
    }

    async fn rpc<T>(&self, m: &'static str, f: impl Future<Output = T>) -> T {
        *self.calls.lock().expect("lock").entry(m).or_default() += 1;
        if self.inflight.fetch_add(1, SeqCst) == 0 {
            self.rounds.fetch_add(1, SeqCst);
        }
        tokio::task::yield_now().await;
        let out = f.await;
        self.inflight.fetch_sub(1, SeqCst);
        out
    }

    fn note_send(&self, tx: &SignedTx) {
        let Ok(env) = TxEnvelope::decode_2718(&mut tx.raw().as_ref()) else {
            return;
        };
        let input = env.input();
        let ceiling = 21_000 + 40 * input.len() as u64;
        let delta = i128::from(ceiling) - i128::from(intrinsic_gas(input));
        self.sent.lock().expect("lock").insert(tx.hash(), delta);
        self.txs.fetch_add(1, SeqCst);
        self.bytes.fetch_add(input.len() as u64, SeqCst);
    }

    fn note_receipt(&self, r: &Receipt) {
        if let Some(delta) = self.sent.lock().expect("lock").remove(&r.tx_hash) {
            let gas = u64::try_from(i128::from(r.gas_used) + delta).unwrap_or(0);
            self.gas.fetch_add(gas, SeqCst);
        }
    }
}

impl<A: Arkiv> Arkiv for Meter<'_, A> {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        // The real client verifies it once per process, inside its first batch.
        self.inner.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.rpc("head", self.inner.head()).await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.rpc("base_fee", self.inner.base_fee()).await
    }
    async fn head_fee(&self) -> Result<(u64, u128), StoreError> {
        // One block header on the real client.
        self.rpc("head_fee", self.inner.head_fee()).await
    }
    fn heads_ws(&self) -> Option<crate::heads::WsConfig> {
        self.inner.heads_ws()
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> Result<QueryPage, StoreError> {
        self.rpc("query", self.inner.query(q, o)).await
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        self.rpc("entity_nonce", self.inner.entity_nonce(c)).await
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.rpc("balance", self.inner.balance(a)).await
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.rpc("tx_count", self.inner.tx_count(a)).await
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.rpc("tx_count_mined", self.inner.tx_count_mined(a))
            .await
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.rpc("estimate", self.inner.estimate(from, ops)).await
    }
    fn quota(&self) -> Option<Quota> {
        self.inner.quota()
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        self.note_send(&tx);
        self.rpc("send", self.inner.send(tx)).await
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        let r = self.rpc("receipt", self.inner.receipt(h)).await?;
        if let Some(r) = &r {
            self.note_receipt(r);
        }
        Ok(r)
    }
    fn seal_ops(&self, ops: Vec<Operation>) -> Result<Vec<Operation>, StoreError> {
        self.inner.seal_ops(ops)
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.rpc("transaction", self.inner.transaction(h)).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<MinedTx>, StoreError> {
        self.rpc("mined_tx", self.inner.mined_tx(h)).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<MinedTx>, StoreError> {
        self.rpc("block_txs", self.inner.block_txs(n)).await
    }
    async fn tx_at(&self, n: u64, index: u32) -> Result<Option<MinedTx>, StoreError> {
        self.rpc("tx_at", self.inner.tx_at(n, index)).await
    }
    fn seals(&self) -> bool {
        self.inner.seals()
    }
    fn incomplete_reason(&self) -> Option<&'static str> {
        self.inner.incomplete_reason()
    }
    fn open_rows(&self, rows: Vec<EntityData>) -> Vec<EntityData> {
        self.inner.open_rows(rows)
    }
}
