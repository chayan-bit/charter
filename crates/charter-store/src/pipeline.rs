//! `TxPipeline`: nonces, a window of in-flight txs, gas pacing, receipts, resends and
//! revert decoding for one sender (design 4.6), rolling top-ups from a funder (design 4.2
//! step 6), plus greedy batching under caps.

use std::collections::{HashSet, VecDeque};
use std::time::{Duration, Instant};

use alloy_primitives::{Address, Bytes, U256};
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::Operation;

use crate::heads::{self, Heads};
use crate::shape::{Estimates, Shape};
use crate::tx::EXECUTE_BASE_BYTES;
use crate::{
    ARKIV_ADDRESS, Arkiv, BLOCK_GAS_LIMIT, MAX_RAW_TX_BYTES, Receipt, SignedTx, StoreError,
    TxParams, execute_calldata, op_calldata_bytes,
};

/// Room left in a raw tx for the envelope around the calldata (nonce, fees, signature).
pub const ENVELOPE_BYTES: usize = 200;
const TRANSFER_GAS: u64 = 21_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineConfig {
    /// Most unconfirmed txs at once (reth's per-sender pool slots).
    pub window: usize,
    /// Share of block gas this sender may have unconfirmed, in percent.
    pub pace_percent: u64,
    pub block_gas_limit: u64,
    /// Priority fee per gas, wei.
    pub tip: u128,
    /// Upper bound on a tx's gas limit.
    pub gas_cap: u64,
    pub max_raw_bytes: usize,
    /// Above this base fee the pacing budget halves.
    pub high_base_fee: u128,
    pub poll: Duration,
    /// Blocks without a receipt before a tx is resent.
    pub resend_after_blocks: u64,
    pub max_resends: u32,
    /// Price a create job from an earlier estimate of the same shape instead of its own
    /// `eth_estimateGas` (a failed tx is re-estimated and sent once more).
    pub reuse_estimates: bool,
    /// Wait for inclusion on the backend's `newHeads` socket when it has one, polling
    /// on the `poll` timer only as the fallback.
    pub use_heads: bool,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            window: 16,
            pace_percent: 40,
            block_gas_limit: BLOCK_GAS_LIMIT,
            tip: 0,
            gas_cap: 16_000_000,
            max_raw_bytes: MAX_RAW_TX_BYTES,
            high_base_fee: 2_000_000_000,
            poll: Duration::from_millis(250),
            resend_after_blocks: 30,
            max_resends: 3,
            reuse_estimates: true,
            use_heads: true,
        }
    }
}

impl PipelineConfig {
    /// For many large independent txs (a big upload): up to two blocks of gas may be
    /// unconfirmed, so this sender keeps every block it can get full instead of 40 percent.
    pub fn bulk(&self) -> Self {
        Self {
            pace_percent: self.pace_percent.max(200),
            ..self.clone()
        }
    }

    /// `maxFeePerGas` at `base_fee`: twice the base fee plus the tip.
    pub fn max_fee(&self, base_fee: u128) -> u128 {
        2 * base_fee + self.tip
    }

    /// Gas limit of a tx estimated at `gas`: 10 percent headroom, within the cap.
    pub fn gas_limit(&self, gas: u64) -> u64 {
        (gas + gas / 10).min(self.gas_cap).max(gas)
    }

    /// Gas this sender may have unconfirmed at `base_fee`.
    pub fn pace_budget(&self, base_fee: u128) -> u64 {
        let budget = self.block_gas_limit * self.pace_percent / 100;
        match base_fee > self.high_base_fee {
            true => budget / 2,
            false => budget,
        }
    }

    /// Max-fee reserve of one full window of txs estimated at `gas` each:
    /// `window x gas_limit x max_fee`.
    pub fn window_reserve(&self, gas: u64, base_fee: u128) -> U256 {
        self.reserve(self.window as u64, gas, base_fee)
    }

    /// What a sender must hold to send one tx estimated at `gas` moving `value`: its max
    /// fee at `base_fee` plus the value.
    pub fn tx_reserve(&self, gas: u64, value: U256, base_fee: u128) -> U256 {
        U256::from(self.gas_limit(gas)) * U256::from(self.max_fee(base_fee)) + value
    }

