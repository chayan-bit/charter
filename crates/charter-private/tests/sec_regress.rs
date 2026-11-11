//! Regression tests for the private-repo security review (docs/p2/private-repos.md "Threat
//! model"): each was a working attack on v0.5.0 and must now fail.

mod common;

use charter_core::{Kind, Rid};
use charter_private::entity::GRACE_BLOCKS;
use charter_private::mls::Mls;
use charter_private::msg::{self, T_WELCOME, b32, seal_box};
use charter_private::{Keyring, Role, Sealed};
use charter_store::{AttributeValue, Pred};
use charter_sync::observe;
use common::sec::*;
use common::*;

/// F2: a removed member front-runs a new invitee's Welcome with her own MLS group whose
/// leaf claims the admin's nid. The invitee must still join the real group.
#[test]
fn a_removed_members_forged_welcome_does_not_hijack_the_join() {
    run(async {
        let net = Net::new();
        let (alice, mallory, bob) = (Peer::new(&net, 1), Peer::new(&net, 6), Peer::new(&net, 2));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &mallory, &rid, Role::Read).await;
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"alice v1")], "feat: v1");
        private_push(&net, &alice, &rid, &work)
            .await
            .expect("alice push");
        let mallory_old = ring(&net, &mallory, &rid).await.expect("mallory member");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");

        let kp_id = charter_private::publish_key_package(&net, &bob.cfg, &bob.keys)
            .await
            .expect("bob kp");
        let kp_rows = msg::find(&net, msg::T_KP, vec![observe::b32("ag.h", &kp_id)])
            .await
            .expect("kp rows");
        let kp_bytes = msg::bytes(&kp_rows[0], "ag.body")
            .expect("kp body")
            .to_vec();

        let fake_home = tempfile::tempdir().expect("fake home");
        let fake = Mls::open(fake_home.path(), alice.keys.nid()).expect("fake mls");
        let mut g = fake.create().expect("group");
        let (_commit, welcome, _) = fake.add(&mut g, &kp_bytes).expect("add bob");
        let (ep, key) = fake.epoch_key(&g, &rid).expect("epoch key");
        let mut forged = mallory_old.clone();
        forged.epochs.insert(1_000_000, key);
        forged.roles = [
            (hex::encode(alice.keys.nid()), Role::Admin),
            (hex::encode(mallory.keys.nid()), Role::Admin),
            (hex::encode(bob.keys.nid()), Role::Write),
        ]
        .into();
        let bx = seal_box(&forged, ep, &key).expect("box");
        let op = msg::op(
            T_WELCOME,
            vec![
                ("ag.kp", b32(kp_id), true),
                ("ag.ep", AttributeValue::U64(ep), true),
                ("ag.box", AttributeValue::Bytes(bx), false),
            ],
            welcome,
            1_000,
        )
        .expect("op");
        charter_sync::write::send(
            &net,
            &mallory.keys.wallet(),
            &mallory.cfg.pipeline,
            vec![vec![op]],
        )
        .await
        .expect("inject welcome");

        charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp_id, Role::Write)
            .await
            .expect("genuine add");
        let alice_ring = ring(&net, &alice, &rid).await.expect("alice");
        let bob_ring = ring(&net, &bob, &rid)
            .await
            .expect("bob joins the real group");
        assert_eq!(
            bob_ring.current().expect("cur"),
            alice_ring.current().expect("cur")
        );
        assert!(
            !bob_ring.epochs.contains_key(&1_000_000),
            "mallory's key is not taken"
        );
        assert_eq!(bob_ring.role(&mallory.keys.nid()), None);

        let dest = tmp.path().join("bob");
        clone_into(&Sealed::new(&net, bob_ring), &bob, &rid, &dest)
            .await
            .expect("bob clones");
        commit(&dest, &[("b", b"bob's secret work")], "feat: bob");
        private_push(&net, &bob, &rid, &dest)
            .await
            .expect("bob pushes");
        let mut mallory_view = mallory_old.clone();
        mallory_view.epochs.insert(1_000_000, key);
        let m = opened_git_state(&net, mallory_view, &rid).await;
        let before = opened_git_state(&net, mallory_old.clone(), &rid).await;
        let a = opened_git_state(&net, ring(&net, &alice, &rid).await.expect("alice"), &rid).await;
        // Mallory's extra epoch only makes her own admission stricter, never looser.
        assert!(m <= before, "mallory opens nothing of bob's push");
        assert!(a > m, "alice opens bob's push");
    });
}

