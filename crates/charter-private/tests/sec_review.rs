//! Regression tests for the second private-repo security review (findings N1 to N8 in
//! docs/p2/private-repos.md): each failed, or showed the flaw, before its fix.

mod common;

use std::collections::BTreeSet;
use std::sync::atomic::Ordering;

use charter_core::Kind;
use charter_private::entity::GRACE_BLOCKS;
use charter_private::msg::{self, b32, seal_box};
use charter_private::{Keyring, Role, Sealed};
use charter_store::{Arkiv, AttributeValue};
use charter_sync::observe;
use common::sec::*;
use common::*;

/// The public blinded rid of `rid`, as an outsider reads it off any anchor row.
async fn public_blinded_rid(net: &Net) -> charter_core::Hash32 {
    let anchors = observe::all(net, observe::kind(Kind::Anchor))
        .await
        .expect("anchors");
    match anchors[0].attr("ag.rid") {
        Some(charter_core::AttrValue::Bytes32(h)) => *h,
        other => panic!("ag.rid {other:?}"),
    }
}

/// N1: an outsider who copies the blinded rid writes one unsigned commit row for epoch
/// `epoch`; neither members nor joiners may take its block as the end of an epoch.
fn junk_commit_does_not_freeze(epoch: impl Fn(u64) -> u64 + Send + 'static) {
    run(async move {
        let net = Net::new();
        let [alice, mallory, dan] = [1, 6, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "w");
        commit(&work, &[("a", b"v1")], "feat: v1");
        private_push(&net, &alice, &rid, &work).await.expect("v1");
        let cur = ring(&net, &dan, &rid)
            .await
            .expect("dan")
            .current()
            .expect("c")
            .0;
        let junk = msg::op(
            msg::T_COMMIT,
            vec![
                ("ag.rid", b32(public_blinded_rid(&net).await), true),
                ("ag.ep", AttributeValue::U64(epoch(cur)), true),
            ],
            vec![0x42; 8],
            100_000,
        )
        .expect("op");
        charter_sync::write::send(
            &net,
            &mallory.keys.wallet(),
            &mallory.cfg.pipeline,
            vec![vec![junk]],
        )
        .await
        .expect("junk commit");
        net.mine(GRACE_BLOCKS + 6);

        commit(&work, &[("a", b"v2")], "feat: v2");
        private_push(&net, &alice, &rid, &work).await.expect("v2");
        let v2 = rev(&work, "main");
        assert!(accepts(&net, &dan, &rid, &alice, &v2).await, "dan sees v2");
        assert!(
            accepts(&net, &alice, &rid, &alice, &v2).await,
            "alice sees v2"
        );

        let bob = Peer::new(&net, 2);
        invite(&net, &alice, &bob, &rid, Role::Read).await;
        net.mine(GRACE_BLOCKS + 6);
        commit(&work, &[("a", b"v3")], "feat: v3");
        private_push(&net, &alice, &rid, &work).await.expect("v3");
        let v3 = rev(&work, "main");
        assert!(accepts(&net, &dan, &rid, &alice, &v3).await, "dan sees v3");
        assert!(
            accepts(&net, &bob, &rid, &alice, &v3).await,
            "a new joiner sees v3"
        );
    });
}

#[test]
fn an_outsiders_junk_commit_for_the_next_epoch_does_not_freeze_the_repo() {
    junk_commit_does_not_freeze(|cur| cur + 1);
}

#[test]
fn an_outsiders_junk_commit_for_a_far_epoch_does_not_freeze_the_repo() {
    junk_commit_does_not_freeze(|_| 1 << 40);
}

/// N1: a keyring saved with junk epoch ends (by a client that placed them from any row)
/// recovers when it is next loaded.
#[test]
fn a_keyring_poisoned_by_junk_epoch_ends_recovers_on_load() {
    run(async {
        let net = Net::new();
        let [alice, dan] = [1, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        let cur = ring(&net, &dan, &rid)
            .await
            .expect("dan")
            .current()
            .expect("c")
            .0;
        let mut poisoned = Keyring::load(&dan.cfg.home, &rid)
            .expect("load")
            .expect("ring");
        let block = net.head().await.expect("head");
        poisoned.rotated.insert(cur + 1, block);
        poisoned.rotated.insert(1 << 40, block);
        poisoned.save(&dan.cfg.home).expect("save");
        net.mine(GRACE_BLOCKS + 6);

        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "w");
        commit(&work, &[("a", b"v1")], "feat: v1");
        private_push(&net, &alice, &rid, &work).await.expect("v1");
        assert!(
            accepts(&net, &dan, &rid, &alice, &rev(&work, "main")).await,
            "dan sees alice's push again"
        );
        let healed = Keyring::load(&dan.cfg.home, &rid)
            .expect("load")
            .expect("ring");
        assert!(
            healed.rotated.range(cur + 1..).next().is_none(),
            "junk ends dropped: {:?}",
            healed.rotated
        );
    });
}