    /// The top-up a funded sender holding `have` makes before a tx needing `reserve`: one
    /// transfer covering `slots` such txs; `None` when `have` covers this one. The send
    /// path and `chart ui`'s confirm view both decide with this.
    pub fn top_up(have: U256, reserve: U256, slots: u64) -> Option<U256> {
        (have < reserve).then(|| reserve * U256::from(slots.max(1)) - have)
    }

    /// The most a top-up transfer itself costs in fees at `base_fee`.
    pub fn top_up_fee(&self, base_fee: u128) -> U256 {
        U256::from(TRANSFER_GAS) * U256::from(self.max_fee(base_fee))
    }

    /// Max-fee reserve of one window holding `txs` txs (at most `window`) of `gas` each.
    pub fn reserve(&self, txs: u64, gas: u64, base_fee: u128) -> U256 {
        U256::from(txs.min(self.window as u64))
            * U256::from(self.gas_limit(gas))
            * U256::from(self.max_fee(base_fee))
    }
}

/// One job's included tx. A job that had to be split lands as several.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Landed {
    pub job: usize,
    pub receipt: Receipt,
}

/// Bytes of calldata a run landed and how long it took, for throughput reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Throughput {
    pub txs: usize,
    pub bytes: usize,
    pub elapsed: Duration,
}

impl Throughput {
    /// Landed calldata in MB (10^6 bytes) per second.
    pub fn mb_per_s(&self) -> f64 {
        self.bytes as f64 / self.elapsed.as_secs_f64().max(1e-9) / 1e6
    }
}

#[derive(Debug, Clone)]
enum Job {
    Execute(Vec<Operation>),
    Transfer(Address, U256),
}

#[derive(Debug)]
struct InFlight {
    job: usize,
    body: Job,
    tx: SignedTx,
    gas: u64,
    sent_at: u64,
    resends: u32,
    /// Its gas came from an earlier estimate of the same shape, not its own.
    shape: Option<Shape>,
}

/// A job ready to sign: calldata, recipient, value and gas.
struct Priced {
    input: Bytes,
    to: Address,
    value: U256,
    gas: u64,
    /// Set when the gas was reused from an estimate of this shape.
    reused: Option<Shape>,
}

/// Longest wait for the next `newHeads` block before polling anyway.
const HEAD_WAIT: Duration = Duration::from_millis(2_500);
/// Attempts at sending one signed tx; resending the same bytes can never double-spend.
const SEND_ATTEMPTS: u32 = 3;
/// A head and base fee seen this recently are reused instead of read again.
const FEE_FRESH: Duration = Duration::from_secs(2);

/// Sends jobs (each one `execute(ops)` tx) from one key. See the module docs.
pub struct TxPipeline<'a, A: Arkiv> {
    arkiv: &'a A,
    signer: PrivateKeySigner,
    chain_id: u64,
    cfg: PipelineConfig,
    nonce: Option<u64>,
    /// Tops the sender up from this key before each window that its balance cannot cover.
    funder: Option<PrivateKeySigner>,
    estimates: Estimates,
    /// Jobs whose failed tx did not reproduce on re-estimate; they are estimated afresh
    /// and fail for good the second time.
    retried: HashSet<usize>,
    /// The latest head, its base fee and when they were read.
    seen: Option<(u64, u128, Instant)>,
    heads: Option<Heads>,
    /// The socket failed once: poll on the timer for the rest of the run.
    heads_off: bool,
    /// The nonce was re-read because someone else took it; a second time is an error.
    reread: bool,
}

impl<'a, A: Arkiv> TxPipeline<'a, A> {
    pub fn new(arkiv: &'a A, signer: PrivateKeySigner, chain_id: u64, cfg: PipelineConfig) -> Self {
        Self {
            arkiv,
            signer,
            chain_id,
            cfg,
            nonce: None,
            funder: None,
            estimates: Estimates::default(),
            retried: HashSet::new(),
            seen: None,
            heads: None,
            heads_off: false,
            reread: false,
        }
    }

    /// Before each window of sends, top the sender up from `funder` so its balance covers
    /// the max cost of every tx in flight (design 4.2 step 6).
    pub fn funded_by(mut self, funder: PrivateKeySigner) -> Self {
        self.funder = Some(funder);
        self
    }