/// F3: after removal (and the grace window), a removed member's pushes and issues sealed
/// under her old epoch key are not accepted by remaining members.
#[test]
fn a_removed_members_late_writes_are_not_accepted() {
    run(async {
        let net = Net::new();
        let (alice, mallory, dan) = (Peer::new(&net, 1), Peer::new(&net, 6), Peer::new(&net, 4));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &mallory, &rid, Role::Write).await;
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"alice v1")], "feat: v1");
        private_push(&net, &alice, &rid, &work)
            .await
            .expect("alice push");
        let mallory_old = ring(&net, &mallory, &rid).await.expect("mallory member");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");
        net.mine(GRACE_BLOCKS + 1);

        let mdest = tmp.path().join("m");
        clone_into(
            &Sealed::new(&net, mallory_old.clone()),
            &mallory,
            &rid,
            &mdest,
        )
        .await
        .expect("mallory clone");
        commit(&mdest, &[("evil", b"post-removal")], "feat: evil");
        let mallory_tip = rev(&mdest, "main");
        push(
            &Sealed::new(&net, mallory_old.clone()),
            &mallory,
            &rid,
            &mdest,
            &["refs/heads/main"],
        )
        .await
        .expect("mallory sends after removal");

        let dan_ring = ring(&net, &dan, &rid).await.expect("dan");
        let want = charter_sync::Want {
            extra: [mallory.keys.nid()].into(),
            ..Default::default()
        };
        let view = charter_sync::sync(&Sealed::new(&net, dan_ring), &dan.repo(&rid), &want)
            .await
            .expect("dan sync");
        let tip = hex::decode(&mallory_tip).expect("hex");
        let accepted = view
            .peers
            .get(&mallory.keys.nid())
            .and_then(|p| p.refs_body())
            .is_some_and(|r| r.refs.iter().any(|e| e.oid == tip));
        assert!(
            !accepted,
            "dan ignores a removed member's post-removal head"
        );

        let body = charter_core::CobOpBody {
            rid,
            type_name: "xyz.arkiv.issue".into(),
            object: [0; 32],
            parents: vec![],
            action: "create".into(),
            payload: vec![0xa0],
            ts_ms: 1,
        };
        charter_sync::cob::post(
            &Sealed::new(&net, mallory_old.clone()),
            &mallory.cfg,
            &mallory.keys,
            &body,
        )
        .await
        .expect("mallory sends an issue after removal");
        let dan_ring = ring(&net, &dan, &rid).await.expect("dan");
        let ops = charter_sync::cob::list(&Sealed::new(&net, dan_ring), &rid, None, None)
            .await
            .expect("list");
        assert!(
            ops.iter().all(|o| o.author != mallory.keys.nid()),
            "dan does not see the removed member's new issue"
        );
    });
}

/// F3: a reader's signed git state is ignored by other members (reader-side role check).
#[test]
fn a_readers_push_is_ignored_by_other_members() {
    run(async {
        let net = Net::new();
        let (alice, carol, dan) = (Peer::new(&net, 1), Peer::new(&net, 3), Peer::new(&net, 4));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &carol, &rid, Role::Read).await;
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"alice v1")], "feat: v1");
        private_push(&net, &alice, &rid, &work)
            .await
            .expect("alice push");
        let cdest = tmp.path().join("c");
        let carol_ring = ring(&net, &carol, &rid).await.expect("carol");
        clone_into(&Sealed::new(&net, carol_ring.clone()), &carol, &rid, &cdest)
            .await
            .expect("carol clone");
        commit(&cdest, &[("c", b"reader")], "feat: reader");
        let tip = hex::decode(rev(&cdest, "main")).expect("hex");
        // A modified client: bypasses the CLI's E_ROLE_DENIED and grants itself write locally.
        let mut rogue = carol_ring;
        rogue
            .roles
            .insert(hex::encode(carol.keys.nid()), Role::Write);
        let (ep, roles) = (rogue.current().expect("cur").0, rogue.roles.clone());
        rogue.record_roles(ep, &roles);
        push(
            &Sealed::new(&net, rogue),
            &carol,
            &rid,
            &cdest,
            &["refs/heads/main"],
        )
        .await
        .expect("carol sends");
        let want = charter_sync::Want {
            extra: [carol.keys.nid()].into(),
            ..Default::default()
        };
        let dan_ring = ring(&net, &dan, &rid).await.expect("dan");
        let view = charter_sync::sync(&Sealed::new(&net, dan_ring), &dan.repo(&rid), &want)
            .await
            .expect("dan sync");
        let accepted = view
            .peers
            .get(&carol.keys.nid())
            .and_then(|p| p.refs_body())
            .is_some_and(|r| r.refs.iter().any(|e| e.oid == tip));
        assert!(!accepted, "a reader's head is not git state");
    });
}

