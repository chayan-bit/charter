//! Issue #27 (docs/p5/head-model.md section 15): the identity revokes one of its devices;
//! readers keep that device's writes up to the body the record names and ignore every later
//! one, a device key can never revoke, and a revoked device can no longer push.

mod common;

use charter_core::{Body, ErrorCode, Nid, RevokeBody, Rid, ZERO};
use charter_sync::revoke::RevokeOutcome;
use common::*;

fn dev_nid(p: &Peer) -> Nid {
    charter_sync::device::device_keys(p.home.path())
        .expect("device")
        .nid()
}

/// `who` (holding the identity key) revokes `device` from a fresh sync.
async fn revoke(
    net: &Net,
    who: &Peer,
    rid: &Rid,
    device: &Nid,
) -> charter_sync::Result<RevokeOutcome> {
    let repo = who.repo(rid);
    Box::pin(charter_sync::revoke::revoke_device(
        net,
        &who.cfg,
        &who.keys,
        &repo,
        device,
        &dev_nid(who),
    ))
    .await
}

async fn refs_of(net: &Net, viewer: &Peer, rid: &Rid, owner: &Peer) -> charter_core::RefsBody {
    let view = Box::pin(charter_sync::sync(net, &viewer.repo(rid), &viewer.want()))
        .await
        .expect("sync");
    view.peers[&owner.keys.nid()]
        .refs_body()
        .expect("refs")
        .clone()
}

async fn pin_program(net: &Net, p: &Peer, rid: &Rid) {
    let src = include_str!("../../../programs/bounty.rhai");
    charter_sync::program::publish(net, &p.cfg, &p.keys, rid, "rhai/1", src)
        .await
        .expect("publish");
    let pin = charter_core::ProgramPin {
        engine: "rhai/1".into(),
        hash: charter_program::Program::hash(src),
    };
    let st = charter_sync::state::State::default();
    charter_sync::identity::propose(net, &p.cfg, &p.keys, rid, &st, |b| charter_core::IdBody {
        program: Some(pin),
        ..b.clone()
    })
    .await
    .expect("pin");
}

fn oid_of(refs: &charter_core::RefsBody, name: &str) -> Option<String> {
    refs.get(name).map(hex::encode)
}

/// alice pushes `main`, her laptop (a second device of the same identity) pushes `topic`.
async fn two_devices(net: &Net, tmp: &std::path::Path) -> (Peer, Peer, Rid, std::path::PathBuf) {
    let alice = Peer::new(net, 1);
    let rid = init(net, &alice, "revoke").await;
    let work = work_repo(tmp, "work");
    commit(&work, &[("a", b"one")], "feat: one");
    Box::pin(push2(net, &alice, &rid, &work, &["refs/heads/main"]))
        .await
        .expect("alice main");
    let laptop = alice.second_device(net);
    let lap = work_repo(tmp, "laptop");
    git(
        &lap,
        &[
            "fetch",
            "-q",
            &work.to_string_lossy(),
            "main:refs/remotes/w/main",
        ],
    );
    git(
        &lap,
        &["checkout", "-q", "-B", "topic", "refs/remotes/w/main"],
    );
    commit(&lap, &[("t", b"t1")], "feat: t1");
    Box::pin(push2(net, &laptop, &rid, &lap, &["refs/heads/topic"]))
        .await
        .expect("laptop topic");
    (alice, laptop, rid, lap)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoke_keeps_the_cut_and_drops_a_racing_later_head_of_the_device() {
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, laptop, rid, lap) = two_devices(&net, tmp.path()).await;
    pin_program(&net, &alice, &rid).await;
    let t1 = rev(&lap, "topic");
    // The revoker reads the laptop's head (T1), then the laptop lands T2 before the revoke.
    let repo = alice.repo(&rid);
    let view = Box::pin(charter_sync::sync(&net, &repo, &alice.want()))
        .await
        .expect("sync");
    commit(&lap, &[("t", b"t2")], "feat: t2");
    Box::pin(push2(&net, &laptop, &rid, &lap, &["refs/heads/topic"]))
        .await
        .expect("racing push lands first");
    let body = charter_sync::revoke::revoke(
        &net,
        &alice.cfg,
        &alice.keys,
        &view,
        &dev_nid(&laptop),
        &dev_nid(&alice),
    )
    .await
    .expect("revoke");
    drop(repo);
    assert_ne!(body.h, ZERO);

    let bob = Peer::new(&net, 2);
    let refs = refs_of(&net, &bob, &rid, &alice).await;
    assert_eq!(
        oid_of(&refs, "refs/heads/topic"),
        Some(t1.clone()),
        "the cut stays, T2 is dropped"
    );
    assert!(refs.get("refs/heads/main").is_some());
    let view = Box::pin(charter_sync::sync(&net, &bob.repo(&rid), &bob.want()))
        .await
        .expect("sync");
    assert!(
        view.program.is_some(),
        "the fold walks the revoked device from its cut"
    );
    let listed = charter_sync::revoke::list(&view, &alice.keys.nid());
    let lap_row = listed
        .iter()
        .find(|d| d.nid == dev_nid(&laptop))
        .expect("listed");
    assert_eq!(lap_row.revoked, Some(Some(body.seq)));

    // The laptop cannot push any more; alice's own device still can.
    commit(&lap, &[("t", b"t3")], "feat: t3");
    let err = Box::pin(push2(&net, &laptop, &rid, &lap, &["refs/heads/topic"]))
        .await
        .expect_err("revoked");
    assert_eq!(code(Err::<(), _>(err)), ErrorCode::DeviceRevoked.as_str());
    let work = tmp.path().join("work");
    commit(&work, &[("a", b"two")], "feat: two");
    Box::pin(push2(&net, &alice, &rid, &work, &["refs/heads/main"]))
        .await
        .expect("alice pushes after the revoke");
    let refs = refs_of(&net, &bob, &rid, &alice).await;
    assert_eq!(oid_of(&refs, "refs/heads/main"), Some(rev(&work, "main")));
    assert_eq!(oid_of(&refs, "refs/heads/topic"), Some(t1));
}