    pub fn address(&self) -> Address {
        self.signer.address()
    }

    /// Send every job and wait for all receipts. A job too big for one tx is split in half
    /// until it fits; a revert stops the run with the decoded reason.
    pub async fn run(&mut self, jobs: Vec<Vec<Operation>>) -> Result<Vec<Landed>, StoreError> {
        let jobs = jobs
            .into_iter()
            .map(|ops| self.arkiv.seal_ops(ops).map(Job::Execute))
            .collect::<Result<Vec<_>, _>>()?;
        self.drive(jobs.into_iter().enumerate().collect()).await
    }

    /// [`TxPipeline::run`] for many independent jobs (a big upload), also reporting the
    /// calldata landed per second. Pair it with [`PipelineConfig::bulk`].
    pub async fn run_measured(
        &mut self,
        jobs: Vec<Vec<Operation>>,
    ) -> Result<(Vec<Landed>, Throughput), StoreError> {
        let bytes = jobs.iter().map(|ops| execute_calldata(ops).len()).sum();
        let started = Instant::now();
        let landed = self.run(jobs).await?;
        let t = Throughput {
            txs: landed.len(),
            bytes,
            elapsed: started.elapsed(),
        };
        Ok((landed, t))
    }

    /// A plain value transfer (wallet top-up).
    pub async fn transfer(&mut self, to: Address, value: U256) -> Result<Receipt, StoreError> {
        let landed = self
            .drive(VecDeque::from([(0, Job::Transfer(to, value))]))
            .await?;
        landed
            .first()
            .map(|l| l.receipt)
            .ok_or_else(|| StoreError::Rpc("transfer produced no receipt".into()))
    }

    async fn drive(
        &mut self,
        mut queue: VecDeque<(usize, Job)>,
    ) -> Result<Vec<Landed>, StoreError> {
        let (mut inflight, mut done) = (Vec::new(), Vec::new());
        loop {
            self.fill(&mut queue, &mut inflight).await?;
            if inflight.is_empty() && queue.is_empty() {
                done.sort_by_key(|l: &Landed| (l.job, l.receipt.block_number));
                return Ok(done);
            }
            self.wait().await;
            self.poll(&mut queue, &mut inflight, &mut done).await?;
        }
    }

    /// Until the next block is announced on the `newHeads` socket (or [`HEAD_WAIT`] passes),
    /// else one poll interval. The socket is opened on the first wait, which returns at
    /// once so a block announced during the handshake is not missed.
    async fn wait(&mut self) {
        if self.cfg.use_heads && self.heads.is_none() && !self.heads_off {
            match self.arkiv.heads_ws() {
                Some(ws) => match tokio::time::timeout(HEAD_WAIT, heads::subscribe(&ws)).await {
                    Ok(Ok(h)) => {
                        self.heads = Some(h);
                        return;
                    }
                    _ => self.heads_off = true,
                },
                None => self.heads_off = true,
            }
        }
        if let Some(h) = self.heads.as_mut() {
            match tokio::time::timeout(HEAD_WAIT, h.next()).await {
                Ok(Ok(Some(_))) | Err(_) => return,
                Ok(_) => {
                    self.heads = None;
                    self.heads_off = true;
                }
            }
        }
        tokio::time::sleep(self.cfg.poll).await;
    }

    /// The head and its base fee: the last seen if fresh, else one read.
    async fn head_fee(&mut self) -> Result<(u64, u128), StoreError> {
        if let Some((head, fee, at)) = self.seen
            && at.elapsed() < FEE_FRESH
        {
            return Ok((head, fee));
        }
        let (head, fee) = self.arkiv.head_fee().await?;
        self.seen = Some((head, fee, Instant::now()));
        Ok((head, fee))
    }

    /// Send queued jobs while the window and the gas budget allow: price and sign them in
    /// order, then send them together (nonces are consecutive, so the node orders them).
    async fn fill(
        &mut self,
        queue: &mut VecDeque<(usize, Job)>,
        inflight: &mut Vec<InFlight>,
    ) -> Result<(), StoreError> {
        while self.fill_once(queue, inflight).await? {}
        Ok(())
    }