/// F7: an outsider's grant published first neither blocks nor replaces the real one.
#[test]
fn an_outsider_grant_does_not_block_the_real_published_epoch_key() {
    run(async {
        let net = Net::new();
        let (alice, mallory, eve) = (Peer::new(&net, 1), Peer::new(&net, 6), Peer::new(&net, 5));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("sponsorware"))
            .await
            .expect("init");
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"released")], "feat: release");
        private_push(&net, &alice, &rid, &work).await.expect("push");
        let alice_ring = ring(&net, &alice, &rid).await.expect("a");
        let epoch = alice_ring.current().expect("c").0;

        let junk = msg::op(
            msg::T_PUB,
            vec![
                ("ag.tag", b32(msg::tag(&rid)), true),
                ("ag.ep", AttributeValue::U64(epoch), true),
            ],
            vec![0x42; 64],
            1_000,
        )
        .expect("op");
        charter_sync::write::send(
            &net,
            &mallory.keys.wallet(),
            &mallory.cfg.pipeline,
            vec![vec![junk]],
        )
        .await
        .expect("junk grant");
        charter_private::publish_epoch_key(&net, &alice.cfg, &alice.keys, &rid, epoch)
            .await
            .expect("real grant");
        let eve_ring = ring(&net, &eve, &rid).await.expect("a ring");
        assert_eq!(eve_ring.id_key, alice_ring.id_key, "the real id key won");
        let (_, listing) = clone_into(
            &Sealed::new(&net, eve_ring),
            &eve,
            &rid,
            &tmp.path().join("e"),
        )
        .await
        .expect("anyone clones");
        assert_eq!(listing.refs[0].1, rev(&work, "main"));
    });
}

/// F5: a key package claiming the admin's nid is refused, so the admin keeps its role.
#[test]
fn a_key_package_claiming_the_admins_nid_is_refused() {
    run(async {
        let net = Net::new();
        let (alice, mallory, bob) = (Peer::new(&net, 1), Peer::new(&net, 6), Peer::new(&net, 2));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        let fake_home = tempfile::tempdir().expect("fake home");
        let fake = Mls::open(fake_home.path(), alice.keys.nid()).expect("fake mls");
        let kp = fake.key_package().expect("kp");
        let id = charter_crypto::sha256(&kp);
        // Even with mallory's own (valid) identity signature attached.
        let sig =
            charter_private::auth::sign_leaf(&mallory.keys, &fake.signature_key()).expect("sig");
        let attrs = vec![
            ("ag.h", b32(id), true),
            (
                "ag.pk",
                AttributeValue::Bytes(mallory.keys.pk().as_bytes().to_vec()),
                false,
            ),
            ("ag.sig", AttributeValue::Bytes(sig), false),
        ];
        let op = msg::op(msg::T_KP, attrs, kp, 1_000).expect("op");
        charter_sync::write::send(
            &net,
            &mallory.keys.wallet(),
            &mallory.cfg.pipeline,
            vec![vec![op]],
        )
        .await
        .expect("kp");
        let added =
            charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &id, Role::Read).await;
        assert_eq!(code(added), "E_BAD_SIG");
        let r = ring(&net, &alice, &rid).await.expect("alice");
        assert_eq!(
            r.role(&alice.keys.nid()),
            Some(Role::Admin),
            "alice is still admin"
        );
        let kp2 = charter_private::publish_key_package(&net, &bob.cfg, &bob.keys)
            .await
            .expect("kp2");
        charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp2, Role::Read)
            .await
            .expect("alice can still add");
        // A genuine second key package of an existing member needs the explicit device path.
        let kp3 = charter_private::publish_key_package(&net, &bob.cfg, &bob.keys)
            .await
            .expect("kp3");
        let again =
            charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp3, Role::Admin)
                .await;
        assert_eq!(
            code(again),
            "E_BAD_FORMAT",
            "an add never changes an existing role"
        );
        assert_eq!(
            ring(&net, &alice, &rid)
                .await
                .expect("a")
                .role(&bob.keys.nid()),
            Some(Role::Read)
        );
    });
}

