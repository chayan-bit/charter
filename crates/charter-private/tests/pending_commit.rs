//! A membership change interrupted after its tx went out is never dropped while that tx can
//! still land, nor replaced once it landed (even when the job split or a revert moved the
//! commit to a later nonce), so a retry cannot fork the admin off the group; nor kept
//! forever once any tx of the control address took its nonce, from this machine or another
//! of the identity, with or without receipts (docs/p2/private-repos.md, "Pending commits").

mod common;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use charter_core::Rid;
use charter_private::Role;
use charter_store::{
    Address, Arkiv, Operation, Query, QueryOpts, QueryPage, Receipt, SignedTx, StoreError, TxHash,
    TxInfo, U256,
};
use common::sec::*;
use common::*;

#[test]
fn an_interrupted_removal_retried_before_it_lands_forks_the_admin() {
    run(async {
        let net = Net::new();
        let [alice, carol, bob, dan] = [1, 3, 2, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &carol, &rid, Role::Admin).await;
        invite(&net, &alice, &bob, &rid, Role::Write).await;
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        for p in [&carol, &bob, &dan] {
            ring(&net, p, &rid).await.expect("join");
        }
        net.mine(100);
        let e0 = current(&ring(&net, &alice, &rid).await.expect("alice"));

        net.hold_mining(true);
        let bob_nid = bob.keys.nid();
        let first = charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob_nid);
        tokio::select! {
            r = first => panic!("finished while mining is held: {r:?}"),
            () = pool_holds(&net, 1) => {} // the user hits Ctrl-C: the future is dropped
        }
        assert_eq!(net.mem.pending(), 1, "the first removal is in the pool");

        let dan_nid = dan.keys.nid();
        let retry = charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &dan_nid);
        let retry = tokio::time::timeout(std::time::Duration::from_secs(5), retry).await;
        let e = retry
            .expect("answers at once")
            .expect_err("refused while the first can still land");
        assert_eq!(e.code(), charter_core::ErrorCode::ConcurrentPush, "{e}");
        assert_eq!(net.mem.pending(), 1, "the refused retry sent nothing");

        net.hold_mining(false);
        net.mine(1);
        let c = ring(&net, &carol, &rid).await.expect("carol");
        let a = ring(&net, &alice, &rid).await.expect("alice");
        assert_eq!(current(&c), e0 + 1, "the group took the first removal");
        assert_eq!(current(&a), current(&c), "alice is on the group's epoch");
        assert_eq!(
            a.epochs.get(&(e0 + 1)),
            c.epochs.get(&(e0 + 1)),
            "alice holds the group's key"
        );

        // The retry now goes through on top, and everyone stays on one epoch.
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &dan_nid)
            .await
            .expect("retry");
        let (a, c) = (
            ring(&net, &alice, &rid).await.expect("alice"),
            ring(&net, &carol, &rid).await.expect("carol"),
        );
        assert_eq!((current(&a), current(&c)), (e0 + 2, e0 + 2));
        assert_eq!(a.epochs.get(&(e0 + 2)), c.epochs.get(&(e0 + 2)));
    });
}