    /// One round of [`TxPipeline::fill`]; `true` when jobs went back to the queue after a
    /// nonce re-read and should be sent again now.
    async fn fill_once(
        &mut self,
        queue: &mut VecDeque<(usize, Job)>,
        inflight: &mut Vec<InFlight>,
    ) -> Result<bool, StoreError> {
        // The base fee can rise while we fill blocks: at most one block old.
        let (sent_at, base_fee) = self.head_fee().await?;
        let budget = self.cfg.pace_budget(base_fee);
        let mut unconfirmed: u64 = inflight.iter().map(|f| f.gas).sum();
        let mut covered = match self.funder {
            Some(_) => Some(self.covered(inflight).await?),
            None => None,
        };
        let mut ready: Vec<(usize, Job, Priced)> = vec![];
        while inflight.len() + ready.len() < self.cfg.window {
            let Some((job, body)) = queue.pop_front() else {
                break;
            };
            let p = match self.price(job, &body).await {
                Ok(p) => p,
                Err(e) => match split(&body, &e) {
                    Some((a, b)) => {
                        queue.push_front((job, b));
                        queue.push_front((job, a));
                        continue;
                    }
                    None => return Err(e),
                },
            };
            // One tx may exceed the budget alone, else nothing that large could ever go.
            let empty = inflight.is_empty() && ready.is_empty();
            if !empty && unconfirmed + p.gas > budget {
                queue.push_front((job, body));
                break;
            }
            let reserve = self.cfg.tx_reserve(p.gas, p.value, base_fee);
            // One transfer covers the rest of the window (free slots, capped at the jobs
            // left) at this tx's size.
            let free = self.cfg.window - inflight.len() - ready.len();
            let slots = (free as u64).min(1 + queue.len() as u64);
            let top = covered.and_then(|h| PipelineConfig::top_up(h, reserve, slots));
            if let (Some(have), Some(amount)) = (covered.as_mut(), top) {
                self.top_up(amount, base_fee).await?;
                *have += amount;
            }
            unconfirmed += p.gas;
            if let Some(have) = covered.as_mut() {
                *have = have.saturating_sub(reserve);
            }
            ready.push((job, body, p));
        }
        self.send_all(ready, base_fee, sent_at, queue, inflight)
            .await
    }

    /// Calldata, recipient, value and gas, checking the byte and gas caps. A create job
    /// of a shape already estimated reuses that estimate unless it failed before.
    async fn price(&mut self, job: usize, body: &Job) -> Result<Priced, StoreError> {
        let ops = match body {
            Job::Transfer(to, value) => {
                return Ok(Priced {
                    input: Bytes::new(),
                    to: *to,
                    value: *value,
                    gas: TRANSFER_GAS,
                    reused: None,
                });
            }
            Job::Execute(ops) => ops,
        };
        let input = execute_calldata(ops);
        let size = input.len() + ENVELOPE_BYTES;
        if size > self.cfg.max_raw_bytes {
            return Err(StoreError::TxTooLarge {
                size,
                cap: self.cfg.max_raw_bytes,
            });
        }
        let shape = self
            .cfg
            .reuse_estimates
            .then(|| Shape::of(ops, &input))
            .flatten()
            .filter(|_| !self.retried.contains(&job));
        let reuse = shape.as_ref().and_then(|s| self.estimates.get(s, &input));
        let gas = match reuse {
            Some(g) => g,
            None => self.arkiv.estimate(self.address(), ops).await?,
        };
        if gas > self.cfg.gas_cap {
            return Err(StoreError::GasCap {
                gas,
                cap: self.cfg.gas_cap,
            });
        }
        let reused = match (reuse, shape) {
            (Some(_), s) => s,
            (None, Some(s)) => {
                self.estimates.put(s, &input, gas);
                None
            }
            (None, None) => None,
        };
        Ok(Priced {
            input,
            to: ARKIV_ADDRESS,
            value: U256::ZERO,
            gas,
            reused,
        })
    }

