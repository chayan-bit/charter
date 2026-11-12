//! A revoked device of a private repo (docs/p5/head-model.md 15.7): a sync only counts its
//! leaf and never sends; an admin's explicit removal ([`charter_private::revoked`]) takes
//! the leaf out, so the device cannot open the next epoch while the member's other devices
//! can; two admins racing make one removal, and two removals or refreshes at once from one
//! admin home leave it in the group; the device's writes after the cut are refused before
//! the removal lands; a revoke a stolen device makes removes nothing on its own; and a
//! membership change interrupted after its tx went out is never dropped while that tx can
//! still land, nor replaced once it landed (even when the job split or a revert moved the
//! commit to a later nonce), so a retry cannot fork the admin off the group.

mod common;

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use charter_core::{Nid, Rid};
use charter_private::{Role, Sealed};
use charter_store::{
    Address, Arkiv, Operation, Query, QueryOpts, QueryPage, Receipt, SignedTx, StoreError, TxHash,
    TxInfo, U256,
};
use common::sec::*;
use common::*;

fn dev(p: &Peer) -> Nid {
    charter_sync::device::device_keys(p.home.path())
        .expect("device")
        .nid()
}

/// A device-head push of `refname` by `p` through its own sealed view.
async fn push_ref(net: &Net, p: &Peer, rid: &Rid, work: &std::path::Path, refname: &str) {
    let sealed = Sealed::new(net, ring(net, p, rid).await.expect("ring"));
    let repo = p.repo(rid);
    let view = Box::pin(charter_sync::sync(&sealed, &repo, &p.want()))
        .await
        .expect("sync");
    let git = charter_sync::git::Git::work(Some(work));
    let hp = charter_sync::HeadPush::prepare(
        &sealed,
        &repo,
        &p.cfg,
        &p.keys,
        view,
        &git,
        &[upd(refname)],
    )
    .expect("prepare");
    Box::pin(hp.run()).await.expect("push");
}

/// bob revokes his laptop through his own sealed view.
async fn revoke_laptop(net: &Net, bob: &Peer, laptop: &Peer, rid: &Rid) {
    let sealed = Sealed::new(net, ring(net, bob, rid).await.expect("bob"));
    Box::pin(charter_sync::revoke::revoke_device(
        &sealed,
        &bob.cfg,
        &bob.keys,
        &bob.repo(rid),
        &dev(laptop),
        &dev(bob),
    ))
    .await
    .expect("revoke");
}

struct World {
    net: Net,
    alice: Peer,
    carol: Peer,
    bob: Peer,
    laptop: Peer,
    dan: Peer,
    rid: Rid,
    tmp: tempfile::TempDir,
}

/// alice (root admin) and carol (admin); bob (write) on two devices, his laptop added with
/// `--device`, both of which have pushed; dan reads.
async fn world() -> World {
    let net = Net::new();
    let [alice, carol, bob, dan] = [1, 3, 2, 4].map(|s| Peer::new(&net, s));
    let laptop = Peer::new(&net, 2);
    let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
        .await
        .expect("init");
    invite(&net, &alice, &carol, &rid, Role::Admin).await;
    invite(&net, &alice, &bob, &rid, Role::Write).await;
    invite(&net, &alice, &dan, &rid, Role::Read).await;
    let kp = charter_private::publish_key_package(&net, &laptop.cfg, &laptop.keys)
        .await
        .expect("laptop kp");
    charter_private::add_device(&net, &alice.cfg, &alice.keys, &rid, &kp)
        .await
        .expect("add laptop");
    for p in [&carol, &bob, &laptop, &dan] {
        ring(&net, p, &rid).await.expect("join");
    }
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "w");
    commit(&work, &[("a.txt", b"one")], "c1");
    push_ref(&net, &bob, &rid, &work, "refs/heads/main").await;
    git(&work, &["checkout", "-q", "-b", "topic"]);
    commit(&work, &[("t.txt", b"t1")], "t1");
    push_ref(&net, &laptop, &rid, &work, "refs/heads/topic").await;
    World {
        net,
        alice,
        carol,
        bob,
        laptop,
        dan,
        rid,
        tmp,
    }
}

