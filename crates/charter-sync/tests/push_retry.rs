//! Issue #26 (docs/p5/head-model.md 7.4): a push refused with `E_CONCURRENT_PUSH` after
//! its bulk pieces landed reuses them on the retry, once it has read each one back and
//! found exactly its bytes; anything it cannot verify is uploaded again.

mod common;

use charter_core::Rid;
use charter_store::fake::Meter;
use charter_sync::HeadPush;
use common::*;

/// Alice and a twin holding a copy of her home (device seed included); the twin lands a
/// head on `other` after alice planned a 48 KiB push of `big`, so alice's head tx is refused
/// after her bulk pieces landed. Returns alice's work repo.
async fn refused_after_upload(net: &Net, alice: &Peer, rid: &Rid, tmp: &std::path::Path) {
    let work = work_repo(tmp, "work");
    commit(&work, &[("a", b"a")], "feat: a");
    push2(net, alice, rid, &work, &["refs/heads/main"])
        .await
        .expect("first push");
    let twin = alice.second_device(net);
    let seed = charter_sync::device::device_seed_path(alice.home.path());
    let copy = charter_sync::device::device_seed_path(twin.home.path());
    std::fs::create_dir_all(copy.parent().expect("dir")).expect("dir");
    std::fs::copy(&seed, &copy).expect("copy seed");
    let dest = tmp.join("twin");
    Box::pin(clone_into(net, &twin, rid, &dest))
        .await
        .expect("clone");
    git(&work, &["checkout", "-q", "-b", "big"]);
    commit(&work, &[("big", &noise(5, 48 << 10))], "feat: big");
    git(&dest, &["checkout", "-q", "-b", "other"]);
    commit(&dest, &[("o", b"other")], "feat: other");
    let (ra, rt) = (alice.repo(rid), twin.repo(rid));
    let a = Box::pin(prepare2(net, alice, &ra, &work, &[upd("refs/heads/big")]))
        .await
        .expect("a");
    let t = Box::pin(prepare2(net, &twin, &rt, &dest, &[upd("refs/heads/other")]))
        .await
        .expect("t");
    Box::pin(t.run()).await.expect("the twin lands first");
    assert_eq!(code(Box::pin(a.run()).await), "E_CONCURRENT_PUSH");
}

/// The retry of `big`, through a meter.
async fn retry(net: &Net, alice: &Peer, rid: &Rid, work: &std::path::Path) -> u64 {
    let m = Meter::new(net);
    let repo = alice.repo(rid);
    let view = Box::pin(charter_sync::sync(&m, &repo, &alice.want()))
        .await
        .expect("sync");
    let git = charter_sync::git::Git::work(Some(work));
    let hp = HeadPush::prepare(
        &m,
        &repo,
        &alice.cfg,
        &alice.keys,
        view,
        &git,
        &[upd("refs/heads/big")],
    )
    .expect("prepare");
    Box::pin(hp.run()).await.expect("the retry lands");
    m.metrics().txs
}

/// A fresh reader gets `big` with every object (the connectivity check passed).
async fn readable(net: &Net, alice: &Peer, rid: &Rid, work: &std::path::Path) {
    let carol = Peer::new(net, 3);
    let view = Box::pin(charter_sync::sync(
        net,
        &carol.repo(rid),
        &charter_sync::Want::default(),
    ))
    .await
    .expect("a fresh reader fetches the retried push");
    let refs = view.peers[&alice.keys.nid()].refs_body().expect("refs");
    let big = refs.get("refs/heads/big").expect("big");
    assert_eq!(hex::encode(big), rev(work, "big"));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_retry_after_a_refused_head_reuses_the_landed_bulk_pieces() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "retry").await;
    let tmp = tempfile::tempdir().expect("tmp");
    Box::pin(refused_after_upload(&net, &alice, &rid, tmp.path())).await;
    let work = tmp.path().join("work");
    let txs = Box::pin(retry(&net, &alice, &rid, &work)).await;
    assert_eq!(txs, 1, "only the head tx: every piece is reused");
    Box::pin(readable(&net, &alice, &rid, &work)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_retry_uploads_again_what_it_cannot_verify() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "retry-bad").await;
    let tmp = tempfile::tempdir().expect("tmp");
    Box::pin(refused_after_upload(&net, &alice, &rid, tmp.path())).await;
    // Point every recorded piece at the first push's head tx instead.
    let record = alice.repo(&rid).outgoing().join("bulk-pieces.json");
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&record).expect("a record")).expect("json");
    let wrong: serde_json::Map<String, serde_json::Value> = json
        .as_object()
        .expect("map")
        .keys()
        .map(|k| (k.clone(), serde_json::json!([2, 0])))
        .collect();
    std::fs::write(&record, serde_json::to_vec(&wrong).expect("json")).expect("write");
    let work = tmp.path().join("work");
    let txs = Box::pin(retry(&net, &alice, &rid, &work)).await;
    assert!(txs > 1, "the pieces go again: {txs} txs");
    Box::pin(readable(&net, &alice, &rid, &work)).await;
}
