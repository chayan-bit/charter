//! Format v2 through `MemArkiv` (docs/p5/head-model.md): push and clone, pieces read from
//! calldata after they expire, snapshot refresh, two devices, migration from v1 and the
//! rollback check.

mod common;

use charter_core::{Start, anchor_chain};
use charter_sync::state::State;
use common::*;

fn own_head(net: &Net, p: &Peer, rid: &charter_core::Rid) -> charter_core::HeadBody {
    let repo = p.repo(rid);
    let view = futures_block(charter_sync::sync(net, &repo, &p.want())).expect("sync");
    let v2 = view.peers[&p.keys.nid()].v2.clone().expect("v2");
    let me = charter_sync::device::device_keys(p.home.path())
        .expect("dev")
        .nid();
    v2.device(&me)
        .and_then(|d| d.head.clone())
        .expect("head")
        .body
}

fn futures_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

#[tokio::test(flavor = "multi_thread")]
async fn v2_push_clone_fetch_round_trip_with_inline_bulk_and_refs_only_pushes() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "v2").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a.txt", b"hello")], "feat: one");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("first push");
    let first = own_head(&net, &alice, &rid);
    assert_eq!(
        (first.seq, first.snapshot.len(), first.packs.len()),
        (0, 1, 0),
        "first push snapshots"
    );

    commit(&work, &[("b.txt", b"small change")], "feat: two");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("inline push");
    commit(&work, &[("big.bin", &noise(7, 150_000))], "feat: three");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("bulk push");
    git(&work, &["branch", "topic", "HEAD~1"]);
    push2(&net, &alice, &rid, &work, &["refs/heads/topic"])
        .await
        .expect("refs-only push");
    let h = own_head(&net, &alice, &rid);
    assert_eq!(h.seq, 3);
    assert_eq!(h.packs.len(), 2, "inline and bulk packs since the snapshot");
    assert!(
        h.packs[0].pieces.iter().all(|p| p.tx != [0; 32]),
        "carried pieces name their tx"
    );
    assert!(
        h.packs[1].pieces.len() > 1,
        "150 KB goes as several bulk pieces"
    );

    // Clone after every piece entity expired: bytes come from calldata.
    net.mine(30);
    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (view, _) = clone_into(&net, &bob, &rid, &dest).await.expect("clone");
    let listing = charter_sync::listing(&view, Some(&alice.keys.nid()));
    assert_eq!(
        listing.refs,
        vec![
            ("refs/heads/main".to_string(), rev(&work, "refs/heads/main")),
            (
                "refs/heads/topic".to_string(),
                rev(&work, "refs/heads/topic")
            ),
        ]
    );
    git(&dest, &["fsck", "--full", "--strict"]);

    // Deletion, then bob fetches incrementally.
    push2(&net, &alice, &rid, &work, &[":refs/heads/topic"])
        .await
        .expect("delete");
    let (view, _) = clone_into(&net, &bob, &rid, &dest).await.expect("fetch");
    assert_eq!(
        charter_sync::listing(&view, Some(&alice.keys.nid()))
            .refs
            .len(),
        1
    );
    let h = own_head(&net, &alice, &rid);
    assert!(
        h.refs.iter().all(|e| e.name != "refs/heads/topic"),
        "no other device: tombstone dropped"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshot_refreshes_once_piece_bytes_pass_one_mebibyte() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "snap").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"a")], "feat: a");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push");
    let first = own_head(&net, &alice, &rid).snapshot;
    for i in 0..3u64 {
        commit(&work, &[("n", &noise(i, 400_000))], "feat: noise");
        push2(&net, &alice, &rid, &work, &["refs/heads/main"])
            .await
            .expect("push");
    }
    let h = own_head(&net, &alice, &rid);
    assert_ne!(h.snapshot, first, "a new snapshot");
    assert!(h.packs.len() < 3, "packs reset at the refresh");
    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (_, listing) = clone_into(&net, &bob, &rid, &dest).await.expect("clone");
    assert_eq!(listing.refs[0].1, rev(&work, "refs/heads/main"));
    git(&dest, &["fsck", "--full"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_merge_branches_and_a_same_branch_race_is_lost_race() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let laptop = alice.second_device(&net);
    let rid = init(&net, &alice, "devs").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"a")], "feat: a");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("desk");
    git(&work, &["checkout", "-q", "-b", "feature"]);
    commit(&work, &[("f", b"f")], "feat: f");
    push2(&net, &laptop, &rid, &work, &["refs/heads/feature"])
        .await
        .expect("laptop");
    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (view, _) = clone_into(&net, &bob, &rid, &dest).await.expect("clone");
    let listing = charter_sync::listing(&view, Some(&alice.keys.nid()));
    assert_eq!(listing.refs.len(), 2, "both devices' refs, one identity");
    assert_eq!(
        view.peers[&alice.keys.nid()]
            .v2
            .as_ref()
            .expect("v2")
            .devices
            .len(),
        2
    );

    // Same branch, both planned from the same state: the first to land wins.
    git(&work, &["checkout", "-q", "main"]);
    let x = commit(&work, &[("x", b"x")], "feat: x");
    git(&work, &["branch", "-f", "y", "main~1"]);
    git(&work, &["checkout", "-q", "y"]);
    commit(&work, &[("y", b"y")], "feat: y");
    let (r1, r2) = (alice.repo(&rid), laptop.repo(&rid));
    let p1 = Box::pin(prepare2(
        &net,
        &alice,
        &r1,
        &work,
        &[charter_sync::Update {
            src: Some(x.clone()),
            dst: "refs/heads/main".into(),
        }],
    ))
    .await
    .expect("p1");
    let p2 = Box::pin(prepare2(
        &net,
        &laptop,
        &r2,
        &work,
        &[charter_sync::Update {
            src: Some("refs/heads/y".into()),
            dst: "refs/heads/main".into(),
        }],
    ))
    .await
    .expect("p2");
    Box::pin(p1.setup()).await.expect("setup 1");
    Box::pin(p2.setup()).await.expect("setup 2");
    let s1 = State::load(&r1.state_path()).expect("s");
    let plan1 = Box::pin(p1.plan(&s1)).await.expect("plan 1");
    let s2 = State::load(&r2.state_path()).expect("s");
    let plan2 = Box::pin(p2.plan(&s2)).await.expect("plan 2");
    Box::pin(p1.finish(plan1)).await.expect("first lands");
    assert_eq!(code(Box::pin(p2.finish(plan2)).await), "E_LOST_RACE");
    let (_, listing) = clone_into(&net, &bob, &rid, &dest).await.expect("fetch");
    let main = listing
        .refs
        .iter()
        .find(|(n, _)| n == "refs/heads/main")
        .expect("main");
    assert_eq!(main.1, x, "the first-landed push holds main");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_v1_repo_migrates_on_the_first_v2_push_and_v1_readers_fail_loudly() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "mig").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"a")], "feat: a");
    git(&work, &["tag", "v1"]);
    push(
        &net,
        &alice,
        &rid,
        &work,
        &["refs/heads/main", "refs/tags/v1"],
    )
    .await
    .expect("v1 push");
    commit(&work, &[("b", b"b")], "feat: b");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("v2 push migrates");
    let h = own_head(&net, &alice, &rid);
    let tag = h
        .refs
        .iter()
        .find(|e| e.name == "refs/tags/v1")
        .expect("v1 ref carried");
    assert_eq!(tag.counter, 1);

    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (view, listing) = clone_into(&net, &bob, &rid, &dest).await.expect("v2 clone");
    assert_eq!(listing.refs.len(), 2);
    assert_eq!(listing.refs[0].1, rev(&work, "refs/heads/main"));
    git(&dest, &["fsck", "--full"]);

    // What v0.3.0 does: build the anchor chain from every anchor-kind row of the peer.
    let pv = &view.peers[&alice.keys.nid()];
    let err = anchor_chain(
        &rid,
        &alice.keys.nid(),
        &pv.info,
        &view.identity,
        &pv.anchors,
        Start::Genesis,
    )
    .expect_err("v1 reader");
    assert_eq!(err.code().as_str(), "E_UNSUPPORTED_VERSION");
}
