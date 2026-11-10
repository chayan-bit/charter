//! Collaborative-object ops (docs/p3/cob-ops.md): format rules and verification.

mod common;

use std::collections::BTreeMap;

use charter_core::*;
use charter_crypto::PublicKey;
use common::*;

/// CBOR `{"title": "t"}`.
const PAYLOAD: &[u8] = &[0xa1, 0x65, b't', b'i', b't', b'l', b'e', 0x61, b't'];

fn op(object: Hash32, action: &str, parents: Vec<Hash32>, ts_ms: u64) -> CobOpBody {
    CobOpBody {
        rid: RID,
        object,
        type_name: "xyz.arkiv.issue".into(),
        action: action.into(),
        parents,
        payload: PAYLOAD.to_vec(),
        ts_ms,
    }
}

fn keys(ps: &[&Peer]) -> BTreeMap<Nid, PublicKey> {
    ps.iter().map(|p| (p.nid, p.pk.clone())).collect()
}

fn ids(v: &[VerifiedOp]) -> Vec<Hash32> {
    v.iter().map(|o| o.id).collect()
}

#[test]
fn create_and_follow_up_ops_round_trip() {
    let (a, b) = (&peers()[0], &peers()[1]);
    let (create, obj) = observe(a, &op(ZERO, "create", vec![], 1), addr(1), 10, 0);
    let (c1, h1) = observe(b, &op(obj, "comment", vec![obj], 2), addr(2), 11, 0);
    let (c2, h2) = observe(a, &op(obj, "close", vec![obj, h1], 3), addr(1), 12, 0);
    let got = verify_ops(&RID, &keys(&[a, b]), &[c2, create, c1]);
    assert_eq!(ids(&got), vec![obj, h1, h2]);
    assert_eq!(got[0].object(), obj);
    assert_eq!(got[2].object(), obj);
    assert_eq!(got[1].author, b.nid);
    assert_eq!(got[2].body.payload, PAYLOAD);
    assert!(got[0].arkiv_order < got[1].arkiv_order);
}

#[test]
fn a_forged_signature_is_dropped() {
    let a = alice();
    let (create, obj) = observe(a, &op(ZERO, "create", vec![], 1), addr(1), 10, 0);
    let body = op(obj, "comment", vec![], 2);
    let mut env = Envelope::sign(&a.sk, Kind::Cob, body.encode()).expect("sign");
    env.sig[0] ^= 1;
    let forged = observe_bytes(&env.encode(), &body, addr(1), 11, 0);
    let got = verify_ops(&RID, &keys(&[a]), &[create, forged]);
    assert_eq!(ids(&got), vec![obj]);
}

#[test]
fn ops_from_unknown_signers_or_other_repos_are_dropped() {
    let (a, stranger) = (&peers()[0], &peers()[2]);
    let (create, obj) = observe(a, &op(ZERO, "create", vec![], 1), addr(1), 10, 0);
    let (c1, _) = observe(stranger, &op(obj, "comment", vec![], 2), addr(3), 11, 0);
    let mut other = op(obj, "comment", vec![], 3);
    other.rid = h32(0x22);
    let (c2, _) = observe(a, &other, addr(1), 12, 0);
    let got = verify_ops(&RID, &keys(&[a]), &[create, c1, c2]);
    assert_eq!(ids(&got), vec![obj]);
}

#[test]
fn parents_from_another_object_or_a_later_op_are_dropped() {
    let a = alice();
    let (x, ox) = observe(a, &op(ZERO, "create", vec![], 1), addr(1), 10, 0);
    let (y, oy) = observe(a, &op(ZERO, "create", vec![], 2), addr(1), 10, 1);
    let (later, hl) = observe(a, &op(ox, "comment", vec![], 9), addr(1), 13, 0);
    let (cross, _) = observe(a, &op(ox, "comment", vec![oy], 3), addr(1), 11, 0);
    let (early, _) = observe(a, &op(ox, "comment", vec![hl], 4), addr(1), 12, 0);
    let (orphan, _) = observe(a, &op(h32(0x77), "comment", vec![], 5), addr(1), 12, 1);
    let got = verify_ops(&RID, &keys(&[a]), &[x, y, later, cross, early, orphan]);
    assert_eq!(ids(&got), vec![ox, oy, hl]);
}

#[test]
fn ordering_follows_arkiv_creation_order_not_ts_ms() {
    let a = alice();
    let (create, obj) = observe(a, &op(ZERO, "create", vec![], 50), addr(1), 10, 0);
    let (first, h1) = observe(a, &op(obj, "comment", vec![], 900), addr(1), 11, 0);
    let (second, h2) = observe(a, &op(obj, "comment", vec![], 100), addr(1), 11, 1);
    let got = verify_ops(&RID, &keys(&[a]), &[second, create, first]);
    assert_eq!(ids(&got), vec![obj, h1, h2]);
}

#[test]
fn a_replayed_op_counts_once_and_ops_must_match_the_object_type() {
    let a = alice();
    let (create, obj) = observe(a, &op(ZERO, "create", vec![], 1), addr(1), 10, 0);
    let (bytes, h1) = sign(a, &op(obj, "comment", vec![], 2));
    let c1 = observe_bytes(&bytes, &op(obj, "comment", vec![], 2), addr(1), 11, 0);
    let replay = observe_bytes(&bytes, &op(obj, "comment", vec![], 2), addr(9), 12, 0);
    let mut patch = op(obj, "comment", vec![], 3);
    patch.type_name = "xyz.arkiv.patch".into();
    let (wrong_type, _) = observe(a, &patch, addr(1), 13, 0);
    let got = verify_ops(&RID, &keys(&[a]), &[create, c1, replay, wrong_type]);
    assert_eq!(ids(&got), vec![obj, h1]);
    assert_eq!(got[1].arkiv_order.block, 11);
}

