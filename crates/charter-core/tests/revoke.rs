//! Revoke records (docs/p5/head-model.md section 15): only the identity revokes, the first
//! record per device counts, the cut body must be the one named, and a v0.5.0 reader stops.

mod common;

use charter_core::*;
use common::history::*;
use common::*;

const DEV_CONTROL: Address = [0xd0; 20];
const DEV_BULK: Address = [0xd1; 20];

fn device(w: &World) -> (PeerInfo, IdentityChain, Device) {
    let dev = &peers()[1];
    let peer = peer_info(&w.binds[..1]);
    let identity = identity_chain(
        &w.rid,
        &peers_map(peer.clone()),
        std::slice::from_ref(&w.root_obs),
        None,
    )
    .expect("identity");
    let body = DeviceBody {
        rid: w.rid,
        alg: dev.pk.alg().id(),
        pk: dev.pk.as_bytes().to_vec(),
        control: DEV_CONTROL,
        bulk: DEV_BULK,
    };
    let (d_obs, _) = observe(alice(), &body, CONTROL0, 30, 0);
    let d = devices(&w.rid, &peer, std::slice::from_ref(&d_obs)).remove(0);
    (peer, identity, d)
}

fn ref_main(seq: u64) -> HeadRef {
    HeadRef {
        name: "refs/heads/main".into(),
        oid: vec![seq as u8 + 1; 20],
        counter: seq + 1,
        stamp: 0,
        base: vec![],
    }
}

fn v3(rid: Rid, seq: u64, pieces: u32) -> HeadBodyV3 {
    let positions: Vec<(u64, u32)> = (0..pieces)
        .map(|i| (900 + u64::from(i / 20), i % 20))
        .collect();
    HeadBodyV3 {
        rid,
        nid: alice().nid,
        seq,
        id_head: rid,
        at: 40,
        pieces_created: 1,
        refs: vec![ref_main(seq)],
        snapshot: vec![[5; 32]],
        packs: vec![PackV3 {
            pack_id: [6; 32],
            codec: 1,
            runs: runs(&positions),
        }],
        prev_tx: if seq == 0 { ZERO } else { [8; 32] },
        object_hash: 1,
        refs_table: None,
    }
}

fn record(rid: Rid, device: Nid, seq: u64, h: Hash32) -> RevokeBody {
    RevokeBody {
        rid,
        device,
        seq,
        h,
        tx: if h == ZERO { ZERO } else { [4; 32] },
    }
}

#[test]
fn only_identity_signed_records_from_its_control_address_count_and_the_first_wins() {
    let w = world();
    let (peer, _, d) = device(w);
    let first = record(w.rid, d.nid, 1, [7; 32]);
    let later = record(w.rid, d.nid, 2, [8; 32]);
    let (a, _) = observe(alice(), &first, CONTROL0, 50, 0);
    let (b, _) = observe(alice(), &later, CONTROL0, 51, 0);
    // A device key cannot sign as the identity; nor does a non-control creator count.
    let (by_device, _) = observe(&peers()[1], &later, CONTROL0, 49, 0);
    let (elsewhere, _) = observe(alice(), &later, DEV_CONTROL, 48, 0);
    let got = revocations(&w.rid, &peer, &[b, a, by_device, elsewhere]);
    assert_eq!(got.len(), 1);
    assert_eq!((got[&d.nid].seq, got[&d.nid].h), (1, [7; 32]));
}

#[test]
fn a_revoke_record_names_a_body_exactly_when_it_names_its_tx() {
    let w = world();
    let mut r = record(w.rid, [1; 32], 3, [7; 32]);
    assert!(r.validate().is_ok());
    r.tx = ZERO;
    assert!(r.validate().is_err());
    let none = record(w.rid, [1; 32], 0, ZERO);
    assert!(none.validate().is_ok());
    assert!(RevokeBody { seq: 2, ..none }.validate().is_err());
}

#[test]
fn the_v050_reader_stops_on_a_revoke_record_with_unsupported_version() {
    let w = world();
    let (row, _) = observe(alice(), &record(w.rid, [1; 32], 0, ZERO), CONTROL0, 50, 0);
    assert!(is_revoke_row(&row) && !is_device_row(&row));
    // v0.5.0 opens anchors of `C_N` as v1 anchors (after setting delegations aside): an
    // unknown layout version, which its anchor chain turns into `E_UNSUPPORTED_VERSION`.
    let err = open::<AnchorBody>(&row)
        .map(|_| ())
        .expect_err("not a v1 anchor");
    assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
}

#[test]
fn a_cut_body_is_accepted_only_when_it_is_the_body_the_record_names() {
    let w = world();
    let (_, identity, d) = device(w);
    let body = v3(w.rid, 2, 1);
    let (row, h) = observe(&peers()[1], &body, DEV_CONTROL, 41, 0);
    let named = Revocation {
        device: d.nid,
        seq: 2,
        h,
        tx: [4; 32],
        record: [5; 32],
        block: 60,
    };
    let head = accept_cut_head(&w.rid, &alice().nid, &d, &identity, &named, &row, 41).expect("cut");
    assert_eq!((head.h, head.body.seq, head.format), (h, 2, HeadFormat::V3));
    for wrong in [
        Revocation {
            h: [9; 32],
            ..named.clone()
        },
        Revocation {
            seq: 3,
            ..named.clone()
        },
    ] {
        let err = accept_cut_head(&w.rid, &alice().nid, &d, &identity, &wrong, &row, 41)
            .expect_err("not the named body");
        assert_eq!(err.code(), ErrorCode::BadRefs);
    }
}
