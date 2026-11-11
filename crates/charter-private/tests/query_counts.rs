//! Private push, clone, fetch and sync must not need more queries than in v0.5.0
//! (docs/p2/private-repos.md "Cost"). The ceilings are the counts measured on v0.5.0.

mod common;

use charter_core::Rid;
use charter_private::{Keyring, Role, Sealed};
use common::*;

/// `(operation, queries on v0.5.0)`; join, add and remove may add queries and are only
/// printed (v0.5.0: 13, 2 and 1).
const CEILINGS: &[(&str, usize)] = &[
    ("push", 20),
    ("clone", 3),
    ("fetch", 12),
    ("sync", 6),
    ("grant reader clone", 14),
];

async fn ring(net: &Net, p: &Peer, rid: &Rid) -> Keyring {
    charter_private::ring_for(net, &p.cfg, Some(&p.keys), rid)
        .await
        .expect("ring")
        .expect("private")
}

#[test]
fn private_push_clone_fetch_and_sync_need_no_more_queries() {
    run(async {
        let net = Net::new();
        let (alice, bob, eve) = (Peer::new(&net, 1), Peer::new(&net, 2), Peer::new(&net, 5));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("counted"))
            .await
            .expect("init");
        let kp = charter_private::publish_key_package(&net, &bob.cfg, &bob.keys)
            .await
            .expect("kp");
        let mut got: Vec<(&str, usize)> = vec![];
        let mut measure = |name, net: &Net| got.push((name, net.take_queries().len()));
        net.take_queries();
        charter_private::add_member(&net, &alice.cfg, &alice.keys, &rid, &kp, Role::Write)
            .await
            .expect("add");
        measure("add (may grow)", &net);
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"v1")], "feat: v1");
        let push = |n: &'static [u8]| {
            commit(&work, &[("a", n)], "feat: next");
            async {
                let r = ring(&net, &alice, &rid).await;
                push(
                    &Sealed::new(&net, r),
                    &alice,
                    &rid,
                    &work,
                    &["refs/heads/main"],
                )
                .await
                .expect("push");
            }
        };

        push(b"v1").await;
        net.take_queries();
        push(b"v2").await;
        measure("push", &net);

        let r = ring(&net, &bob, &rid).await;
        let dest = tmp.path().join("b");
        clone_into(&Sealed::new(&net, r), &bob, &rid, &dest)
            .await
            .expect("join clone");
        measure("join and first clone (may grow)", &net);
        let r = ring(&net, &bob, &rid).await;
        clone_into(&Sealed::new(&net, r), &bob, &rid, &tmp.path().join("b2"))
            .await
            .expect("clone");
        measure("clone", &net);

        push(b"v3").await;
        net.take_queries();
        let r = ring(&net, &bob, &rid).await;
        clone_into(&Sealed::new(&net, r), &bob, &rid, &dest)
            .await
            .expect("fetch");
        measure("fetch", &net);

        let r = ring(&net, &bob, &rid).await;
        charter_sync::sync(&Sealed::new(&net, r), &bob.repo(&rid), &bob.want())
            .await
            .expect("sync");
        measure("sync", &net);

        let epoch = ring(&net, &alice, &rid).await.current().expect("cur").0;
        for e in 0..=epoch {
            charter_private::publish_epoch_key(&net, &alice.cfg, &alice.keys, &rid, e)
                .await
                .expect("grant");
        }
        net.take_queries();
        let r = ring(&net, &eve, &rid).await;
        clone_into(&Sealed::new(&net, r), &eve, &rid, &tmp.path().join("e"))
            .await
            .expect("grant reader clone");
        measure("grant reader clone", &net);

        net.take_queries();
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &bob.keys.nid())
            .await
            .expect("remove");
        measure("remove (may grow)", &net);

        eprintln!("query counts: {got:?}");
        for (name, ceiling) in CEILINGS {
            let n = got.iter().find(|g| g.0 == *name).expect("measured").1;
            assert!(
                n <= *ceiling,
                "{name}: {n} queries, v0.5.0 needed {ceiling}"
            );
        }
    });
}