fn current(r: &charter_private::Keyring) -> u64 {
    r.current().expect("current").0
}

async fn remove_revoked(net: &Net, p: &Peer, rid: &Rid) -> usize {
    charter_private::revoked::remove_revoked(net, &p.cfg, &p.keys, rid)
        .await
        .expect("remove revoked")
}

#[test]
fn a_sync_only_counts_the_revoked_leaf_and_an_admins_removal_shuts_the_device_out() {
    run(async {
        let w = world().await;
        let (net, rid) = (&w.net, &w.rid);
        let e0 = current(&ring(net, &w.alice, rid).await.expect("alice"));
        revoke_laptop(net, &w.bob, &w.laptop, rid).await;

        let ctl = w.alice.keys.control_address(rid);
        let sent = net.tx_count(ctl).await.expect("n");
        let a = ring(net, &w.alice, rid).await.expect("alice syncs");
        assert_eq!(current(&a), e0, "a sync sends nothing");
        assert_eq!(a.revoked_leaves, 1, "the admin's keyring counts the leaf");
        assert_eq!(net.tx_count(ctl).await.expect("n"), sent);
        let d = ring(net, &w.dan, rid).await.expect("dan");
        assert_eq!(d.revoked_leaves, 0, "only an admin's keyring counts it");

        assert_eq!(remove_revoked(net, &w.alice, rid).await, 1);
        let a = ring(net, &w.alice, rid).await.expect("alice syncs");
        assert_eq!(current(&a), e0 + 1, "the admin's removal made one epoch");
        assert_eq!(a.revoked_leaves, 0);
        let (_, list) = charter_private::members(net, &w.alice.cfg, &w.alice.keys, rid)
            .await
            .expect("members");
        assert!(
            list.contains(&(w.bob.keys.nid(), Role::Write, 1, 0)),
            "{list:?}"
        );

        let work = w.tmp.path().join("w");
        git(&work, &["checkout", "-q", "main"]);
        commit(&work, &[("b.txt", b"two")], "c2");
        push_ref(net, &w.bob, rid, &work, "refs/heads/main").await;
        let b = ring(net, &w.bob, rid).await.expect("bob");
        assert_eq!(b.epochs.get(&(e0 + 1)), a.epochs.get(&(e0 + 1)));
        let l = ring(net, &w.laptop, rid)
            .await
            .expect("laptop keeps old keys");
        assert!(
            !l.epochs.contains_key(&(e0 + 1)),
            "the laptop holds no new key"
        );
        let (by_bob, by_laptop) = (
            opened_git_state(net, b, rid).await,
            opened_git_state(net, l, rid).await,
        );
        assert!(
            by_laptop < by_bob,
            "rows sealed in the new epoch stay shut to the laptop ({by_laptop} of {by_bob})"
        );
        let again = ring(net, &w.carol, rid).await.expect("carol syncs");
        assert_eq!(current(&again), e0 + 1);
        assert_eq!(
            remove_revoked(net, &w.carol, rid).await,
            0,
            "no second removal"
        );
    });
}

