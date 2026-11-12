//! `MemArkiv`: an in-memory Arkiv node for tests (design 7.2, "MemArkiv must model").
//!
//! Time moves only when the test calls [`MemArkiv::mine`]. Sent txs wait in a pool; a
//! block takes at most 16 consecutive-nonce txs per sender and 30M gas, executes them
//! atomically per tx with the node's revert rules, then purges expired entities.
//! Queries go through the node's own parser and filter. Fault hooks let tests drop
//! txs, force reverts, hide entities, lie about `entityNonce` and cap page sizes.

mod eval;
mod exec;
mod meter;
mod persist;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Mutex, MutexGuard};

use alloy_consensus::transaction::SignerRecoverable;
use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, B256, Bytes, TxHash, U256};
use alloy_sol_types::SolCall;
use arkiv_bindings::{IEntityRegistry, Operation};

pub use crate::derive_key;
pub use exec::MAX_LIFETIME;
use exec::{Env, World, intrinsic_gas};
pub use meter::{Meter, Metrics};
pub use persist::FileMemArkiv;

use crate::{
    ARKIV_ADDRESS, Arkiv, BLOCK_GAS_LIMIT, MAX_RAW_TX_BYTES, MinedTx, Query, QueryOpts, QueryPage,
    Receipt, SignedTx, StoreError, TX_GAS_CAP, TxInfo, execute_calldata,
};

/// Txs one sender may have pending; the 17th is refused (stricter than Rabdi, which queues more).
pub const POOL_SLOTS_PER_SENDER: usize = 16;
/// Txs from one sender in one block (MEASURED ceiling).
pub const MAX_TXS_PER_SENDER_PER_BLOCK: usize = 16;
/// Base fee floor, 1 gwei.
pub const MIN_BASE_FEE: u128 = 1_000_000_000;

#[derive(Debug, Clone)]
struct Pending {
    hash: TxHash,
    from: Address,
    nonce: u64,
    to: Address,
    value: U256,
    input: Bytes,
    gas_limit: u64,
    max_fee: u128,
    tip: u128,
}

impl Pending {
    fn info(&self, block_number: Option<u64>) -> TxInfo {
        TxInfo {
            hash: self.hash,
            from: self.from,
            to: Some(self.to),
            value: self.value,
            block_number,
        }
    }
}

#[derive(Debug, Default)]
struct Faults {
    drop: BTreeSet<u64>,
    revert_at: Option<usize>,
    withheld: HashSet<B256>,
    nonce_lies: HashMap<Address, u64>,
    max_page_bytes: Option<usize>,
    count_lies: u64,
}

#[derive(Debug)]
struct Inner {
    chain_id: u64,
    head: u64,
    base_fee: u128,
    world: World,
    pool: Vec<Pending>,
    receipts: HashMap<TxHash, Receipt>,
    txs: HashMap<TxHash, TxInfo>,
    /// Calldata of every mined tx, kept forever, and each block's txs in order.
    mined: HashMap<TxHash, MinedTx>,
    blocks: HashMap<u64, Vec<TxHash>>,
    block_gas: HashMap<(u64, Address), u64>,
    sends: u64,
    faults: Faults,
}

/// See the module docs.
#[derive(Debug)]
pub struct MemArkiv(Mutex<Inner>);