#[test]
fn a_removal_whose_tx_was_dropped_is_replaced_at_its_nonce_and_the_retry_goes_through() {
    run(async {
        let net = Net::new();
        let [alice, carol, bob, dan] = [1, 3, 2, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &carol, &rid, Role::Admin).await;
        invite(&net, &alice, &bob, &rid, Role::Write).await;
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        for p in [&carol, &bob, &dan] {
            ring(&net, p, &rid).await.expect("join");
        }
        let e0 = current(&ring(&net, &alice, &rid).await.expect("alice"));
        let control = alice.keys.control_address(&rid);
        let n0 = net.tx_count_mined(control).await.expect("nonce");

        // The commit tx is accepted but never mined (evicted from the pool).
        net.drop_sends_from(control, 0);
        let bob_nid = bob.keys.nid();
        let first = charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob_nid);
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), first).await;
        assert!(
            !matches!(first, Ok(Ok(()))),
            "the dropped removal did not land: {first:?}"
        );
        net.heal();

        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &dan.keys.nid())
            .await
            .expect("the retry replaces the lost tx at its nonce, then goes through");
        assert!(net.tx_count_mined(control).await.expect("nonce") >= n0 + 2);
        let (a, c) = (
            ring(&net, &alice, &rid).await.expect("alice"),
            ring(&net, &carol, &rid).await.expect("carol"),
        );
        assert_eq!(
            (current(&a), current(&c)),
            (e0 + 1, e0 + 1),
            "one removal: dan's"
        );
        assert_eq!(a.epochs.get(&(e0 + 1)), c.epochs.get(&(e0 + 1)));
        let (_, list) = charter_private::members(&net, &carol.cfg, &carol.keys, &rid)
            .await
            .expect("members");
        assert!(
            list.iter().any(|m| m.0 == bob.keys.nid()),
            "bob is still in: {list:?}"
        );
        assert!(
            !list.iter().any(|m| m.0 == dan.keys.nid()),
            "dan is out: {list:?}"
        );
    });
}

/// `Net` behind a flaky edge: sends from one address after the first `pass` are kept back
/// instead of reaching the node; once armed, the next `tx_count_mined` first hands them to
/// the node and mines a block, so a commit lands between a retry's refresh and its nonce read.
/// The next `lag` receipt lookups answer "not found" (a node behind the one that answered
/// the nonce), and while `fail_estimates` is set every estimate fails.
struct Edge<'a> {
    net: &'a Net,
    hold: Mutex<Option<(Address, usize)>>,
    held: Mutex<Vec<SignedTx>>,
    armed: AtomicBool,
    lag: AtomicUsize,
    fail_estimates: AtomicBool,
    /// Receipts of txs mined up to this block read as missing (pruned).
    pruned: std::sync::atomic::AtomicU64,
    /// Every block reads as empty (a node that does not have it yet).
    blind: AtomicBool,
    /// Every send that reached the node through this edge.
    sent: Mutex<Vec<SignedTx>>,
}

impl<'a> Edge<'a> {
    fn new(net: &'a Net) -> Self {
        Self {
            net,
            hold: Mutex::new(None),
            held: Mutex::new(vec![]),
            armed: AtomicBool::new(false),
            lag: AtomicUsize::new(0),
            fail_estimates: AtomicBool::new(false),
            pruned: Default::default(),
            blind: AtomicBool::new(false),
            sent: Mutex::new(vec![]),
        }
    }

    fn lagging(self, lag: usize) -> Self {
        self.lag.store(lag, Ordering::SeqCst);
        self
    }

    fn sent_from(&self, from: Address) -> usize {
        let sent = self.sent.lock().expect("sent");
        sent.iter().filter(|t| t.from() == from).count()
    }

    fn holding(self, from: Address, pass: usize) -> Self {
        *self.hold.lock().expect("hold") = Some((from, pass));
        self
    }

    /// Stops holding; the next nonce read lands what was held.
    fn arm(&self) {
        *self.hold.lock().expect("hold") = None;
        self.armed.store(true, Ordering::SeqCst);
    }

    fn held(&self) -> Vec<SignedTx> {
        self.held.lock().expect("held").clone()
    }