#[tokio::test(flavor = "multi_thread")]
async fn only_the_identity_key_from_its_control_address_can_revoke() {
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, laptop, rid, _lap) = two_devices(&net, tmp.path()).await;
    let lap_dev = charter_sync::device::device_keys(laptop.home.path()).expect("dev");
    let forged = RevokeBody {
        rid,
        device: dev_nid(&alice),
        seq: 0,
        h: ZERO,
        tx: ZERO,
    };
    assert!(forged.validate().is_ok());
    let by_device = charter_sync::write::sign(lap_dev.sk(), &forged).expect("sign");
    let by_identity = charter_sync::write::sign(alice.keys.sk(), &forged).expect("sign");
    // Neither the device key, nor the identity key from an address that is not the
    // identity's control address, makes a record readers accept.
    let other = outsider(&net);
    for bytes in [&by_device.bytes, &by_identity.bytes] {
        let op = charter_sync::write::entity_op::<RevokeBody>(bytes, 1_000).expect("op");
        send_ops(&net, &other, vec![op]).await;
    }
    let repo = alice.repo(&rid);
    let view = Box::pin(charter_sync::sync(&net, &repo, &alice.want()))
        .await
        .expect("sync");
    drop(repo);
    let listed = charter_sync::revoke::list(&view, &alice.keys.nid());
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|d| d.revoked.is_none()), "{listed:?}");
    let bob = Peer::new(&net, 2);
    let refs = refs_of(&net, &bob, &rid, &alice).await;
    assert!(refs.get("refs/heads/main").is_some() && refs.get("refs/heads/topic").is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoke_ends_the_denial_of_service_of_a_stolen_control_address() {
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, laptop, rid, lap) = two_devices(&net, tmp.path()).await;
    let t1 = rev(&lap, "topic");
    // alice's machine verified the laptop's head once.
    let _ = refs_of(&net, &alice, &rid, &alice).await;
    // A thief with the laptop's control key creates one entity outside a head tx: every
    // reader now fails closed on that device (head-model 6).
    let c_d = charter_sync::device::device_keys(laptop.home.path())
        .expect("dev")
        .control(&rid);
    net.fund(c_d.address());
    send_ops(
        &net,
        &c_d,
        vec![charter_sync::pieces::marker_op().expect("op")],
    )
    .await;
    let bob = Peer::new(&net, 2);
    let err = Box::pin(charter_sync::sync(&net, &bob.repo(&rid), &bob.want()))
        .await
        .map(|_| ());
    assert_eq!(code(err), ErrorCode::ControlNotLive.as_str());

    // The identity revokes the laptop at the head its machine last verified.
    let out = revoke(&net, &alice, &rid, &dev_nid(&laptop))
        .await
        .expect("revoke");
    assert_ne!(out.record.h, ZERO);
    let refs = refs_of(&net, &bob, &rid, &alice).await;
    assert_eq!(
        oid_of(&refs, "refs/heads/topic"),
        Some(t1),
        "readers recover, the cut stays"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_device_cannot_revoke_this_machines_own_device_or_one_twice() {
    let net = Net::new();
    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, laptop, rid, _lap) = two_devices(&net, tmp.path()).await;
    let own = revoke(&net, &alice, &rid, &dev_nid(&alice))
        .await
        .map(|_| ());
    assert_eq!(code(own), ErrorCode::BadFormat.as_str());
    revoke(&net, &alice, &rid, &dev_nid(&laptop))
        .await
        .expect("first");
    let twice = revoke(&net, &alice, &rid, &dev_nid(&laptop))
        .await
        .map(|_| ());
    assert_eq!(code(twice), ErrorCode::BadFormat.as_str());
}