    /// Signs `ready` with consecutive nonces and sends them concurrently. A nonce someone
    /// else took is re-read once and the affected jobs go back to the queue.
    async fn send_all(
        &mut self,
        ready: Vec<(usize, Job, Priced)>,
        base_fee: u128,
        sent_at: u64,
        queue: &mut VecDeque<(usize, Job)>,
        inflight: &mut Vec<InFlight>,
    ) -> Result<bool, StoreError> {
        if ready.is_empty() {
            return Ok(false);
        }
        let mut nonce = match self.nonce {
            Some(n) => n,
            None => self.arkiv.tx_count(self.address()).await?,
        };
        let mut signed = Vec::with_capacity(ready.len());
        let mut ready = ready.into_iter();
        while let Some((job, body, p)) = ready.next() {
            let params = TxParams {
                nonce,
                to: p.to,
                value: p.value,
                input: p.input,
                gas_limit: self.cfg.gas_limit(p.gas),
                max_fee_per_gas: self.cfg.max_fee(base_fee),
                max_priority_fee_per_gas: self.cfg.tip,
            };
            let tx = SignedTx::sign(&self.signer, self.chain_id, params)?;
            if tx.raw().len() > self.cfg.max_raw_bytes {
                // Only the envelope estimate was off: shrink this job and resend it later.
                let e = StoreError::TxTooLarge {
                    size: tx.raw().len(),
                    cap: self.cfg.max_raw_bytes,
                };
                let (a, b) = split(&body, &e).ok_or(e)?;
                let rest: Vec<_> = ready.map(|(j, b, _)| (j, b)).collect();
                for item in rest.into_iter().rev() {
                    queue.push_front(item);
                }
                queue.push_front((job, b));
                queue.push_front((job, a));
                break;
            }
            nonce += 1;
            signed.push((job, body, p.gas, p.reused, tx));
        }
        self.nonce = Some(nonce);
        let results = futures_util::future::join_all(
            signed.iter().map(|(.., tx)| send_retrying(self.arkiv, tx)),
        )
        .await;
        let mut stolen = vec![];
        for ((job, body, gas, shape, tx), r) in signed.into_iter().zip(results) {
            match r {
                Ok(()) => inflight.push(InFlight {
                    job,
                    body,
                    tx,
                    gas,
                    sent_at,
                    resends: 0,
                    shape,
                }),
                Err(StoreError::NonceTooLow(m)) => stolen.push((job, body, m)),
                Err(e) => return Err(e),
            }
        }
        if stolen.is_empty() {
            self.reread = false;
            return Ok(false);
        }
        // Someone else used our nonces: re-read once; a second time is an error.
        if std::mem::replace(&mut self.reread, true) {
            return Err(StoreError::InconsistentView(
                "nonce still too low after re-reading it".into(),
            ));
        }
        self.nonce = None;
        for (job, body, _) in stolen.into_iter().rev() {
            queue.push_front((job, body));
        }
        Ok(true)
    }

    /// The sender's balance not already reserved by txs in flight.
    async fn covered(&self, inflight: &[InFlight]) -> Result<U256, StoreError> {
        let have = self.arkiv.balance(self.address()).await?;
        let reserved: U256 = inflight.iter().map(|f| f.tx.max_cost()).sum();
        Ok(have.saturating_sub(reserved))
    }