    async fn holds(&self, n: usize) {
        for _ in 0..250 {
            if self.held.lock().expect("held").len() >= n {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("never held {n} txs");
    }
}

impl Arkiv for Edge<'_> {
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
        if self.armed.swap(false, Ordering::SeqCst) {
            let held = std::mem::take(&mut *self.held.lock().expect("held"));
            for tx in held {
                self.net
                    .mem
                    .send(tx)
                    .await
                    .expect("the held tx reaches the node");
            }
            self.net.mine(1);
        }
        self.net.tx_count_mined(a).await
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.net.transaction(h).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.net.mined_tx(h).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<charter_store::MinedTx>, StoreError> {
        let txs = self.net.block_txs(n).await?;
        Ok(match self.blind.load(Ordering::SeqCst) {
            true => vec![],
            false => txs,
        })
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        if self.fail_estimates.load(Ordering::SeqCst) {
            return Err(StoreError::Rpc("estimate refused".into()));
        }
        self.net.estimate(from, ops).await
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        let kept = {
            let mut hold = self.hold.lock().expect("hold");
            match hold.as_mut() {
                Some((from, 0)) if *from == tx.from() => true,
                Some((from, pass)) if *from == tx.from() => {
                    *pass -= 1;
                    false
                }
                _ => false,
            }
        };
        if !kept {
            self.sent.lock().expect("sent").push(tx.clone());
            return self.net.send(tx).await;
        }
        let mut held = self.held.lock().expect("held");
        if !held.iter().any(|t| t.hash() == tx.hash()) {
            held.push(tx.clone());
        }
        Ok(tx.hash())
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        let lagging = self
            .lag
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        // Read anyway: polling a receipt is what mines the fake chain's next block.
        let r = self.net.receipt(h).await?;
        let pruned = |r: &Receipt| r.block_number <= self.pruned.load(Ordering::SeqCst);
        Ok(r.filter(|r| !lagging && !pruned(r)))
    }
}

/// alice (root admin), carol (admin, no peer document yet), bob (write) and dan (read).
async fn group() -> (Net, [Peer; 4], Rid) {
    let net = Net::new();
    let [alice, carol, bob, dan] = [1, 3, 2, 4].map(|s| Peer::new(&net, s));
    let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
        .await
        .expect("init");
    invite(&net, &alice, &carol, &rid, Role::Admin).await;
    invite(&net, &alice, &bob, &rid, Role::Write).await;
    invite(&net, &alice, &dan, &rid, Role::Read).await;
    for p in [&carol, &bob, &dan] {
        ring(&net, p, &rid).await.expect("join");
    }
    (net, [alice, carol, bob, dan], rid)
}

/// `a` and `b` are both on epoch `want` with the same key.
async fn one_epoch(net: &Net, a: &Peer, b: &Peer, rid: &Rid, want: u64) {
    let (ra, rb) = (
        ring(net, a, rid).await.expect("a"),
        ring(net, b, rid).await.expect("b"),
    );
    assert_eq!(
        (current(&ra), current(&rb)),
        (want, want),
        "one epoch for both"
    );
    assert_eq!(ra.epochs.get(&want), rb.epochs.get(&want), "the same key");
}

/// `admin`'s retry while its earlier commit lands right after the retry's refresh is
/// refused (the next refresh merges that commit), never sent as a second commit of the
/// epoch; then a retry goes through on top.
async fn retry_while_it_lands(
    edge: &Edge<'_>,
    admin: &Peer,
    other: &Peer,
    dan: &Peer,
    rid: &Rid,
    e0: u64,
) {
    edge.arm();
    let e = charter_private::remove_member(edge, &admin.cfg, &admin.keys, rid, &dan.keys.nid())
        .await
        .expect_err("refused: the earlier commit just landed");
    assert_eq!(e.code(), charter_core::ErrorCode::ConcurrentPush, "{e}");
    one_epoch(edge.net, admin, other, rid, e0 + 1).await;
    charter_private::remove_member(edge.net, &admin.cfg, &admin.keys, rid, &dan.keys.nid())
        .await
        .expect("the retry goes through on top");
    one_epoch(edge.net, admin, other, rid, e0 + 2).await;
}

#[test]
fn an_interrupted_commit_landing_during_the_retry_is_merged_not_forked() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let e0 = current(&ring(&net, &alice, &rid).await.expect("alice"));
        net.hold_mining(true);
        let bob = bob.keys.nid();
        let first = charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob);
        tokio::select! {
            r = first => panic!("finished while mining is held: {r:?}"),
            () = pool_holds(&net, 1) => {}
        }
        net.hold_mining(false);
        retry_while_it_lands(&Edge::new(&net), &alice, &carol, &dan, &rid, e0).await;
    });
}