fn rejects(b: &CobOpBody) -> bool {
    matches!(b.validate(), Err(FormatError::Field { .. }))
}

#[test]
fn field_rules_reject_bad_names_actions_and_create_shape() {
    assert!(op(ZERO, "create", vec![], 1).validate().is_ok());
    let mut b = op(ZERO, "create", vec![], 1);
    b.type_name = "Xyz".into();
    assert!(rejects(&b));
    b.type_name = "a".repeat(65);
    assert!(rejects(&b));
    b.type_name = String::new();
    assert!(rejects(&b));
    assert!(rejects(&op(ZERO, &"a".repeat(33), vec![], 1)));
    assert!(rejects(&op(ZERO, "comment", vec![], 1)), "zero object");
    assert!(
        rejects(&op(h32(1), "create", vec![], 1)),
        "create on object"
    );
    assert!(
        rejects(&op(ZERO, "create", vec![h32(1)], 1)),
        "create parents"
    );
}

#[test]
fn payloads_must_be_small_canonical_cbor_without_floats() {
    let with = |payload: Vec<u8>| CobOpBody {
        payload,
        ..op(ZERO, "create", vec![], 1)
    };
    let oversize = [vec![0x5a, 0, 1, 0, 0], vec![7; 65_536]].concat();
    assert!(oversize.len() > MAX_PAYLOAD);
    assert!(rejects(&with(oversize)), "oversize");
    assert!(rejects(&with(vec![0xf9, 0x3c, 0x00])), "f16 float");
    assert!(
        rejects(&with(vec![0xfb, 0, 0, 0, 0, 0, 0, 0, 0])),
        "f64 float"
    );
    assert!(rejects(&with(vec![0x18, 0x01])), "non-shortest int");
    assert!(
        rejects(&with(vec![0xa2, 0x61, b'b', 0, 0x61, b'a', 0])),
        "keys"
    );
    assert!(rejects(&with(vec![0x9f, 0xff])), "indefinite array");
    assert!(rejects(&with(vec![0xc1, 0x00])), "tag");
    assert!(rejects(&with(vec![0x01, 0x02])), "trailing");
    assert!(rejects(&with(vec![])), "empty");
    assert!(
        rejects(&with([vec![0x81; 100], vec![0]].concat())),
        "too deep"
    );
    let nested = [0x82, 0xa1, 0x61, b'a', 0x20, 0xf6].to_vec();
    assert!(with(nested).validate().is_ok());
    let big = with([vec![0x59, 0xff, 0xf0], vec![7; 65_520]].concat());
    assert!(big.validate().is_ok(), "payload cap alone is 64 KiB");
    assert!(
        Envelope::sign(&alice().sk, Kind::Cob, big.encode()).is_err(),
        "but the envelope cap still binds"
    );
}

#[test]
fn attributes_carry_query_keys_within_arkiv_limits() {
    let a = alice();
    let (create, obj) = observe(a, &op(ZERO, "create", vec![], 1), addr(1), 10, 0);
    assert_eq!(create.attr("ag.cob"), Some(&AttrValue::Bytes32(obj)));
    assert_eq!(
        create.attr("ag.type"),
        Some(&AttrValue::Str("xyz.arkiv.issue".into()))
    );
    let (c1, h1) = observe(a, &op(obj, "comment", vec![], 2), addr(1), 11, 0);
    assert_eq!(c1.attr("ag.cob"), Some(&AttrValue::Bytes32(obj)));
    assert_eq!(
        c1.attr("ag.action"),
        Some(&AttrValue::Str("comment".into()))
    );
    assert_eq!(c1.attr("ag.nid"), Some(&AttrValue::Bytes32(a.nid)));
    assert_eq!(c1.attr("ag.h"), Some(&AttrValue::Bytes32(h1)));
    assert_eq!(c1.attr("ag.rid"), Some(&AttrValue::Bytes32(RID)));
    check_arkiv_rules(&c1.attributes).expect("arkiv rules");
}

#[test]
fn ops_after_prior_accept_follow_ups_of_known_objects_only_once() {
    let (a, b) = (&peers()[0], &peers()[1]);
    let (create, obj) = observe(a, &op(ZERO, "create", vec![], 1), addr(1), 10, 0);
    let (c1, h1) = observe(b, &op(obj, "comment", vec![obj], 2), addr(2), 11, 0);
    let prior = verify_ops(&RID, &keys(&[a, b]), &[create.clone(), c1.clone()]);
    let (c2, h2) = observe(a, &op(obj, "close", vec![obj, h1], 3), addr(1), 12, 0);
    let (orphan, _) = observe(a, &op([7; 32], "comment", vec![], 4), addr(1), 12, 1);
    let got = verify_ops_after(&RID, &keys(&[a, b]), &prior, &[create, c1, orphan, c2]);
    assert_eq!(
        ids(&got),
        vec![h2],
        "prior ops are not repeated; follow-ups see them"
    );
    assert!(verify_ops_after(&RID, &keys(&[a]), &[], &[]).is_empty());
}
