//! A commit's attestation signs the control address that sends it, so a byte copy of an
//! expired commit re-created from any other wallet places no epoch end for a joiner or a
//! grant reader, with or without its attestation, and a removed member's late writes stay
//! out (docs/p2/private-repos.md "Threat model").

mod common;

use charter_private::entity::GRACE_BLOCKS;
use charter_private::msg::{self, T_COMMIT};
use charter_private::{Role, Sealed};
use charter_store::AttributeValue;
use common::sec::*;
use common::*;

/// The commit row `o` again, byte for byte, as an op anyone can send.
fn copy_of(o: &charter_core::Observed) -> charter_store::Operation {
    copy(o, true)
}

/// [`copy_of`], with its attestation left out unless `att`.
fn copy(o: &charter_core::Observed, att: bool) -> charter_store::Operation {
    let b = |n: &str| msg::bytes(o, n).expect(n).to_vec();
    let h = |n: &str| match o.attr(n) {
        Some(charter_core::AttrValue::Bytes32(v)) => *v,
        x => panic!("{n} {x:?}"),
    };
    let ep = msg::u64_of(o, "ag.ep").expect("ep");
    let mut attrs = vec![
        ("ag.rid", msg::b32(h("ag.rid")), true),
        ("ag.ct", msg::b32(h("ag.ct")), true),
        ("ag.ep", AttributeValue::U64(ep), true),
        ("ag.box", AttributeValue::Bytes(b("ag.box")), false),
    ];
    if att {
        attrs.push(("ag.att", AttributeValue::Bytes(b("ag.att")), false));
    }
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

fn issue(rid: charter_core::Rid, ts: u64) -> charter_core::CobOpBody {
    charter_core::CobOpBody {
        rid,
        type_name: "xyz.arkiv.issue".into(),
        object: [0; 32],
        parents: vec![],
        action: "create".into(),
        payload: vec![0xa0],
        ts_ms: ts,
    }
}

/// R2 for grant readers: once the commits up to a removal expired, a copy of the removal
/// commit from any wallet (`att`: with its attestation, else without) must not place the
/// next epoch's start, so a removed member's late write under the old epoch stays out.
fn expired_prefix_copy(att: bool) {
    run(async move {
        let net = Net::new();
        let [mut alice, mallory, dan, eve] = [1, 6, 4, 5].map(|s| Peer::new(&net, s));
        alice.cfg.control_lifetime = 1_000_000;
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        let r0 = ring(&net, &alice, &rid).await.expect("alice");
        charter_sync::cob::post(
            &Sealed::new(&net, r0),
            &alice.cfg,
            &alice.keys,
            &issue(rid, 1),
        )
        .await
        .expect("alice's peer doc, long-lived");
        alice.cfg.control_lifetime = 120;
        invite(&net, &alice, &mallory, &rid, Role::Write).await;
        let mallory_old = ring(&net, &mallory, &rid).await.expect("mallory");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");
        let r = ring(&net, &alice, &rid).await.expect("alice");
        let rows = msg::find(&net, T_COMMIT, vec![msg::rid_pred(&r)])
            .await
            .expect("commits");
        let removal = rows
            .iter()
            .find(|c| msg::u64_of(c, "ag.ep") == Some(2))
            .expect("removal commit");
        let copy = copy(removal, att);
        alice.cfg.control_lifetime = 1_000_000;
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        charter_private::publish_epoch_keys(&net, &alice.cfg, &alice.keys, &rid, 0..=3)
            .await
            .expect("grants");

        net.mine(GRACE_BLOCKS + 200);
        let old = Sealed::new(&net, mallory_old);
        charter_sync::cob::post(&old, &mallory.cfg, &mallory.keys, &issue(rid, 2))
            .await
            .expect("late issue under epoch 1");
        let by_mallory = |r: charter_private::Keyring| async {
            charter_sync::cob::list(&Sealed::new(&net, r), &rid, None, None)
                .await
                .expect("list")
                .iter()
                .any(|o| o.author == mallory.keys.nid())
        };
        let reader = || async {
            charter_private::ring_for(&net, &eve.cfg, None, &rid)
                .await
                .expect("grant reader")
                .expect("private")
        };
        assert!(
            !by_mallory(reader().await).await,
            "late write shown without the copy"
        );
        net.mine(10);
        let wallet = mallory.keys.wallet();
        charter_sync::write::send(&net, &wallet, &mallory.cfg.pipeline, vec![vec![copy]])
            .await
            .expect("copy");
        assert!(
            !by_mallory(reader().await).await,
            "a copy from mallory's wallet admitted her late write"
        );
    });
}

#[test]
fn a_copy_of_an_expired_prefix_commit_moves_a_grant_readers_epoch_end() {
    expired_prefix_copy(true);
}

#[test]
fn a_copy_without_its_attestation_of_an_expired_prefix_commit_moves_nothing() {
    expired_prefix_copy(false);
}
