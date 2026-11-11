//! What the sync loop spends of the RPC quota (docs/p6/ui.md 4.2): a simulated hour of
//! one-second ticks against the fake store, counting every `arkiv_query`, which Rabdi bills
//! 10 units of a per-IP 10,000 (docs/p0/arkiv-limits.md).

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use charter_core::{Rid, ZERO};
use charter_store::{
    Address, Arkiv, Operation, Query, QueryOpts, QueryPage, Quota, Receipt, SignedTx, StoreError,
    TxHash, TxInfo, U256,
};
use charter_ui::pace::Pacer;
use charter_ui::security::Auth;
use charter_ui::sync_loop::{Kind, Outcome, round};
use charter_ui::{App, Shared, cache, router};
use common::*;
use serde_json::json;

const HOUR: u64 = 3600;

/// The fake store behind a query counter, with an injectable quota report and 429.
struct Counting {
    net: Net,
    queries: AtomicU64,
    quota: Mutex<Option<Quota>>,
    /// The next query is refused with this `Retry-After`.
    refuse: Mutex<Option<Duration>>,
}

impl Counting {
    fn queries(&self) -> u64 {
        self.queries.load(Ordering::SeqCst)
    }
}

impl Arkiv for Counting {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        self.net.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.net.head().await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.net.base_fee().await
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> Result<QueryPage, StoreError> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        if let Some(d) = self.refuse.lock().expect("lock").take() {
            return Err(StoreError::RateLimited {
                retry_after: Some(d),
            });
        }
        self.net.query(q, o).await
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        self.net.entity_nonce(c).await
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.net.balance(a).await
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.net.tx_count(a).await
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.net.tx_count_mined(a).await
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.net.transaction(h).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.net.mined_tx(h).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<charter_store::MinedTx>, StoreError> {
        self.net.block_txs(n).await
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.net.estimate(from, ops).await
    }
    fn quota(&self) -> Option<Quota> {
        self.quota.lock().expect("lock").clone()
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        self.net.send(tx).await
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        self.net.receipt(h).await
    }
}

struct Sim {
    c: Counting,
    alice: Peer,
    rid: Rid,
    shared: Arc<Shared>,
    pacer: Pacer,
    t0: Instant,
    /// Queries of one full round of this repo with nothing new.
    round_cost: u64,
    /// Feed a `newHeads` block every 2 s, as Rabdi's socket does.
    heads: bool,
}

/// A repo `chart ui` has synced before (`view.json` cached), unless `fresh`.
async fn sim(seed: u8, fresh: bool) -> Sim {
    let c = Counting {
        net: Net::new(),
        queries: AtomicU64::new(0),
        quota: Mutex::new(None),
        refuse: Mutex::new(None),
    };
    let alice = Peer::new(&c.net, seed);
    let rid = init(&c.net, &alice, "paced").await;
    let mut round_cost = 0;
    if !fresh {
        round(&c, &alice.cfg, &rid).await.expect("round");
        // A round that finds the lock held stops early; measure one that ran.
        loop {
            c.queries.store(0, Ordering::SeqCst);
            let r = round(&c, &alice.cfg, &rid).await.expect("round");
            if matches!(r, Outcome::Synced(_)) {
                break;
            }
        }
        round_cost = c.queries.swap(0, Ordering::SeqCst);
    }
    Sim {
        c,
        alice,
        rid,
        shared: Arc::new(Shared::default()),
        pacer: Pacer::default(),
        t0: Instant::now(),
        round_cost,
        heads: false,
    }
}

impl Sim {
    /// Ticks once a second over `[from, to)` seconds; returns the queries spent.
    async fn run(&mut self, from: u64, to: u64) -> u64 {
        let before = self.c.queries();
        for s in from..to {
            let now = self.t0 + Duration::from_secs(s);
            if self.heads {
                self.pacer.head(Some(s / 2), now);
            }
            let cfg = &self.alice.cfg;
            self.pacer.tick(&self.c, cfg, &self.shared, now).await;
        }
        self.c.queries() - before
    }

    fn view_cached(&self) -> bool {
        self.alice.repo(&self.rid).dir.join(cache::VIEW).exists()
    }

