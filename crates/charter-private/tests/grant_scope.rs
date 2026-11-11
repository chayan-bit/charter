//! A published grant reveals only the epochs it names: commit rows carry just their own
//! epoch's key, welcome rows carry the history under a key only that epoch's members
//! derive, while joiners still get the full history and members still read everything.

mod common;

use charter_core::Rid;
use charter_private::msg::{self, T_COMMIT, T_WELCOME, open_box};
use charter_private::{Keyring, Role};
use common::sec::*;
use common::*;

/// A repo with a push at epoch 0, then bob, carol and dan added (epochs 1 to 3) and a
/// push at epoch 3. Returns the network, the four peers, an outsider, the repo and the
/// work tree.
async fn four_epochs() -> (Net, [Peer; 5], Rid, tempfile::TempDir) {
    let net = Net::new();
    let peers = [1, 2, 3, 4, 5].map(|s| Peer::new(&net, s));
    let [alice, bob, carol, dan, _eve] = &peers;
    let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
        .await
        .expect("init");
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "w");
    commit(&work, &[("a.txt", b"epoch 0")], "c0");
    private_push(&net, alice, &rid, &work)
        .await
        .expect("push 0");
    for p in [bob, carol, dan] {
        invite(&net, alice, p, &rid, Role::Write).await;
    }
    commit(&work, &[("a.txt", b"epoch 3")], "c3");
    private_push(&net, alice, &rid, &work)
        .await
        .expect("push 3");
    (net, peers, rid, tmp)
}

/// Every keyring that a commit or welcome row for `epoch` opens to under `key`.
async fn boxes_opened(net: &Net, rid: &Rid, epoch: u64, key: &[u8; 32]) -> Vec<Keyring> {
    let mut opened = vec![];
    for t in [T_COMMIT, T_WELCOME] {
        for row in msg::find(net, t, vec![]).await.expect("rows") {
            if msg::u64_of(&row, "ag.ep") != Some(epoch) {
                continue;
            }
            if let Some(r) = msg::bytes(&row, "ag.box").and_then(|b| open_box(rid, epoch, key, b)) {
                opened.push(r);
            }
        }
    }
    opened
}

fn epochs(r: &Keyring) -> Vec<u64> {
    r.epochs.keys().copied().collect()
}

/// The leak this fixes: a grant for epoch 3 opened epoch 3's commit and welcome rows,
/// which held every earlier epoch's key sealed under epoch 3's key, so a reader holding
/// grants for epochs 0 and 3 also read epochs 1 and 2.
#[test]
fn a_grant_for_one_epoch_opens_no_earlier_epoch() {
    run(async {
        let (net, [alice, .., eve], rid, _tmp) = four_epochs().await;
        let full = ring(&net, &alice, &rid).await.expect("alice");
        assert_eq!(epochs(&full), vec![0, 1, 2, 3]);
        // Epoch 0 holds the root identity, which a grant reader needs to check signers.
        for e in [0, 3] {
            charter_private::publish_epoch_key(&net, &alice.cfg, &alice.keys, &rid, e)
                .await
                .expect("grant");
        }
        let key3 = full.epochs[&3];
        for r in boxes_opened(&net, &rid, 3, &key3).await {
            assert!(
                r.epochs.keys().all(|e| *e >= 3),
                "epoch 3's key opens a row holding epochs {:?}",
                epochs(&r)
            );
        }
        let eve_ring = charter_private::ring_for(&net, &eve.cfg, None, &rid)
            .await
            .expect("grant reader")
            .expect("private");
        assert_eq!(
            epochs(&eve_ring),
            vec![0, 3],
            "the grants cover epochs 0 and 3 only"
        );
        assert_eq!(eve_ring.epochs[&3], key3);
        assert_eq!(eve_ring.epochs[&0], full.epochs[&0]);
    });
}

/// A grant over a range is one grant per epoch, and opens exactly those epochs: not the
/// later ones, though their commits name the same admins.
#[test]
fn a_range_grant_opens_exactly_its_epochs() {
    run(async {
        let (net, [alice, .., eve], rid, _tmp) = four_epochs().await;
        let full = ring(&net, &alice, &rid).await.expect("alice");
        let grants =
            charter_private::publish_epoch_keys(&net, &alice.cfg, &alice.keys, &rid, 0..=1)
                .await
                .expect("grants");
        assert_eq!(grants.len(), 2);
        assert_eq!(
            grants[1],
            charter_private::export_epoch_key(&alice.cfg.home, &rid, 1).expect("export")
        );
        let eve_ring = charter_private::ring_for(&net, &eve.cfg, None, &rid)
            .await
            .expect("grant reader")
            .expect("private");
        assert_eq!(epochs(&eve_ring), vec![0, 1]);
        for e in [0, 1] {
            assert_eq!(eve_ring.epochs[&e], full.epochs[&e]);
        }
    });
}

/// A member added at the newest epoch still receives every earlier epoch's key, admin
/// set and epoch start, and reads the git state pushed before it joined.
#[test]
fn a_joiner_still_gets_full_history() {
    run(async {
        let (net, [alice, _, _, dan, _], rid, _tmp) = four_epochs().await;
        let full = ring(&net, &alice, &rid).await.expect("alice");
        let joined = ring(&net, &dan, &rid).await.expect("dan joins");
        assert_eq!(joined.epochs, full.epochs);
        assert_eq!(joined.adm_log, full.adm_log);
        assert_eq!(joined.rotated, full.rotated);
        let all = opened_git_state(&net, full, &rid).await;
        assert!(all > 0);
        assert_eq!(opened_git_state(&net, joined, &rid).await, all);
    });
}

/// Members who apply the later commits (which now carry only their own epoch) still end
/// with every epoch and read everything.
#[test]
fn members_still_read_everything() {
    run(async {
        let (net, [alice, bob, carol, _, _], rid, _tmp) = four_epochs().await;
        let full = ring(&net, &alice, &rid).await.expect("alice");
        let all = opened_git_state(&net, full.clone(), &rid).await;
        for p in [&bob, &carol] {
            let r = ring(&net, p, &rid).await.expect("member");
            assert_eq!(r.epochs, full.epochs);
            assert_eq!(r.adm_log, full.adm_log);
            assert_eq!(r.rotated, full.rotated);
            assert_eq!(opened_git_state(&net, r, &rid).await, all);
        }
    });
}
