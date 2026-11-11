//! Speculative sync and the head probe (docs/p5/head-model.md 5.6).
//!
//! A full sync records every query its view build and program reads sent, with the pages
//! that came back, in `view-memo.json`. The next sync of the repo then either
//!
//! - replays those pages without sending them, when it runs within [`window`] of the last
//!   full sync and the probe is unchanged: the `(key, $updatedAt)` list of every `id`,
//!   `idacc`, `bind`, `anchor` and `head` entity of the repo, plus that of every `cob` op
//!   when the repo pins a program; any create, delete, patch, extend or expiry changes it;
//! - or sends every recorded query at once and serves the build from those answers.
//!
//! `entityNonce` reads and count-only queries are never recorded: they are sent live, in
//! the build's order, so every nonce is read after the rows it checks (head-model 6). A
//! recorded page is served at most once per sync, so a second read of the same rows (the
//! full nonce bracket after the short one failed) always goes to the network. Any failure
//! of a replayed or speculative build falls back to the plain build.

use std::collections::BTreeMap;
use std::sync::Mutex;

use charter_core::{Kind, Rid};
use charter_store::{
    Address, Arkiv, Attrs, B256, EntityData, MAX_LIMIT, Operation, Pred, Query, QueryOpts,
    QueryPage, Receipt, Select, SignedTx, StoreError, TxHash, TxInfo, U256,
};
use futures_util::future::{join_all, try_join, try_join_all};
use serde::{Deserialize, Serialize};

use super::{View, Want};
use crate::observe::{b32, kind};
use crate::repo::Repo;
use crate::state::{State, read_json, write_json};
use crate::{Result, SyncError};

/// Seconds a full sync's pages may be replayed behind an unchanged probe.
pub const WINDOW_ENV: &str = "CHARTER_PROBE_WINDOW_SECS";
const DEFAULT_WINDOW: u64 = 600;
const FILE: &str = "view-memo.json";
/// Recorded queries sent at once by a speculative sync.
const PREFETCH_PARALLEL: usize = 32;

