//! Keepalive, status, identity revisions and search (design 3.5, 5, 6).

mod common;

use charter_core::Kind;
use charter_store::U256;
use charter_sync::keepalive::{self, KeepOpts};
use charter_sync::search::{SearchQuery, search};
use charter_sync::state::State;
use charter_sync::{identity, write};
use common::*;

#[tokio::test(flavor = "multi_thread")]
async fn keepalive_before_expiry_keeps_the_clone_working_and_ignores_spam() {
    let net = Net::new();
    let mut alice = Peer::new(&net, 1);
    alice.cfg.data_lifetime = 400;
    let rid = init(&net, &alice, "kept").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "w");
    commit(&work, &[("f", &noise(3, 200_000))], "feat: f");
    push(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push");
    let spammer = outsider(&net);
    let spam = write::chunk_op(&rid, &[9; 32], 0, b"spam", 400).expect("op");
    send_ops(&net, &spammer, vec![spam]).await;

    let repo = alice.repo(&rid);
    let opts = KeepOpts {
        horizon: 100_000,
        renew_below: 1_000,
        ..KeepOpts::default()
    };
    let wallet = alice.keys.wallet();
    let dry = KeepOpts {
        dry_run: true,
        ..opts.clone()
    };
    let planned = keepalive::keepalive(&net, &repo, &alice.cfg, &wallet, &dry)
        .await
        .expect("dry");
    assert_eq!(
        (planned.extended, planned.gas),
        (6, 0),
        "4 chunks, refs, manifest: {planned:?}"
    );
    assert!(
        planned.estimate > U256::ZERO,
        "a dry run prices what it would extend: {planned:?}"
    );
    let report = keepalive::keepalive(&net, &repo, &alice.cfg, &wallet, &opts)
        .await
        .expect("keep");
    assert_eq!(report.extended, planned.extended);
    assert!(report.gas > 0 && report.txs == 1, "{report:?}");
    assert!(report.min_expiry_blocks.expect("set") > 1_000);

    let spam_left = created_by(&net, spammer.address()).await;
    assert!(
        spam_left[0].expires_at < 1_000,
        "spam with the repo's rid is not extended"
    );
    net.mine(500);
    clone_into(&net, &Peer::new(&net, 2), &rid, &tmp.path().join("d"))
        .await
        .expect("still live");

    let (view, lines) = keepalive::status(&net, &repo, &alice.cfg)
        .await
        .expect("status");
    assert!(view.equivocations.is_empty());
    assert_eq!(lines.len(), 1);
    assert!(lines[0].delegate && lines[0].head.map(|h| h.0) == Some(0));
    assert_eq!(
        (lines[0].expiring_14d, lines[0].min_expiry_days),
        (6, Some(2)),
        "horizon 100k blocks"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revision_needs_a_majority_of_the_current_delegates() {
    let net = Net::new();
    let (alice, bob) = (Peer::new(&net, 1), Peer::new(&net, 2));
    let rid = init(&net, &alice, "alpha-one").await;
    let st = State::default();
    let bob_nid = bob.keys.nid();
    // A new delegate must have a peer on Arkiv: bob's first write gives him one.
    let bobs_own = init(&net, &bob, "bobs-own").await;
    identity::propose(&net, &alice.cfg, &alice.keys, &rid, &st, |h| {
        let mut b = h.clone();
        b.delegates.push(bob_nid);
        b
    })
    .await
    .expect("add bob");
    let chain = identity::show(&net, &rid, &st).await.expect("show");
    assert_eq!(
        chain.head().body.delegates.len(),
        2,
        "one of one delegates suffices"
    );

    let rename = identity::propose(&net, &alice.cfg, &alice.keys, &rid, &st, |h| {
        let mut b = h.clone();
        b.name = "gamma".into();
        b
    })
    .await
    .expect("rename");
    let pending = identity::show(&net, &rid, &st).await.expect("show");
    assert_eq!(
        pending.head().body.name,
        "alpha-one",
        "one of two is no majority"
    );
    let by_old_name = SearchQuery {
        name: Some("alpha-one".into()),
        limit: 10,
        ..Default::default()
    };
    assert_eq!(search(&net, &by_old_name).await.expect("search").len(), 1);

    identity::accept(&net, &bob.cfg, &bob.keys, &rid, &rename)
        .await
        .expect("accept");
    let chain = identity::show(&net, &rid, &st).await.expect("show");
    assert_eq!(chain.head().body.name, "gamma");
    assert!(
        search(&net, &by_old_name).await.expect("search").is_empty(),
        "stale name dropped"
    );
    let by_peer = SearchQuery {
        peer: Some(bob_nid),
        limit: 10,
        ..Default::default()
    };
    let mut hits: Vec<_> = search(&net, &by_peer)
        .await
        .expect("search")
        .iter()
        .map(|h| h.rid)
        .collect();
    let mut want = vec![rid, bobs_own];
    hits.sort();
    want.sort();
    assert_eq!(hits, want, "bob delegates alpha and his own repo");
    assert!(identity::delegates_text(&chain.head().body.delegates).contains(", "));
}

#[tokio::test(flavor = "multi_thread")]
async fn search_intersects_predicates_and_verifies_heads() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rust = init(&net, &alice, "alpha-rs").await;
    let mut m = meta("alpha-go");
    m.lang = Some("go".into());
    m.license = Some("Apache-2.0".into());
    let go = identity::init(&net, &alice.cfg, &alice.keys, &m)
        .await
        .expect("init");
    let other = init(&net, &alice, "beta").await;
    let q = |f: fn(&mut SearchQuery)| {
        let mut q = SearchQuery {
            limit: 50,
            ..Default::default()
        };
        f(&mut q);
        q
    };
    let rids =
        |hits: Vec<charter_sync::search::Hit>| hits.into_iter().map(|h| h.rid).collect::<Vec<_>>();
    let prefix = search(&net, &q(|q| q.prefix = Some("alpha".into())))
        .await
        .expect("s");
    assert_eq!(prefix.len(), 2);
    let lic = q(|q| {
        q.prefix = Some("alpha".into());
        q.licenses = vec!["MIT".into(), "BSD-3-Clause".into()];
    });
    assert_eq!(rids(search(&net, &lic).await.expect("s")), vec![rust]);
    let lang = q(|q| q.lang = Some("go".into()));
    assert_eq!(rids(search(&net, &lang).await.expect("s")), vec![go]);

    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "w");
    commit(&work, &[("f", b"x")], "feat: f");
    push(&net, &alice, &other, &work, &["refs/heads/main"])
        .await
        .expect("push");
    let recent = search(&net, &q(|q| q.recent_days = Some(1)))
        .await
        .expect("s");
    assert_eq!(rids(recent.clone()), vec![other]);
    assert!(recent[0].at.is_some());
    assert!(entities(&net, &other, Kind::Anchor).await.len() == 1);
    assert!(
        search(&net, &q(|_| {}))
            .await
            .expect("no predicates")
            .is_empty()
    );
}