impl MemArkiv {
    pub fn new(chain_id: u64) -> Self {
        Self(Mutex::new(Inner {
            chain_id,
            head: 1,
            base_fee: MIN_BASE_FEE,
            world: World::default(),
            pool: Vec::new(),
            receipts: HashMap::new(),
            txs: HashMap::new(),
            mined: HashMap::new(),
            blocks: HashMap::new(),
            block_gas: HashMap::new(),
            sends: 0,
            faults: Faults::default(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panicking test thread must not poison every later assertion.
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn fund(&self, a: Address, wei: U256) {
        *self.lock().world.balances.entry(a).or_default() += wei;
    }

    /// Overwrite `a`'s balance (tests that need an exact amount).
    pub fn set_balance(&self, a: Address, wei: U256) {
        self.lock().world.balances.insert(a, wei);
    }

    pub fn set_base_fee(&self, wei: u128) {
        self.lock().base_fee = wei;
    }

    /// Produce one block; returns its number.
    pub fn mine(&self) -> u64 {
        self.lock().mine()
    }

    /// Gas `sender`'s txs used in block `n`.
    pub fn block_gas_of(&self, n: u64, sender: Address) -> u64 {
        self.lock()
            .block_gas
            .get(&(n, sender))
            .copied()
            .unwrap_or(0)
    }

    /// Txs waiting in the pool.
    pub fn pending(&self) -> usize {
        self.lock().pool.len()
    }

    /// Drop the `n` sends that follow the next `skip` sends: accepted, never mined.
    pub fn drop_sends(&self, skip: u64, n: u64) {
        let mut g = self.lock();
        let from = g.sends + skip;
        g.faults.drop.extend(from..from + n);
    }

    pub fn drop_next(&self, n: u64) {
        self.drop_sends(0, n);
    }

    /// The next mined `execute` tx reverts at op `k` (the whole tx is rolled back).
    pub fn revert_next_at(&self, k: usize) {
        self.lock().faults.revert_at = Some(k);
    }

    /// Hide an entity from every query and count.
    pub fn withhold(&self, key: B256) {
        self.lock().faults.withheld.insert(key);
    }

    /// Report `n` from `entityNonce(creator)`.
    pub fn lie_entity_nonce(&self, creator: Address, n: u64) {
        self.lock().faults.nonce_lies.insert(creator, n);
    }

    /// The next `n` queries selecting `count` report a different, growing count.
    pub fn lie_counts(&self, n: u64) {
        self.lock().faults.count_lies = n;
    }

    /// Answer HTTP 413 to any query whose JSON response would exceed `bytes`.
    pub fn fail_pages_above(&self, bytes: usize) {
        self.lock().faults.max_page_bytes = Some(bytes);
    }
}

impl Inner {
    fn confirmed_nonce(&self, a: Address) -> u64 {
        self.world.nonces.get(&a).copied().unwrap_or(0)
    }

    fn balance(&self, a: Address) -> U256 {
        self.world.balances.get(&a).copied().unwrap_or_default()
    }

    /// Next nonce counting the consecutive run of pending txs.
    fn pending_nonce(&self, a: Address) -> u64 {
        let mut n = self.confirmed_nonce(a);
        while self.pool.iter().any(|p| p.from == a && p.nonce == n) {
            n += 1;
        }
        n
    }

    fn admit(&mut self, p: Pending, raw_len: usize) -> Result<TxHash, StoreError> {
        if raw_len > MAX_RAW_TX_BYTES {
            return Err(StoreError::TxTooLarge {
                size: raw_len,
                cap: MAX_RAW_TX_BYTES,
            });
        }
        if p.gas_limit > TX_GAS_CAP {
            return Err(StoreError::GasCap {
                gas: p.gas_limit,
                cap: TX_GAS_CAP,
            });
        }
        if self.receipts.contains_key(&p.hash) || self.pool.iter().any(|q| q.hash == p.hash) {
            return Ok(p.hash); // "already known"
        }
        let confirmed = self.confirmed_nonce(p.from);
        if p.nonce < confirmed {
            return Err(StoreError::NonceTooLow(format!(
                "nonce {} below {confirmed}",
                p.nonce
            )));
        }
        if self
            .pool
            .iter()
            .any(|q| q.from == p.from && q.nonce == p.nonce)
        {
            return Err(StoreError::Rpc(
                "replacement transaction underpriced".into(),
            ));
        }
        if self.pool.iter().filter(|q| q.from == p.from).count() >= POOL_SLOTS_PER_SENDER {
            return Err(StoreError::Rpc("txpool is full".into()));
        }
        let need = U256::from(p.gas_limit) * U256::from(p.max_fee) + p.value;
        let have = self.balance(p.from);
        if have < need {
            return Err(StoreError::InsufficientFunds {
                address: p.from,
                need,
                have,
            });
        }
        let ordinal = self.sends;
        self.sends += 1;
        let hash = p.hash;
        if !self.faults.drop.remove(&ordinal) {
            self.pool.push(p);
        }
        Ok(hash)
    }

    /// The txs the next block takes, in pool order per sender, nonces consecutive.
    fn select(&self) -> Vec<Pending> {
        let mut senders: Vec<Address> = Vec::new();
        for p in &self.pool {
            if !senders.contains(&p.from) {
                senders.push(p.from);
            }
        }
        let mut picked = Vec::new();
        let mut gas = 0u64;
        for s in senders {
            let mut spent = U256::ZERO;
            for n in (self.confirmed_nonce(s)..).take(MAX_TXS_PER_SENDER_PER_BLOCK) {
                let Some(p) = self.pool.iter().find(|p| p.from == s && p.nonce == n) else {
                    break;
                };
                spent += U256::from(p.gas_limit) * U256::from(p.max_fee) + p.value;
                let fits = gas + p.gas_limit <= BLOCK_GAS_LIMIT && p.max_fee >= self.base_fee;
                if !fits || spent > self.balance(s) {
                    break;
                }
                gas += p.gas_limit;
                picked.push(p.clone());
            }
        }
        picked
    }

    fn mine(&mut self) -> u64 {
        let n = self.head + 1;
        self.world.entities.retain(|_, s| s.entity.expires_at > n);
        let picked = self.select();
        self.pool
            .retain(|p| !picked.iter().any(|q| q.hash == p.hash));
        let mut used = 0u64;
        for (i, p) in picked.iter().enumerate() {
            let tx_index = u32::try_from(i).unwrap_or(u32::MAX);
            used += self.include(p, n, tx_index);
        }
        let target = u128::from(BLOCK_GAS_LIMIT / 2);
        let (used, base) = (u128::from(used), self.base_fee);
        self.base_fee = if used >= target {
            base + base * (used - target) / target / 8
        } else {
            (base - base * (target - used) / target / 8).max(MIN_BASE_FEE)
        };
        self.head = n;
        n
    }

    /// Execute one tx in block `n`, charge it, record its receipt; returns gas used.
    fn include(&mut self, p: &Pending, n: u64, tx_index: u32) -> u64 {
        let base = intrinsic_gas(&p.input);
        let outcome = if p.to == ARKIV_ADDRESS {
            let env = Env {
                chain_id: self.chain_id,
                caller: p.from,
                block: n,
                tx_index,
            };
            match IEntityRegistry::executeCall::abi_decode(&p.input) {
                Ok(call) => match self.faults.revert_at.take() {
                    Some(k) => Err(format!("injected revert at op {k}")),
                    None => self.world.execute(&env, &call.ops),
                },
                Err(e) => Err(format!("bad calldata: {e}")),
            }
        } else {
            *self.world.balances.entry(p.from).or_default() -= p.value;
            *self.world.balances.entry(p.to).or_default() += p.value;
            Ok(0)
        };
        let (success, gas_used) = match outcome {
            Ok(g) if base + g <= p.gas_limit => (true, base + g),
            _ => (false, p.gas_limit.min(base.max(21_000))),
        };
        let price = self.base_fee + p.tip.min(p.max_fee - self.base_fee);
        *self.world.balances.entry(p.from).or_default() -= U256::from(gas_used) * U256::from(price);
        *self.world.nonces.entry(p.from).or_default() += 1;
        *self.block_gas.entry((n, p.from)).or_default() += gas_used;
        let r = Receipt {
            tx_hash: p.hash,
            block_number: n,
            success,
            gas_used,
            effective_gas_price: price,
            index: tx_index,
        };
        self.receipts.insert(p.hash, r);
        self.txs.insert(p.hash, p.info(Some(n)));
        let mined = MinedTx {
            hash: p.hash,
            from: p.from,
            to: Some(p.to),
            block: n,
            index: tx_index,
            nonce: p.nonce,
            input: p.input.clone(),
        };
        self.mined.insert(p.hash, mined);
        self.blocks.entry(n).or_default().push(p.hash);
        gas_used
    }

    fn query(&mut self, q: &Query, opts: &QueryOpts) -> Result<QueryPage, StoreError> {
        let text = q.render()?;
        let json = opts.to_json()?;
        let mut live: Vec<&exec::Stored> = self
            .world
            .entities
            .values()
            .filter(|s| !self.faults.withheld.contains(&B256::from(s.entity.key)))
            .collect();
        live.sort_by_key(|s| (s.tx_pos, s.entity.key));
        let mut resp = eval::run(&live, &text, &json, self.head)?;
        if let Some(c) = resp.count.as_mut().filter(|_| self.faults.count_lies > 0) {
            *c += self.faults.count_lies;
            self.faults.count_lies -= 1;
        }
        if let Some(cap) = self.faults.max_page_bytes {
            let size = serde_json::to_vec(&resp)
                .map(|v| v.len())
                .unwrap_or(usize::MAX);
            if size > cap {
                return Err(StoreError::Http {
                    status: 413,
                    body: format!("response of {size} B"),
                });
            }
        }
        Ok(QueryPage {
            block_number: resp.block_number,
            count: resp.count,
            rows: resp.data,
        })
    }

    fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        let env = Env {
            chain_id: self.chain_id,
            caller: from,
            block: self.head + 1,
            tx_index: 0,
        };
        let mut dry = self.world.clone();
        let g = dry
            .execute(&env, ops)
            .map_err(|reason| StoreError::Reverted { tx: None, reason })?;
        let gas = intrinsic_gas(&execute_calldata(ops)) + g;
        if gas > TX_GAS_CAP {
            return Err(StoreError::GasCap {
                gas,
                cap: TX_GAS_CAP,
            });
        }
        Ok(gas)
    }
}

fn decode(tx: &SignedTx, chain_id: u64) -> Result<Pending, StoreError> {
    let env = TxEnvelope::decode_2718(&mut tx.raw().as_ref())
        .map_err(|e| StoreError::Rpc(format!("rlp: {e}")))?;
    if env.chain_id() != Some(chain_id) {
        return Err(StoreError::Rpc(format!(
            "invalid chain id {:?}",
            env.chain_id()
        )));
    }
    let from = env
        .recover_signer()
        .map_err(|e| StoreError::Rpc(format!("invalid signature: {e}")))?;
    Ok(Pending {
        hash: *env.tx_hash(),
        from,
        nonce: env.nonce(),
        to: env.to().unwrap_or_default(),
        value: env.value(),
        input: env.input().clone(),
        gas_limit: env.gas_limit(),
        max_fee: env.max_fee_per_gas(),
        tip: env.max_priority_fee_per_gas().unwrap_or(0),
    })
}

impl Arkiv for MemArkiv {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        Ok(self.lock().chain_id)
    }
    async fn head(&self) -> Result<u64, StoreError> {
        Ok(self.lock().head)
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        Ok(self.lock().base_fee)
    }
    async fn query(&self, q: &Query, opts: &QueryOpts) -> Result<QueryPage, StoreError> {
        self.lock().query(q, opts)
    }
    async fn entity_nonce(&self, creator: Address) -> Result<u64, StoreError> {
        let g = self.lock();
        let truth = g.world.entity_nonce.get(&creator).copied().unwrap_or(0);
        Ok(g.faults.nonce_lies.get(&creator).copied().unwrap_or(truth))
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        Ok(self.lock().balance(a))
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        Ok(self.lock().pending_nonce(a))
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        Ok(self.lock().confirmed_nonce(a))
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.lock().estimate(from, ops)
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        let mut g = self.lock();
        let p = decode(&tx, g.chain_id)?;
        g.admit(p, tx.raw().len())
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        Ok(self.lock().receipts.get(&h).copied())
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        let g = self.lock();
        let pending = || g.pool.iter().find(|p| p.hash == h).map(|p| p.info(None));
        Ok(g.txs.get(&h).copied().or_else(pending))
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<MinedTx>, StoreError> {
        Ok(self.lock().mined.get(&h).cloned())
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<MinedTx>, StoreError> {
        let g = self.lock();
        let hashes = g.blocks.get(&n).map(Vec::as_slice).unwrap_or_default();
        Ok(hashes
            .iter()
            .filter_map(|h| g.mined.get(h).cloned())
            .collect())
    }
}