/// How long after a full sync the probe may stand in for it ([`WINDOW_ENV`], default 600 s).
pub fn window() -> u64 {
    std::env::var(WINDOW_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_WINDOW)
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `QueryOpts` as stored (the store type has no serde).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Opts {
    limit: u32,
    offset: u64,
    flags: [bool; 8],
    attrs: Option<Vec<String>>,
    all_attrs: bool,
}

impl From<&QueryOpts> for Opts {
    fn from(o: &QueryOpts) -> Self {
        let s = &o.select;
        Self {
            limit: o.limit,
            offset: o.offset,
            flags: [
                s.key,
                s.owner,
                s.creator,
                s.created_at,
                s.updated_at,
                s.expires_at,
                s.creation_flags,
                s.count,
            ],
            attrs: match &s.attrs {
                Attrs::Named(n) => Some(n.clone()),
                _ => None,
            },
            all_attrs: s.attrs == Attrs::All,
        }
    }
}

impl From<&Opts> for QueryOpts {
    fn from(o: &Opts) -> Self {
        let [
            key,
            owner,
            creator,
            created_at,
            updated_at,
            expires_at,
            creation_flags,
            count,
        ] = o.flags;
        let attrs = match (&o.attrs, o.all_attrs) {
            (Some(n), _) => Attrs::Named(n.clone()),
            (None, true) => Attrs::All,
            (None, false) => Attrs::None,
        };
        Self {
            limit: o.limit,
            offset: o.offset,
            select: Select {
                key,
                owner,
                creator,
                created_at,
                updated_at,
                expires_at,
                creation_flags,
                attrs,
                count,
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Page {
    block: u64,
    count: Option<u64>,
    rows: Vec<EntityData>,
}

impl From<&QueryPage> for Page {
    fn from(p: &QueryPage) -> Self {
        Self {
            block: p.block_number,
            count: p.count,
            rows: p.rows.clone(),
        }
    }
}

impl From<Page> for QueryPage {
    fn from(p: Page) -> Self {
        Self {
            block_number: p.block,
            count: p.count,
            rows: p.rows,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    query: Query,
    opts: Opts,
    page: Page,
}

/// The probe: `(key, $updatedAt)` of the repo's control-plane entities and of its ops.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Probe {
    block: u64,
    rows: Vec<(B256, u64)>,
    ops: Vec<(B256, u64)>,
}

/// What the last full sync of a repo read.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Memo {
    /// Unix seconds of the full sync.
    at: u64,
    /// The repo pinned a program (its op list is then part of the probe).
    pinned: bool,
    probe: Option<Probe>,
    entries: Vec<Entry>,
}

impl Memo {
    fn load(repo: &Repo) -> Option<Self> {
        read_json(&repo.dir.join(FILE)).ok().flatten()
    }

    /// The probe of a recent full sync, which its pages may be replayed behind.
    fn replayable(&self) -> Option<&Probe> {
        let p = self.probe.as_ref()?;
        (now().saturating_sub(self.at) < window()).then_some(p)
    }

    /// Replayable pages: only those read at or after the probe's block, so nothing a page
    /// missed can have happened before the probe (the rest are sent live).
    fn pages(&self, probe: &Probe) -> BTreeMap<String, QueryPage> {
        let mut out = BTreeMap::new();
        for e in self.entries.iter().filter(|e| e.page.block >= probe.block) {
            out.entry(key(&e.query, &QueryOpts::from(&e.opts)))
                .or_insert_with(|| e.page.clone().into());
        }
        out
    }
}

/// Forgets the recorded pages of `repo`, so its next sync is a full one.
pub fn expire(repo: &Repo) -> Result<()> {
    match std::fs::remove_file(repo.dir.join(FILE)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(SyncError::io(repo.dir.join(FILE).display(), e))
        }
        _ => Ok(()),
    }
}

fn trace(path: &str, detail: impl FnOnce() -> String) {
    if charter_store::trace::enabled() {
        eprintln!("chart-trace: sync path {path} {}", detail());
    }
}

fn key(q: &Query, o: &QueryOpts) -> String {
    format!("{}\u{0}{o:?}", q.render().unwrap_or_default())
}

fn probe_filter(rid: &Rid) -> Pred {
    let kinds = [Kind::Id, Kind::IdAcc, Kind::Bind, Kind::Anchor, Kind::Head].map(kind);
    Pred::And(vec![b32("ag.rid", rid), Pred::Or(kinds.to_vec())])
}

/// A probe list: the first page carries the count, the other pages go out together.
async fn probe_rows<A: Arkiv>(a: &A, filter: Pred) -> Result<(u64, Vec<(B256, u64)>)> {
    let q = Query::from(filter);
    let opts = |offset| QueryOpts {
        limit: MAX_LIMIT,
        offset,
        select: Select {
            key: true,
            updated_at: true,
            count: true,
            ..Select::default()
        },
    };
    let first = a.query(&q, &opts(0)).await?;
    let count = first
        .count
        .ok_or_else(|| SyncError::coded(charter_core::ErrorCode::Rpc, "probe count missing"))?;
    let offsets = (u64::from(MAX_LIMIT)..count).step_by(MAX_LIMIT as usize);
    let q = &q;
    let rest = try_join_all(offsets.map(|o| {
        let op = opts(o);
        async move { a.query(q, &op).await }
    }))
    .await?;
    let mut block = first.block_number;
    let mut rows = vec![];
    for p in std::iter::once(first).chain(rest) {
        block = block.max(p.block_number);
        rows.extend(p.rows.into_iter().map(|r| {
            let up = r.updated_at.or(r.created_at).unwrap_or(0);
            (r.key.unwrap_or_default(), up)
        }));
    }
    rows.sort();
    Ok((block, rows))
}

/// The op list is read only for a repo the last full sync saw pinned (`with_ops`): it is
/// compared only then. Both lists go out together, so it costs no round trip of its own
/// unless it alone runs past one page. A repo pinned since its memo was written has an
/// empty op list in it, so its next probe differs and that sync is a full one.
async fn probe<A: Arkiv>(a: &A, rid: &Rid, with_ops: bool) -> Result<Probe> {
    let ops = async {
        match with_ops {
            true => probe_rows(a, Pred::And(vec![b32("ag.rid", rid), kind(Kind::Cob)])).await,
            false => Ok((0, vec![])),
        }
    };
    let ((block, rows), (ops_block, ops)) = try_join(probe_rows(a, probe_filter(rid)), ops).await?;
    Ok(Probe {
        block: block.max(ops_block),
        rows,
        ops,
    })
}

/// An `Arkiv` that serves recorded pages once each and records every page it returns.
pub struct Served<'a, A> {
    inner: &'a A,
    ready: Mutex<BTreeMap<String, QueryPage>>,
    log: Mutex<Vec<Entry>>,
    /// The block the probe was read at, answered as the head (it is this sync's head).
    head: Option<u64>,
}

impl<'a, A: Arkiv> Served<'a, A> {
    fn new(inner: &'a A, ready: BTreeMap<String, QueryPage>) -> Self {
        Self {
            inner,
            ready: Mutex::new(ready),
            log: Mutex::new(vec![]),
            head: None,
        }
    }

    fn at(self, probe: Option<&Probe>) -> Self {
        Self {
            head: probe.map(|p| p.block),
            ..self
        }
    }

    fn take(&self, k: &str) -> Option<QueryPage> {
        self.ready.lock().ok()?.remove(k)
    }

    fn record(&self, q: &Query, o: &QueryOpts, p: &QueryPage) {
        if let Ok(mut log) = self.log.lock() {
            log.push(Entry {
                query: q.clone(),
                opts: o.into(),
                page: p.into(),
            });
        }
    }

    fn entries(&self) -> Vec<Entry> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

impl<A: Arkiv> Arkiv for Served<'_, A> {
    async fn chain_id(&self) -> std::result::Result<u64, StoreError> {
        self.inner.chain_id().await
    }
    async fn head(&self) -> std::result::Result<u64, StoreError> {
        match self.head {
            Some(h) => Ok(h),
            None => self.inner.head().await,
        }
    }
    async fn base_fee(&self) -> std::result::Result<u128, StoreError> {
        self.inner.base_fee().await
    }
    async fn head_fee(&self) -> std::result::Result<(u64, u128), StoreError> {
        self.inner.head_fee().await
    }
    fn heads_ws(&self) -> Option<charter_store::heads::WsConfig> {
        self.inner.heads_ws()
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> std::result::Result<QueryPage, StoreError> {
        // Counts sit inside nonce brackets (completeness): always live, never recorded.
        if *o == QueryOpts::count_only() {
            return self.inner.query(q, o).await;
        }
        let page = match self.take(&key(q, o)) {
            Some(p) => p,
            None => self.inner.query(q, o).await?,
        };
        self.record(q, o, &page);
        Ok(page)
    }
    async fn entity_nonce(&self, c: Address) -> std::result::Result<u64, StoreError> {
        self.inner.entity_nonce(c).await
    }
    async fn balance(&self, a: Address) -> std::result::Result<U256, StoreError> {
        self.inner.balance(a).await
    }
    async fn tx_count(&self, a: Address) -> std::result::Result<u64, StoreError> {
        self.inner.tx_count(a).await
    }
    async fn tx_count_mined(&self, a: Address) -> std::result::Result<u64, StoreError> {
        self.inner.tx_count_mined(a).await
    }
    async fn transaction(&self, h: TxHash) -> std::result::Result<Option<TxInfo>, StoreError> {
        self.inner.transaction(h).await
    }
    async fn mined_tx(
        &self,
        h: TxHash,
    ) -> std::result::Result<Option<charter_store::MinedTx>, StoreError> {
        self.inner.mined_tx(h).await
    }
    async fn block_txs(
        &self,
        n: u64,
    ) -> std::result::Result<Vec<charter_store::MinedTx>, StoreError> {
        self.inner.block_txs(n).await
    }
    async fn tx_at(
        &self,
        n: u64,
        i: u32,
    ) -> std::result::Result<Option<charter_store::MinedTx>, StoreError> {
        self.inner.tx_at(n, i).await
    }
    fn seals(&self) -> bool {
        self.inner.seals()
    }
    fn open_rows(&self, rows: Vec<EntityData>) -> Vec<EntityData> {
        self.inner.open_rows(rows)
    }
    async fn estimate(
        &self,
        from: Address,
        ops: &[Operation],
    ) -> std::result::Result<u64, StoreError> {
        self.inner.estimate(from, ops).await
    }
    async fn send(&self, tx: SignedTx) -> std::result::Result<TxHash, StoreError> {
        self.inner.send(tx).await
    }
    async fn receipt(&self, h: TxHash) -> std::result::Result<Option<Receipt>, StoreError> {
        self.inner.receipt(h).await
    }
    fn seal_ops(&self, ops: Vec<Operation>) -> std::result::Result<Vec<Operation>, StoreError> {
        self.inner.seal_ops(ops)
    }
}

/// One sync's reads: the backend to read through, and what to record afterwards.
pub struct Session<'a, A> {
    pub backend: Served<'a, A>,
    /// The probe read with this sync's pages (`None`: replayed, or not probed).
    probe: Option<Probe>,
    pub replayed: bool,
}

impl<A: Arkiv> Session<'_, A> {
    /// Records this full sync's pages for the next sync (a replayed sync keeps the old
    /// record, so the window counts from the last full sync).
    pub fn save(&self, repo: &Repo, view: &View) -> Result<()> {
        if self.replayed {
            return Ok(());
        }
        let memo = Memo {
            at: now(),
            pinned: view.identity.head().body.program.is_some(),
            probe: self.probe.clone(),
            entries: self.backend.entries(),
        };
        write_json(&repo.dir.join(FILE), &memo)
    }
}

/// Builds the verified view of `repo`: replayed behind an unchanged probe, else from the
/// last sync's queries sent at once, else plainly (module docs).
pub async fn build<'a, A: Arkiv>(
    a: &'a A,
    repo: &Repo,
    state: &State,
    want: &Want,
) -> Result<(View, Session<'a, A>)> {
    let rid = repo.rid;
    let memo = Memo::load(repo).unwrap_or_default();
    // A probe that differed is reused by the speculative build: it was read before the
    // prefetch, so every prefetched page is read no earlier than it.
    let mut earlier = None;
    if let Some(old) = memo.replayable() {
        let mut pages = memo.pages(old);
        let (q, o) = identity_query(&rid);
        // A first clone reads its identity rows through the private-repo check, so its
        // record may lack them: read them live, once, with the probe.
        let missing = !pages.contains_key(&key(&q, &o));
        let (fresh, ident) = futures_util::future::join(probe(a, &rid, memo.pinned), async {
            match missing {
                true => a.query(&q, &o).await.ok(),
                false => None,
            }
        })
        .await;
        let same = fresh
            .as_ref()
            .is_ok_and(|p| p.rows == old.rows && (!memo.pinned || p.ops == old.ops));
        trace("probe", || {
            let n = memo.entries.len();
            format!("same={same} replayable_pages={} of {n}", pages.len())
        });
        if same {
            if let Some(p) = ident {
                pages.insert(key(&q, &o), p);
            }
            let backend = Served::new(a, pages).at(fresh.as_ref().ok());
            if let Ok(view) = super::build(&backend, &rid, state, want).await {
                trace("replay", String::new);
                let s = Session {
                    backend,
                    probe: None,
                    replayed: true,
                };
                return Ok((view, s));
            }
        }
        earlier = fresh.ok();
    }
    if !memo.entries.is_empty() {
        // Else the probe goes out with the prefetch; a page read at an earlier block than
        // the probe is never replayed (`Memo::pages`).
        let probed = async {
            match earlier {
                Some(p) => Ok(p),
                None => probe(a, &rid, memo.pinned).await,
            }
        };
        let (p, pages) = futures_util::future::join(probed, prefetch(a, &rid, &memo)).await;
        let backend = Served::new(a, pages).at(p.as_ref().ok());
        if let Ok(view) = super::build(&backend, &rid, state, want).await {
            trace("speculative", String::new);
            let s = Session {
                backend,
                probe: p.ok(),
                replayed: false,
            };
            return Ok((view, s));
        }
    }
    let backend = Served::new(a, BTreeMap::new());
    let (p, view) = futures_util::future::join(
        probe(a, &rid, memo.pinned),
        super::build(&backend, &rid, state, want),
    )
    .await;
    trace("plain", || format!("probed={}", p.is_ok()));
    let s = Session {
        backend,
        probe: p.ok(),
        replayed: false,
    };
    Ok((view?, s))
}

/// The first page of the view's identity read ([`super::identity_filter`] through
/// `observe::all`).
fn identity_query(rid: &Rid) -> (Query, QueryOpts) {
    let opts = QueryOpts {
        limit: MAX_LIMIT,
        offset: 0,
        select: Select {
            count: true,
            ..crate::observe::full_select()
        },
    };
    (Query::from(super::identity_filter(rid)), opts)
}

/// Every recorded query of `memo` sent again, at most [`PREFETCH_PARALLEL`] at a time;
/// a query that fails here is just sent again by the build.
async fn prefetch<A: Arkiv>(a: &A, rid: &Rid, memo: &Memo) -> BTreeMap<String, QueryPage> {
    let mut wanted: BTreeMap<String, (Query, QueryOpts)> = BTreeMap::new();
    for e in &memo.entries {
        let o = QueryOpts::from(&e.opts);
        wanted
            .entry(key(&e.query, &o))
            .or_insert_with(|| (e.query.clone(), o));
    }
    let (q, o) = identity_query(rid);
    wanted.entry(key(&q, &o)).or_insert((q, o));
    let wanted: Vec<_> = wanted.into_iter().collect();
    let mut out = BTreeMap::new();
    for group in wanted.chunks(PREFETCH_PARALLEL) {
        let pages = join_all(group.iter().map(|(_, (q, o))| a.query(q, o))).await;
        for ((k, _), p) in group.iter().zip(pages) {
            if let Ok(p) = p {
                out.insert(k.clone(), p);
            }
        }
    }
    out
}
