//! The write worker: prices drafts and sends them. Sending goes through `cob::post`,
//! `release::payload`, `TxPipeline::transfer` and `charter_escrow::ops::post`, as the
//! CLI does; [`Tap`] wraps the backend only to record each tx hash as it is sent. Both
//! pricing and sending pick the backend with `charter_private::for_repo`, the CLI's rule,
//! so a private repo's draft is priced and sent sealed.

use std::sync::{Arc, Mutex};

use charter_core::{CobOpBody, Hash32, Rid, ZERO};
use charter_crypto::MasterSeed;
use charter_escrow::ledger;
use charter_escrow::ops::{self as escrow, DEPOSIT, DepositPayload};
use charter_store::{
    Address, Arkiv, B256, Operation, PipelineConfig, QueryOpts, QueryPage, Receipt, SignedTx,
    StoreError, TxHash, TxInfo, TxPipeline, U256,
};
use charter_sync::cob::{self, json};
use charter_sync::push_send::first_write_ops;
use charter_sync::release::{self, RELEASE_TYPE};
use charter_sync::write::{entity_op, price, sign};
use charter_sync::{Config, Keys, view};

use super::{Balance, Draft, Estimate, Obj, Shared, Status, Step};

/// Gas of a plain value transfer.
const TRANSFER_GAS: u64 = 21_000;

pub struct Ctx<A> {
    pub a: Arc<A>,
    pub cfg: Config,
    pub keys: Keys,
    pub balance: Balance,
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn now_ms() -> u64 {
    crate::sync_loop::now_ms()
}

/// The unsigned op of an `Op` step; `prev` is the previous step's op id.
fn body(rid: &Rid, step: &Step, prev: Hash32) -> Result<CobOpBody, String> {
    let Step::Op {
        type_name,
        action,
        object,
        payload,
    } = step
    else {
        return Err("not an op step".into());
    };
    let object = match object {
        Obj::New => ZERO,
        Obj::Id(h) => *h,
        Obj::Prev => prev,
    };
    Ok(CobOpBody {
        rid: *rid,
        type_name: (*type_name).into(),
        object,
        parents: if object == ZERO { vec![] } else { vec![object] },
        action: (*action).into(),
        payload: json::to_cbor(payload).map_err(err)?,
        ts_ms: now_ms(),
    })
}

/// What `chart escrow deposit` posts for a transfer `tx` (no expiry).
fn deposit(tx: TxHash, amount: U256, purpose: &str) -> DepositPayload {
    DepositPayload {
        tx,
        amount: amount.to_string(),
        purpose: purpose.to_owned(),
        expiry: None,
    }
}

/// The deposit op as a step, for pricing with a placeholder tx.
fn deposit_step(appt_object: Hash32, amount: U256, purpose: &str) -> Result<Step, String> {
    Ok(Step::Op {
        type_name: escrow::TYPE,
        action: DEPOSIT,
        object: Obj::Id(appt_object),
        payload: serde_json::to_value(deposit(B256::ZERO, amount, purpose)).map_err(err)?,
    })
}

impl<A: Arkiv> Ctx<A> {
    fn wallet(&self) -> Address {
        self.keys.wallet().address()
    }

    fn keep_balance(&self, wei: U256) {
        if let Ok(mut b) = self.balance.lock() {
            *b = Some((wei, now_ms()));
        }
    }

    /// One balance read, for the header chip and the wallet page.
    pub async fn read_balance(&self) {
        if let Ok(w) = self.a.balance(self.wallet()).await {
            self.keep_balance(w);
        }
    }

    async fn bound<B: Arkiv>(&self, b: &B, rid: &Rid) -> Result<bool, String> {
        let p = view::resolve_peer(b, rid, &self.keys.nid()).await;
        Ok(p.map_err(err)?.is_some_and(|p| !p.binds.links.is_empty()))
    }

    /// Hashes release artifacts into their op; every other step is kept as is.
    async fn resolve(step: Step) -> Result<Step, String> {
        let Step::Release(p) = step else {
            return Ok(step);
        };
        Ok(Step::Op {
            type_name: RELEASE_TYPE,
            action: "create",
            object: Obj::New,
            payload: release::payload(&p).await.map_err(err)?,
        })
    }

