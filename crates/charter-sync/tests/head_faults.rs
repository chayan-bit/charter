//! Format v2 under faults (docs/p5/head-model.md 5.4, 6, 7.1): a head patched back to an
//! older signed body, landings that must skip reverted or foreign txs, and a push that
//! must wait for an earlier head tx still in flight.

mod common;

use charter_core::{HeadBody, Observed, Rid};
use charter_store::{Address, Arkiv, SignedTx, TxParams, U256};
use charter_sync::heads::{LANDING_WINDOW, calldata_rows, landing};
use charter_sync::keys::Keys;
use charter_sync::write::{entity_op, entity_patch_op};
use common::*;

fn dev_of(p: &Peer) -> Keys {
    charter_sync::device::device_keys(p.home.path()).expect("dev")
}

/// `n` v2 pushes of `main` from a work repo under `dir`.
async fn pushed(net: &Net, p: &Peer, rid: &Rid, dir: &std::path::Path, n: usize) {
    let work = work_repo(dir, "work");
    for i in 0..n {
        commit(&work, &[("f", format!("{i}").as_bytes())], "feat: step");
        push2(net, p, rid, &work, &["refs/heads/main"])
            .await
            .expect("push");
    }
}

async fn device_view(net: &Net, p: &Peer, rid: &Rid) -> charter_sync::heads::DeviceView {
    let view = Box::pin(charter_sync::sync(net, &p.repo(rid), &p.want()))
        .await
        .expect("sync");
    view.peers[&p.keys.nid()]
        .v2
        .as_ref()
        .expect("v2")
        .device(&dev_of(p).nid())
        .expect("device")
        .clone()
}

/// The signed head envelope a mined tx carried.
async fn head_envelope(net: &Net, tx: [u8; 32]) -> Vec<u8> {
    let mined = net.mem.mined_tx(tx.into()).await.expect("rpc").expect("tx");
    let row: Observed = calldata_rows(net, &mined)
        .into_iter()
        .find(|o| o.attr("ag.kind") == Some(&charter_core::AttrValue::Str("head".into())))
        .expect("head row");
    body_of(&row)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_head_patched_back_to_an_older_signed_body_fails_closed() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "rollback").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 2).await;
    let d = device_view(&net, &alice, &rid).await;
    let head = d.head.expect("head");
    assert_eq!(head.body.seq, 1);

    // The `C_d` key holder patches the head back to the seq 0 body, signature intact.
    let old = head_envelope(&net, head.body.prev_tx).await;
    let dev = dev_of(&alice);
    net.fund(dev.control_address(&rid));
    let op = entity_patch_op::<HeadBody>(head.key, &old).expect("op");
    send_ops(&net, &dev.control(&rid), vec![op]).await;

    let carol = Peer::new(&net, 3);
    let r = Box::pin(clone_into(&net, &carol, &rid, &tmp.path().join("c"))).await;
    assert_eq!(code(r), "E_CONTROL_NOT_LIVE", "a fresh client rejects it");
}