/// N2: the root delegate is removed from the group by another admin, then publishes a
/// signed grant for an epoch it never held, with its own commit record. A grant reader
/// keeps the genuine grants and refuses that one.
#[test]
fn a_removed_root_delegates_forged_grant_is_refused() {
    run(async {
        let net = Net::new();
        let [alice, bob, eve] = [1, 2, 5].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("sponsorware"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Admin).await;
        for e in 0..=1 {
            charter_private::publish_epoch_key(&net, &alice.cfg, &alice.keys, &rid, e)
                .await
                .expect("grant");
        }
        let old = ring(&net, &alice, &rid).await.expect("alice");
        charter_private::remove_member(&net, &bob.cfg, &bob.keys, &rid, &alice.keys.nid())
            .await
            .expect("bob removes alice");

        let (forged_ep, fake_key) = (2, [0x5a; 32]);
        let mut forged = old.clone();
        forged.epochs.insert(forged_ep, fake_key);
        let admins: BTreeSet<String> = [hex::encode(alice.keys.nid())].into();
        forged.adm_log.insert(forged_ep, admins.clone());
        let body = vec![0x42; 16];
        let bx = seal_box(&forged, forged_ep, &fake_key).expect("box");
        let att = charter_private::auth::sign_commit(
            &alice.keys,
            &rid,
            &old.id_key,
            forged_ep,
            &fake_key,
            &body,
            &bx,
            &admins,
        )
        .expect("sign");
        let ct = charter_private::auth::commits_tag(&old.id_key, &rid);
        let record = msg::op(
            msg::T_COMMIT,
            vec![
                ("ag.ct", b32(ct), true),
                ("ag.ep", AttributeValue::U64(forged_ep), true),
                ("ag.box", AttributeValue::Bytes(bx), false),
                ("ag.att", AttributeValue::Bytes(att), false),
            ],
            body,
            1_000,
        )
        .expect("op");
        let roles = [(alice.keys.nid(), Role::Admin)];
        let grant = charter_private::auth::encode_grant(
            &alice.keys,
            &rid,
            forged_ep,
            &old.id_key,
            &fake_key,
            &roles,
        )
        .expect("grant");
        let pub_op = msg::op(
            msg::T_PUB,
            vec![
                ("ag.tag", b32(msg::tag(&rid)), true),
                ("ag.ep", AttributeValue::U64(forged_ep), true),
            ],
            grant,
            1_000,
        )
        .expect("op");
        charter_sync::write::send(
            &net,
            &alice.keys.wallet(),
            &alice.cfg.pipeline,
            vec![vec![record, pub_op]],
        )
        .await
        .expect("forge");

        let eve_ring = charter_private::ring_for(&net, &eve.cfg, None, &rid)
            .await
            .expect("grant reader")
            .expect("private");
        assert_eq!(
            eve_ring.epochs.keys().copied().collect::<Vec<_>>(),
            vec![0, 1],
            "the genuine grants count, the forged one does not"
        );
        assert_eq!(eve_ring.epochs.get(&1), old.epochs.get(&1));
    });
}

/// N3: a non-root admin running a client that skips the root-delegate check makes the
/// commit that would migrate a legacy repo; the other members refuse to apply it.
#[test]
fn a_non_root_admin_cannot_migrate_a_legacy_repo() {
    run(async {
        let net = Net::new();
        let [alice, bob, carol] = [1, 2, 3].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Admin).await;
        ring(&net, &bob, &rid).await.expect("bob joins");
        ring(&net, &alice, &rid)
            .await
            .expect("alice applies her add");
        let mut legacy = Keyring::load(&alice.cfg.home, &rid)
            .expect("load")
            .expect("ring");
        legacy.format = 1;
        legacy.adm_log.clear();
        legacy.save(&alice.cfg.home).expect("save");

        // Bob's keyring says format 2, so his client skips the migration check.
        let kp = charter_private::publish_key_package(&net, &carol.cfg, &carol.keys)
            .await
            .expect("kp");
        charter_private::add_member(&net, &bob.cfg, &bob.keys, &rid, &kp, Role::Read)
            .await
            .expect("bob's commit lands");
        let a = ring(&net, &alice, &rid).await.expect("alice");
        assert_eq!(
            (a.current().expect("c").0, a.format),
            (1, 1),
            "alice did not apply bob's migrating commit"
        );
    });
}