/// carol's first commit carries her peer document; a tx cap that fits each op but not both
/// splits the job, so the commit row rides the second tx, at the next nonce. Interrupted
/// between the parts, the record names that second tx, so its landing during the retry is
/// recognised.
#[test]
fn a_split_commit_job_is_recorded_at_the_nonce_of_the_tx_carrying_the_commit() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let e0 = current(&ring(&net, &carol, &rid).await.expect("carol"));
        let control = carol.keys.control_address(&rid);
        let n0 = net.tx_count(control).await.expect("nonce");
        let mut cfg = carol.cfg.clone();
        cfg.pipeline.max_raw_bytes = SPLIT_CAP;
        let edge = Edge::new(&net).holding(control, 1);
        let bob = bob.keys.nid();
        let first = charter_private::remove_member(&edge, &cfg, &carol.keys, &rid, &bob);
        tokio::select! {
            r = first => panic!("finished with its commit held back: {r:?}"),
            () = edge.holds(1) => {}
        }
        let held = edge.held();
        assert_eq!(
            held[0].nonce(),
            n0 + 1,
            "the job split: the commit rides the second tx"
        );
        retry_while_it_lands(&edge, &carol, &alice, &dan, &rid, e0).await;
    });
}

/// A raw tx cap above carol's commit tx alone (about 20.6 KB) and below it with her peer
/// document (over 25 KB).
const SPLIT_CAP: usize = 23_000;

/// The commit tx is mined but reverts; the pipeline resends it at the next nonce, and the
/// record moves there before that resend goes out.
#[test]
fn a_reverted_commit_resent_at_the_next_nonce_is_recorded_there() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let e0 = current(&ring(&net, &alice, &rid).await.expect("alice"));
        let control = alice.keys.control_address(&rid);
        let n0 = net.tx_count(control).await.expect("nonce");
        net.mem.revert_next_at(0);
        let edge = Edge::new(&net).holding(control, 1);
        let bob = bob.keys.nid();
        let first = charter_private::remove_member(&edge, &alice.cfg, &alice.keys, &rid, &bob);
        tokio::select! {
            r = first => panic!("finished with its resend held back: {r:?}"),
            () = edge.holds(1) => {}
        }
        assert_eq!(edge.held()[0].nonce(), n0 + 1, "resent at the next nonce");
        retry_while_it_lands(&edge, &alice, &carol, &dan, &rid, e0).await;
    });
}

/// The interrupted commit lands between the retry's refresh and its nonce read, and the
/// node answering receipts lags `lag` lookups behind the one that answered the nonce.
async fn lagging_receipt(lag: usize) {
    let (net, [alice, carol, bob, dan], rid) = group().await;
    let e0 = current(&ring(&net, &alice, &rid).await.expect("alice"));
    net.hold_mining(true);
    let bob = bob.keys.nid();
    let first = charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob);
    tokio::select! {
        r = first => panic!("finished while mining is held: {r:?}"),
        () = pool_holds(&net, 1) => {} // Ctrl-C after the broadcast
    }
    net.hold_mining(false);
    let edge = Edge::new(&net).lagging(lag);
    edge.arm();
    let dan = dan.keys.nid();
    let e = charter_private::remove_member(&edge, &alice.cfg, &alice.keys, &rid, &dan)
        .await
        .expect_err("refused while its earlier commit landed or may have");
    assert_eq!(e.code(), charter_core::ErrorCode::ConcurrentPush, "{e}");
    one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
}

#[test]
fn a_lagging_receipt_for_the_landed_commit_is_refused_not_forked() {
    run(lagging_receipt(1));
}

#[test]
fn an_up_to_date_receipt_for_the_landed_commit_is_refused_not_forked() {
    run(lagging_receipt(0));
}