#[tokio::test(flavor = "multi_thread")]
async fn landing_skips_a_reverted_tx_and_a_create_of_another_entity() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "landing").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 2).await;
    let d = device_view(&net, &alice, &rid).await;
    let mut head = d.head.clone().expect("head");
    // Replays below carry the seq 0 body; search for that body's `h`.
    let env = head_envelope(&net, head.body.prev_tx).await;
    head.h = charter_core::content_hash(charter_core::Kind::Head, &env);

    let dev = dev_of(&alice);
    let signer = dev.control(&rid);
    net.fund(dev.control_address(&rid));
    let start = net.head().await.expect("head");
    // 1. A patch of the head carrying this body, mined and reverted (sent by hand: the
    // pipeline would resend a failed tx that re-estimates fine).
    net.mem.revert_next_at(0);
    let patch = entity_patch_op::<HeadBody>(head.key, &env).expect("op");
    let c_d = dev.control_address(&rid);
    let params = TxParams {
        nonce: net.mem.tx_count(c_d).await.expect("nonce"),
        to: charter_store::ARKIV_ADDRESS,
        value: U256::ZERO,
        input: charter_store::execute_calldata(std::slice::from_ref(&patch)),
        gas_limit: 3_000_000,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 0,
    };
    let tx = SignedTx::sign(&signer, CHAIN, params).expect("sign");
    net.mem.send(tx.clone()).await.expect("send");
    net.mine(1);
    let r = net
        .mem
        .receipt(tx.hash())
        .await
        .expect("rpc")
        .expect("mined");
    assert!(!r.success, "the injected revert is on chain");
    // 2. A create of another entity carrying the same body.
    send_ops(
        &net,
        &signer,
        vec![entity_op::<HeadBody>(&env, 100).expect("op")],
    )
    .await;
    // 3. The real patch.
    send_ops(&net, &signer, vec![patch]).await;
    let end = net.head().await.expect("head");

    // Search as if the body named `start` and the head was last updated at `end`.
    head.body.at = start;
    head.updated_at = end;
    let l = landing(&net, &d.device, &head, LANDING_WINDOW)
        .await
        .expect("rpc")
        .expect("found");
    assert_eq!(
        l.block, end,
        "only the successful patch of this head counts"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_waits_for_an_earlier_head_tx_still_in_flight() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "inflight").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;

    // A tx from `C_d` sits in the pool (a crash right after sending a head tx).
    let dev = dev_of(&alice);
    let c_d: Address = dev.control_address(&rid);
    net.fund(c_d);
    let params = TxParams {
        nonce: net.mem.tx_count_mined(c_d).await.expect("nonce"),
        to: c_d,
        value: U256::ZERO,
        input: Default::default(),
        gas_limit: 21_000,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 0,
    };
    let tx = SignedTx::sign(&dev.control(&rid), CHAIN, params).expect("sign");
    net.mem.send(tx).await.expect("send");

    let work = tmp.path().join("work");
    commit(&work, &[("g", b"next")], "feat: next");
    let r = push2(&net, &alice, &rid, &work, &["refs/heads/main"]).await;
    assert_eq!(
        code(r),
        "E_CONCURRENT_PUSH",
        "no second body on the same prev_tx"
    );

    net.mine(1);
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push once the earlier tx landed");
    let d = device_view(&net, &alice, &rid).await;
    assert_eq!(d.head.expect("head").body.seq, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copied_device_seed_cannot_fork_the_body_chain() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "twin").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;
    // A second machine with a copy of the whole home, device seed included.
    let twin = alice.second_device(&net);
    let seed = charter_sync::device::device_seed_path(alice.home.path());
    let copy = charter_sync::device::device_seed_path(twin.home.path());
    std::fs::create_dir_all(copy.parent().expect("dir")).expect("dir");
    std::fs::copy(&seed, &copy).expect("copy seed");
    let dest = tmp.path().join("twin");
    Box::pin(clone_into(&net, &twin, &rid, &dest))
        .await
        .expect("clone");

    // Both plan from the same view; alice lands first.
    let work = tmp.path().join("work");
    commit(&work, &[("a", b"alice")], "feat: alice");
    commit(&dest, &[("t", b"twin")], "feat: twin");
    let up = [upd("refs/heads/main")];
    let (ra, rt) = (alice.repo(&rid), twin.repo(&rid));
    let a = Box::pin(prepare2(&net, &alice, &ra, &work, &up))
        .await
        .expect("a");
    let t = Box::pin(prepare2(&net, &twin, &rt, &dest, &up))
        .await
        .expect("t");
    Box::pin(a.run()).await.expect("alice pushes");
    let r = Box::pin(t.run()).await;
    assert_eq!(
        code(r),
        "E_CONCURRENT_PUSH",
        "the twin refuses to fork the chain"
    );
    drop((ra, rt)); // release the repo locks
    let d = device_view(&net, &alice, &rid).await;
    assert_eq!(d.head.expect("head").body.seq, 1, "alice's body stands");
}
