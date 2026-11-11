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
    // The device itself does not mistake the rollback for a body it may repair.
    let work = tmp.path().join("work");
    commit(&work, &[("g", b"after")], "feat: after");
    let r = push2(&net, &alice, &rid, &work, &["refs/heads/main"]).await;
    assert_eq!(code(r), "E_CONTROL_NOT_LIVE", "no repair over a rollback");
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

#[tokio::test(flavor = "multi_thread")]
async fn a_new_device_pushes_while_another_device_lands_a_branch_it_never_fetched() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "race-branch").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;
    let laptop = alice.second_device(&net);
    let dest = tmp.path().join("laptop");
    Box::pin(clone_into(&net, &laptop, &rid, &dest))
        .await
        .expect("clone");
    git(&dest, &["checkout", "-q", "-b", "two"]);
    commit(&dest, &[("two", b"laptop")], "feat: two");
    let rl = laptop.repo(&rid);
    let t = Box::pin(prepare2(
        &net,
        &laptop,
        &rl,
        &dest,
        &[upd("refs/heads/two")],
    ))
    .await
    .expect("prepare");

    // Meanwhile the first device lands a branch the laptop's cache never saw.
    let work = tmp.path().join("work");
    git(&work, &["checkout", "-q", "-b", "one"]);
    commit(&work, &[("one", b"desk")], "feat: one");
    push2(&net, &alice, &rid, &work, &["refs/heads/one"])
        .await
        .expect("desk push");

    Box::pin(t.run())
        .await
        .expect("the laptop's first push lands");
    drop(rl);
    let view = Box::pin(charter_sync::sync(&net, &alice.repo(&rid), &alice.want()))
        .await
        .expect("sync");
    let refs = view.peers[&alice.keys.nid()]
        .refs_body()
        .expect("refs")
        .clone();
    let names: Vec<&str> = refs.refs.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["refs/heads/main", "refs/heads/one", "refs/heads/two"]
    );
}

/// The seq of the head entity of the device's control address, read raw.
async fn raw_head_seq(net: &Net, rid: &Rid) -> u64 {
    let rows = entities(net, rid, charter_core::Kind::Head).await;
    let [row] = rows.as_slice() else {
        panic!("one head entity, got {}", rows.len())
    };
    charter_core::open::<HeadBody>(row).expect("head").body.seq
}