/// N4: the landing check of an extended head reads its blocks once; later reads reuse the
/// verdict.
#[test]
fn an_extended_heads_landing_check_is_paid_once() {
    run(async {
        let net = Net::new();
        let [alice, bob, mallory, dan] = [1, 2, 6, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        for (p, role) in [
            (&bob, Role::Write),
            (&mallory, Role::Read),
            (&dan, Role::Read),
        ] {
            invite(&net, &alice, p, &rid, role).await;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let bw = work_repo(tmp.path(), "b");
        commit(&bw, &[("b", b"v1")], "feat: b1");
        let bob_old = ring(&net, &bob, &rid).await.expect("b");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");
        head_push(&Sealed::new(&net, bob_old), &bob, &rid, &bw)
            .await
            .expect("bob pushes in the window");
        net.mine(GRACE_BLOCKS + 1);
        extend_head(&net, &alice, &bob).await;

        assert!(
            accepts(&net, &dan, &rid, &bob, &rev(&bw, "main")).await,
            "bob's push is kept"
        );

        let heads = charter_store::Pred::And(vec![
            observe::kind(Kind::Head),
            observe::b32("ag.nid", &bob.keys.nid()),
        ]);
        let mut reads = vec![];
        for _ in 0..2 {
            let sealed = Sealed::new(&net, ring(&net, &dan, &rid).await.expect("dan"))
                .caching(&dan.cfg.home);
            net.block_reads.store(0, Ordering::Relaxed);
            let rows = observe::all(&sealed, heads.clone()).await.expect("heads");
            reads.push(net.block_reads.load(Ordering::Relaxed));
            assert!(rows.iter().all(|o| o.attr("ag.ep").is_none()), "opened");
        }
        assert!(reads[0] > 0, "the first read checks the landing: {reads:?}");
        assert_eq!(reads[1], 0, "later reads reuse the verdict: {reads:?}");
    });
}

/// N5: the same key package is added, its owner removed before joining, then added again,
/// so two verified welcomes at different epochs exist. The joiner takes the newest one.
#[test]
fn two_welcomes_for_one_key_package_join_at_the_newest() {
    run(async {
        let net = Net::new();
        let [alice, bob] = [1, 2].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        let kp = charter_private::publish_key_package(&net, &bob.cfg, &bob.keys)
            .await
            .expect("kp");
        charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp, Role::Write)
            .await
            .expect("add");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob.keys.nid())
            .await
            .expect("remove before bob joins");
        charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp, Role::Read)
            .await
            .expect("add the same key package again");
        let b = ring(&net, &bob, &rid).await.expect("bob joins");
        let a = ring(&net, &alice, &rid).await.expect("alice");
        assert_eq!(b.current().expect("c"), a.current().expect("c"));
        assert_eq!(b.role(&bob.keys.nid()), Some(Role::Read));
    });
}

/// N6: a backend sealing one private repo neither passes the write guard for another
/// private repo nor sends that repo's entities in plaintext.
#[test]
fn a_backend_sealing_one_repo_refuses_another_repos_writes() {
    run(async {
        let net = Net::new();
        let alice = Peer::new(&net, 1);
        let mut rids = vec![];
        for name in ["a", "b"] {
            let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta(name))
                .await
                .expect("init");
            rids.push(rid);
        }
        let sealed_a = Sealed::new(&net, ring(&net, &alice, &rids[0]).await.expect("a"));
        let guard = |rid| charter_sync::guard::require_sealed(&sealed_a, &alice.cfg.home, rid);
        assert!(guard(&rids[0]).is_ok(), "its own repo");
        assert_eq!(code(guard(&rids[1])), "E_UNSEALED_PRIVATE");
        let op_b = charter_store::create_op(
            &[
                ("ag.kind", AttributeValue::Str("anchor".into()), true),
                ("ag.rid", AttributeValue::Bytes32(rids[1]), true),
                ("ag.body", AttributeValue::Bytes(b"secret".to_vec()), false),
            ],
            1_000,
            charter_store::random_salt(),
        )
        .expect("op");
        assert!(
            sealed_a.seal_ops(vec![op_b]).is_err(),
            "not sent in plaintext"
        );
    });
}

