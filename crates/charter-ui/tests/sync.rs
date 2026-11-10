//! The UI sync round (docs/p6/ui.md 4.2): skips when the repo lock is busy, and keeps
//! `cobs.json` equal to a full `cob::list` as ops arrive.

mod common;

use charter_core::ZERO;
use charter_sync::cob;
use charter_ui::cache::{self, Cobs, RepoView};
use charter_ui::sync_loop::{Outcome, round};
use common::*;
use serde_json::json;

#[tokio::test(flavor = "multi_thread")]
async fn a_round_skips_while_a_push_holds_the_lock_then_catches_up() {
    let net = Net::new();
    let alice = Peer::new(&net, 3);
    let rid = init(&net, &alice, "busy").await;
    let held = alice.repo(&rid);
    assert_eq!(
        round(&net, &alice.cfg, &rid).await.expect("round"),
        Outcome::Busy
    );
    assert!(
        !held.dir.join(cache::VIEW).exists(),
        "a skipped round writes nothing"
    );
    drop(held);
    assert_eq!(
        round(&net, &alice.cfg, &rid).await.expect("round"),
        Outcome::Synced
    );
    let dir = alice.repo(&rid).dir.clone();
    let v: RepoView = cache::read(&dir.join(cache::VIEW))
        .expect("read")
        .expect("view");
    assert_eq!((v.name.as_str(), v.revisions.len()), ("busy", 1));
}

#[tokio::test(flavor = "multi_thread")]
async fn rounds_keep_the_op_cache_equal_to_a_full_list() {
    let net = Net::new();
    let alice = Peer::new(&net, 4);
    let rid = init(&net, &alice, "ops").await;
    let issue = post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.issue",
        "create",
        ZERO,
        json!({"title": "a"}),
    )
    .await;
    round(&net, &alice.cfg, &rid).await.expect("first");
    post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.issue",
        "comment",
        issue,
        json!({"body": "b"}),
    )
    .await;
    post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.patch",
        "create",
        ZERO,
        json!({"title": "p"}),
    )
    .await;
    round(&net, &alice.cfg, &rid).await.expect("second");
    round(&net, &alice.cfg, &rid).await.expect("idle");
    let dir = alice.repo(&rid).dir.clone();
    let c: Cobs = cache::read(&dir.join(cache::COBS))
        .expect("read")
        .expect("cobs");
    let cached: Vec<_> = c.ops().expect("ops").iter().map(|o| o.id).collect();
    let full: Vec<_> = cob::list(&net, &rid, None, None)
        .await
        .expect("list")
        .iter()
        .map(|o| o.id)
        .collect();
    assert_eq!(cached, full);
    assert_eq!(cached.len(), 3);
}
