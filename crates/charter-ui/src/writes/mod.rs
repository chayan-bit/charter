//! Signed writes (docs/p6/ui.md 4.4): a form becomes a [`Draft`], which is priced with
//! no signing ([`Writer::prepare`]) and kept server side under a random id; only the
//! confirm POST ([`Writer::confirm`]) signs and sends it, through the same charter-sync
//! and charter-escrow functions the CLI calls. Keys stay in this process.

pub mod form;
mod run;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use charter_core::{Hash32, Rid};
use charter_store::{Address, Arkiv, TxHash, U256};
use charter_sync::release::Publish;
use charter_sync::{Config, Keys};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};

/// A confirmation not sent within this long is dropped; the form must be sent again.
pub const DRAFT_TTL: Duration = Duration::from_secs(15 * 60);

/// The object an op belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Obj {
    /// A create op: a new object.
    New,
    Id(Hash32),
    /// The object the previous step created (a review's `approve` after its `create`).
    Prev,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Op {
        type_name: &'static str,
        action: &'static str,
        object: Obj,
        payload: Value,
    },
    /// Hashed into an `Op` when the draft is prepared (`chart release publish`'s code).
    Release(Publish),
    /// A wallet transfer of `amount` wei to the repo's escrow, then the deposit op that
    /// claims it (`chart escrow deposit`'s code).
    Deposit { amount: U256, purpose: String },
    /// One MLS commit removing every revoked device's leaf from a private repo's group
    /// (`chart member sync`'s code), priced from the commit it would send.
    RemoveRevoked,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Draft {
    pub rid: Rid,
    /// What the confirm dialog calls it: `Open issue`, `Approve patch`, ...
    pub label: String,
    /// The page the form was on.
    pub back: String,
    pub steps: Vec<Step>,
}

/// The price of a prepared draft, all in wei.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Estimate {
    /// Every tx the send makes, top-ups included.
    pub txs: usize,
    pub entities: usize,
    /// At most this in fees: estimated gas at the pipeline's fee cap, top-ups included.
    pub cost: U256,
    /// Wallet-to-control-address transfers the send makes first, by amount.
    pub top_ups: Vec<U256>,
    /// The top-ups' own fees at most (part of `cost`).
    pub top_up_fee: U256,
    /// Moved besides fees (an escrow deposit).
    pub value: U256,
    /// What the wallet and the repo's control address hold now.
    pub balance: U256,
    /// What the wallet alone holds now.
    pub wallet: U256,
    /// At most this leaves the wallet itself: top-ups, escrow deposits and their fees.
    pub from_wallet: U256,
    /// Blocks the written records are kept before they need a renewal.
    pub lifetime: u64,
}

/// A sent write, as its status page shows it.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub label: String,
    pub back: String,
    /// Every tx sent, in order (top-ups and resends included).
    pub txs: Vec<TxHash>,
    /// Ids of the ops written.
    pub ids: Vec<Hash32>,
    pub block: Option<u64>,
    pub error: Option<String>,
    /// The fee each mined tx paid, from the receipts the send waited on.
    pub fees: Vec<(TxHash, U256)>,
    /// What the dialog showed, or would have.
    pub est: Estimate,
    /// Sent without the cost dialog (the small-write setting).
    pub skipped: bool,
    /// When it was confirmed and sent (ms).
    pub started_ms: u64,
    pub done_ms: u64,
}

impl Status {
    /// Fees paid: the receipts' sum once every tx has one, else the estimate (`true`).
    pub fn spent(&self) -> (U256, bool) {
        let seen = self.fees.iter().fold(U256::ZERO, |s, (_, f)| s + f);
        match self
            .txs
            .iter()
            .all(|h| self.fees.iter().any(|(t, _)| t == h))
        {
            true if !self.txs.is_empty() => (seen, false),
            _ => (self.est.cost, true),
        }
    }
}

type Shared = Arc<Mutex<Status>>;

/// The wallet's last known balance and when it was read (ms); never read for a page.
pub type Balance = Arc<Mutex<Option<(U256, u64)>>>;

enum Job {
    Prepare(Draft, oneshot::Sender<Result<(Draft, Estimate), String>>),
    Send(Draft, Shared),
    Balance(Option<oneshot::Sender<()>>),
}

