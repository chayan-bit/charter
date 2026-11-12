//! Private repos whose device head goes past the head body cap: the v4 head, its ref table
//! and its pieces are sealed like every other row, members still read every ref, and an
//! extended v4 head is admitted by its body's landing like a v3 one.

mod common;

use charter_core::{AttrValue, Envelope, Kind, Rid};
use charter_private::entity::GRACE_BLOCKS;
use charter_private::{Role, Sealed};
use charter_store::{Arkiv, Pred};
use charter_sync::observe;
use common::sec::*;
use common::*;

/// Past the 62 KB head body (about 1,350 short tags), as in the bench gate.
const TAGS: usize = 1_500;

/// Points `TAGS` tags at `work`'s HEAD; returns every ref name to push.
fn tag_all(work: &std::path::Path) -> Vec<String> {
    let oid = rev(work, "HEAD");
    let tags: Vec<String> = (0..TAGS).map(|i| format!("refs/tags/v{i:05}")).collect();
    let packed: String = tags.iter().map(|t| format!("{oid} {t}\n")).collect();
    std::fs::write(work.join(".git/packed-refs"), packed).expect("packed-refs");
    [tags, vec!["refs/heads/main".into()]].concat()
}

/// A device-head push of `names` through `a`.
async fn head_push_refs<A: Arkiv>(
    a: &A,
    p: &Peer,
    rid: &Rid,
    work: &std::path::Path,
    names: &[String],
) -> charter_sync::Result<()> {
    let repo = p.repo(rid);
    let view = Box::pin(charter_sync::sync(a, &repo, &p.want())).await?;
    let git = charter_sync::git::Git::work(Some(work));
    let updates: Vec<_> = names.iter().map(|n| upd(n)).collect();
    let hp = charter_sync::HeadPush::prepare(a, &repo, &p.cfg, &p.keys, view, &git, &updates)?;
    Box::pin(hp.run()).await
}

/// Envelope kinds of `p`'s heads as `viewer` opens them.
async fn head_kinds(net: &Net, viewer: &Peer, rid: &Rid, p: &Peer) -> Vec<Kind> {
    let s = Sealed::new(net, ring(net, viewer, rid).await.expect("ring"));
    let f = Pred::And(vec![
        observe::kind(Kind::Head),
        observe::b32("ag.nid", &p.keys.nid()),
    ]);
    observe::all(&s, f)
        .await
        .expect("heads")
        .iter()
        .filter_map(|o| match o.attr("ag.body") {
            Some(AttrValue::Bytes(b)) => Envelope::decode(b).ok().map(|e| e.kind),
            _ => None,
        })
        .collect()
}

/// Whether `viewer` sees `p` publishing ref `name`.
async fn sees_ref(net: &Net, viewer: &Peer, rid: &Rid, p: &Peer, name: &str) -> bool {
    let want = charter_sync::Want {
        extra: [p.keys.nid()].into(),
        ..Default::default()
    };
    let r = ring(net, viewer, rid).await.expect("viewer");
    let view = charter_sync::sync(&Sealed::new(net, r), &viewer.repo(rid), &want)
        .await
        .expect("sync");
    view.peers
        .get(&p.keys.nid())
        .and_then(|v| v.refs_body())
        .is_some_and(|r| r.refs.iter().any(|e| e.name == name))
}

#[test]
fn a_private_head_past_the_body_cap_keeps_its_ref_table_sealed() {
    run(async {
        let net = Net::new();
        let [alice, bob, dan] = [1, 2, 4].map(|s| Peer::new(&net, s));
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        invite(&net, &alice, &bob, &rid, Role::Write).await;
        invite(&net, &alice, &dan, &rid, Role::Read).await;
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "b");
        commit(&work, &[("b", b"v1")], "feat: b1");
        let names = tag_all(&work);
        let sealed = Sealed::new(&net, ring(&net, &bob, &rid).await.expect("ring"));
        let from = net.head().await.expect("head");
        head_push_refs(&sealed, &bob, &rid, &work, &names)
            .await
            .expect("bob pushes past the body cap");

        assert!(
            head_kinds(&net, &dan, &rid, &bob)
                .await
                .contains(&Kind::HeadV4),
            "the head names a ref table"
        );
        assert!(
            sees_ref(&net, &dan, &rid, &bob, "refs/tags/v01499").await,
            "a member reads refs from the table"
        );

        // On chain, every row the push wrote (pieces of the pack and of the table, the head)
        // is sealed: it carries an epoch and never the plain rid.
        let (mut pieces, to) = (0, net.head().await.expect("head"));
        for n in from..=to {
            for tx in net.block_txs(n).await.expect("block") {
                for row in charter_sync::heads::calldata_rows(&net, &tx) {
                    if row.attr("ag.rid") == Some(&AttrValue::Bytes32(rid)) {
                        panic!("a row in block {n} names the plain rid");
                    }
                    if row.attr("ag.kind") == Some(&AttrValue::Str("piece".into())) {
                        assert!(row.attr("ag.ep").is_some(), "an unsealed piece in {n}");
                        pieces += 1;
                    }
                }
            }
        }
        assert!(pieces > 0, "the table went out as pieces");
    });
}

/// The v4 twin of `an_extended_head_that_landed_in_the_grace_window_is_kept`: the landing
/// check must read a v4 body's `at`, or the head is dropped once someone extends it.
#[test]
fn an_extended_v4_head_that_landed_in_the_grace_window_is_kept() {
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
        let work = work_repo(tmp.path(), "b");
        commit(&work, &[("b", b"v1")], "feat: b1");
        let names = tag_all(&work);
        let bob_old = ring(&net, &bob, &rid).await.expect("b");
        charter_private::remove_member(&net, &alice.cfg, &alice.keys, &rid, &mallory.keys.nid())
            .await
            .expect("remove mallory");

        // Bob had not refreshed: his push seals under the old epoch, inside the grace window.
        head_push_refs(&Sealed::new(&net, bob_old), &bob, &rid, &work, &names)
            .await
            .expect("bob pushes in the window");
        net.mine(GRACE_BLOCKS + 1);
        extend_head(&net, &alice, &bob).await;

        assert!(
            head_kinds(&net, &dan, &rid, &bob)
                .await
                .contains(&Kind::HeadV4),
            "the extended head is a v4 head and is admitted"
        );
        assert!(
            sees_ref(&net, &dan, &rid, &bob, "refs/tags/v01499").await,
            "bob's table refs are kept"
        );
    });
}
