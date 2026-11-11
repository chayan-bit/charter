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
    let (first_piece, _) = net.stats.span("tx_at").expect("piece reads");
    let (_, last_chunk) = net.stats.span("ag.pack").expect("chunk reads");
    assert!(
        first_piece < last_chunk,
        "pieces start before the snapshot chunks finish ({first_piece} vs {last_chunk})"
    );
}

/// Alice pushes twice (format v2); bob clones.
async fn pushed_and_cloned(
    net: &Net,
    tmp: &std::path::Path,
) -> (Peer, Peer, charter_core::Rid, std::path::PathBuf) {
    let alice = Peer::new(net, 1);
    let rid = init(net, &alice, "memo").await;
    let work = work_repo(tmp, "work");
    commit(&work, &[("a", b"one")], "feat: one");
    push2(net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push");
    let bob = Peer::new(net, 2);
    clone_into(net, &bob, &rid, &tmp.join("dest"))
        .await
        .expect("clone");
    (alice, bob, rid, work)
}

fn main_of(view: &charter_sync::View, nid: &charter_core::Nid) -> String {
    charter_sync::listing(view, Some(nid))
        .refs
        .into_iter()
        .find(|(n, _)| n == "refs/heads/main")
        .map(|(_, o)| o)
        .expect("main")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_behind_an_unchanged_probe_replays_and_a_push_is_seen_at_once() {
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, bob, rid, work) = pushed_and_cloned(&net, tmp.path()).await;
    let repo = bob.repo(&rid);

    net.stats.reset();
    let view = charter_sync::sync(&net, &repo, &charter_sync::Want::default())
        .await
        .expect("replayed sync");
    assert_eq!(main_of(&view, &alice.keys.nid()), rev(&work, "main"));
    let sent: Vec<String> = net
        .stats
        .calls()
        .into_iter()
        .filter(|c| c.starts_with("query"))
        .collect();
    let probe_or_count = |c: &String| {
        c.contains("str('bind')") || c.contains("str('cob')") || c.starts_with("query $creator")
    };
    assert!(
        sent.iter().all(probe_or_count),
        "only the probe and live counts go out: {sent:#?}"
    );

    commit(&work, &[("b", b"two")], "feat: two");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("second push");
    let view = charter_sync::sync(&net, &repo, &charter_sync::Want::default())
        .await
        .expect("sync after the push");
    assert_eq!(
        main_of(&view, &alice.keys.nid()),
        rev(&work, "main"),
        "the patched head changed the probe"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_window_sends_the_last_syncs_queries_at_once() {
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, bob, rid, work) = pushed_and_cloned(&net, tmp.path()).await;
    let repo = bob.repo(&rid);
    let memo = repo.dir.join("view-memo.json");
    let mut m: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&memo).expect("memo")).expect("json");
    m["at"] = 0.into();
    std::fs::write(&memo, serde_json::to_vec(&m).expect("json")).expect("write");

    net.stats.reset();
    let view = charter_sync::sync(&net, &repo, &charter_sync::Want::default())
        .await
        .expect("speculative sync");
    assert_eq!(main_of(&view, &alice.keys.nid()), rev(&work, "main"));
    let at_once = net
        .stats
        .max_inflight_queries
        .load(std::sync::atomic::Ordering::SeqCst);
    assert!(at_once >= 5, "recorded queries go out together: {at_once}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_head_costs_one_nonce_read_when_it_accounts_for_the_nonce() {
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, bob, rid, _) = pushed_and_cloned(&net, tmp.path()).await;
    charter_sync::view::memo::expire(&bob.repo(&rid)).expect("expire");
    let c_d = charter_sync::device::device_keys(alice.home.path())
        .expect("device")
        .control_address(&rid);
    net.stats.reset();
    clone_into(&net, &bob, &rid, &tmp.path().join("dest"))
        .await
        .expect("fetch");
    assert_eq!(
        net.stats.count(&format!("nonce {c_d}")),
        1,
        "rows, then one nonce"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_first_peer_document_falls_back_to_every_document() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let forger = outsider(&net);
    let attrs = [
        (
            "ag.kind",
            charter_store::AttributeValue::Str("peer".into()),
            true,
        ),
        (
            "ag.nid",
            charter_store::AttributeValue::Bytes32(alice.keys.nid()),
            true,
        ),
        (
            "ag.body",
            charter_store::AttributeValue::Bytes(vec![1, 2, 3]),
            false,
        ),
    ];
    let op = charter_store::create_op_flags(&attrs, 1000, 7, 1).expect("op");
    send_ops(&net, &forger, vec![op]).await;
    let rid = init(&net, &alice, "forged").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a", b"one")], "feat: one");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push");
    let bob = Peer::new(&net, 2);
    let (view, _) = clone_into(&net, &bob, &rid, &tmp.path().join("dest"))
        .await
        .expect("clone");
    assert_eq!(main_of(&view, &alice.keys.nid()), rev(&work, "main"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_extended_cob_op_of_a_pinned_repo_changes_the_probe() {
    use charter_store::AttributeValue as V;
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, bob, rid, _) = pushed_and_cloned(&net, tmp.path()).await;
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
    let mallory = outsider(&net);
    let rid_attr = V::Bytes32(rid);
    let op = charter_store::create_op(
        &[
            ("ag.kind", V::Str("cob".into()), true),
            ("ag.rid", rid_attr, true),
        ],
        10_000,
        1,
    )
    .expect("op");
    send_ops(&net, &mallory, vec![op]).await;
    let repo = bob.repo(&rid);
    charter_sync::sync(&net, &repo, &charter_sync::Want::default())
        .await
        .expect("full sync");
    // The memo written before the pin holds no op list; this sync records one.
    charter_sync::sync(&net, &repo, &charter_sync::Want::default())
        .await
        .expect("sync recording the ops");

    // Same op count, new `$updatedAt` (ops are read-only, so an extend): the probe sees it.
    let key = created_by(&net, mallory.address()).await[0].key;
    extend(&net, &mallory, &[key], 20_000).await;
    net.stats.reset();
    charter_sync::sync(&net, &repo, &charter_sync::Want::default())
        .await
        .expect("sync after the extend");
    let sent = net.stats.calls();
    let probe_or_count = |c: &&String| {
        c.contains("str('bind')") || c.contains("str('cob')") || c.starts_with("query $creator")
    };
    assert!(
        sent.iter()
            .any(|c| c.starts_with("query") && !probe_or_count(&c)),
        "the changed probe sends the view's queries: {sent:#?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unpinned_repo_with_many_ops_sends_no_op_list_with_its_probe() {
    use charter_store::AttributeValue as V;
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (_, bob, rid, _) = pushed_and_cloned(&net, tmp.path()).await;
    let mallory = outsider(&net);
    for group in (0..120u128).collect::<Vec<_>>().chunks(40) {
        let ops = group
            .iter()
            .map(|salt| {
                let attrs = [
                    ("ag.kind", V::Str("cob".into()), true),
                    ("ag.rid", V::Bytes32(rid), true),
                ];
                charter_store::create_op(&attrs, 10_000, *salt).expect("op")
            })
            .collect();
        send_ops(&net, &mallory, ops).await;
    }
    let repo = bob.repo(&rid);
    charter_sync::sync(&net, &repo, &charter_sync::Want::default())
        .await
        .expect("full sync");
    let cob_queries = |net: &Net| {
        let calls = net.stats.calls();
        calls
            .iter()
            .filter(|c| c.starts_with("query") && c.contains("str('cob')"))
            .count()
    };
    for path in ["replayed", "plain"] {
        if path == "plain" {
            charter_sync::view::memo::expire(&repo).expect("expire");
        }
        net.stats.reset();
        charter_sync::sync(&net, &repo, &charter_sync::Want::default())
            .await
            .expect("sync");
        // v0.4.0 sent one count-only query for the ops of every repo.
        let n = cob_queries(&net);
        eprintln!("{path} sync: {n} op queries");
        assert!(n <= 1, "{path} sync of an unpinned repo pages its ops: {n}");
    }
}
