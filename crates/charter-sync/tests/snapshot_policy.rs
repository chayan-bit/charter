//! Snapshot policy of format v2 pushes (docs/p5/head-model.md 4.4): manifest facts come
//! from the local cache, and a refresh adds a layer instead of rewriting the snapshot.

mod common;

use common::*;

fn futures_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

fn own_head(net: &Net, p: &Peer, rid: &charter_core::Rid) -> charter_core::DeviceHead {
    let repo = p.repo(rid);
    let view = futures_block(charter_sync::sync(net, &repo, &p.want())).expect("sync");
    let v2 = view.peers[&p.keys.nid()].v2.clone().expect("v2");
    let me = charter_sync::device::device_keys(p.home.path())
        .expect("dev")
        .nid();
    v2.device(&me).and_then(|d| d.head.clone()).expect("head")
}

fn manifest_reads(queries: &[String]) -> usize {
    queries
        .iter()
        .filter(|q| q.contains("\"manifest\""))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_piece_push_reads_no_snapshot_manifest_it_signed_itself() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "facts").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"a")], "feat: a");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("snapshot push");
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
    net.take_queries();
    Box::pin(p.run()).await.expect("piece push");
    assert_eq!(manifest_reads(&net.take_queries()), 0, "facts are cached");
    drop(repo);
    assert_eq!(own_head(&net, &alice, &rid).packs.len(), 1, "a piece push");
}
#[tokio::test(flavor = "multi_thread")]
async fn a_refresh_adds_a_layer_that_a_fresh_reader_clones() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "layers").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("base", &noise(9, 300_000))], "feat: base");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("snapshot push");
    let base = own_head(&net, &alice, &rid).body.snapshot;
    assert_eq!(base.len(), 1);
    for i in 0..3u64 {
        commit(&work, &[("n", &noise(i, 400_000))], "feat: noise");
        push2(&net, &alice, &rid, &work, &["refs/heads/main"])
            .await
            .expect("push");
    }
    let h = own_head(&net, &alice, &rid);
    assert_eq!(h.body.snapshot.len(), 2, "the refresh appended one layer");
    assert_eq!(
        h.body.snapshot[0], base[0],
        "the base layer is kept, not rewritten"
    );
    assert!(h.packs.len() < 3, "packs reset at the refresh");
    // Clone after the pieces expired.
    net.mine(30);
    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (_, listing) = clone_into(&net, &bob, &rid, &dest).await.expect("clone");
    assert_eq!(listing.refs[0].1, rev(&work, "refs/heads/main"));
    git(&dest, &["fsck", "--full"]);
    // A push on top of the layers still lands, and a fresh clone still verifies.
    commit(&work, &[("c", b"c")], "feat: c");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push after layer");
    let carol = Peer::new(&net, 3);
    let dest2 = tmp.path().join("dest2");
    let (_, listing) = clone_into(&net, &carol, &rid, &dest2)
        .await
        .expect("clone 2");
    assert_eq!(listing.refs[0].1, rev(&work, "refs/heads/main"));
    git(&dest2, &["fsck", "--full"]);
}
