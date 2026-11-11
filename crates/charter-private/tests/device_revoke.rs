//! A revoked device of a private repo (docs/p5/head-model.md 15.7): an admin's client
//! removes its MLS leaf on its next sync, so it cannot open the next epoch while the
//! member's other devices can; two admins racing make one removal; and the device's writes
//! after the cut are refused before the removal lands.

mod common;

use charter_core::{Nid, Rid};
use charter_private::{Role, Sealed};
use charter_store::Arkiv;
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

#[test]
fn an_admins_sync_removes_the_revoked_leaf_and_the_device_cannot_open_the_next_epoch() {
    run(async {
        let w = world().await;
        let (net, rid) = (&w.net, &w.rid);
        let e0 = current(&ring(net, &w.alice, rid).await.expect("alice"));
        revoke_laptop(net, &w.bob, &w.laptop, rid).await;

        let a = ring(net, &w.alice, rid).await.expect("alice syncs");
        assert_eq!(current(&a), e0 + 1, "the admin's sync removed the leaf");
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
        assert_eq!(current(&again), e0 + 1, "no second removal");
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
        let (a, c, ()) = tokio::join!(ring(net, &w.alice, rid), ring(net, &w.carol, rid), release);
        a.expect("alice");
        c.expect("carol");
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

        // No admin has synced: the laptop's leaf is still in the group.
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
