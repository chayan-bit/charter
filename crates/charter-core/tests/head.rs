//! Format v2 (docs/p5/head-model.md sections 4 to 6): device delegation, device heads and
//! the rollback check over signed, observed entities.

mod common;

use charter_core::*;
use common::history::*;
use common::*;

const DEV_CONTROL: Address = [0xd0; 20];
const DEV_BULK: Address = [0xd1; 20];

fn device_body(rid: Rid, dev: &Peer) -> DeviceBody {
    DeviceBody {
        rid,
        alg: dev.pk.alg().id(),
        pk: dev.pk.as_bytes().to_vec(),
        control: DEV_CONTROL,
        bulk: DEV_BULK,
    }
}

fn head_body(rid: Rid, seq: u64, pieces_created: u64, prev_tx: Hash32) -> HeadBody {
    HeadBody {
        rid,
        nid: alice().nid,
        seq,
        id_head: rid,
        at: 40,
        pieces_created,
        refs: vec![HeadRef {
            name: "refs/heads/main".into(),
            oid: vec![seq as u8 + 1; 20],
            counter: seq + 1,
            stamp: 0,
            base: vec![],
        }],
        snapshot: vec![],
        packs: vec![],
        prev_tx,
        object_hash: 1,
    }
}

struct Setup {
    peer: PeerInfo,
    identity: IdentityChain,
    device: Device,
}

fn setup() -> Setup {
    let w = world();
    let dev = &peers()[1];
    let peer = peer_info(&w.binds[..1]);
    let identity = identity_chain(
        &w.rid,
        &peers_map(peer.clone()),
        std::slice::from_ref(&w.root_obs),
        None,
    )
    .expect("identity");
    let (d_obs, _) = observe(alice(), &device_body(w.rid, dev), CONTROL0, 30, 0);
    let devices = devices(&w.rid, &peer, std::slice::from_ref(&d_obs));
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].nid, dev.nid);
    Setup {
        peer,
        identity,
        device: devices[0].clone(),
    }
}

fn nonce(n: u64) -> HeadNonce {
    HeadNonce {
        address: DEV_CONTROL,
        before: n,
        after: n,
    }
}

#[test]
fn a_v1_reader_sees_the_device_delegation_as_an_unsupported_anchor() {
    let w = world();
    let (obs, _) = observe(alice(), &device_body(w.rid, &peers()[1]), CONTROL0, 30, 0);
    assert!(is_device_row(&obs));
    let err = open::<AnchorBody>(&obs).expect_err("v0.3.0 cannot read it");
    assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
}

#[test]
fn delegations_need_the_identity_signature_and_its_control_address() {
    let w = world();
    let s = setup();
    let dev = &peers()[1];
    let (self_signed, _) = observe(dev, &device_body(w.rid, dev), CONTROL0, 30, 0);
    let (foreign, _) = observe(alice(), &device_body(w.rid, dev), FOREIGN, 30, 0);
    assert!(devices(&w.rid, &s.peer, &[self_signed, foreign]).is_empty());
}

#[test]
fn heads_are_accepted_only_at_the_current_nonce() {
    let w = world();
    let s = setup();
    let dev = &peers()[1];
    // seq 0: head + 1 piece; seq 1 (refs-only): + marker piece; seq 2: + 3 bulk-free inline pieces.
    let bodies = [
        head_body(w.rid, 0, 1, ZERO),
        head_body(w.rid, 1, 2, h32(1)),
        head_body(w.rid, 2, 5, h32(2)),
    ];
    let mut mark: Option<PeerMark> = None;
    for (i, b) in bodies.iter().enumerate() {
        let (obs, _) = observe(dev, b, DEV_CONTROL, 31, 0);
        let got = accept_device_head(
            &w.rid,
            &alice().nid,
            &s.device,
            &s.identity,
            std::slice::from_ref(&obs),
            &nonce(b.pieces_created + 1),
            mark.as_ref(),
        )
        .expect("current head accepted")
        .expect("head present");
        assert_eq!(got.body.seq, i as u64);
        mark = Some(got.mark());
        if i > 0 {
            // A node serving the previous body while C_d's nonce moved on: rollback.
            let (old, _) = observe(dev, &bodies[i - 1], DEV_CONTROL, 31, 0);
            let err = accept_device_head(
                &w.rid,
                &alice().nid,
                &s.device,
                &s.identity,
                &[old],
                &nonce(b.pieces_created + 1),
                None,
            )
            .expect_err("rolled-back head");
            assert_eq!(err.code(), ErrorCode::ControlNotLive);
        }
    }
    // Below the local mark even if a node lied about the nonce too.
    let (old, _) = observe(dev, &bodies[0], DEV_CONTROL, 31, 0);
    let err = accept_device_head(
        &w.rid,
        &alice().nid,
        &s.device,
        &s.identity,
        &[old],
        &nonce(2),
        mark.as_ref(),
    )
    .expect_err("below mark");
    assert_eq!(err.code(), ErrorCode::StaleHead);
}

#[test]
fn a_head_from_another_creator_or_signer_is_ignored_and_fails_closed() {
    let w = world();
    let s = setup();
    let b = head_body(w.rid, 0, 1, ZERO);
    let (wrong_creator, _) = observe(&peers()[1], &b, DEV_BULK, 31, 0);
    let (wrong_signer, _) = observe(&peers()[2], &b, DEV_CONTROL, 31, 0);
    let err = accept_device_head(
        &w.rid,
        &alice().nid,
        &s.device,
        &s.identity,
        &[wrong_creator, wrong_signer],
        &nonce(2),
        None,
    )
    .expect_err("C_d created entities but no valid head is live");
    assert_eq!(err.code(), ErrorCode::ControlNotLive);
}

#[test]
fn the_delegation_stays_in_every_incremental_v1_anchor_read() {
    let w = world();
    let (obs, _) = observe(alice(), &device_body(w.rid, &peers()[1]), CONTROL0, 30, 0);
    assert_eq!(obs.attr("ag.seq"), Some(&AttrValue::U64(u64::MAX)));
}

#[test]
fn oid_and_base_lengths_follow_the_object_format() {
    let w = world();
    let mut b = head_body(w.rid, 0, 1, ZERO);
    assert!(b.validate().is_ok());
    b.object_hash = 2;
    assert!(b.validate().is_err(), "SHA-1 oid in a SHA-256 head");
    b.object_hash = 1;
    b.refs[0].base = vec![1; 32];
    assert!(b.validate().is_err(), "base of the wrong length");
    b.refs[0].base = vec![];
    b.object_hash = 3;
    assert!(b.validate().is_err());
}
