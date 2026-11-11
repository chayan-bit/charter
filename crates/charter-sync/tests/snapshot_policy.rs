//! Snapshot policy of format v2 pushes (docs/p5/head-model.md 4.4): manifest facts come
//! from the local cache, and a refresh adds a layer instead of rewriting the snapshot.

mod common;

use common::*;

fn futures_block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

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