    /// Gas of one op step, signed with a throwaway key of the same algorithm so the
    /// entity has the exact size the real one will have; nothing of the user is signed.
    async fn op_gas<B: Arkiv>(
        &self,
        b: &B,
        tmp: &Keys,
        d: &Draft,
        step: &Step,
        prev: &mut Hash32,
        bind: &mut bool,
    ) -> Result<(u64, usize), String> {
        let s = sign(tmp.sk(), &body(&d.rid, step, *prev)?).map_err(err)?;
        let mut ops: Vec<Operation> = match std::mem::take(bind) {
            true => first_write_ops(tmp, &d.rid, &self.cfg).map_err(err)?,
            false => vec![],
        };
        ops.push(entity_op::<CobOpBody>(&s.bytes, self.cfg.control_lifetime).map_err(err)?);
        let from = self.keys.control_address(&d.rid);
        let gas = b.estimate(from, &ops).await.map_err(err)?;
        *prev = s.h;
        Ok((gas, ops.len()))
    }

    /// The backend that sends to `rid`: sealed under the keys current now for a private repo.
    async fn for_repo<'b, B: Arkiv>(
        &self,
        a: &'b B,
        rid: &Rid,
    ) -> Result<charter_private::ForRepo<'b, B>, String> {
        charter_private::for_repo(a, &self.cfg, Some(&self.keys), rid)
            .await
            .map_err(err)
    }

    /// Prices `d` at its sealed size for a private repo. Pricing reads the keyring the last
    /// sync saved and makes no group query; the send refreshes it once (the CLI's one).
    pub async fn prepare(&self, d: Draft) -> Result<(Draft, Estimate), String> {
        let b = charter_private::for_repo_cached(&*self.a, &self.cfg, Some(&self.keys), &d.rid)
            .await
            .map_err(err)?;
        let tmp = Keys::from_seed(MasterSeed::generate().map_err(err)?);
        let (mut prev, mut bind) = (ZERO, !self.bound(&b, &d.rid).await?);
        let base_fee = b.base_fee().await.map_err(err)?;
        let control = self.keys.control_address(&d.rid);
        let (w, mut have) =
            futures_util::future::try_join(b.balance(self.wallet()), b.balance(control))
                .await
                .map_err(err)?;
        self.keep_balance(w);
        let mut e = Estimate {
            balance: w + have,
            wallet: w,
            lifetime: self.cfg.control_lifetime,
            ..Estimate::default()
        };
        let mut steps = Vec::with_capacity(d.steps.len());
        for step in d.steps.clone() {
            let step = Self::resolve(step).await?;
            let op = match &step {
                Step::Deposit { amount, purpose } => {
                    let appt = ledger::load(&b, &d.rid).await.map_err(|e| {
                        format!("this repo has no escrow to fund a bounty through: {e}")
                    })?;
                    // The escrow transfer goes from the wallet itself: never topped up.
                    (e.value, e.txs) = (e.value + amount, e.txs + 1);
                    e.cost += price(TRANSFER_GAS, base_fee);
                    e.from_wallet += *amount + price(TRANSFER_GAS, base_fee);
                    deposit_step(appt.appointment.object, *amount, purpose)?
                }
                _ => step.clone(),
            };
            let (gas, n) = self.op_gas(&b, &tmp, &d, &op, &mut prev, &mut bind).await?;
            self.control_tx(&mut e, &mut have, gas, base_fee);
            e.entities += n;
            steps.push(step);
        }
        Ok((Draft { steps, ..d }, e))
    }

    /// One control-address tx of `gas`, preceded by the wallet top-up the send path makes
    /// when `have` cannot cover it (each op is its own one-job `send_funded` run).
    fn control_tx(&self, e: &mut Estimate, have: &mut U256, gas: u64, base_fee: u128) {
        let pc = &self.cfg.pipeline;
        let reserve = pc.tx_reserve(gas, U256::ZERO, base_fee);
        if let Some(amount) = PipelineConfig::top_up(*have, reserve, 1) {
            let fee = pc.top_up_fee(base_fee);
            e.top_ups.push(amount);
            e.from_wallet += amount + fee;
            (e.top_up_fee, e.cost, e.txs) = (e.top_up_fee + fee, e.cost + fee, e.txs + 1);
            *have += amount;
        }
        e.txs += 1;
        e.cost += price(gas, base_fee);
        // What the op spends at today's base fee; the next step sees the rest.
        *have = have.saturating_sub(U256::from(gas) * U256::from(base_fee + pc.tip));
    }

    async fn run<B: Arkiv>(&self, tap: &B, d: &Draft) -> Result<Vec<Hash32>, String> {
        let (cfg, keys) = (&self.cfg, &self.keys);
        let (mut prev, mut ids) = (ZERO, vec![]);
        for step in &d.steps {
            prev = match step {
                Step::Deposit { amount, purpose } => {
                    let appt = ledger::load(tap, &d.rid).await.map_err(err)?.appointment;
                    let chain_id = tap.chain_id().await.map_err(err)?;
                    let mut p = TxPipeline::new(tap, keys.wallet(), chain_id, cfg.pipeline.clone());
                    let tx = p
                        .transfer(appt.address, *amount)
                        .await
                        .map_err(err)?
                        .tx_hash;
                    let p = deposit(tx, *amount, purpose);
                    escrow::post(tap, cfg, keys, &d.rid, appt.object, DEPOSIT, &p)
                        .await
                        .map_err(err)?
                }
                Step::Release(_) => return Err("the release was not prepared".into()),
                op => {
                    let b = body(&d.rid, op, prev)?;
                    cob::post(tap, cfg, keys, &b).await.map_err(err)?
                }
            };
            ids.push(prev);
        }
        Ok(ids)
    }

    /// Sends the prepared draft, recording txs as they go out and the block of the last.
    pub async fn send(&self, d: Draft, status: Shared) {
        let tap = Tap {
            a: self.a.clone(),
            status: status.clone(),
        };
        let out = match self.for_repo(&tap, &d.rid).await {
            Ok(b) => self.run(&b, &d).await,
            Err(e) => Err(e),
        };
        let last = status.lock().ok().and_then(|s| s.txs.last().copied());
        let block = match (&out, last) {
            (Ok(_), Some(h)) => tap.receipt(h).await.ok().flatten().map(|r| r.block_number),
            _ => None,
        };
        // Before the status says confirmed, so the page that shows it has the new balance.
        if out.is_ok() {
            self.read_balance().await;
        }
        if let Ok(mut s) = status.lock() {
            match (out, block) {
                (Ok(ids), Some(b)) => (s.ids, s.block) = (ids, Some(b)),
                (Ok(ids), None) => {
                    s.ids = ids;
                    s.error = Some("sent, but no receipt was found for the last tx".into());
                }
                (Err(e), _) => s.error = Some(e),
            }
            if s.block.is_some() {
                s.done_ms = now_ms();
                self.ledger(&d, &s);
            }
        }
    }

    /// Appends a confirmed write to the spend ledger; a ledger error is logged, never
    /// turned into a failed write (the write is on chain already).
    fn ledger(&self, d: &Draft, s: &Status) {
        let (wei, estimated) = s.spent();
        let entry = crate::spend::Spend {
            ts_ms: s.done_ms,
            block: s.block.unwrap_or_default(),
            repo: charter_sync::ids::hex32(&d.rid),
            action: d.label.clone(),
            wei: wei.to_string(),
            value: s.est.value.to_string(),
            txs: s.txs.len(),
            skipped: s.skipped,
            estimated,
        };
        if let Err(e) = crate::spend::record(&self.cfg.home, entry) {
            charter_sync::warn(&format!("ui: spend ledger not updated: {e}"));
        }
    }
}