/// A tx of the control address waits in the pool at the nonce a plug would take (a push of
/// this device, or the old commit): the retry refuses without sending, as a plug priced
/// above it would replace it on a real node.
#[test]
fn a_plug_is_never_sent_over_a_pool_tx_of_the_same_address() {
    run(async {
        let (net, [alice, _carol, bob, dan], rid) = group().await;
        net.hold_mining(true);
        let bob = bob.keys.nid();
        let first = charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob);
        tokio::select! {
            r = first => panic!("finished while mining is held: {r:?}"),
            () = pool_holds(&net, 1) => {}
        }
        let edge = Edge::new(&net);
        let dan = dan.keys.nid();
        let e = charter_private::remove_member(&edge, &alice.cfg, &alice.keys, &rid, &dan)
            .await
            .expect_err("refused while a tx of the address is in the pool");
        assert_eq!(e.code(), charter_core::ErrorCode::ConcurrentPush, "{e}");
        let control = alice.keys.control_address(&rid);
        assert_eq!(edge.sent_from(control), 0, "no plug was sent");
        assert_eq!(net.mem.pending(), 1, "the pool tx is untouched");
    });
}

/// The commit's tx was lost, the retry's plug took its nonce, and that retry then failed
/// before its own commit went out: the plug is recorded, so the next retry proves the old
/// commit can never land instead of waiting on a receipt that will never come.
#[test]
fn a_plug_that_landed_before_a_failed_retry_settles_the_old_commit() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let e0 = current(&ring(&net, &alice, &rid).await.expect("alice"));
        let control = alice.keys.control_address(&rid);
        net.drop_sends_from(control, 0);
        let bob = bob.keys.nid();
        let first = charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob);
        let first = tokio::time::timeout(std::time::Duration::from_secs(5), first).await;
        assert!(
            !matches!(first, Ok(Ok(()))),
            "the lost commit did not land: {first:?}"
        );
        net.heal();

        let edge = Edge::new(&net);
        edge.fail_estimates.store(true, Ordering::SeqCst);
        let dan = dan.keys.nid();
        charter_private::remove_member(&edge, &alice.cfg, &alice.keys, &rid, &dan)
            .await
            .expect_err("the commit cannot be priced");
        assert_eq!(edge.sent_from(control), 1, "only the plug went out");

        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &dan)
            .await
            .expect("the plug proves the old commit can never land");
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}

/// Alice's removal of bob: accepted by the node but never mined (evicted), then given up.
async fn lost_removal(net: &Net, alice: &Peer, bob: &Peer, rid: &Rid) {
    net.drop_sends_from(alice.keys.control_address(rid), 0);
    let bob = bob.keys.nid();
    let first = charter_private::remove_member(net, &alice.cfg, &alice.keys, rid, &bob);
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), first).await;
    assert!(
        !matches!(first, Ok(Ok(()))),
        "the dropped removal did not land: {first:?}"
    );
    net.heal();
}

/// Alice's identity on a second machine (its own device and leaf, the same control
/// address), joined, with the epoch it is on.
async fn desktop(net: &Net, alice: &Peer, rid: &Rid) -> (Peer, u64) {
    let desktop = Peer::new(net, 1);
    let kp = charter_private::publish_key_package(net, &desktop.cfg, &desktop.keys)
        .await
        .expect("kp");
    charter_private::add_device(net, &alice.cfg, &alice.keys, rid, &kp)
        .await
        .expect("add desktop");
    let epoch = current(&ring(net, &desktop, rid).await.expect("desktop joins"));
    ring(net, alice, rid).await.expect("alice");
    assert_eq!(
        desktop.keys.control_address(rid),
        alice.keys.control_address(rid)
    );
    (desktop, epoch)
}

async fn remove<A: Arkiv>(a: &A, admin: &Peer, rid: &Rid, who: &Peer) -> charter_sync::Result<()> {
    charter_private::remove_member(a, &admin.cfg, &admin.keys, rid, &who.keys.nid()).await
}

