//! Read-path round trips (docs/p5/speed-and-cost.md): pieces fetched concurrently and
//! alongside the snapshot download, the cached `ring_for` answer, speculative sync, the
//! head probe and the one-row peer query.

mod common;

use common::*;

#[tokio::test(flavor = "multi_thread")]
async fn clone_fetches_piece_txs_concurrently_and_alongside_the_snapshot() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "pieces").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", &noise(1, 200_000))], "feat: snapshot");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("snapshot push");
    commit(&work, &[("b", &noise(2, 400_000))], "feat: bulk");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("bulk push");
    net.mine(30);

    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    net.stats.reset();
    let (_, listing) = clone_into(&net, &bob, &rid, &dest).await.expect("clone");
    assert_eq!(listing.refs[0].1, rev(&work, "refs/heads/main"));
    git(&dest, &["fsck", "--full", "--strict"]);

    assert!(
        net.stats.max_mined() >= 2,
        "piece txs in flight at once: {}",
        net.stats.max_mined()
    );
    let (first_piece, _) = net.stats.span("mined_tx").expect("piece reads");
    let (_, last_chunk) = net.stats.span("ag.pack").expect("chunk reads");
    assert!(
        first_piece < last_chunk,
        "pieces start before the snapshot chunks finish ({first_piece} vs {last_chunk})"
    );
}

