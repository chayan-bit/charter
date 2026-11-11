//! Push funding (design 4.2 step 6): one up-front wallet check for the predicted spend plus
//! one window's reserve, then rolling top-ups; a top-up the wallet cannot cover stops the
//! push with `E_INSUFFICIENT_FUNDS` and the next push resumes with only what is missing.

mod common;

use charter_core::{CHUNK_SIZE, Kind};
use charter_store::{Arkiv, U256};
use charter_sync::write::chunk_op;
use common::*;

const PACK: usize = 2_000_000;

/// `alice` with a repo and one 2 MB noise commit not yet pushed.
async fn setup(
    net: &Net,
) -> (
    Peer,
    charter_core::Rid,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    let alice = Peer::new(net, 1);
    let rid = init(net, &alice, "funding").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "w");
    commit(&work, &[("big", &noise(3, PACK))], "feat: big");
    (alice, rid, tmp, work)
}

/// Wallet + control + bulk balance, the funds the up-front check may count.
async fn pooled(net: &Net, p: &Peer, rid: &charter_core::Rid) -> U256 {
    let mut sum = U256::ZERO;
    for a in [
        p.keys.wallet().address(),
        p.keys.control_address(rid),
        p.keys.bulk_address(rid),
    ] {
        sum += net.balance(a).await.expect("balance");
    }
    sum
}

/// Sets alice's wallet so the pooled balance is exactly `total`.
async fn pool_to(net: &Net, p: &Peer, rid: &charter_core::Rid, total: U256) {
    let wallet = p.keys.wallet().address();
    let others = pooled(net, p, rid).await - net.balance(wallet).await.expect("b");
    net.mem.set_balance(wallet, total - others);
}