/// A grant this home sends at the lost commit's nonce is recorded, so its receipt settles it.
#[test]
fn a_grant_from_the_same_home_after_a_lost_removal_settles_the_old_commit() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let e0 = current(&ring(&net, &alice, &rid).await.expect("alice"));
        lost_removal(&net, &alice, &bob, &rid).await;
        charter_private::publish_epoch_key(&net, &alice.cfg, &alice.keys, &rid, e0)
            .await
            .expect("the grant takes the lost commit's nonce");
        remove(&net, &alice, &rid, &dan)
            .await
            .expect("the grant proves the old commit can never land");
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}

/// A grant from the admin's other machine is recorded nowhere here: the tx at the lost
/// commit's nonce is found from the block of the entity it created, in one block read.
#[test]
fn a_grant_from_the_admins_other_machine_after_a_lost_removal_settles_the_old_commit() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let (desktop, e0) = desktop(&net, &alice, &rid).await;
        net.mine(64); // no earlier entity of the address is within the search's margin
        lost_removal(&net, &alice, &bob, &rid).await;
        charter_private::publish_epoch_key(&net, &desktop.cfg, &desktop.keys, &rid, e0)
            .await
            .expect("the desktop's grant takes the lost commit's nonce");
        let reads = net.block_reads.load(Ordering::SeqCst);
        remove(&net, &alice, &rid, &dan)
            .await
            .expect("the desktop's grant proves the old commit can never land");
        assert_eq!(
            net.block_reads.load(Ordering::SeqCst) - reads,
            1,
            "one block read"
        );
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}

/// With every receipt gone the recorded grant is still found by the tx at the nonce.
#[test]
fn a_pruned_receipt_still_settles_the_old_commit_by_the_tx_at_its_nonce() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let e0 = current(&ring(&net, &alice, &rid).await.expect("alice"));
        lost_removal(&net, &alice, &bob, &rid).await;
        charter_private::publish_epoch_key(&net, &alice.cfg, &alice.keys, &rid, e0)
            .await
            .expect("grant");
        let edge = Edge::new(&net);
        edge.pruned
            .store(net.head().await.expect("head"), Ordering::SeqCst);
        remove(&edge, &alice, &rid, &dan)
            .await
            .expect("the tx at the nonce is the recorded grant");
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}

/// A node that does not have the block yet finds no tx at the nonce: refused, and found
/// once it has the block (the search starts over after a pass to the head).
#[test]
fn a_node_without_the_block_refuses_until_it_has_it() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let (desktop, e0) = desktop(&net, &alice, &rid).await;
        lost_removal(&net, &alice, &bob, &rid).await;
        charter_private::publish_epoch_key(&net, &desktop.cfg, &desktop.keys, &rid, e0)
            .await
            .expect("grant");
        let edge = Edge::new(&net);
        edge.blind.store(true, Ordering::SeqCst);
        let e = remove(&edge, &alice, &rid, &dan)
            .await
            .expect_err("no tx at the nonce is seen yet");
        assert_eq!(e.code(), charter_core::ErrorCode::ConcurrentPush, "{e}");
        edge.blind.store(false, Ordering::SeqCst);
        remove(&edge, &alice, &rid, &dan)
            .await
            .expect("found once the node has the block");
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}

/// A zero-value transfer from `desktop`'s control address (no entity).
async fn transfer_from(net: &Net, desktop: &Peer, rid: &Rid) {
    let control = desktop.keys.control(rid);
    let chain = net.chain_id().await.expect("chain");
    let pipeline = desktop.cfg.pipeline.clone();
    charter_store::TxPipeline::new(net, control.clone(), chain, pipeline)
        .funded_by(desktop.keys.wallet())
        .transfers(vec![(control.address(), U256::ZERO)])
        .await
        .expect("the transfer takes the lost commit's nonce");
}

/// The most blocks a search that is bounded from above should read.
const FEW_READS: usize = 8;

