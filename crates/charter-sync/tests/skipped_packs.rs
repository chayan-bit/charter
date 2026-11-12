//! Issue #25 (docs/p5/head-model.md 5.5): a listed pack that no live ref needs and that
//! cannot be indexed (its thin bases left with a snapshot refresh) is downloaded once, not
//! on every fetch, while the heads that list it stay the same.

mod common;

use charter_sync::state::State;
use common::*;

fn applied(p: &Peer, rid: &charter_core::Rid) -> std::collections::BTreeSet<String> {
    State::load(&p.repo(rid).state_path())
        .expect("state")
        .applied_packs
}

fn piece_reads(net: &Net) -> usize {
    net.stats.count("tx_at") + net.stats.count("mined_tx")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pack_of_a_deleted_branch_that_cannot_index_is_not_downloaded_again() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "skip").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"a")], "feat: a");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("snapshot push");
    // Device 1 pushes `topic` as pieces; device 2 changes a few bytes of its file, so its
    // pack is thin on device 1's blob.
    let file = noise(3, 24 << 10);
    git(&work, &["checkout", "-q", "-b", "topic"]);
    commit(&work, &[("t", &file)], "feat: topic");
    push2(&net, &alice, &rid, &work, &["refs/heads/topic"])
        .await
        .expect("topic");
    let laptop = alice.second_device(&net);
    let dest = tmp.path().join("laptop");
    Box::pin(clone_into(&net, &laptop, &rid, &dest))
        .await
        .expect("clone");
    // `topic` is not canonical (only the default branch and tags are): take it from the cache.
    let topic = rev(&work, "topic");
    charter_sync::git::Git::work(Some(&dest))
        .fetch_pack_from(&laptop.repo(&rid).git_dir(), std::slice::from_ref(&topic))
        .expect("fetch topic");
    git(&dest, &["checkout", "-q", "-b", "topic", &topic]);
    let mut changed = file.clone();
    changed[100..108].copy_from_slice(b"changed!");
    commit(&dest, &[("t", &changed)], "feat: topic edit");
    push2(&net, &laptop, &rid, &dest, &["refs/heads/topic"])
        .await
        .expect("laptop topic");
    // Device 1 deletes `topic` with a push big enough to refresh its snapshot, which then
    // covers only `main`: the blob device 2's pack deltas against is gone from every head.
    git(&work, &["checkout", "-q", "main"]);
    commit(&work, &[("big", &noise(4, 1_200 << 10))], "feat: big");
    let up = [upd("refs/heads/main"), upd(":refs/heads/topic")];
    let repo = alice.repo(&rid);
    let p = Box::pin(prepare2(&net, &alice, &repo, &work, &up))
        .await
        .expect("prepare");
    Box::pin(p.run()).await.expect("refresh push");
    drop(repo);

    let carol = Peer::new(&net, 3);
    let (view, _) = Box::pin(clone_into(&net, &carol, &rid, &tmp.path().join("c")))
        .await
        .expect("a fresh clone skips the pack with a warning");
    let refs = view.peers[&alice.keys.nid()].refs_body().expect("refs");
    assert!(refs.get("refs/heads/topic").is_none(), "topic is deleted");
    let laptop_head = view.peers[&alice.keys.nid()]
        .v2
        .as_ref()
        .expect("v2")
        .heads()
        .find(|(_, h)| !h.packs.is_empty())
        .map(|(_, h)| h.packs[0].pack_id)
        .expect("device 2 still lists its pack");
    let unindexed = hex::encode(laptop_head);
    assert!(
        !applied(&carol, &rid).contains(&unindexed),
        "precondition: the pack did not index"
    );

    net.stats.reset();
    let again = Box::pin(charter_sync::sync(
        &net,
        &carol.repo(&rid),
        &charter_sync::Want::default(),
    ))
    .await
    .expect("fetch");
    assert_eq!(
        piece_reads(&net),
        0,
        "the same heads: no piece is read again"
    );
    assert_eq!(
        again.peers[&alice.keys.nid()].refs_body(),
        view.peers[&alice.keys.nid()].refs_body(),
        "the refs are unchanged"
    );

    // A new head of the identity re-checks the pack once, then skips it again.
    commit(&work, &[("a", b"a2")], "feat: a2");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push");
    let fetch = || async {
        Box::pin(charter_sync::sync(
            &net,
            &carol.repo(&rid),
            &charter_sync::Want::default(),
        ))
        .await
        .expect("fetch")
    };
    net.stats.reset();
    let after = fetch().await;
    assert!(piece_reads(&net) > 0, "a changed head is checked again");
    assert_eq!(
        hex::encode(
            after.peers[&alice.keys.nid()]
                .refs_body()
                .expect("refs")
                .refs[0]
                .oid
                .clone()
        ),
        rev(&work, "main")
    );
    net.stats.reset();
    fetch().await;
    assert_eq!(piece_reads(&net), 0, "and skipped again");
}