/// F8: the id key rotates on removal, so a removed member no longer finds new entities by
/// her old blinded rid.
#[test]
fn a_removed_member_no_longer_links_new_entities() {
    run(async {
        let net = Net::new();
        let (alice, mallory) = (Peer::new(&net, 1), Peer::new(&net, 6));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &mallory, &rid, Role::Read).await;
        let old = ring(&net, &mallory, &rid).await.expect("m");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove");
        let blinded = charter_private::seal::blind(&old.id_key, "rid", &rid);
        let count = || async {
            observe::all(&net, observe::b32("ag.rid", &blinded))
                .await
                .expect("q")
                .len()
        };
        let before = count().await;
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"after removal")], "feat: after");
        private_push(&net, &alice, &rid, &work).await.expect("push");
        let after = count().await;
        assert_eq!(
            after, before,
            "nothing new under the removed member's blinded rid"
        );
    });
}

/// F9: a sealed chunk no longer carries the plain pack id of the same content pushed to a
/// public repo.
#[test]
fn a_sealed_chunk_does_not_name_the_public_pack_id() {
    run(async {
        let net = Net::new();
        let (alice, bob) = (Peer::new(&net, 1), Peer::new(&net, 2));
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("big", &noise(3, 300_000))], "feat: data");
        let public = charter_sync::identity::init(&net, &alice.cfg, &alice.keys, &meta("open"))
            .await
            .expect("public");
        push(&net, &alice, &public, &work, &["refs/heads/main"])
            .await
            .expect("public push");
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("private");
        private_push(&net, &alice, &rid, &work)
            .await
            .expect("private push");
        let rows = observe::all(&net, observe::kind(Kind::Chunk))
            .await
            .expect("q");
        let packs = |sealed: bool| -> std::collections::BTreeSet<String> {
            rows.iter()
                .filter(|o| o.attr("ag.ep").is_some() == sealed)
                .filter_map(|o| o.attr("ag.pack").map(|v| format!("{v:?}")))
                .collect()
        };
        let (p, s) = (packs(false), packs(true));
        assert!(!s.is_empty(), "the private push wrote sealed chunks");
        assert_eq!(
            p.intersection(&s).count(),
            0,
            "no sealed chunk names a public pack id"
        );
        // Members still read it: a clone fetches the chunks by their blinded pack ids.
        invite(&net, &alice, &bob, &rid, Role::Read).await;
        let bob_ring = ring(&net, &bob, &rid).await.expect("bob");
        let (_, listing) = clone_into(
            &Sealed::new(&net, bob_ring),
            &bob,
            &rid,
            &tmp.path().join("b"),
        )
        .await
        .expect("bob clones");
        assert_eq!(listing.refs[0].1, rev(&work, "main"));
    });
}

/// F4: removal drops every leaf of the nid, so a second device loses access too; a second
/// device is added only through the explicit device path.
#[test]
fn removal_drops_every_device_of_the_member() {
    run(async {
        let net = Net::new();
        let (alice, bob) = (Peer::new(&net, 1), Peer::new(&net, 2));
        let bob2 = Peer::new(&net, 2); // same identity seed, second machine ($CHARTER_HOME)
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Write).await;
        let kp2 = charter_private::publish_key_package(&net, &bob2.cfg, &bob2.keys)
            .await
            .expect("kp2");
        let plain =
            charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp2, Role::Write)
                .await;
        assert_eq!(code(plain), "E_BAD_FORMAT", "a second leaf needs --device");
        charter_private::add_device(&net, &alice.cfg, &alice.keys, &rid, &kp2)
            .await
            .expect("add device");
        let (_, list) = charter_private::members(&net, &alice.cfg, &alice.keys, &rid)
            .await
            .expect("list");
        assert!(list.contains(&(bob.keys.nid(), Role::Write, 2)), "{list:?}");
        ring(&net, &bob, &rid).await.expect("bob");
        ring(&net, &bob2, &rid).await.expect("bob2");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob.keys.nid())
            .await
            .expect("remove bob");
        let a = ring(&net, &alice, &rid).await.expect("alice");
        let b2 = ring(&net, &bob2, &rid).await.expect("bob2 keeps old keys");
        assert!(
            b2.current().expect("c").0 < a.current().expect("c").0,
            "bob2 is out too"
        );
    });
}