/// A plain transfer (no entity) from the other machine far past the lost commit: the walk
/// down from the head finds it in the first attempt.
#[test]
fn a_nonce_taken_far_past_the_commit_by_a_transfer_is_found_in_one_attempt() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let (desktop, e0) = desktop(&net, &alice, &rid).await;
        lost_removal(&net, &alice, &bob, &rid).await;
        net.mine(300);
        transfer_from(&net, &desktop, &rid).await;
        let reads = net.block_reads.load(Ordering::SeqCst);
        remove(&net, &alice, &rid, &dan)
            .await
            .expect("found right under the head");
        let reads = net.block_reads.load(Ordering::SeqCst) - reads;
        assert!(reads <= FEW_READS, "{reads} block reads");
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}

/// The same transfer buried far under the head: the blocks are read in bounded attempts that
/// go on down from where the last one stopped.
#[test]
fn a_nonce_taken_far_below_the_head_is_found_over_attempts() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let (desktop, e0) = desktop(&net, &alice, &rid).await;
        lost_removal(&net, &alice, &bob, &rid).await;
        transfer_from(&net, &desktop, &rid).await;
        net.mine(300);
        let reads = net.block_reads.load(Ordering::SeqCst);
        let e = remove(&net, &alice, &rid, &dan)
            .await
            .expect_err("one attempt reads a bounded number of blocks");
        assert_eq!(e.code(), charter_core::ErrorCode::ConcurrentPush, "{e}");
        assert_eq!(net.block_reads.load(Ordering::SeqCst) - reads, 256);
        remove(&net, &alice, &rid, &dan)
            .await
            .expect("the next attempt goes on down and finds it");
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}

/// The other machine's grant reverts at the lost commit's nonce half an hour later (2 s
/// blocks) and lands at the next one: its entity bounds the search from above, so the
/// walk down from it settles the old commit in the first attempt.
#[test]
fn a_reverted_grant_from_the_other_machine_a_while_after_a_lost_removal_settles_in_one_attempt() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let (desktop, e0) = desktop(&net, &alice, &rid).await;
        lost_removal(&net, &alice, &bob, &rid).await;
        net.mine(1000);
        net.mem.revert_next_at(0);
        charter_private::publish_epoch_key(&net, &desktop.cfg, &desktop.keys, &rid, e0)
            .await
            .expect("the grant lands at the next nonce");
        let reads = net.block_reads.load(Ordering::SeqCst);
        remove(&net, &alice, &rid, &dan)
            .await
            .expect("no refused attempt");
        let reads = net.block_reads.load(Ordering::SeqCst) - reads;
        assert!(reads <= FEW_READS, "{reads} block reads");
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}

/// The other machine's grant still in the pool when the retry runs: refused without a plug,
/// then settled by that grant once it is mined.
#[test]
fn a_grant_in_the_pool_from_the_other_machine_is_waited_on_then_settles_the_commit() {
    run(async {
        let (net, [alice, carol, bob, dan], rid) = group().await;
        let (desktop, e0) = desktop(&net, &alice, &rid).await;
        lost_removal(&net, &alice, &bob, &rid).await;
        net.hold_mining(true);
        let grant = charter_private::publish_epoch_key(&net, &desktop.cfg, &desktop.keys, &rid, e0);
        let retry = async {
            pool_holds(&net, 1).await;
            let e = remove(&net, &alice, &rid, &dan).await;
            net.hold_mining(false);
            e
        };
        let (grant, e) = tokio::join!(grant, retry);
        grant.expect("the grant lands once mining resumes");
        let e = e.expect_err("refused while the grant waits in the pool");
        assert_eq!(e.code(), charter_core::ErrorCode::ConcurrentPush, "{e}");
        remove(&net, &alice, &rid, &dan)
            .await
            .expect("the mined grant proves the old commit can never land");
        one_epoch(&net, &alice, &carol, &rid, e0 + 1).await;
    });
}