async fn required(net: &Net, p: &Peer, rid: &charter_core::Rid, work: &std::path::Path) -> U256 {
    let repo = p.repo(rid);
    let push = prepare(net, p, &repo, work, &[upd("refs/heads/main")])
        .await
        .expect("prepare");
    push.cost().await.expect("cost").required()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_leaves_at_most_one_window_on_the_bulk_address() {
    let net = Net::new();
    let (alice, rid, _tmp, work) = setup(&net).await;
    push(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push");
    let bulk = alice.keys.bulk_address(&rid);
    let probe = chunk_op(
        &rid,
        &[0; 32],
        0,
        &vec![0xa5; CHUNK_SIZE],
        alice.cfg.data_lifetime,
    )
    .expect("op");
    let gas = net.estimate(bulk, &[probe]).await.expect("estimate");
    let base = net.base_fee().await.expect("fee");
    let window = alice.cfg.pipeline.window_reserve(gas, base);
    let left = net.balance(bulk).await.expect("balance");
    assert!(left <= window, "bulk keeps {left} > one window {window}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_fails_up_front_only_when_the_wallet_cannot_cover_spend_plus_a_window() {
    let net = Net::new();
    let (alice, rid, _tmp, work) = setup(&net).await;
    let need = required(&net, &alice, &rid, &work).await;
    // Estimates move a little with random salts, so test either side with a margin.
    pool_to(&net, &alice, &rid, need - need / U256::from(10)).await;
    let r = push(&net, &alice, &rid, &work, &["refs/heads/main"]).await;
    assert_eq!(code(r), "E_INSUFFICIENT_FUNDS");
    let bulk = alice.keys.bulk_address(&rid);
    assert!(created_by(&net, bulk).await.is_empty(), "nothing sent");

    pool_to(&net, &alice, &rid, need + need / U256::from(20)).await;
    push(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("spend plus one window is enough");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_top_up_the_wallet_cannot_cover_stops_mid_push_and_the_retry_resumes() {
    let net = Net::new();
    let (alice, rid, tmp, work) = setup(&net).await;
    let need = required(&net, &alice, &rid, &work).await;
    pool_to(&net, &alice, &rid, need + need / U256::from(20)).await;
    let base = net.base_fee().await.expect("fee");
    let head = net.head().await.expect("head");
    net.hold_base_fee(head + 2, 2 * base);
    let r = push(&net, &alice, &rid, &work, &["refs/heads/main"]).await;
    assert_eq!(code(r), "E_INSUFFICIENT_FUNDS");
    let total = charter_core::chunk_count(PACK as u64) as usize;
    let landed = entities(&net, &rid, Kind::Chunk).await.len();
    assert!(
        landed > 0 && landed < total,
        "stopped mid-push after {landed} chunks"
    );
    assert!(alice.repo(&rid).pending_path().exists());

    // Time passes before the user retries: txs in flight at the failure land first.
    net.heal();
    net.mine(3);
    net.mem.set_base_fee(base);
    net.fund(alice.keys.wallet().address());
    push(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("retry");
    let chunks = entities(&net, &rid, Kind::Chunk).await.len();
    // The pack is the 2 MB blob plus a few hundred bytes, still 31 chunks.
    assert_eq!(chunks, total, "every chunk landed once, none sent twice");
    let bulk = alice.keys.bulk_address(&rid);
    assert_eq!(
        created_by(&net, bulk).await.len(),
        total + 2,
        "plus one manifest, one refs"
    );
    assert_eq!(entities(&net, &rid, Kind::Anchor).await.len(), 1);
    let (_, l) = clone_into(&net, &Peer::new(&net, 5), &rid, &tmp.path().join("d"))
        .await
        .expect("clone");
    assert_eq!(l.refs[0].1, rev(&work, "main"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_small_push_is_priced_for_its_real_chunk_sizes_not_full_chunks() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "small").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "w");
    commit(&work, &[("README", b"v0\n".as_slice())], "feat: v0");
    let repo = alice.repo(&rid);
    let p = prepare(&net, &alice, &repo, &work, &[upd("refs/heads/main")])
        .await
        .expect("prepare");
    let cost = p.cost().await.expect("cost");

    let bulk = alice.keys.bulk_address(&rid);
    let full = chunk_op(
        &rid,
        &[0; 32],
        0,
        &vec![0xa5; CHUNK_SIZE],
        alice.cfg.data_lifetime,
    );
    let full_gas = net
        .estimate(bulk, &[full.expect("op")])
        .await
        .expect("estimate");
    // Refs, manifest and one chunk of a few hundred bytes: less than one full chunk.
    assert!(
        cost.bulk_gas < full_gas,
        "a few-byte push is priced at {} bulk gas, a full chunk is {full_gas}",
        cost.bulk_gas
    );
}

/// Issue #28: the racing top-up and this device's own landed (at least `txs` wallet txs
/// mined) and the shared wallet's nonce has no gap.
async fn wallet_settled(net: &Net, p: &Peer, txs: u64) {
    let w = p.keys.wallet().address();
    let mined = net.mem.tx_count_mined(w).await.expect("mined");
    assert_eq!(
        net.mem.tx_count(w).await.expect("pending"),
        mined,
        "no stuck nonce"
    );
    assert!(mined >= txs, "{mined} wallet txs mined");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_whose_wallet_top_up_collides_with_another_device_still_pushes() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "two-devices").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "w");
    commit(&work, &[("a", b"a")], "feat: a");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("first device");
    let laptop = alice.second_device(&net);
    let dest = tmp.path().join("laptop");
    Box::pin(clone_into(&net, &laptop, &rid, &dest))
        .await
        .expect("clone");
    git(&dest, &["checkout", "-q", "-b", "topic"]);
    commit(&dest, &[("t", b"t")], "feat: topic");
    let before = net
        .mem
        .tx_count(alice.keys.wallet().address())
        .await
        .expect("n");
    // The laptop's first push tops up its new `C_d` from the wallet; alice's other device
    // takes the nonce it read.
    net.race_wallet(alice.keys.wallet());
    push2(&net, &laptop, &rid, &dest, &["refs/heads/topic"])
        .await
        .expect("the push lands despite the collision");
    wallet_settled(&net, &alice, before + 2).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_devices_pushing_at_once_from_one_wallet_both_land() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "at-once").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "w");
    commit(&work, &[("a", b"a")], "feat: a");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("first device");
    let laptop = alice.second_device(&net);
    let dest = tmp.path().join("laptop");
    Box::pin(clone_into(&net, &laptop, &rid, &dest))
        .await
        .expect("clone");
    git(&dest, &["checkout", "-q", "-b", "topic"]);
    commit(&dest, &[("t", &noise(9, 40 << 10))], "feat: topic");
    commit(&work, &[("b", &noise(8, 40 << 10))], "feat: b");
    let (a, b) = tokio::join!(
        Box::pin(push2(&net, &alice, &rid, &work, &["refs/heads/main"])),
        Box::pin(push2(&net, &laptop, &rid, &dest, &["refs/heads/topic"])),
    );
    a.expect("alice");
    b.expect("laptop");
    let w = alice.keys.wallet().address();
    let mined = net.mem.tx_count_mined(w).await.expect("mined");
    assert_eq!(
        net.mem.tx_count(w).await.expect("pending"),
        mined,
        "no stuck nonce"
    );
}
