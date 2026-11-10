//! `TxPipeline`: nonces, a window of in-flight txs, gas pacing, receipts, resends and
//! revert decoding for one sender (design 4.6), rolling top-ups from a funder (design 4.2
//! step 6), plus greedy batching under caps.

use std::collections::VecDeque;
use std::time::Duration;

use alloy_primitives::{Address, Bytes, U256};
use alloy_signer_local::PrivateKeySigner;
use arkiv_bindings::Operation;

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
        }
    }
}

impl PipelineConfig {
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
}

/// Sends jobs (each one `execute(ops)` tx) from one key. See the module docs.
pub struct TxPipeline<'a, A: Arkiv> {
    arkiv: &'a A,
    signer: PrivateKeySigner,
    chain_id: u64,
    cfg: PipelineConfig,
    nonce: Option<u64>,
    /// Tops the sender up from this key before each window that its balance cannot cover.
    funder: Option<PrivateKeySigner>,
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
            tokio::time::sleep(self.cfg.poll).await;
            self.poll(&mut inflight, &mut done).await?;
        }
    }

    /// Send queued jobs while the window and the gas budget allow.
    async fn fill(
        &mut self,
        queue: &mut VecDeque<(usize, Job)>,
        inflight: &mut Vec<InFlight>,
    ) -> Result<(), StoreError> {
        // Read fresh every round: the base fee can rise while we fill blocks.
        let (base_fee, sent_at) =
            futures_util::future::try_join(self.arkiv.base_fee(), self.arkiv.head()).await?;
        let budget = self.cfg.pace_budget(base_fee);
        let mut unconfirmed: u64 = inflight.iter().map(|f| f.gas).sum();
        let mut covered = match self.funder {
            Some(_) => Some(self.covered(inflight).await?),
            None => None,
        };
        while inflight.len() < self.cfg.window {
            let Some((job, body)) = queue.pop_front() else {
                break;
            };
            let (input, to, value, gas) = match self.price(&body).await {
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
            if !inflight.is_empty() && unconfirmed + gas > budget {
                queue.push_front((job, body));
                break;
            }
            let reserve = self.cfg.tx_reserve(gas, value, base_fee);
            // One transfer covers the rest of the window (free slots, capped at the jobs
            // left) at this tx's size.
            let slots = ((self.cfg.window - inflight.len()) as u64).min(1 + queue.len() as u64);
            let top = covered.and_then(|h| PipelineConfig::top_up(h, reserve, slots));
            if let (Some(have), Some(amount)) = (covered.as_mut(), top) {
                self.top_up(amount, base_fee).await?;
                *have += amount;
            }
            let tx = match self.sign_and_send(input, to, value, gas, base_fee).await {
                Err(e @ StoreError::TxTooLarge { .. }) => match split(&body, &e) {
                    Some((a, b)) => {
                        queue.push_front((job, b));
                        queue.push_front((job, a));
                        continue;
                    }
                    None => return Err(e),
                },
                other => other?,
            };
            unconfirmed += gas;
            if let Some(have) = covered.as_mut() {
                *have = have.saturating_sub(reserve);
            }
            inflight.push(InFlight {
                job,
                body,
                tx,
                gas,
                sent_at,
                resends: 0,
            });
        }
        Ok(())
    }

    /// Calldata, recipient, value and estimated gas, checking the byte and gas caps.
    async fn price(&self, body: &Job) -> Result<(Bytes, Address, U256, u64), StoreError> {
        let ops = match body {
            Job::Transfer(to, value) => return Ok((Bytes::new(), *to, *value, TRANSFER_GAS)),
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
        let gas = self.arkiv.estimate(self.address(), ops).await?;
        if gas > self.cfg.gas_cap {
            return Err(StoreError::GasCap {
                gas,
                cap: self.cfg.gas_cap,
            });
        }
        Ok((input, ARKIV_ADDRESS, U256::ZERO, gas))
    }

    async fn sign_and_send(
        &mut self,
        input: Bytes,
        to: Address,
        value: U256,
        gas: u64,
        base_fee: u128,
    ) -> Result<SignedTx, StoreError> {
        let gas_limit = self.cfg.gas_limit(gas);
        for attempt in 0..2 {
            let nonce = match self.nonce {
                Some(n) => n,
                None => self.arkiv.tx_count(self.address()).await?,
            };
            let params = TxParams {
                nonce,
                to,
                value,
                input: input.clone(),
                gas_limit,
                max_fee_per_gas: self.cfg.max_fee(base_fee),
                max_priority_fee_per_gas: self.cfg.tip,
            };
            let tx = SignedTx::sign(&self.signer, self.chain_id, params)?;
            if tx.raw().len() > self.cfg.max_raw_bytes {
                return Err(StoreError::TxTooLarge {
                    size: tx.raw().len(),
                    cap: self.cfg.max_raw_bytes,
                });
            }
            match self.arkiv.send(tx.clone()).await {
                Ok(_) => {
                    self.nonce = Some(nonce + 1);
                    return Ok(tx);
                }
                // Someone else used this nonce: re-read it once and re-sign.
                Err(StoreError::NonceTooLow(_)) if attempt == 0 => self.nonce = None,
                Err(e) => return Err(e),
            }
        }
        Err(StoreError::InconsistentView(
            "nonce still too low after re-reading it".into(),
        ))
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
        inflight: &mut Vec<InFlight>,
        done: &mut Vec<Landed>,
    ) -> Result<(), StoreError> {
        // The head and every receipt are independent reads: one round trip, not 1 + n.
        let hashes: Vec<_> = inflight.iter().map(|f| f.tx.hash()).collect();
        let (head, receipts) = futures_util::future::try_join(
            self.arkiv.head(),
            futures_util::future::try_join_all(hashes.iter().map(|h| self.arkiv.receipt(*h))),
        )
        .await?;
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
                Some(_) => return Err(self.revert_reason(f).await),
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

    /// Why a mined tx failed: re-estimate it to recover the registry error when possible.
    async fn revert_reason(&self, f: &InFlight) -> StoreError {
        let reason = match &f.body {
            Job::Execute(ops) => match self.arkiv.estimate(self.address(), ops).await {
                Err(StoreError::Reverted { reason, .. }) => reason,
                Err(StoreError::InsufficientFunds {
                    address,
                    need,
                    have,
                }) => {
                    return StoreError::InsufficientFunds {
                        address,
                        need,
                        have,
                    };
                }
                _ => "status 0 (does not reproduce on re-estimate)".into(),
            },
            Job::Transfer(..) => "transfer failed".into(),
        };
        StoreError::Reverted {
            tx: Some(f.tx.hash()),
            reason,
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