/// N7: a removed member keeps its keyring and is added again on the same home, with a
/// lower role. It rejoins at the new epoch; between removal and re-add it holds no role.
#[test]
fn a_removed_member_readded_on_the_same_home_rejoins_with_the_new_role() {
    run(async {
        let net = Net::new();
        let [alice, bob] = [1, 2].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Write).await;
        ring(&net, &bob, &rid).await.expect("bob in");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob.keys.nid())
            .await
            .expect("remove bob");
        let out = ring(&net, &bob, &rid).await.expect("old history");
        assert_eq!(out.role(&bob.keys.nid()), None, "no stale write role");

        invite(&net, &alice, &bob, &rid, Role::Read).await;
        let b = ring(&net, &bob, &rid).await.expect("bob rejoins");
        let a = ring(&net, &alice, &rid).await.expect("alice");
        assert_eq!(b.current().expect("c"), a.current().expect("c"));
        assert_eq!(b.role(&bob.keys.nid()), Some(Role::Read));
    });
}

/// N7: the same, but the removed member deleted its keyring before the re-add (its MLS
/// state still holds the old group).
#[test]
fn a_readded_member_without_its_old_keyring_rejoins() {
    run(async {
        let net = Net::new();
        let [alice, bob] = [1, 2].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Write).await;
        ring(&net, &bob, &rid).await.expect("bob in");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob.keys.nid())
            .await
            .expect("remove bob");
        std::fs::remove_file(Keyring::path(&bob.cfg.home, &rid)).expect("rm keyring");

        invite(&net, &alice, &bob, &rid, Role::Read).await;
        let b = ring(&net, &bob, &rid).await.expect("bob rejoins");
        let a = ring(&net, &alice, &rid).await.expect("alice");
        assert_eq!(b.current().expect("c"), a.current().expect("c"));
        assert_eq!(b.role(&bob.keys.nid()), Some(Role::Read));
    });
}

/// N8: a writer demoted to read (removed, then added again as reader on the same home)
/// still holds the device key its earlier device record authorizes; its new heads do not
/// count.
#[test]
fn a_demoted_writers_device_heads_do_not_count() {
    run(async {
        let net = Net::new();
        let [alice, bob, dan] = [1, 2, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Write).await;
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        let tmp = tempfile::tempdir().expect("tmp");
        let bw = work_repo(tmp.path(), "b");
        commit(&bw, &[("b", b"v1")], "feat: b1");
        let bob_ring = ring(&net, &bob, &rid).await.expect("bob");
        head_push(&Sealed::new(&net, bob_ring), &bob, &rid, &bw)
            .await
            .expect("bob v1 as writer");
        assert!(
            accepts(&net, &dan, &rid, &bob, &rev(&bw, "main")).await,
            "baseline"
        );

        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob.keys.nid())
            .await
            .expect("remove bob");
        invite(&net, &alice, &bob, &rid, Role::Read).await;
        let demoted = ring(&net, &bob, &rid).await.expect("bob rejoins");
        assert_eq!(demoted.role(&bob.keys.nid()), Some(Role::Read));
        net.mine(GRACE_BLOCKS + 6);
        commit(&bw, &[("b", b"v2")], "feat: b2 as reader");
        let _ = head_push(&Sealed::new(&net, demoted), &bob, &rid, &bw).await;
        assert!(
            !accepts(&net, &dan, &rid, &bob, &rev(&bw, "main")).await,
            "a reader's head is not git state"
        );
    });
}

