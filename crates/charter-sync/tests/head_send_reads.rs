//! The reads a format v2 push makes (docs/p5/speed-and-cost.md 3.15: every RPC is about
//! 0.3 s on Rabdi, so sequential round trips are the push's latency): a small push prices
//! its head patch without `eth_estimateGas`, takes the landing's tx index from the
//! receipt, and stays within a fixed number of round trips.

mod common;

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

use charter_store::{
    Address, Arkiv, MinedTx, Operation, Query, QueryOpts, QueryPage, Receipt, SignedTx, StoreError,
    TxHash, TxInfo, U256,
};
use charter_sync::HeadPush;
use common::*;

/// `Net` counting calls by method, and round trips: calls that overlap (each yields
/// once, as a network call would) share one; a call starting with none in flight opens one.
struct Counting<'a> {
    net: &'a Net,
    calls: Mutex<BTreeMap<&'static str, usize>>,
    inflight: AtomicUsize,
    rounds: AtomicUsize,
}

impl<'a> Counting<'a> {
    fn new(net: &'a Net) -> Self {
        Self {
            net,
            calls: Mutex::default(),
            inflight: AtomicUsize::new(0),
            rounds: AtomicUsize::new(0),
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

    fn reset(&self) {
        self.calls.lock().expect("lock").clear();
        self.rounds.store(0, SeqCst);
    }

    fn rounds(&self) -> usize {
        self.rounds.load(SeqCst)
    }
    fn count(&self, m: &str) -> usize {
        self.calls
            .lock()
            .expect("lock")
            .get(m)
            .copied()
            .unwrap_or(0)
    }
}

impl Arkiv for Counting<'_> {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        // Verified once per process by the real client, inside its first batch.
        self.net.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.rpc("head", self.net.head()).await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.rpc("base_fee", self.net.base_fee()).await
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> Result<QueryPage, StoreError> {
        self.rpc("query", self.net.query(q, o)).await
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        self.rpc("entity_nonce", self.net.entity_nonce(c)).await
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.rpc("balance", self.net.balance(a)).await
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.rpc("tx_count", self.net.tx_count(a)).await
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.rpc("tx_count_mined", self.net.tx_count_mined(a)).await
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.rpc("estimate", self.net.estimate(from, ops)).await
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        self.rpc("send", self.net.send(tx)).await
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        self.rpc("receipt", self.net.receipt(h)).await
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.rpc("transaction", self.net.transaction(h)).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<MinedTx>, StoreError> {
        self.rpc("mined_tx", self.net.mined_tx(h)).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<MinedTx>, StoreError> {
        self.rpc("block_txs", self.net.block_txs(n)).await
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_small_push_sends_its_head_patch_without_an_estimate_or_a_tx_lookup() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "lean-head").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"one")], "feat: one");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("first push creates the head");

    commit(&work, &[("a", b"two")], "feat: two");
    let counting = Counting::new(&net);
    let repo = alice.repo(&rid);
    let view = Box::pin(charter_sync::sync(&counting, &repo, &alice.want()))
        .await
        .expect("sync");
    let hp = HeadPush::prepare(
        &counting,
        &repo,
        &alice.cfg,
        &alice.keys,
        view,
        &charter_sync::git::Git::work(Some(&work)),
        &[upd("refs/heads/main")],
    )
    .expect("prepare");
    counting.reset();
    Box::pin(hp.run()).await.expect("small push");
    // Setup, plan (with entityNonce), send, the receipt poll, and the 3-round re-check;
    // The 5-round re-check made it 9.
    assert!(counting.rounds() <= 6, "{} round trips", counting.rounds());
    assert_eq!(
        counting.count("estimate"),
        0,
        "the head patch is not estimated"
    );
    assert_eq!(
        counting.count("mined_tx"),
        0,
        "the tx index comes from the receipt"
    );

    let state = charter_sync::state::State::load(&repo.state_path()).expect("state");
    let (tx, landing) = state
        .landings
        .values()
        .map(|l| (l.tx, *l))
        .max_by_key(|(_, l)| l.block)
        .expect("a landing");
    let mined = net.mem.mined_tx(tx.into()).await.expect("rpc").expect("tx");
    assert_eq!(
        landing.index, mined.index,
        "the recorded index is the real one"
    );
}