/// Migration: a legacy (format 1) repo upgrades on a root delegate's next membership change
/// and gains the stopper a pre-0.5.1 client fails on; any other admin is refused until then.
#[test]
fn a_root_delegate_upgrades_a_legacy_repo_and_leaves_a_stopper_for_old_clients() {
    run(async {
        let net = Net::new();
        let (alice, bob, carol) = (Peer::new(&net, 1), Peer::new(&net, 2), Peer::new(&net, 3));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Admin).await;
        ring(&net, &bob, &rid).await.expect("bob joins");
        for p in [&alice, &bob] {
            let mut r = Keyring::load(&p.cfg.home, &rid)
                .expect("load")
                .expect("ring");
            r.format = 1;
            r.save(&p.cfg.home).expect("save");
        }
        let gen0 = Keyring::load(&alice.cfg.home, &rid)
            .expect("load")
            .expect("ring")
            .id_key;
        let kp = charter_private::publish_key_package(&net, &carol.cfg, &carol.keys)
            .await
            .expect("kp");
        let by_bob =
            charter_private::add_member(&net, &bob.cfg, &bob.keys, &rid, &kp, Role::Read).await;
        assert_eq!(
            code(by_bob),
            "E_ROLE_DENIED",
            "only a root delegate upgrades"
        );
        charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp, Role::Read)
            .await
            .expect("alice upgrades");

        let old_query = Pred::And(vec![
            observe::kind(Kind::Id),
            observe::b32("ag.rid", &charter_private::seal::blind(&gen0, "rid", &rid)),
        ]);
        let rows = observe::all(&net, old_query).await.expect("identity rows");
        let stop = rows
            .iter()
            .find(|o| o.attr("ag.v") == Some(&charter_core::AttrValue::U64(msg::STOP_V)))
            .expect("stopper under the generation-0 blinded rid");
        assert_eq!(
            stop.creator,
            alice.keys.control_address(&rid),
            "from a bound control address"
        );

        assert_eq!(
            ring(&net, &bob, &rid).await.expect("bob").format,
            charter_private::keyring::FORMAT
        );
        assert_eq!(
            ring(&net, &carol, &rid).await.expect("carol").format,
            charter_private::keyring::FORMAT
        );
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"after upgrade")], "feat: v1");
        private_push(&net, &alice, &rid, &work)
            .await
            .expect("alice pushes");
        let carol_ring = ring(&net, &carol, &rid).await.expect("carol");
        let (_, listing) = clone_into(
            &Sealed::new(&net, carol_ring),
            &carol,
            &rid,
            &tmp.path().join("c"),
        )
        .await
        .expect("carol clones past the stopper");
        assert_eq!(listing.refs[0].1, rev(&work, "main"));
    });
}

/// The migration stopper, read as a pre-0.5.1 client finds it: under the generation-0
/// blinded rid.
async fn stopper(
    net: &Net,
    gen0: &charter_private::seal::Key,
    rid: &Rid,
) -> charter_core::Observed {
    let old_query = Pred::And(vec![
        observe::kind(Kind::Id),
        observe::b32("ag.rid", &charter_private::seal::blind(gen0, "rid", rid)),
    ]);
    observe::all(net, old_query)
        .await
        .expect("identity rows")
        .into_iter()
        .find(|o| o.attr("ag.v") == Some(&charter_core::AttrValue::U64(msg::STOP_V)))
        .expect("stopper")
}

