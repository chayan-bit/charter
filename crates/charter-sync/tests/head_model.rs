//! Format v2 through `MemArkiv` (docs/p5/head-model.md): push and clone, pieces read from
//! calldata after they expire, snapshot refresh, two devices, migration from v1 and the
//! rollback check.

mod common;

use charter_core::{Hash32, Start, anchor_chain};
use charter_store::Arkiv;
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

#[tokio::test(flavor = "multi_thread")]
async fn a_half_done_migration_keeps_serving_the_v1_refs() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "half").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"a")], "feat: a");
    push(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("v1 push");
    commit(&work, &[("b", b"b")], "feat: b");
    let repo = alice.repo(&rid);
    let p = Box::pin(prepare2(
        &net,
        &alice,
        &repo,
        &work,
        &[upd("refs/heads/main")],
    ))
    .await
    .expect("prepare");
    Box::pin(p.setup())
        .await
        .expect("delegation lands, then the push stops");
    drop(p);
    drop(repo);
    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (view, _) = Box::pin(clone_into(&net, &bob, &rid, &dest))
        .await
        .expect("clone");
    let pv = &view.peers[&alice.keys.nid()];
    assert!(
        pv.v2.is_some() && pv.head.is_some(),
        "devices known, v1 head still read"
    );
    let refs = charter_sync::listing(&view, Some(&alice.keys.nid())).refs;
    assert_eq!(
        refs,
        vec![("refs/heads/main".to_string(), rev(&work, "main~1"))]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_clone_skips_a_listed_pack_no_live_ref_reaches() {
    let net = Net::new();
    let desk = Peer::new(&net, 1);
    let laptop = desk.second_device(&net);
    let rid = init(&net, &desk, "skip").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("base", &noise(1, 50_000))], "feat: base");
    push2(&net, &desk, &rid, &work, &["refs/heads/main"])
        .await
        .expect("desk snapshot");
    git(&work, &["checkout", "-q", "-b", "topic"]);
    commit(&work, &[("base", &noise(2, 50_000))], "feat: topic");
    push2(&net, &laptop, &rid, &work, &["refs/heads/topic"])
        .await
        .expect("laptop thin pack");
    push2(&net, &desk, &rid, &work, &[":refs/heads/topic"])
        .await
        .expect("delete topic");
    // Force main onto unrelated history big enough to refresh the desk's snapshot.
    git(&work, &["checkout", "-q", "--orphan", "fresh"]);
    git(&work, &["rm", "-rfq", "."]);
    commit(&work, &[("new", &noise(3, 1_200_000))], "feat: fresh");
    let repo = desk.repo(&rid);
    let forced = charter_sync::Update {
        src: Some("refs/heads/fresh".into()),
        dst: "refs/heads/main".into(),
    };
    let p = Box::pin(prepare2(&net, &desk, &repo, &work, &[forced]))
        .await
        .expect("prepare");
    Box::pin(p.run()).await.expect("forced push");
    drop(repo);
    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (view, _) = Box::pin(clone_into(&net, &bob, &rid, &dest))
        .await
        .expect("clone despite the dead pack");
    let refs = charter_sync::listing(&view, Some(&desk.keys.nid())).refs;
    assert_eq!(
        refs,
        vec![("refs/heads/main".to_string(), rev(&work, "fresh"))]
    );
    git(&dest, &["fsck", "--full"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rolled_back_or_hidden_head_fails_closed_and_refs_only_pushes_verify() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "rb").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"a")], "feat: a");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push");
    git(&work, &["branch", "side"]);
    push2(&net, &alice, &rid, &work, &["refs/heads/side"])
        .await
        .expect("refs-only push");
    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (view, _) = Box::pin(clone_into(&net, &bob, &rid, &dest))
        .await
        .expect("legit heads verify");
    let dev = charter_sync::device::device_keys(alice.home.path()).expect("dev");
    let c_d = dev.control_address(&rid);
    let head = view.peers[&alice.keys.nid()]
        .v2
        .as_ref()
        .expect("v2")
        .device(&dev.nid())
        .expect("dev")
        .head
        .clone()
        .expect("head");
    assert_eq!(head.body.pieces_created, 2, "a marker piece per push");

    // A node whose nonce is ahead of the head it serves: the head was rolled back.
    let real = net.mem.entity_nonce(c_d).await.expect("nonce");
    net.mem.lie_entity_nonce(c_d, real + 1);
    let carol = Peer::new(&net, 3);
    let r = Box::pin(clone_into(&net, &carol, &rid, &tmp.path().join("c1"))).await;
    assert_eq!(code(r), "E_CONTROL_NOT_LIVE");
    net.mem.lie_entity_nonce(c_d, real);

    // A node that hides the head while the device has created entities.
    net.mem.withhold(head.key.into());
    let dave = Peer::new(&net, 4);
    let r = Box::pin(clone_into(&net, &dave, &rid, &tmp.path().join("c2"))).await;
    assert_eq!(code(r), "E_CONTROL_NOT_LIVE");
}

#[tokio::test(flavor = "multi_thread")]
async fn keepalive_keeps_every_v1_refs_entity_a_program_pinned_before_migration_folds() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "pinned-v1").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"1")], "feat: seed");
    push(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("v1 push");
    // Pin a program under v1, then one more v1 push after the pin.
    let src = include_str!("../../../programs/bounty.rhai");
    charter_sync::program::publish(&net, &alice.cfg, &alice.keys, &rid, "rhai/1", src)
        .await
        .expect("publish");
    let pin = charter_core::ProgramPin {
        engine: "rhai/1".into(),
        hash: charter_program::Program::hash(src),
    };
    let st = charter_sync::state::State::default();
    charter_sync::identity::propose(&net, &alice.cfg, &alice.keys, &rid, &st, |b| {
        charter_core::IdBody {
            program: Some(pin),
            ..b.clone()
        }
    })
    .await
    .expect("pin");
    commit(&work, &[("a", b"2")], "feat: after the pin");
    push(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("v1 push after the pin");
    // The first v0.4.0 push migrates.
    commit(&work, &[("a", b"3")], "feat: migrate");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("migration");

    let view = Box::pin(charter_sync::sync(&net, &alice.repo(&rid), &alice.want()))
        .await
        .expect("sync");
    assert!(view.program.is_some(), "the fold runs");
    let keep: std::collections::BTreeSet<Hash32> = charter_sync::keepalive::keep_set(&net, &view)
        .await
        .expect("keep set")
        .iter()
        .map(|k| k.key)
        .collect();
    let refs = entities(&net, &rid, charter_core::Kind::Refs).await;
    assert_eq!(refs.len(), 2, "the seed vote and the post-pin push");
    for r in &refs {
        assert!(
            keep.contains(&r.key),
            "the fold reads refs entity {}",
            h32_hex(&r.key)
        );
    }
}
