//! The escrow agent: folds obligations into payouts and expired deposits into refunds.
//!
//! Crash safety: before a transfer is sent, its intent (the settlement it is for and the
//! exact signed tx params, nonce included) is journaled. On restart the agent finishes the
//! journaled intent first: it posts the op if the tx landed, rebroadcasts the same tx if
//! not. A key is paid at most once because the agent skips keys that already have a
//! settlement op and reuses the journaled nonce, so a retry can never be a second transfer.

use std::fs::File;
use std::path::PathBuf;

use charter_core::Rid;
use charter_store::{Address, Arkiv, Receipt, SignedTx, TxHash, TxParams, U256};
use charter_sync::ids::hex32;
use charter_sync::{Config, SyncError};
use serde::{Deserialize, Serialize};

use crate::keys::{EscrowKeys, escrow_home};
use crate::ledger::{self, Ledger, refund_key};
use crate::obligation::ObligationSource;
use crate::ops::{self, PAID, REFUNDED, SettlePayload};
use crate::{Result, internal};

/// Gas of a plain transfer.
const TRANSFER_GAS: u64 = 21_000;
/// Receipt polls before leaving a sent tx to the next round.
const WAIT_POLLS: u32 = 120;

/// One settlement to make: a payout of a key or a refund of a deposit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transfer {
    pub action: String,
    pub key: String,
    pub to: Address,
    pub amount: U256,
    pub deposit: Option<TxHash>,
}

/// A journaled transfer: re-signing `params` reproduces `tx` exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Intent {
    pub transfer: Transfer,
    pub nonce: u64,
    pub gas_limit: u64,
    pub max_fee: u128,
    pub tip: u128,
    pub tx: TxHash,
}

/// What one round did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub paid: Vec<String>,
    pub refunded: Vec<String>,
    /// Obligations that could not be paid (unfunded or unresolvable `to`), with why.
    pub skipped: Vec<String>,
}

pub struct Agent<'a, A: Arkiv, O: ObligationSource> {
    pub a: &'a A,
    pub cfg: &'a Config,
    pub keys: &'a EscrowKeys,
    pub rid: Rid,
    pub source: &'a O,
}

impl<'a, A: Arkiv, O: ObligationSource> Agent<'a, A, O> {
    fn journal_path(&self) -> PathBuf {
        escrow_home(&self.cfg.home)
            .join("journal")
            .join(format!("{}.json", hex32(&self.rid)))
    }

