//! Format v4 (docs/p5/head-model.md section 14): a head that names a ref table, the stop
//! of a v0.5.0 reader on it, and the byte identity of v3 bodies without a table.

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

fn nonce(n: u64) -> HeadNonce {
    HeadNonce {
        address: DEV_CONTROL,
        before: n,
        after: n,
    }
}

fn table() -> RefTable {
    RefTable {
        table_id: [9; 32],
        codec: 0,
        runs: vec![PieceRun {
            block: 39,
            index: 0,
            count: 2,
        }],
        stamp: 0,
    }
}

fn v4(rid: Rid) -> HeadBodyV4 {
    HeadBodyV4(HeadBodyV3 {
        refs_table: Some(table()),
        ..v3(rid, 0, 3)
    })
}

#[test]
fn a_v3_body_without_a_table_encodes_no_field_12() {
    let body = v3(world().rid, 0, 3);
    let bytes = body.encode();
    // A CBOR map of 11 pairs (0xab), as v0.5.0 wrote it.
    assert_eq!(bytes[0], 0xab, "map of 11 fields");
    assert_eq!(HeadBodyV3::decode(&bytes).expect("decode"), body);
    let with = v4(world().rid);
    assert_eq!(with.encode()[0], 0xac, "a v4 body adds exactly field 12");
}

#[test]
fn a_v4_head_is_accepted_with_its_table_and_inline_refs() {
    let w = world();
    let (_, identity, d) = device(w);
    let body = v4(w.rid);
    let (row, h) = observe(&peers()[1], &body, DEV_CONTROL, 41, 0);
    assert_eq!(row.attr("ag.v"), Some(&AttrValue::U64(4)));
    let head = accept_device_head(
        &w.rid,
        &alice().nid,
        &d,
        &identity,
        std::slice::from_ref(&row),
        &nonce(2),
        None,
    )
    .expect("v4 head")
    .expect("present");
    assert_eq!(head.format, HeadFormat::V4);
    assert_eq!(head.h, h);
    assert_eq!(head.refs_table, Some(table()));
    assert_eq!(
        head.body.refs, body.0.refs,
        "inline entries until the table is read"
    );
}

#[test]
fn the_v050_reader_stops_on_a_v4_head_with_unsupported_version() {
    let w = world();
    let (row, _) = observe(&peers()[1], &v4(w.rid), DEV_CONTROL, 41, 0);
    // v0.5.0 opens head rows as v2 and v3 bodies only: both see an unknown layout version,
    // which its reader turns into `E_UNSUPPORTED_VERSION` (head-model 14.5).
    for err in [
        open::<HeadBody>(&row).map(|_| ()).expect_err("not v2"),
        open::<HeadBodyV3>(&row).map(|_| ()).expect_err("not v3"),
    ] {
        assert!(matches!(err, FormatError::Unsupported { .. }), "{err:?}");
        assert_eq!(err.code(), ErrorCode::UnsupportedVersion);
    }
}

#[test]
fn a_v4_signature_never_opens_as_v3_and_a_v3_body_cannot_carry_a_table() {
    let w = world();
    let body = v4(w.rid);
    let (bytes, _) = sign(&peers()[1], &body);
    let env = Envelope::decode(&bytes).expect("env");
    assert_eq!(env.kind, Kind::HeadV4);
    assert!(
        HeadBodyV3::decode(&env.body).is_err(),
        "field 12 is v4 only"
    );
    let mut no_table = body.clone();
    no_table.0.refs_table = None;
    assert!(
        HeadBodyV4::decode(&no_table.encode()).is_err(),
        "v4 needs field 12"
    );
}

#[test]
fn a_v4_minimal_patch_opens_from_calldata_with_its_table() {
    let w = world();
    let body = v4(w.rid);
    let (bytes, h) = sign(&peers()[1], &body);
    let attrs = head_patch_attributes(Kind::HeadV4, &bytes);
    let full = observe_bytes(&bytes, &body, DEV_CONTROL, 41, 0);
    let minimal = Observed {
        attributes: attrs,
        ..full.clone()
    };
    for row in [&full, &minimal] {
        let (env, got, got_h) = open_head_calldata(row).expect("open");
        assert_eq!((got, got_h), (body.0.clone(), h));
        assert_eq!(env.kind, Kind::HeadV4);
    }
    let v3_hash = Observed {
        attributes: head_patch_attributes(Kind::HeadV3, &bytes),
        ..full
    };
    assert!(open_head_calldata(&v3_hash).is_err(), "ag.h names the kind");
}