/// The backend, recording every tx hash it sends into the write's status.
pub struct Tap<A> {
    a: Arc<A>,
    status: Arc<Mutex<Status>>,
}

impl<A: Arkiv> Arkiv for Tap<A> {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        self.a.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.a.head().await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.a.base_fee().await
    }
    async fn head_fee(&self) -> Result<(u64, u128), StoreError> {
        self.a.head_fee().await
    }
    fn heads_ws(&self) -> Option<charter_store::heads::WsConfig> {
        self.a.heads_ws()
    }
    async fn query(
        &self,
        q: &charter_store::Query,
        o: &QueryOpts,
    ) -> Result<QueryPage, StoreError> {
        self.a.query(q, o).await
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        self.a.entity_nonce(c).await
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.a.balance(a).await
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.a.tx_count(a).await
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.a.tx_count_mined(a).await
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.a.estimate(from, ops).await
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        let h = self.a.send(tx).await?;
        if let Ok(mut s) = self.status.lock() {
            s.txs.push(h);
        }
        Ok(h)
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        let r = self.a.receipt(h).await?;
        if let (Some(r), Ok(mut s)) = (&r, self.status.lock())
            && !s.fees.iter().any(|(t, _)| *t == h)
        {
            let fee = U256::from(r.gas_used) * U256::from(r.effective_gas_price);
            s.fees.push((h, fee));
        }
        Ok(r)
    }
    fn seal_ops(&self, ops: Vec<Operation>) -> Result<Vec<Operation>, StoreError> {
        self.a.seal_ops(ops)
    }
    fn open_rows(&self, rows: Vec<charter_store::EntityData>) -> Vec<charter_store::EntityData> {
        self.a.open_rows(rows)
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.a.transaction(h).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.a.mined_tx(h).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<charter_store::MinedTx>, StoreError> {
        self.a.block_txs(n).await
    }
    async fn tx_at(&self, n: u64, i: u32) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.a.tx_at(n, i).await
    }
    fn seals(&self) -> bool {
        self.a.seals()
    }
    fn seals_for(&self, rid: &[u8; 32]) -> bool {
        self.a.seals_for(rid)
    }
}