/// Keepalive extends the stopper with the rest of the control plane, so pre-0.5.1 clients
/// keep failing closed for as long as the repo is kept alive.
#[test]
fn keepalive_extends_the_migration_stopper() {
    run(async {
        let net = Net::new();
        let (mut alice, carol) = (Peer::new(&net, 1), Peer::new(&net, 3));
        alice.cfg.control_lifetime = 2_000; // the stopper soon falls due
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        let mut r = Keyring::load(&alice.cfg.home, &rid)
            .expect("load")
            .expect("ring");
        r.format = 1;
        r.save(&alice.cfg.home).expect("save");
        let gen0 = r.id_key;
        invite(&net, &alice, &carol, &rid, Role::Read).await;
        let before = stopper(&net, &gen0, &rid).await.expires_at;

        let horizon = 100_000;
        let opts = charter_sync::keepalive::KeepOpts {
            horizon,
            renew_below: horizon,
            max_spend: None,
            dry_run: false,
        };
        let sealed = Sealed::new(&net, ring(&net, &alice, &rid).await.expect("alice"));
        charter_sync::keepalive::keepalive(
            &sealed,
            &alice.repo(&rid),
            &alice.cfg,
            &alice.keys.wallet(),
            &opts,
        )
        .await
        .expect("keepalive");
        let after = stopper(&net, &gen0, &rid).await.expires_at;
        assert!(
            after > before + 50_000,
            "stopper extended: {before} -> {after}"
        );
    });
}

/// Residual of F2: an admin is removed, then forges a welcome for a published key package
/// with everything she kept (old keys, admin role, a valid composite signature) and her
/// own commit records, before any genuine welcome exists. The joiner replays the admin
/// history from the root and refuses it.
#[test]
fn an_ex_admins_forged_welcome_is_refused_even_without_a_genuine_one() {
    run(async {
        let net = Net::new();
        let (alice, mallory, bob) = (Peer::new(&net, 1), Peer::new(&net, 6), Peer::new(&net, 2));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &mallory, &rid, Role::Admin).await;
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("m", b"while admin")], "feat: m");
        private_push(&net, &mallory, &rid, &work)
            .await
            .expect("mallory writes (peer doc)");
        let old = ring(&net, &mallory, &rid).await.expect("mallory");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");
        let kp_id = charter_private::publish_key_package(&net, &bob.cfg, &bob.keys)
            .await
            .expect("bob kp");
        let kp_rows = msg::find(&net, msg::T_KP, vec![observe::b32("ag.h", &kp_id)])
            .await
            .expect("kp rows");
        let kp = msg::bytes(&kp_rows[0], "ag.body").expect("kp").to_vec();

        // Her own group, grown past the real epoch, with bob added last.
        let fake_home = tempfile::tempdir().expect("fake home");
        let fake = Mls::open(fake_home.path(), mallory.keys.nid()).expect("fake mls");
        let mut g = fake.create().expect("group");
        let mut forged = old.clone();
        let admins: std::collections::BTreeSet<String> = [alice.keys.nid(), mallory.keys.nid()]
            .iter()
            .map(hex::encode)
            .collect();
        let mut records = vec![];
        let mut welcome = vec![];
        for i in 0..3u8 {
            let other = tempfile::tempdir().expect("other");
            let dummy = Mls::open(other.path(), [40 + i; 32]).expect("dummy");
            let add = if i == 2 {
                kp.clone()
            } else {
                dummy.key_package().expect("kp")
            };
            let (commit, w, _) = fake.add(&mut g, &add).expect("add");
            let (ep, key) = fake.epoch_key(&g, &rid).expect("key");
            forged.epochs.insert(ep, key);
            forged.adm_log.insert(ep, admins.clone());
            forged
                .roles
                .insert(hex::encode(mallory.keys.nid()), Role::Admin);
            forged
                .roles
                .insert(hex::encode(bob.keys.nid()), Role::Write);
            let roles = forged.roles.clone();
            forged.record_roles(ep, &roles);
            records.push((ep, key, commit));
            welcome = w;
        }
        let (ep, key) = fake.epoch_key(&g, &rid).expect("key");
        let bx = seal_box(&forged, ep, &key).expect("box");
        let att_msg = charter_private::auth::att_msg(&rid, ep, &welcome, &bx, Some(&kp_id));
        let att = charter_private::auth::seal_att(
            &mallory.keys,
            charter_private::auth::CTX_WELCOME,
            &key,
            &rid,
            ep,
            &att_msg,
        )
        .expect("att");
        let mut ops = vec![];
        for (e, k, commit) in records {
            let cbx = seal_box(&forged, e, &k).expect("box");
            let signed = charter_private::auth::sign_commit(
                &mallory.keys,
                &rid,
                e,
                &k,
                &commit,
                &cbx,
                &admins,
            )
            .expect("sign");
            let ct = charter_private::auth::commits_tag(&old.id_key, &rid);
            let attrs = vec![
                ("ag.ct", b32(ct), true),
                ("ag.ep", AttributeValue::U64(e), true),
                ("ag.box", AttributeValue::Bytes(cbx), false),
                ("ag.att", AttributeValue::Bytes(signed), false),
            ];
            ops.push(msg::op(msg::T_COMMIT, attrs, commit, 1_000).expect("op"));
        }
        let attrs = vec![
            ("ag.kp", b32(kp_id), true),
            ("ag.ep", AttributeValue::U64(ep), true),
            ("ag.box", AttributeValue::Bytes(bx), false),
            ("ag.att", AttributeValue::Bytes(att), false),
        ];
        ops.push(msg::op(T_WELCOME, attrs, welcome, 1_000).expect("op"));
        charter_sync::write::send(
            &net,
            &mallory.keys.wallet(),
            &mallory.cfg.pipeline,
            vec![ops],
        )
        .await
        .expect("inject");

        assert_eq!(
            code(ring(&net, &bob, &rid).await),
            "E_BAD_SIG",
            "bob refuses it"
        );
        assert!(
            Keyring::load(&bob.cfg.home, &rid).expect("load").is_none(),
            "bob holds no keys of mallory's group"
        );
        charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp_id, Role::Write)
            .await
            .expect("genuine add");
        let b = ring(&net, &bob, &rid)
            .await
            .expect("bob joins the real group");
        let a = ring(&net, &alice, &rid).await.expect("alice");
        assert_eq!(b.current().expect("c"), a.current().expect("c"));
    });
}