    /// Moves `amount` from the funder to the sender and waits for it to land; fails with
    /// `E_INSUFFICIENT_FUNDS` naming the funder when it cannot cover the transfer.
    async fn top_up(&self, amount: U256, base_fee: u128) -> Result<(), StoreError> {
        let Some(funder) = &self.funder else {
            return Ok(());
        };
        let max_fee = self.cfg.max_fee(base_fee);
        let need = amount + self.cfg.top_up_fee(base_fee);
        let have = self.arkiv.balance(funder.address()).await?;
        if have < need {
            return Err(StoreError::InsufficientFunds {
                address: funder.address(),
                need,
                have,
            });
        }
        let params = TxParams {
            nonce: self.arkiv.tx_count(funder.address()).await?,
            to: self.address(),
            value: amount,
            input: Bytes::new(),
            gas_limit: TRANSFER_GAS,
            max_fee_per_gas: max_fee,
            max_priority_fee_per_gas: self.cfg.tip,
        };
        let tx = SignedTx::sign(funder, self.chain_id, params)?;
        self.arkiv.send(tx.clone()).await?;
        let (mut sent_at, mut resends) = (self.arkiv.head().await?, 0);
        loop {
            tokio::time::sleep(self.cfg.poll).await;
            if let Some(r) = self.arkiv.receipt(tx.hash()).await? {
                return match r.success {
                    true => Ok(()),
                    false => Err(StoreError::Reverted {
                        tx: Some(tx.hash()),
                        reason: "top-up transfer failed".into(),
                    }),
                };
            }
            let head = self.arkiv.head().await?;
            if head >= sent_at + self.cfg.resend_after_blocks {
                if resends >= self.cfg.max_resends {
                    return Err(StoreError::Dropped(tx.hash()));
                }
                (sent_at, resends) = (head, resends + 1);
                match self.arkiv.send(tx.clone()).await {
                    Ok(_) | Err(StoreError::NonceTooLow(_)) => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }

    /// Collect receipts; resend txs that have waited too long; surface reverts.
    async fn poll(
        &mut self,
        queue: &mut VecDeque<(usize, Job)>,
        inflight: &mut Vec<InFlight>,
        done: &mut Vec<Landed>,
    ) -> Result<(), StoreError> {
        // The head (with its base fee, for the next fill) and every receipt are
        // independent reads: one round trip, not 1 + n.
        let hashes: Vec<_> = inflight.iter().map(|f| f.tx.hash()).collect();
        let ((head, fee), receipts) = futures_util::future::try_join(
            self.arkiv.head_fee(),
            futures_util::future::try_join_all(hashes.iter().map(|h| self.arkiv.receipt(*h))),
        )
        .await?;
        self.seen = Some((head, fee, Instant::now()));
        let mut receipts = receipts.into_iter().map(Some).collect::<Vec<_>>();
        let mut i = 0;
        while i < inflight.len() {
            let f = &mut inflight[i];
            let pos = hashes
                .iter()
                .position(|h| *h == f.tx.hash())
                .ok_or_else(|| StoreError::Rpc("in-flight tx without a poll slot".into()))?;
            match receipts[pos].take().flatten() {
                Some(r) if r.success => {
                    done.push(Landed {
                        job: f.job,
                        receipt: r,
                    });
                    inflight.swap_remove(i);
                    continue;
                }
                Some(_) => {
                    let f = inflight.swap_remove(i);
                    let again = self.failed(f).await?;
                    queue.push_front(again);
                    continue;
                }
                None if head >= f.sent_at + self.cfg.resend_after_blocks => {
                    if f.resends >= self.cfg.max_resends {
                        return Err(StoreError::Dropped(f.tx.hash()));
                    }
                    f.resends += 1;
                    f.sent_at = head;
                    match self.arkiv.send(f.tx.clone()).await {
                        // The nonce moved on: its receipt may simply not be visible yet.
                        Ok(_) | Err(StoreError::NonceTooLow(_)) => {}
                        Err(e) => return Err(e),
                    }
                }
                None => {}
            }
            i += 1;
        }
        Ok(())
    }

    /// A mined tx failed, so none of its ops took effect. If re-estimating it reverts, that
    /// is the error; if not (its gas was too low, or the state moved), the job is returned
    /// to be priced afresh and sent once more.
    async fn failed(&mut self, f: InFlight) -> Result<(usize, Job), StoreError> {
        let reason = match &f.body {
            Job::Execute(ops) => match self.arkiv.estimate(self.address(), ops).await {
                Err(StoreError::Reverted { reason, .. }) => reason,
                Err(e @ StoreError::InsufficientFunds { .. }) => return Err(e),
                _ if self.retried.insert(f.job) => {
                    if let Some(s) = &f.shape {
                        self.estimates.forget(s);
                    }
                    return Ok((f.job, f.body));
                }
                _ => "status 0 (does not reproduce on re-estimate)".into(),
            },
            Job::Transfer(..) => "transfer failed".into(),
        };
        Err(StoreError::Reverted {
            tx: Some(f.tx.hash()),
            reason,
        })
    }
}

/// Sends one signed tx, resending the same bytes on an RPC error: an identical tx can land
/// at most once, so this never double-spends. Other errors return at once.
async fn send_retrying<A: Arkiv>(a: &A, tx: &SignedTx) -> Result<(), StoreError> {
    let mut attempt = 1;
    loop {
        match a.send(tx.clone()).await {
            Ok(_) => return Ok(()),
            Err(StoreError::Rpc(_)) if attempt < SEND_ATTEMPTS => {
                tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Halve an `execute` job that hit a size or gas cap; `None` when it cannot shrink.
fn split(body: &Job, e: &StoreError) -> Option<(Job, Job)> {
    match (body, e) {
        (Job::Execute(ops), StoreError::TxTooLarge { .. } | StoreError::GasCap { .. })
            if ops.len() > 1 =>
        {
            let (a, b) = ops.split_at(ops.len() / 2);
            Some((Job::Execute(a.to_vec()), Job::Execute(b.to_vec())))
        }
        _ => None,
    }
}

/// Limits for [`batch`]: keepalive uses 100 ops and 15M predicted gas per tx (design 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchLimits {
    pub max_ops: usize,
    pub max_gas: u64,
    pub max_bytes: usize,
}

impl Default for BatchLimits {
    fn default() -> Self {
        Self {
            max_ops: 100,
            max_gas: 15_000_000,
            max_bytes: MAX_RAW_TX_BYTES - ENVELOPE_BYTES,
        }
    }
}

/// Greedily pack `(op, predicted gas)` into jobs, in order, under every limit. The
/// predicted gas includes the op's share; 21,000 per tx is added here. An op that
/// alone breaks a limit gets a job of its own; the pipeline then reports the cap.
pub fn batch(items: Vec<(Operation, u64)>, limits: BatchLimits) -> Vec<Vec<Operation>> {
    let mut jobs: Vec<Vec<Operation>> = Vec::new();
    let (mut cur, mut gas, mut bytes) = (Vec::new(), TRANSFER_GAS, EXECUTE_BASE_BYTES);
    for (op, g) in items {
        let b = op_calldata_bytes(&op);
        let over = cur.len() + 1 > limits.max_ops
            || gas + g > limits.max_gas
            || bytes + b > limits.max_bytes;
        if over && !cur.is_empty() {
            jobs.push(std::mem::take(&mut cur));
            (gas, bytes) = (TRANSFER_GAS, EXECUTE_BASE_BYTES);
        }
        gas += g;
        bytes += b;
        cur.push(op);
    }
    if !cur.is_empty() {
        jobs.push(cur);
    }
    jobs
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    fn ext(i: u8) -> Operation {
        Operation::extend_expiry(B256::repeat_byte(i), 0, 100)
    }

    #[test]
    fn batch_respects_op_gas_and_byte_limits() {
        let items: Vec<_> = (0..250u8).map(|i| (ext(i), 50_000)).collect();
        let jobs = batch(items.clone(), BatchLimits::default());
        assert_eq!(
            jobs.iter().map(Vec::len).collect::<Vec<_>>(),
            vec![100, 100, 50]
        );
        let by_gas = batch(
            items.clone(),
            BatchLimits {
                max_gas: 21_000 + 3 * 50_000,
                ..BatchLimits::default()
            },
        );
        assert!(
            by_gas.iter().all(|j| j.len() == 3 || j.len() == 1),
            "gas caps a tx at 3"
        );
        let per_op = op_calldata_bytes(&ext(0));
        let by_bytes = batch(
            items,
            BatchLimits {
                max_bytes: EXECUTE_BASE_BYTES + 2 * per_op,
                ..BatchLimits::default()
            },
        );
        assert!(by_bytes.iter().all(|j| j.len() <= 2));
        assert!(batch(vec![], BatchLimits::default()).is_empty());
    }

    #[test]
    fn an_oversized_op_still_gets_its_own_job() {
        let jobs = batch(
            vec![(ext(1), 20_000_000), (ext(2), 1)],
            BatchLimits::default(),
        );
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn split_halves_only_multi_op_jobs_on_cap_errors() {
        let cap = StoreError::GasCap { gas: 2, cap: 1 };
        let two = Job::Execute(vec![ext(1), ext(2)]);
        assert!(split(&two, &cap).is_some());
        assert!(split(&Job::Execute(vec![ext(1)]), &cap).is_none());
        assert!(split(&two, &StoreError::Rpc("x".into())).is_none());
        assert!(split(&Job::Transfer(Address::ZERO, U256::ZERO), &cap).is_none());
    }
}