#[test]
fn two_admins_racing_make_exactly_one_removal() {
    run(async {
        let w = world().await;
        let (net, rid) = (&w.net, &w.rid);
        let e0 = current(&ring(net, &w.alice, rid).await.expect("alice"));
        ring(net, &w.carol, rid).await.expect("carol");
        revoke_laptop(net, &w.bob, &w.laptop, rid).await;

        // Both admins read the pending revoke before either removal is mined.
        let ctl = |p: &Peer| p.keys.control_address(rid);
        let sent = |p: &Peer| net.tx_count(ctl(p));
        let (a0, c0) = (
            sent(&w.alice).await.expect("n"),
            sent(&w.carol).await.expect("n"),
        );
        net.hold_mining(true);
        // Bounded, so a client that never sends cannot hang the test.
        let release = async {
            for _ in 0..250 {
                let a = sent(&w.alice).await.expect("n");
                let c = sent(&w.carol).await.expect("n");
                if a > a0 && c > c0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            net.hold_mining(false);
        };
        let (a, c, ()) = tokio::join!(
            remove_revoked(net, &w.alice, rid),
            remove_revoked(net, &w.carol, rid),
            release
        );
        assert_eq!((a, c), (1, 1), "both admins saw the leaf");
        let (a, c) = (
            ring(net, &w.alice, rid).await.expect("alice again"),
            ring(net, &w.carol, rid).await.expect("carol again"),
        );
        let racing: Vec<_> =
            charter_private::msg::find(net, charter_private::msg::T_COMMIT, vec![])
                .await
                .expect("commits")
                .into_iter()
                .filter(|o| charter_private::msg::u64_of(o, "ag.ep") == Some(e0 + 1))
                .collect();
        assert_eq!(racing.len(), 2, "both admins sent a removal");
        let b = ring(net, &w.bob, rid).await.expect("bob");
        for r in [&a, &c, &b] {
            assert_eq!(current(r), e0 + 1, "one removal, one new epoch");
            assert_eq!(r.epochs.get(&(e0 + 1)), a.epochs.get(&(e0 + 1)));
        }
        let (_, list) = charter_private::members(net, &w.carol.cfg, &w.carol.keys, rid)
            .await
            .expect("members");
        assert!(
            list.contains(&(w.bob.keys.nid(), Role::Write, 1, 0)),
            "{list:?}"
        );
    });
}

#[test]
fn the_revoked_devices_writes_after_the_cut_are_refused_before_the_removal_lands() {
    run(async {
        let w = world().await;
        let (net, rid) = (&w.net, &w.rid);
        let e0 = current(&ring(net, &w.dan, rid).await.expect("dan"));
        let lap = w.tmp.path().join("w");
        let t1 = rev(&lap, "topic");
        // bob's view still shows T1 when the laptop lands T2, then he revokes at T1.
        let sealed = Sealed::new(net, ring(net, &w.bob, rid).await.expect("bob"));
        let view = Box::pin(charter_sync::sync(&sealed, &w.bob.repo(rid), &w.bob.want()))
            .await
            .expect("bob sync");
        commit(&lap, &[("t.txt", b"t2")], "t2");
        push_ref(net, &w.laptop, rid, &lap, "refs/heads/topic").await;
        let t2 = rev(&lap, "topic");
        Box::pin(charter_sync::revoke::revoke(
            &sealed,
            &w.bob.cfg,
            &w.bob.keys,
            &view,
            &dev(&w.laptop),
            &dev(&w.bob),
        ))
        .await
        .expect("revoke at T1");

        // No admin has removed it: the laptop's leaf is still in the group.
        let d = ring(net, &w.dan, rid).await.expect("dan");
        assert_eq!(current(&d), e0, "the removal has not landed");
        assert!(
            accepts(net, &w.dan, rid, &w.bob, &t1).await,
            "the cut stays"
        );
        assert!(
            !accepts(net, &w.dan, rid, &w.bob, &t2).await,
            "a write after the cut is refused"
        );
    });
}

/// Two removals and a refresh at once from one admin home (the UI's round, a CLI command):
/// the home lock runs them one after another, the second sees nothing left to remove, and
/// the admin still follows the next change another admin makes.
#[test]
fn one_admin_home_removing_and_refreshing_at_once_stays_in_the_group() {
    run(async {
        let w = world().await;
        let (net, rid) = (&w.net, &w.rid);
        let e0 = current(&ring(net, &w.alice, rid).await.expect("alice"));
        revoke_laptop(net, &w.bob, &w.laptop, rid).await;
        let (x, y, z) = tokio::join!(
            remove_revoked(net, &w.alice, rid),
            remove_revoked(net, &w.alice, rid),
            ring(net, &w.alice, rid)
        );
        z.expect("refresh");
        assert_eq!(x + y, 1, "one removal from the home");
        let at = |o: &charter_core::Observed| charter_private::msg::u64_of(o, "ag.ep");
        let commits = charter_private::msg::find(net, charter_private::msg::T_COMMIT, vec![])
            .await
            .expect("commits");
        assert_eq!(commits.iter().filter(|o| at(o) == Some(e0 + 1)).count(), 1);

        let eve = Peer::new(net, 5);
        invite(net, &w.carol, &eve, rid, Role::Read).await;
        let c = current(&ring(net, &w.carol, rid).await.expect("carol"));
        let a = current(&ring(net, &w.alice, rid).await.expect("alice"));
        assert_eq!((c, a), (e0 + 2, e0 + 2), "alice follows carol's add");
    });
}

/// Every device holds the identity seed, so a stolen laptop can sign a revoke of its
/// owner's main device (docs/p2/private-repos.md "Lost or stolen machine"). No sync acts on
/// it: the main device keeps its leaf and reads the next epoch unless an admin chooses to
/// remove it.
#[test]
fn a_revoke_from_a_stolen_device_removes_nothing_until_an_admin_acts() {
    run(async {
        let w = world().await;
        let (net, rid) = (&w.net, &w.rid);
        let e0 = current(&ring(net, &w.alice, rid).await.expect("alice"));
        revoke_laptop(net, &w.laptop, &w.bob, rid).await;
        for p in [&w.alice, &w.carol, &w.dan] {
            assert_eq!(current(&ring(net, p, rid).await.expect("sync")), e0);
        }
        let eve = Peer::new(net, 5);
        invite(net, &w.alice, &eve, rid, Role::Read).await;
        let b = ring(net, &w.bob, rid).await.expect("bob");
        assert!(b.epochs.contains_key(&(e0 + 1)), "the main device reads on");
    });
}

/// Waits (bounded) until the pool holds `n` txs.
async fn pool_holds(net: &Net, n: usize) {
    for _ in 0..250 {
        if net.mem.pending() >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the pool never held {n} txs");
}

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
struct Edge<'a> {
    net: &'a Net,
    hold: Mutex<Option<(Address, usize)>>,
    held: Mutex<Vec<SignedTx>>,
    armed: AtomicBool,
}

impl<'a> Edge<'a> {
    fn new(net: &'a Net) -> Self {
        Self {
            net,
            hold: Mutex::new(None),
            held: Mutex::new(vec![]),
            armed: AtomicBool::new(false),
        }
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
        self.net.block_txs(n).await
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
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
            return self.net.send(tx).await;
        }
        let mut held = self.held.lock().expect("held");
        if !held.iter().any(|t| t.hash() == tx.hash()) {
            held.push(tx.clone());
        }
        Ok(tx.hash())
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        self.net.receipt(h).await
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

/// Pricing a removal counts the zero-value transfer the send would make first to retire an
/// earlier commit of this home still pending at a free nonce, and sends nothing itself.
#[test]
fn a_priced_removal_counts_the_transfer_that_retires_a_pending_commit() {
    run(async {
        let w = world().await;
        let (net, rid, alice) = (&w.net, &w.rid, &w.alice);
        revoke_laptop(net, &w.bob, &w.laptop, rid).await;
        let priced = || charter_private::revoked::removal_ops(net, &alice.cfg, &alice.keys, rid);
        let (_, plugs) = priced().await.expect("price").expect("a removal");
        assert_eq!(plugs, 0, "nothing pending: no transfer first");

        net.hold_mining(true);
        let bob = w.bob.keys.nid();
        let first = charter_private::remove_member(net, &alice.cfg, &alice.keys, rid, &bob);
        tokio::select! {
            r = first => panic!("finished while mining is held: {r:?}"),
            () = pool_holds(net, 1) => {}
        }
        let (_, plugs) = priced().await.expect("price").expect("a removal");
        assert_eq!(plugs, 1, "the pending commit's nonce is still free");
        assert_eq!(net.mem.pending(), 1, "pricing sent nothing");
    });
}