/// Residual of F3: a writer's push that lands inside the grace window after a commit, and
/// is then extended by someone else, stays accepted (its body landed in time); a removed
/// member's later patch of a head she made in time stays refused even once extended.
#[test]
fn an_extended_head_that_landed_in_the_grace_window_is_kept() {
    run(async {
        let net = Net::new();
        let [alice, bob, mallory, dan] = [1, 2, 6, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        for (p, role) in [
            (&bob, Role::Write),
            (&mallory, Role::Write),
            (&dan, Role::Read),
        ] {
            invite(&net, &alice, p, &rid, role).await;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let (bw, mw) = (work_repo(tmp.path(), "b"), work_repo(tmp.path(), "m"));
        commit(&bw, &[("b", b"v1")], "feat: b1");
        commit(&mw, &[("m", b"v1")], "feat: m1");
        head_push(
            &Sealed::new(&net, ring(&net, &bob, &rid).await.expect("ring")),
            &bob,
            &rid,
            &bw,
        )
        .await
        .expect("bob v1");
        head_push(
            &Sealed::new(&net, ring(&net, &mallory, &rid).await.expect("ring")),
            &mallory,
            &rid,
            &mw,
        )
        .await
        .expect("mallory v1");
        let (bob_old, mallory_old) = (
            ring(&net, &bob, &rid).await.expect("b"),
            ring(&net, &mallory, &rid).await.expect("m"),
        );
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");

        // Bob had not refreshed: his push seals under the old epoch, inside the grace window.
        commit(&bw, &[("b", b"v2")], "feat: b2");
        head_push(&Sealed::new(&net, bob_old), &bob, &rid, &bw)
            .await
            .expect("bob v2 in the window");
        net.mine(GRACE_BLOCKS + 1);
        commit(&mw, &[("m", b"v2")], "feat: m2");
        head_push(&Sealed::new(&net, mallory_old), &mallory, &rid, &mw)
            .await
            .expect("mallory patches late");
        extend_head(&net, &alice, &bob).await;
        extend_head(&net, &alice, &mallory).await;

        assert!(
            accepts(&net, &dan, &rid, &bob, &rev(&bw, "main")).await,
            "bob's v2 is kept"
        );
        assert!(
            !accepts(&net, &dan, &rid, &mallory, &rev(&mw, "main")).await,
            "mallory's late patch is not"
        );
    });
}