    /// One agent per repo at a time: concurrent agents could race to the same key.
    fn lock(&self) -> Result<File> {
        let p = self.journal_path().with_extension("lock");
        let dir = p
            .parent()
            .ok_or_else(|| internal("journal has no parent"))?;
        std::fs::create_dir_all(dir).map_err(|e| SyncError::io(dir.display(), e))?;
        let f = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&p)
            .map_err(|e| SyncError::io(p.display(), e))?;
        f.lock().map_err(|e| SyncError::io(p.display(), e))?;
        Ok(f)
    }

    pub fn journal(&self) -> Result<Option<Intent>> {
        let p = self.journal_path();
        match std::fs::read(&p) {
            Ok(b) => {
                serde_json::from_slice(&b).map_err(|e| internal(format!("{}: {e}", p.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(SyncError::io(p.display(), e)),
        }
    }

    fn save(&self, intent: Option<&Intent>) -> Result<()> {
        let p = self.journal_path();
        let Some(i) = intent else {
            return match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    Err(SyncError::io(p.display(), e))
                }
                _ => Ok(()),
            };
        };
        // Temp file (0600), fsync, rename: a crash after `send` never loses the journal.
        charter_sync::state::write_json(&p, i)
    }

    /// The ledger, refusing to act unless this agent is the one appointed.
    pub async fn ledger(&self) -> Result<Ledger> {
        let l = Box::pin(ledger::load(self.a, &self.rid)).await?;
        let appt = &l.appointment;
        if appt.nid != self.keys.id.nid() || appt.address != self.keys.address() {
            return Err(internal("this escrow is not the one the repo appointed"));
        }
        Ok(l)
    }

    /// Payouts then refunds the ledger allows now, in program order.
    pub async fn plan(&self, l: &Ledger) -> Result<(Vec<Transfer>, Vec<String>)> {
        let obligations = self.source.obligations(&self.rid)?;
        let (mut out, mut skipped) = (vec![], vec![]);
        let due = l.due(&obligations)?;
        for o in obligations
            .iter()
            .filter(|o| o.is_pay() && !l.settled(&o.key))
        {
            let Some(d) = due.iter().find(|d| d.obligation.key == o.key) else {
                skipped.push(format!("{}: purpose {:?} underfunded", o.key, o.purpose));
                continue;
            };
            match ledger::resolve_to(self.a, &self.rid, &o.to).await? {
                Some(to) => out.push(Transfer {
                    action: PAID.into(),
                    key: o.key.clone(),
                    to,
                    amount: d.amount,
                    deposit: None,
                }),
                None => skipped.push(format!("{}: cannot resolve {:?}", o.key, o.to)),
            }
        }
        let head = self.a.head().await?;
        out.extend(
            l.refundable(&obligations, head)
                .into_iter()
                .map(|d| Transfer {
                    action: REFUNDED.into(),
                    key: refund_key(&d.tx),
                    to: d.from,
                    amount: d.amount,
                    deposit: Some(d.tx),
                }),
        );
        Ok((out, skipped))
    }

    /// One round: finish any journaled transfer, then settle everything due.
    pub async fn run_once(&self) -> Result<Report> {
        let _lock = self.lock()?;
        let mut report = Report::default();
        let mut l = self.ledger().await?;
        if let Some(i) = self.journal()? {
            Box::pin(self.recover(&l, i, &mut report)).await?;
            l = self.ledger().await?;
        }
        let (plan, skipped) = Box::pin(self.plan(&l)).await?;
        report.skipped = skipped;
        for t in plan {
            let i = Box::pin(self.send(&t)).await?;
            Box::pin(self.finish(&l, &i, &mut report)).await?;
        }
        Ok(report)
    }

    /// Journals then sends `t`; a crash after this leaves the intent for [`Self::run_once`].
    pub async fn start(&self, t: &Transfer) -> Result<Intent> {
        let _lock = self.lock()?;
        Box::pin(self.send(t)).await
    }

    async fn send(&self, t: &Transfer) -> Result<Intent> {
        let addr = self.keys.address();
        let (base, nonce, chain_id, have) = futures_util::future::try_join4(
            self.a.base_fee(),
            self.a.tx_count(addr),
            self.a.chain_id(),
            self.a.balance(addr),
        )
        .await?;
        let p = &self.cfg.pipeline;
        let mut i = Intent {
            transfer: t.clone(),
            nonce,
            gas_limit: p.gas_limit(TRANSFER_GAS),
            max_fee: p.max_fee(base),
            tip: p.tip,
            tx: TxHash::ZERO,
        };
        let tx = self.sign_on(&i, chain_id)?;
        if have < tx.max_cost() {
            return Err(internal(format!(
                "escrow holds {have} wei, transfer needs {}",
                tx.max_cost()
            )));
        }
        i.tx = tx.hash();
        self.save(Some(&i))?;
        self.a.send(tx).await?;
        Ok(i)
    }

    fn sign_on(&self, i: &Intent, chain_id: u64) -> Result<SignedTx> {
        let p = TxParams {
            nonce: i.nonce,
            to: i.transfer.to,
            value: i.transfer.amount,
            input: Default::default(),
            gas_limit: i.gas_limit,
            max_fee_per_gas: i.max_fee,
            max_priority_fee_per_gas: i.tip,
        };
        Ok(SignedTx::sign(self.keys.payout(), chain_id, p)?)
    }

    async fn sign(&self, i: &Intent) -> Result<SignedTx> {
        let chain_id = self.a.chain_id().await?;
        self.sign_on(i, chain_id)
    }

    async fn recover(&self, l: &Ledger, i: Intent, report: &mut Report) -> Result<()> {
        if l.settled(&i.transfer.key) {
            return self.save(None);
        }
        if self.a.receipt(i.tx).await?.is_none() {
            let mined = self.a.tx_count_mined(self.keys.address()).await?;
            if mined > i.nonce {
                return Err(internal(format!(
                    "journaled nonce {} was used by another tx; run `chart escrow audit`",
                    i.nonce
                )));
            }
            // shortcut: rebroadcasts at the journaled fee, never replaces; a fee spike stalls it.
            let tx = self.sign(&i).await?;
            if tx.hash() != i.tx {
                return Err(internal(
                    "journaled params do not reproduce the journaled tx",
                ));
            }
            self.a.send(tx).await?;
        }
        Box::pin(self.finish(l, &i, report)).await
    }

    async fn wait(&self, h: TxHash) -> Result<Option<Receipt>> {
        for _ in 0..WAIT_POLLS {
            if let Some(r) = self.a.receipt(h).await? {
                return Ok(Some(r));
            }
            tokio::time::sleep(self.cfg.pipeline.poll).await;
        }
        Ok(None)
    }

    /// Waits for the receipt, posts the settlement op, clears the journal.
    async fn finish(&self, l: &Ledger, i: &Intent, report: &mut Report) -> Result<()> {
        let Some(r) = self.wait(i.tx).await? else {
            return Err(internal(format!(
                "transfer {} not mined yet; will retry",
                i.tx
            )));
        };
        let t = &i.transfer;
        if !r.success {
            self.save(None)?;
            return Err(internal(format!(
                "transfer {} for {} reverted",
                i.tx, t.key
            )));
        }
        let p = SettlePayload {
            key: t.key.clone(),
            tx: i.tx,
            to: t.to,
            amount: t.amount.to_string(),
            deposit: t.deposit,
        };
        let (a, cfg, keys) = (self.a, self.cfg, &self.keys.id);
        ops::post(a, cfg, keys, &self.rid, l.appointment.object, &t.action, &p).await?;
        self.save(None)?;
        match t.action.as_str() {
            REFUNDED => report.refunded.push(t.key.clone()),
            _ => report.paid.push(t.key.clone()),
        }
        Ok(())
    }

    /// Rounds forever, `interval` apart; a failed round is logged and retried.
    pub async fn run(&self, interval: std::time::Duration) -> Result<()> {
        loop {
            match self.run_once().await {
                Ok(r) => eprintln!(
                    "escrow: paid {:?} refunded {:?} skipped {:?}",
                    r.paid, r.refunded, r.skipped
                ),
                Err(e) => eprintln!("escrow: round failed: {e}"),
            }
            tokio::time::sleep(interval).await;
        }
    }
}