#[tokio::test(flavor = "multi_thread")]
async fn a_copied_seed_with_a_head_tx_in_flight_cannot_land_a_second_head() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "in-flight-twin").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;
    let twin = alice.second_device(&net);
    let seed = charter_sync::device::device_seed_path(alice.home.path());
    let copy = charter_sync::device::device_seed_path(twin.home.path());
    std::fs::create_dir_all(copy.parent().expect("dir")).expect("dir");
    std::fs::copy(&seed, &copy).expect("copy seed");
    let dest = tmp.path().join("twin");
    Box::pin(clone_into(&net, &twin, &rid, &dest))
        .await
        .expect("clone");
    commit(&dest, &[("t", b"twin")], "feat: twin");
    let rt = twin.repo(&rid);
    let t = Box::pin(prepare2(&net, &twin, &rt, &dest, &[upd("refs/heads/main")]))
        .await
        .expect("prepare");
    Box::pin(t.setup()).await.expect("setup");
    let state = charter_sync::state::State::load(&rt.state_path()).expect("state");
    let plan = Box::pin(t.plan(&state)).await.expect("plan");

    // Machine A's head tx (a create from C_d) waits in the pool while B signs.
    let dev = dev_of(&alice);
    let c_d = dev.control_address(&rid);
    let params = TxParams {
        nonce: net.mem.tx_count(c_d).await.expect("nonce"),
        to: charter_store::ARKIV_ADDRESS,
        value: U256::ZERO,
        input: charter_store::execute_calldata(&[
            charter_sync::pieces::piece_op(&rid, b"a").expect("op")
        ]),
        gas_limit: 3_000_000,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 0,
    };
    let tx = SignedTx::sign(&dev.control(&rid), CHAIN, params).expect("sign");
    net.mem.send(tx).await.expect("send");

    let r = Box::pin(t.finish(plan)).await;
    assert_eq!(
        code(r),
        "E_CONCURRENT_PUSH",
        "B fails instead of landing behind A"
    );
    net.mine(1);
    assert_eq!(
        raw_head_seq(&net, &rid).await,
        0,
        "no second head body landed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_repairs_its_head_after_a_copied_seed_landed_a_second_head_tx() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "repair").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;
    // What an old client's double landing leaves: C_d created more than the head records.
    let dev = dev_of(&alice);
    net.fund(dev.control_address(&rid));
    let extra = charter_sync::pieces::piece_op(&rid, b"lost body").expect("op");
    send_ops(&net, &dev.control(&rid), vec![extra]).await;

    let carol = Peer::new(&net, 3);
    let r = Box::pin(clone_into(&net, &carol, &rid, &tmp.path().join("c1"))).await;
    assert_eq!(code(r), "E_CONTROL_NOT_LIVE", "every reader fails closed");
    // A copy of the seed that did not send the current body cannot repair it.
    let twin = alice.second_device(&net);
    let copy = charter_sync::device::device_seed_path(twin.home.path());
    std::fs::create_dir_all(copy.parent().expect("dir")).expect("dir");
    std::fs::copy(
        charter_sync::device::device_seed_path(alice.home.path()),
        &copy,
    )
    .expect("copy seed");
    let r = Box::pin(clone_into(&net, &twin, &rid, &tmp.path().join("t"))).await;
    assert_eq!(code(r), "E_CONTROL_NOT_LIVE");

    // The machine that sent it repairs it on its next push.
    let work = tmp.path().join("work");
    commit(&work, &[("g", b"next")], "feat: next");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("repairing push");
    let dest = tmp.path().join("c2");
    Box::pin(clone_into(&net, &carol, &rid, &dest))
        .await
        .expect("readable again");
    assert_eq!(rev(&dest, "HEAD"), rev(&work, "HEAD"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_device_never_overwrites_a_commit_another_device_landed_after_its_listing() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "race-main").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;
    let laptop = alice.second_device(&net);
    let dest = tmp.path().join("laptop");
    Box::pin(clone_into(&net, &laptop, &rid, &dest))
        .await
        .expect("clone");
    commit(&dest, &[("n", b"laptop")], "feat: n");
    let rl = laptop.repo(&rid);
    // git checked `main` against this listing.
    let t = Box::pin(prepare2(
        &net,
        &laptop,
        &rl,
        &dest,
        &[upd("refs/heads/main")],
    ))
    .await
    .expect("prepare");

    // The desk lands M on main before the laptop's first push (delegation, then head).
    let work = tmp.path().join("work");
    commit(&work, &[("m", b"desk")], "feat: m");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("desk push");

    let r = Box::pin(t.run()).await;
    assert_eq!(code(r), "E_LOST_RACE", "M must not be overwritten");
    drop(rl);
    let view = Box::pin(charter_sync::sync(&net, &alice.repo(&rid), &alice.want()))
        .await
        .expect("sync");
    let refs = view.peers[&alice.keys.nid()]
        .refs_body()
        .expect("refs")
        .clone();
    assert_eq!(hex::encode(&refs.refs[0].oid), rev(&work, "HEAD"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_base_that_is_not_an_ancestor_of_its_commit_fails_closed() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "forged-base").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;
    let work = tmp.path().join("work");
    let accepted = hex::decode(rev(&work, "HEAD")).expect("hex");
    // A force push of an unrelated root commit over main.
    git(&work, &["checkout", "-q", "--orphan", "x"]);
    commit(&work, &[("x", b"unrelated")], "feat: x");
    let ra = alice.repo(&rid);
    let forced = charter_sync::Update {
        src: Some("refs/heads/x".into()),
        dst: "refs/heads/main".into(),
    };
    let hp = Box::pin(prepare2(&net, &alice, &ra, &work, &[forced]))
        .await
        .expect("prepare");
    let state = charter_sync::state::State::load(&ra.state_path()).expect("state");
    let mut plan = Box::pin(hp.plan(&state)).await.expect("plan");
    // A modified client claims the accepted head as the base, so a fold would call it ff.
    for e in plan.refs.iter_mut().filter(|e| e.name == "refs/heads/main") {
        e.base = accepted.clone();
    }
    Box::pin(hp.finish(plan))
        .await
        .expect("the writer lands it");
    drop(ra);

    let carol = Peer::new(&net, 3);
    let r = Box::pin(clone_into(&net, &carol, &rid, &tmp.path().join("c"))).await;
    assert_eq!(
        code(r),
        "E_BAD_REFS",
        "every reader with the objects refuses it"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repair_never_overwrites_a_head_a_copied_seed_landed_after_the_plan() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "repair-race").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;
    let twin = alice.second_device(&net);
    let seed = charter_sync::device::device_seed_path(alice.home.path());
    let copy = charter_sync::device::device_seed_path(twin.home.path());
    std::fs::create_dir_all(copy.parent().expect("dir")).expect("dir");
    std::fs::copy(&seed, &copy).expect("copy seed");
    let dest = tmp.path().join("twin");
    Box::pin(clone_into(&net, &twin, &rid, &dest))
        .await
        .expect("clone");

    // Alice (who sent the current head X) plans a branch push; its upload is slow.
    let work = tmp.path().join("work");
    git(&work, &["checkout", "-q", "-b", "a-branch"]);
    commit(&work, &[("a", b"alice")], "feat: a");
    let ra = alice.repo(&rid);
    let a = Box::pin(prepare2(
        &net,
        &alice,
        &ra,
        &work,
        &[upd("refs/heads/a-branch")],
    ))
    .await
    .expect("prepare");

    // Meanwhile the twin lands Y on top of X, with a consistent nonce.
    git(&dest, &["checkout", "-q", "-b", "t-branch"]);
    commit(&dest, &[("t", b"twin")], "feat: t");
    push2(&net, &twin, &rid, &dest, &["refs/heads/t-branch"])
        .await
        .expect("twin push");

    let r = Box::pin(a.run()).await;
    assert_eq!(code(r), "E_CONCURRENT_PUSH", "no repair over a landed head");
    drop(ra);
    let view = Box::pin(charter_sync::sync(&net, &twin.repo(&rid), &twin.want()))
        .await
        .expect("the twin still fetches");
    let refs = view.peers[&alice.keys.nid()]
        .refs_body()
        .expect("refs")
        .clone();
    let names: Vec<&str> = refs.refs.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["refs/heads/main", "refs/heads/t-branch"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_base_covered_by_the_next_body_before_any_fetch_fails_the_fold() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "covered-base").await;
    let tmp = tempfile::tempdir().expect("tmp");
    pushed(&net, &alice, &rid, tmp.path(), 1).await;
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
    let work = tmp.path().join("work");
    let accepted = hex::decode(rev(&work, "HEAD")).expect("hex");
    git(&work, &["checkout", "-q", "--orphan", "x"]);
    commit(&work, &[("x", b"unrelated")], "feat: x");
    let ra = alice.repo(&rid);
    let forced = charter_sync::Update {
        src: Some("refs/heads/x".into()),
        dst: "refs/heads/main".into(),
    };
    let hp = Box::pin(prepare2(&net, &alice, &ra, &work, &[forced]))
        .await
        .expect("prepare");
    // A modified client: the cover push is listed before the forgery, so its own sync
    // never refuses the forged body.
    git(&work, &["checkout", "-q", "-b", "cover"]);
    commit(&work, &[("c", b"cover")], "feat: cover");
    let cover = Box::pin(prepare2(
        &net,
        &alice,
        &ra,
        &work,
        &[upd("refs/heads/cover")],
    ))
    .await
    .expect("prepare cover");
    let state = charter_sync::state::State::load(&ra.state_path()).expect("state");
    let mut plan = Box::pin(hp.plan(&state)).await.expect("plan");
    for e in plan.refs.iter_mut().filter(|e| e.name == "refs/heads/main") {
        e.base = accepted.clone();
    }
    Box::pin(hp.finish(plan)).await.expect("forged body lands");
    // The next body (a new branch) clears the forged base before any reader fetched.
    let state = charter_sync::state::State::load(&ra.state_path()).expect("state");
    let plan = Box::pin(cover.plan(&state)).await.expect("plan cover");
    Box::pin(cover.finish(plan)).await.expect("cover lands");
    drop(ra);

    let carol = Peer::new(&net, 3);
    let r = Box::pin(clone_into(&net, &carol, &rid, &tmp.path().join("c"))).await;
    assert_eq!(code(r), "E_BAD_REFS", "the fold sees the forged base");
}