    async fn verify_panel(&self) -> String {
        let home = self.alice.home.path().to_path_buf();
        let app = Arc::new(App::new(home, self.shared.clone(), CSRF.into()));
        let r = router(app, Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())));
        let hex = charter_sync::ids::hex32(&self.rid);
        send(r, req(&format!("/r/{hex}/verify"))).await.text()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn with_no_page_open_only_the_first_sync_of_an_uncached_repo_runs() {
    let mut s = sim(40, true).await;
    s.shared.want(s.rid);
    let first = s.run(0, 1).await;
    assert!(first > 0 && s.view_cached(), "chart ui <rid> syncs at once");
    let hour = s.run(1, HOUR).await;
    eprintln!("no page open: first sync {first} queries, then {hour} in the hour");
    assert_eq!(hour, 0, "no page open, no queries");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_idle_repo_costs_under_100_queries_an_hour() {
    let mut s = sim(41, false).await;
    let _other = s.shared.viewer([9; 32]); // a page of another repo is open
    let q = s.run(0, HOUR).await;
    eprintln!("idle repo: {q} queries/hour (one round = {})", s.round_cost);
    assert!(q < 100, "{q}");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_repo_on_screen_with_no_changes_is_probed_every_ten_seconds() {
    let mut s = sim(42, false).await;
    s.shared.focus(s.rid);
    let _v = s.shared.viewer(s.rid);
    let q = s.run(0, HOUR).await;
    eprintln!(
        "on screen, no changes: {q} queries/hour (one round = {})",
        s.round_cost
    );
    // One round when the page opens (the cache may be stale), then one-query probes.
    let probes = HOUR / 10;
    assert!(q <= s.round_cost + 2 + probes, "{q}");
    assert!(q >= probes, "{q}");
}

#[tokio::test(flavor = "multi_thread")]
async fn new_heads_alone_never_trigger_more_than_the_probe() {
    let mut s = sim(46, false).await;
    s.heads = true;
    s.shared.focus(s.rid);
    let _v = s.shared.viewer(s.rid);
    let q = s.run(0, HOUR).await;
    eprintln!("on screen with newHeads, no changes: {q} queries/hour");
    assert!(q <= s.round_cost + 2 + HOUR / 10, "{q}");
}

#[tokio::test(flavor = "multi_thread")]
async fn one_change_on_screen_costs_one_round_and_reaches_the_page() {
    let mut s = sim(43, false).await;
    s.shared.focus(s.rid);
    let _v = s.shared.viewer(s.rid);
    let mut events = s.shared.subscribe();
    let before = s.run(0, HOUR / 2).await;
    while events.try_recv().is_ok() {}
    let body = json!({"title": "new"});
    post(
        &s.c.net,
        &s.alice,
        &s.rid,
        "xyz.arkiv.issue",
        "create",
        ZERO,
        body,
    )
    .await;
    let changed = Instant::now();
    let after = s.run(HOUR / 2, HOUR).await;
    let q = before + after;
    eprintln!(
        "on screen, one change: {q} queries/hour (one round = {})",
        s.round_cost
    );
    let mut kinds = vec![];
    while let Ok(e) = events.try_recv() {
        kinds.push(e.kind);
    }
    assert!(kinds.contains(&Kind::Op), "the new issue reached the page");
    assert!(q <= 2 * s.round_cost + 4 + HOUR / 10, "{q}");
    assert!(
        changed.elapsed() < Duration::from_secs(60),
        "simulated, not slept"
    );
}

/// A format v3 push (what `chart` sends) from another device of alice, then `[from, to)` of
/// ticks; the page must hear of it. Returns the queries.
async fn push_until_seen(
    s: &mut Sim,
    from: u64,
    to: u64,
    dev: &Peer,
    work: &std::path::Path,
    r: &str,
) -> u64 {
    let mut events = s.shared.subscribe();
    push2(&s.c.net, dev, &s.rid, work, &[r])
        .await
        .expect("push");
    let q = s.run(from, to).await;
    let mut kinds = vec![];
    while let Ok(e) = events.try_recv() {
        kinds.push(e.kind);
    }
    assert!(
        kinds.contains(&Kind::Push),
        "{r} never reached the open page"
    );
    q
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refs_only_push_from_another_device_reaches_the_open_page() {
    let mut s = sim(47, false).await;
    s.shared.focus(s.rid);
    let _v = s.shared.viewer(s.rid);
    let dev = s.alice.second_device(&s.c.net);
    let dir = tempfile::tempdir().expect("work");
    let work = work_repo(dir.path(), "w");
    commit(&work, &[("a.txt", b"a\n")], "first");
    s.run(0, 30).await;
    // Its first push creates the device's head; the second only patches it in place.
    push_until_seen(&mut s, 30, 60, &dev, &work, "refs/heads/main").await;
    git(&work, &["branch", "side"]);
    let quiet = s.run(60, 120).await;
    let q = push_until_seen(&mut s, 120, 150, &dev, &work, "refs/heads/side").await;
    eprintln!(
        "refs-only push: {quiet} queries in 60 s with no change, {q} in the 30 s that caught it (one round = {})",
        s.round_cost
    );
    assert!(quiet <= 6, "a probe every 10 s, one query each: {quiet}");
    let branches = s.alice.repo(&s.rid).dir.join(cache::VIEW);
    let view = std::fs::read_to_string(branches).expect("view");
    assert!(
        view.contains("refs/heads/side"),
        "the round recorded the new branch"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_draining_quota_pauses_background_rounds_and_the_page_says_so() {
    let mut s = sim(44, false).await;
    s.shared.focus(s.rid);
    let _v = s.shared.viewer(s.rid);
    *s.c.quota.lock().expect("lock") = Some(Quota {
        limit: Some(10_000),
        remaining: Some(1_500),
        reset: Some(1_800),
        policy: None,
        used_percent: None,
        refused: false,
        retry_after: None,
        at: Instant::now(),
    });
    assert_eq!(s.run(0, 600).await, 0, "85 % used: no queries");
    let panel = s.verify_panel().await;
    assert!(
        panel.contains("paused: RPC quota, resumes in 30 min"),
        "{panel}"
    );
    *s.c.quota.lock().expect("lock") = None;
    assert_eq!(s.run(600, 1800).await, 0, "paused until the window resets");
    assert!(s.run(1800, 1802).await > 0, "then resumes");
    assert!(!s.verify_panel().await.contains("paused"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_429_is_never_retried_before_its_retry_after() {
    let mut s = sim(45, false).await;
    s.shared.focus(s.rid);
    let _v = s.shared.viewer(s.rid);
    *s.c.refuse.lock().expect("lock") = Some(Duration::from_secs(120));
    assert_eq!(
        s.run(0, 1).await,
        1,
        "the refused probe, and nothing after it"
    );
    assert!(
        s.verify_panel()
            .await
            .contains("paused: RPC quota, resumes in 2 min")
    );
    assert_eq!(s.run(1, 120).await, 0, "no query before Retry-After");
    assert!(s.run(120, 121).await > 0, "then the loop carries on");
}