/// The UI's handle on the write worker, which owns the backend and the keys.
pub struct Writer {
    jobs: mpsc::UnboundedSender<Job>,
    drafts: Mutex<HashMap<String, (Instant, Draft, Estimate)>>,
    writes: Mutex<HashMap<String, Shared>>,
    /// The wallet that pays.
    pub address: Address,
    /// This computer's peer id, hex.
    pub nid: String,
    balance: Balance,
}

impl Writer {
    /// Prices `d` (hashing release artifacts on the way) without signing anything, and
    /// keeps it for [`Writer::confirm`]; returns its id, the prepared draft and the price.
    pub async fn prepare(&self, d: Draft) -> Result<(String, Draft, Estimate), String> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Job::Prepare(d, tx))
            .map_err(|_| "the write worker stopped".to_owned())?;
        let (d, e) = rx
            .await
            .map_err(|_| "the write worker stopped".to_owned())??;
        let id = crate::security::random_hex().map_err(|e| e.to_string())?;
        if let Ok(mut m) = self.drafts.lock() {
            m.retain(|_, (t, _, _)| t.elapsed() < DRAFT_TTL);
            m.insert(id.clone(), (Instant::now(), d.clone(), e.clone()));
        }
        Ok((id, d, e))
    }

    /// Signs and sends the draft `id` of repo `rid`, once; `false` if it is unknown,
    /// expired or already sent. `skipped` records that no dialog was shown.
    pub fn confirm(&self, rid: &Rid, id: &str, skipped: bool) -> bool {
        let taken = self.drafts.lock().ok().and_then(|mut m| m.remove(id));
        let fresh =
            |(at, d, _): &(Instant, Draft, Estimate)| at.elapsed() < DRAFT_TTL && d.rid == *rid;
        let Some((_, d, est)) = taken.filter(fresh) else {
            return false;
        };
        let status = Arc::new(Mutex::new(Status {
            label: d.label.clone(),
            back: d.back.clone(),
            est,
            skipped,
            started_ms: crate::sync_loop::now_ms(),
            ..Status::default()
        }));
        if let Ok(mut w) = self.writes.lock() {
            w.insert(id.to_owned(), status.clone());
        }
        self.jobs.send(Job::Send(d, status)).is_ok()
    }

    pub fn status(&self, id: &str) -> Option<Status> {
        let w = self.writes.lock().ok()?;
        let s = w.get(id)?.lock().ok()?;
        Some(s.clone())
    }

    /// The wallet's last known balance and when it was read.
    pub fn balance(&self) -> Option<(U256, u64)> {
        self.balance.lock().ok().and_then(|b| *b)
    }

    /// Reads the balance again and waits for it.
    pub async fn refresh(&self) {
        let (tx, rx) = oneshot::channel();
        if self.jobs.send(Job::Balance(Some(tx))).is_ok() {
            let _ = rx.await;
        }
    }
}

/// Starts the worker on the current runtime; each job runs as its own task.
pub fn spawn<A: Arkiv + 'static>(a: Arc<A>, cfg: Config, keys: Keys) -> Arc<Writer> {
    let (jobs, mut rx) = mpsc::unbounded_channel();
    let (address, nid) = (
        keys.wallet().address(),
        charter_sync::ids::hex32(&keys.nid()),
    );
    let balance = Balance::default();
    let ctx = Arc::new(run::Ctx {
        a,
        cfg,
        keys,
        balance: balance.clone(),
    });
    // One read at start, so the header shows a balance; pages never read the chain.
    let _ = jobs.send(Job::Balance(None));
    tokio::spawn(async move {
        while let Some(job) = rx.recv().await {
            let ctx = ctx.clone();
            tokio::spawn(async move {
                match job {
                    Job::Prepare(d, reply) => {
                        let _ = reply.send(ctx.prepare(d).await);
                    }
                    Job::Send(d, status) => ctx.send(d, status).await,
                    Job::Balance(done) => {
                        ctx.read_balance().await;
                        if let Some(d) = done {
                            let _ = d.send(());
                        }
                    }
                }
            });
        }
    });
    Arc::new(Writer {
        jobs,
        drafts: Mutex::default(),
        writes: Mutex::default(),
        address,
        nid,
        balance,
    })
}
