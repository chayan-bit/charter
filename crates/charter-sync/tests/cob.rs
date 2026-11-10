//! Collaborative-object ops through the write path and back (docs/p3/cob-ops.md).

mod common;

use charter_core::{CobOpBody, Hash32, Kind, Rid, ZERO};
use charter_sync::cob::{self, json};
use common::*;
use serde_json::json;

fn op(
    rid: &Rid,
    object: Hash32,
    action: &str,
    parents: Vec<Hash32>,
    v: serde_json::Value,
) -> CobOpBody {
    CobOpBody {
        rid: *rid,
        object,
        type_name: "xyz.arkiv.issue".into(),
        action: action.into(),
        parents,
        payload: json::to_cbor(&v).expect("payload"),
        ts_ms: 1,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ops_post_and_list_in_arkiv_order_with_any_known_author() {
    let net = Net::new();
    let (alice, bob) = (Peer::new(&net, 1), Peer::new(&net, 2));
    let rid = init(&net, &alice, "cobs").await;
    let create = op(
        &rid,
        ZERO,
        "create",
        vec![],
        json!({"title": "bug", "n": -3}),
    );
    let obj = cob::post(&net, &alice.cfg, &alice.keys, &create)
        .await
        .expect("create");
    let c1 = op(&rid, obj, "comment", vec![obj], json!({"body": "me too"}));
    let h1 = cob::post(&net, &bob.cfg, &bob.keys, &c1)
        .await
        .expect("bob, no delegate");
    let other = op(&rid, ZERO, "create", vec![], json!(null));
    let obj2 = cob::post(&net, &alice.cfg, &alice.keys, &other)
        .await
        .expect("second");

    let ops = cob::list(&net, &rid, Some(&obj), None).await.expect("list");
    let ids: Vec<Hash32> = ops.iter().map(|o| o.id).collect();
    assert_eq!(ids, vec![obj, h1]);
    assert_eq!(ops[1].author, bob.keys.nid());
    assert_eq!(
        json::to_json(&ops[0].body.payload).expect("json"),
        json!({"n": -3, "title": "bug"})
    );
    let all = cob::list(&net, &rid, None, Some("xyz.arkiv.issue"))
        .await
        .expect("all");
    assert_eq!(
        all.iter().map(|o| o.id).collect::<Vec<_>>(),
        vec![obj, h1, obj2]
    );
    assert!(
        cob::list(&net, &rid, None, Some("xyz.arkiv.patch"))
            .await
            .expect("none")
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_oversize_payload_is_rejected_before_any_write() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "big").await;
    let before = entities(&net, &rid, Kind::Cob).await.len();
    let big = op(&rid, ZERO, "create", vec![], json!("x".repeat(70_000)));
    let r = cob::post(&net, &alice.cfg, &alice.keys, &big).await;
    assert_eq!(code(r), "E_BAD_FORMAT");
    let near = op(&rid, ZERO, "create", vec![], json!("x".repeat(63_000)));
    let r = cob::post(&net, &alice.cfg, &alice.keys, &near).await;
    assert_eq!(code(r), "E_BAD_FORMAT", "the entity cap binds below 64 KiB");
    assert_eq!(entities(&net, &rid, Kind::Cob).await.len(), before);
}

#[test]
fn json_payloads_are_canonical_and_reject_floats() {
    let v = json!({"zz": 1, "a": [true, null, "s", 18446744073709551615u64, -9]});
    let cbor = json::to_cbor(&v).expect("cbor");
    assert_eq!(json::to_json(&cbor).expect("back"), v);
    assert_eq!(cbor[..3], [0xa2, 0x61, b'a'], "shorter key first");
    let err = json::to_cbor(&json!({"x": 1.5})).expect_err("float");
    assert_eq!(err.code().as_str(), "E_BAD_FORMAT");
    assert!(json::to_cbor(&json!([1.0])).is_err(), "1.0 is a float too");
    assert_eq!(
        json::to_json(&[0x42, 1, 2]).expect("bytes"),
        json!({"$bytes": "0102"})
    );
    assert_eq!(
        json::to_json(&[0xa1, 0x01, 0x02]).expect("int key"),
        json!({"1": 2})
    );
}

async fn post(net: &Net, p: &Peer, b: CobOpBody) -> Hash32 {
    cob::post(net, &p.cfg, &p.keys, &b).await.expect("post")
}

#[tokio::test(flavor = "multi_thread")]
async fn list_since_merged_into_a_cache_equals_a_full_list() {
    let net = Net::new();
    let (alice, bob) = (Peer::new(&net, 1), Peer::new(&net, 2));
    let rid = init(&net, &alice, "since").await;
    let obj = post(
        &net,
        &alice,
        op(&rid, ZERO, "create", vec![], json!({"title": "a"})),
    )
    .await;
    post(
        &net,
        &bob,
        op(&rid, obj, "comment", vec![obj], json!({"body": "1"})),
    )
    .await;
    let first = cob::list_since(&net, &rid, 0, &[]).await.expect("first");
    assert_eq!(first.ops.len(), 2);
    let last = first.last_block.expect("a block was seen");

    // A comment on the old object (its create op is not in the new range) and a new object.
    post(&net, &alice, op(&rid, obj, "close", vec![obj], json!({}))).await;
    let obj2 = post(
        &net,
        &bob,
        op(&rid, ZERO, "create", vec![], json!({"title": "b"})),
    )
    .await;
    post(
        &net,
        &alice,
        op(&rid, obj2, "comment", vec![obj2], json!({"body": "2"})),
    )
    .await;
    let next = cob::list_since(&net, &rid, last + 1, &first.ops)
        .await
        .expect("next");
    assert_eq!(next.ops.len(), 3, "only the new ops");
    assert!(next.last_block > Some(last));

    let merged: Vec<Hash32> = first.ops.iter().chain(&next.ops).map(|o| o.id).collect();
    let full = cob::list(&net, &rid, None, None).await.expect("full");
    assert_eq!(merged, full.iter().map(|o| o.id).collect::<Vec<_>>());
    let blocks: Vec<u64> = first
        .ops
        .iter()
        .chain(&next.ops)
        .map(|o| o.arkiv_order.block)
        .collect();
    assert!(
        blocks.windows(2).all(|w| w[0] <= w[1]),
        "cache stays in Arkiv order"
    );

    let idle = cob::list_since(&net, &rid, next.last_block.expect("seen") + 1, &full)
        .await
        .expect("idle");
    assert!(idle.ops.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn post_signed_pipelines_ops_in_order_one_tx_each() {
    use charter_store::Arkiv;
    let net = Net::new();
    let (alice, bob) = (Peer::new(&net, 1), Peer::new(&net, 2));
    let rid = init(&net, &alice, "many").await;
    let create = op(&rid, ZERO, "create", vec![], json!({"title": "review"}));
    let create = cob::sign_op(&bob.keys, &create).expect("sign");
    let id = create.h;
    let approve = op(&rid, id, "approve", vec![id], json!(null));
    let approve = cob::sign_op(&bob.keys, &approve).expect("sign");
    let control = bob.keys.control_address(&rid);
    let ids = cob::post_signed(&net, &bob.cfg, &bob.keys, &rid, &[create, approve])
        .await
        .expect("post");
    assert_eq!(ids[0], id, "ids are content hashes, known before sending");
    assert_eq!(
        net.tx_count_mined(control).await.expect("n"),
        2,
        "peer, bind and create in one tx; approve in the next"
    );
    let listed: Vec<Hash32> = cob::list(&net, &rid, Some(&id), None)
        .await
        .expect("list")
        .iter()
        .map(|o| o.id)
        .collect();
    assert_eq!(listed, ids);

    let blobs: Vec<CobOpBody> = (0..3)
        .map(|i| {
            op(
                &rid,
                ZERO,
                "create",
                vec![],
                json!({"i": i, "d": "x".repeat(50_000)}),
            )
        })
        .collect();
    let ids = cob::post_many(&net, &bob.cfg, &bob.keys, &blobs)
        .await
        .expect("post big");
    assert_eq!(
        net.tx_count_mined(control).await.expect("n"),
        5,
        "one tx per op"
    );
    let all: Vec<Hash32> = cob::list(&net, &rid, None, None)
        .await
        .expect("list")
        .iter()
        .map(|o| o.id)
        .filter(|h| ids.contains(h))
        .collect();
    assert_eq!(all, ids, "in order");
    assert!(
        cob::post_many(&net, &bob.cfg, &bob.keys, &[])
            .await
            .expect("none")
            .is_empty()
    );
}
