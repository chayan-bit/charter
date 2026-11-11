//! Format v3 (docs/p5/head-model.md section 13): compact piece runs, pack codecs, minimal
//! head patches, and the stop of a v0.4.0 reader on a v3 head.
//!
//! `accept_device_head`, `open` and the v2 body here are unchanged since v0.4.0 (bda7571),
//! so the first test runs the v0.4.0 reader itself against v3 rows.

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
fn the_v040_reader_stops_on_a_v3_head_with_unsupported_version() {
    let w = world();
    let (_, identity, d) = device(w);
    let (row, _) = observe(&peers()[1], &v3(w.rid, 0, 3), DEV_CONTROL, 41, 0);
    assert_eq!(row.attr("ag.v"), Some(&AttrValue::U64(3)));
    assert_eq!(row.attr("ag.kind"), Some(&AttrValue::Str("head".into())));
    let err = accept_device_head(
        &w.rid,
        &alice().nid,
        &d,
        &identity,
        std::slice::from_ref(&row),
        &nonce(2),
        None,
    )
    .expect_err("v0.4.0 cannot read v3");
    assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
    assert_eq!(err.code().as_str(), "E_UNSUPPORTED_VERSION");
}

#[test]
fn a_v3_body_round_trips_and_its_signature_is_domain_separated() {
    let w = world();
    let body = v3(w.rid, 1, 34);
    assert_eq!(HeadBodyV3::decode(&body.encode()).expect("decode"), body);
    let (bytes, h) = sign(&peers()[1], &body);
    let env = Envelope::decode(&bytes).expect("env");
    assert_eq!(env.kind, Kind::HeadV3);
    assert_eq!(h, content_hash(Kind::HeadV3, &bytes));
    assert!(env.verify(&peers()[1].pk).is_ok());
    // The same body bytes signed as a v2 head never verify as v3.
    let as_v2 = Envelope {
        kind: Kind::Head,
        ..env.clone()
    };
    assert!(as_v2.verify(&peers()[1].pk).is_err());
}

#[test]
fn compact_runs_cut_a_34_piece_pack_list_by_over_90_percent() {
    let w = world();
    let pieces: Vec<PieceRef> = (0..34)
        .map(|i| PieceRef {
            tx: [i as u8 + 1; 32],
            sha256: [i as u8 + 2; 32],
        })
        .collect();
    let v2_packs = minicbor::to_vec(vec![HeadPack {
        pack_id: [6; 32],
        pieces,
    }])
    .expect("cbor");
    let v3_packs = minicbor::to_vec(&v3(w.rid, 1, 34).packs).expect("cbor");
    assert_eq!(v3(w.rid, 1, 34).packs[0].runs.len(), 2, "two blocks of txs");
    assert!(
        v3_packs.len() * 10 < v2_packs.len(),
        "{} vs {}",
        v3_packs.len(),
        v2_packs.len()
    );
}

#[test]
fn runs_merge_consecutive_txs_of_one_block_only() {
    let r = runs(&[(0, 0), (7, 3), (7, 4), (7, 6), (8, 7)]);
    assert_eq!(
        r,
        vec![
            PieceRun::HERE,
            PieceRun {
                block: 7,
                index: 3,
                count: 2
            },
            PieceRun {
                block: 7,
                index: 6,
                count: 1
            },
            PieceRun {
                block: 8,
                index: 7,
                count: 1
            },
        ]
    );
}

#[test]
fn bad_runs_codecs_and_oversized_lists_are_refused() {
    let w = world();
    let mut b = v3(w.rid, 1, 1);
    b.packs[0].codec = 2;
    assert!(HeadBodyV3::decode(&b.encode()).is_err(), "unknown codec");
    let mut b = v3(w.rid, 1, 1);
    b.packs[0].runs = vec![PieceRun {
        block: 0,
        index: 1,
        count: 1,
    }];
    assert!(
        HeadBodyV3::decode(&b.encode()).is_err(),
        "block 0 is only this tx"
    );
    let mut b = v3(w.rid, 1, 1);
    b.packs[0].runs = vec![PieceRun {
        block: 9,
        index: 0,
        count: MAX_HEAD_PIECES as u32 + 1,
    }];
    assert!(HeadBodyV3::decode(&b.encode()).is_err(), "too many pieces");
    let mut b = v3(w.rid, 1, 1);
    b.prev_tx = ZERO;
    assert!(
        HeadBodyV3::decode(&b.encode()).is_err(),
        "v2 rules still apply"
    );
}

#[test]
fn a_minimal_patch_carries_only_body_and_hash_and_opens_from_calldata() {
    let w = world();
    let body = v3(w.rid, 2, 1);
    let (bytes, h) = sign(&peers()[1], &body);
    let attrs = head_patch_attributes(&bytes);
    let names: Vec<&str> = attrs.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["ag.body", "ag.h"]);
    // Signatures are randomized: both rows carry the same envelope bytes.
    let full = observe_bytes(&bytes, &body, DEV_CONTROL, 41, 0);
    let minimal = Observed {
        attributes: attrs,
        ..full.clone()
    };
    for row in [&full, &minimal] {
        let (env, got, got_h) = open_head_calldata(row).expect("open");
        assert_eq!((got, got_h), (body.clone(), h));
        assert!(env.verify(&peers()[1].pk).is_ok());
    }
    let mut forged = minimal.clone();
    for a in forged.attributes.iter_mut().filter(|a| a.name == "ag.h") {
        a.value = AttrValue::Bytes32([0; 32]);
    }
    assert!(
        open_head_calldata(&forged).is_err(),
        "ag.h must match the body"
    );
}
