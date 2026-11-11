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
    // Setup, plan (with entityNonce), send, the receipt poll, and the re-check (the
    // peer, its anchors and the device heads read together); the 5-round re-check made it 9.
    assert!(counting.rounds() <= 5, "{} round trips", counting.rounds());
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

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_read_of_this_identity_reads_peer_anchors_and_heads_together() {
    let net = Net::new();
    let alice = Peer::new(&net, 3);
    let rid = init(&net, &alice, "lean-plan").await;
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
    let state = charter_sync::state::State::load(&repo.state_path()).expect("state");
    counting.reset();
    // A plan that does not reuse the sync's view reads this identity's heads afresh.
    Box::pin(hp.plan(&state)).await.expect("plan");
    assert!(
        counting.rounds() <= 2,
        "peer, anchors and device heads in 2 round trips, not 4: {}",
        counting.rounds()
    );
}

/// Blocks after `since` holding a value transfer from `from` to `to`.
async fn transfer_blocks(net: &Net, since: u64, from: Address, to: Address) -> Vec<u64> {
    let head = net.mem.head().await.expect("head");
    let mut out = vec![];
    for n in since + 1..=head {
        for t in net.mem.block_txs(n).await.expect("block") {
            if t.from == from && t.to == Some(to) && t.input.is_empty() {
                out.push(n);
            }
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn a_first_push_tops_up_in_one_block_and_creates_its_manifest_in_the_head_tx() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "first-push").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", &noise(1, 300_000))], "feat: one");
    let since = net.mem.head().await.expect("head");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("first push");

    let wallet = alice.keys.wallet().address();
    let dev = charter_sync::device::device_keys(alice.home.path()).expect("dev");
    let c_d = transfer_blocks(&net, since, wallet, dev.control_address(&rid)).await;
    let control = transfer_blocks(&net, since, wallet, alice.keys.control_address(&rid)).await;
    assert_eq!(c_d.len(), 1, "one top-up of C_d: {c_d:?}");
    assert!(
        control.iter().all(|b| c_d.contains(b)),
        "the control top-up shares C_d's block: {control:?} vs {c_d:?}"
    );

    let kind = |o: &charter_core::Observed, k: &str| {
        o.attr("ag.kind") == Some(&charter_core::AttrValue::Str(k.into()))
    };
    let bulk = created_by(&net, dev.bulk_address(&rid)).await;
    let last_chunk = bulk
        .iter()
        .filter(|o| kind(o, "chunk"))
        .map(|o| o.block)
        .max()
        .expect("a snapshot was sent");
    assert!(
        !bulk.iter().any(|o| kind(o, "manifest")),
        "no manifest from B_d"
    );
    // Head-model 13.2: the manifest rides in the head tx, a block after its chunks (so its
    // index terms are not charged as new twice), and replaces the marker.
    let control = created_by(&net, dev.control_address(&rid)).await;
    let blocks = |k: &str| -> Vec<u64> {
        control
            .iter()
            .filter(|o| kind(o, k))
            .map(|o| o.block)
            .collect()
    };
    let (manifest, head) = (blocks("manifest"), blocks("head"));
    assert_eq!(manifest.len(), 1, "one manifest from C_d");
    assert_eq!(manifest, head, "the manifest is in the head tx");
    assert!(manifest[0] > last_chunk, "a block after the chunks");
    assert_eq!(
        control.len(),
        2,
        "no marker beside the manifest: {control:?}"
    );
}
