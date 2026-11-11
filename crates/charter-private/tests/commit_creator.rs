//! A commit's attestation signs the control address that sends it, so a byte copy of an
//! expired commit re-created from any other wallet places no epoch end for a joiner, and a
//! removed member's late writes stay out (docs/p2/private-repos.md "Threat model").

mod common;

use charter_private::entity::GRACE_BLOCKS;
use charter_private::msg::{self, T_COMMIT};
use charter_private::{Role, Sealed};
use charter_store::AttributeValue;
use common::sec::*;
use common::*;

/// The commit row `o` again, byte for byte, as an op anyone can send.
fn copy_of(o: &charter_core::Observed) -> charter_store::Operation {
    let b = |n: &str| msg::bytes(o, n).expect(n).to_vec();
    let h = |n: &str| match o.attr(n) {
        Some(charter_core::AttrValue::Bytes32(v)) => *v,
        x => panic!("{n} {x:?}"),
    };
    let ep = msg::u64_of(o, "ag.ep").expect("ep");
    let attrs = vec![
        ("ag.rid", msg::b32(h("ag.rid")), true),
        ("ag.ct", msg::b32(h("ag.ct")), true),
        ("ag.ep", AttributeValue::U64(ep), true),
        ("ag.box", AttributeValue::Bytes(b("ag.box")), false),
        ("ag.att", AttributeValue::Bytes(b("ag.att")), false),
    ];
    msg::op(T_COMMIT, attrs, b("ag.body"), 1_000_000).expect("op")
}

#[test]
fn a_copy_of_an_expired_commit_from_another_wallet_places_no_epoch_end() {
    run(async {
        let net = Net::new();
        let [mut alice, mallory, dan] = [1, 6, 4].map(|s| Peer::new(&net, s));
        alice.cfg.control_lifetime = 1_000_000;
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &mallory, &rid, Role::Write).await;
        let mallory_old = ring(&net, &mallory, &rid).await.expect("mallory");
        alice.cfg.control_lifetime = 120; // only the removal commit is short-lived
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");
        alice.cfg.control_lifetime = 1_000_000;
        let r = ring(&net, &alice, &rid).await.expect("alice");
        let cur = r.current().expect("cur").0;
        let removal_at = |rows: &[charter_core::Observed]| {
            rows.iter()
                .find(|c| msg::u64_of(c, "ag.ep") == Some(cur))
                .cloned()
        };
        let rows = msg::find(&net, T_COMMIT, vec![msg::rid_pred(&r)])
            .await
            .expect("commits");
        let copy = copy_of(&removal_at(&rows).expect("removal commit"));

        net.mine(GRACE_BLOCKS + 10);
        let late = charter_core::CobOpBody {
            rid,
            type_name: "xyz.arkiv.issue".into(),
            object: [0; 32],
            parents: vec![],
            action: "create".into(),
            payload: vec![0xa0],
            ts_ms: 1,
        };
        let old = Sealed::new(&net, mallory_old);
        charter_sync::cob::post(&old, &mallory.cfg, &mallory.keys, &late)
            .await
            .expect("late issue");
        net.mine(200);
        let rows = msg::find(&net, T_COMMIT, vec![msg::rid_pred(&r)])
            .await
            .expect("commits");
        assert!(removal_at(&rows).is_none(), "the removal commit expired");
        let wallet = mallory.keys.wallet();
        charter_sync::write::send(&net, &wallet, &mallory.cfg.pipeline, vec![vec![copy]])
            .await
            .expect("copy");

        invite(&net, &alice, &dan, &rid, Role::Read).await;
        let dr = match ring(&net, &dan, &rid).await {
            Ok(dr) => dr,
            Err(e) => {
                assert!(e.to_string().contains("commit history"), "{e}");
                return;
            }
        };
        let seen = charter_sync::cob::list(&Sealed::new(&net, dr), &rid, None, None)
            .await
            .expect("list")
            .iter()
            .any(|o| o.author == mallory.keys.nid());
        assert!(!seen, "a joiner admits a removed member's late write");
    });
}