/// A writer's head push after an unrelated removal (a new id-key generation) still counts.
#[test]
fn a_writers_head_push_after_an_unrelated_removal_counts() {
    run(async {
        let net = Net::new();
        let [alice, bob, mallory, dan] = [1, 2, 6, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        for (p, role) in [
            (&bob, Role::Write),
            (&mallory, Role::Read),
            (&dan, Role::Read),
        ] {
            invite(&net, &alice, p, &rid, role).await;
        }
        let tmp = tempfile::tempdir().expect("tmp");
        let bw = work_repo(tmp.path(), "b");
        commit(&bw, &[("b", b"v1")], "feat: b1");
        let r = ring(&net, &bob, &rid).await.expect("r");
        head_push(&Sealed::new(&net, r), &bob, &rid, &bw)
            .await
            .expect("bob v1");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");
        commit(&bw, &[("b", b"v2")], "feat: b2");
        let r = ring(&net, &bob, &rid).await.expect("r");
        head_push(&Sealed::new(&net, r), &bob, &rid, &bw)
            .await
            .expect("bob v2");
        assert!(accepts(&net, &dan, &rid, &bob, &rev(&bw, "main")).await);
    });
}

/// A signed record for epoch 0, which no commit makes, does not derail a joiner's replay.
#[test]
fn a_root_delegates_record_for_epoch_zero_does_not_block_joins() {
    run(async {
        let net = Net::new();
        let [alice, bob] = [1, 2].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        let gen0 = ring(&net, &alice, &rid).await.expect("alice").id_key;
        let (body, bx, key) = (vec![0x42; 8], vec![0x43; 8], [0x5a; 32]);
        let admins: BTreeSet<String> = [hex::encode(alice.keys.nid())].into();
        let att = charter_private::auth::sign_commit(
            &alice.keys,
            &rid,
            &gen0,
            0,
            &key,
            &body,
            &bx,
            &admins,
        )
        .expect("sign");
        let ct = charter_private::auth::commits_tag(&gen0, &rid);
        let record = msg::op(
            msg::T_COMMIT,
            vec![
                ("ag.ct", b32(ct), true),
                ("ag.ep", AttributeValue::U64(0), true),
                ("ag.box", AttributeValue::Bytes(bx), false),
                ("ag.att", AttributeValue::Bytes(att), false),
            ],
            body,
            1_000,
        )
        .expect("op");
        charter_sync::write::send(
            &net,
            &alice.keys.wallet(),
            &alice.cfg.pipeline,
            vec![vec![record]],
        )
        .await
        .expect("junk record");
        invite(&net, &alice, &bob, &rid, Role::Read).await;
        let b = ring(&net, &bob, &rid).await.expect("bob joins");
        assert_eq!(b.role(&bob.keys.nid()), Some(Role::Read));
    });
}

/// A removed member whose fetch finds a peer's entries incomplete is told plainly that it
/// was removed, under the stable `E_CONTROL_NOT_LIVE` code and with no raw byte arrays; a
/// member seeing the same gets the plain completeness failure, in hex.
#[test]
fn a_removed_members_incomplete_fetch_says_it_was_removed() {
    run(async {
        let net = Net::new();
        let [alice, bob, carol] = [1, 2, 3].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Read).await;
        invite(&net, &alice, &carol, &rid, Role::Read).await;
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "w");
        commit(&work, &[("a.txt", b"one")], "c1");
        private_push(&net, &alice, &rid, &work).await.expect("push");
        let fetch = |p: &'static Peer| async {
            let b = charter_private::for_repo(&net, &p.cfg, Some(&p.keys), &rid).await?;
            charter_sync::sync(&b, &p.repo(&rid), &charter_sync::Want::default()).await
        };
        let (bob, carol): (&'static Peer, &'static Peer) =
            (Box::leak(Box::new(bob)), Box::leak(Box::new(carol)));
        fetch(bob).await.expect("bob reads while a member");
        let ctl = alice.keys.control_address(&rid).into_array();
        let q = charter_store::Query::from(observe::creators(&BTreeSet::from([ctl])));
        let rows = charter_store::paging::offset_all(&net, &q, observe::full_select())
            .await
            .expect("rows");
        let pushed = rows
            .last()
            .and_then(|r| r.key)
            .expect("the push's last row");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob.keys.nid())
            .await
            .expect("remove bob");
        net.mem.withhold(pushed);

        assert!(
            ring(&net, bob, &rid).await.expect("bob").removed,
            "bob's refresh sees the removal"
        );
        let e = fetch(bob).await.expect_err("incomplete");
        let text = e.to_string();
        assert_eq!(e.code(), charter_core::ErrorCode::ControlNotLive, "{text}");
        assert!(text.contains("removed from this private repo"), "{text}");
        assert!(!text.contains('['), "no raw byte arrays: {text}");
        let e = fetch(carol).await.expect_err("incomplete for a member too");
        let text = e.to_string();
        assert_eq!(e.code(), charter_core::ErrorCode::ControlNotLive, "{text}");
        assert!(!text.contains("removed") && !text.contains('['), "{text}");
    });
}
