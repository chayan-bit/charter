//! Helpers shared by the private-repo security regression tests.

use charter_core::{Kind, Rid};
use charter_private::{Keyring, Role, Sealed};
use charter_store::Pred;
use charter_sync::observe;

use super::*;

pub async fn ring(net: &Net, p: &Peer, rid: &Rid) -> charter_sync::Result<Keyring> {
    charter_private::ring_for(net, &p.cfg, Some(&p.keys), rid)
        .await?
        .ok_or_else(|| charter_sync::SyncError::coded(charter_core::ErrorCode::Internal, "public"))
}

pub async fn invite(net: &Net, alice: &Peer, bob: &Peer, rid: &Rid, role: Role) {
    let kp = charter_private::publish_key_package(net, &bob.cfg, &bob.keys)
        .await
        .expect("key package");
    charter_private::add_member(net, &alice.cfg, &alice.keys, rid, &kp, role)
        .await
        .expect("add");
}

pub async fn private_push(
    net: &Net,
    p: &Peer,
    rid: &Rid,
    work: &std::path::Path,
) -> charter_sync::Result<()> {
    let ring = ring(net, p, rid).await?;
    push(&Sealed::new(net, ring), p, rid, work, &["refs/heads/main"]).await
}

/// Anchors and heads of `rid` that `r` opens (rows it cannot open keep `ag.ep`).
pub async fn opened_git_state(net: &Net, r: Keyring, rid: &Rid) -> usize {
    let s = Sealed::new(net, r);
    let kinds = Pred::Or(vec![observe::kind(Kind::Anchor), observe::kind(Kind::Head)]);
    let f = Pred::And(vec![kinds, observe::b32("ag.rid", rid)]);
    observe::all(&s, f)
        .await
        .expect("git state")
        .iter()
        .filter(|o| o.attr("ag.ep").is_none())
        .count()
}

/// Peer `p`'s refs as `viewer` accepts them, by tip.
pub async fn accepts(net: &Net, viewer: &Peer, rid: &Rid, p: &Peer, tip: &str) -> bool {
    let want = charter_sync::Want {
        extra: [p.keys.nid()].into(),
        ..Default::default()
    };
    let r = ring(net, viewer, rid).await.expect("viewer");
    let view = charter_sync::sync(&Sealed::new(net, r), &viewer.repo(rid), &want)
        .await
        .expect("sync");
    let tip = hex::decode(tip).expect("hex");
    view.peers
        .get(&p.keys.nid())
        .and_then(|v| v.refs_body())
        .is_some_and(|r| r.refs.iter().any(|e| e.oid == tip))
}

/// A device-head push (what the CLI's helper does) through `a`.
pub async fn head_push<A: charter_store::Arkiv>(
    a: &A,
    p: &Peer,
    rid: &Rid,
    work: &std::path::Path,
) -> charter_sync::Result<()> {
    let repo = p.repo(rid);
    let view = Box::pin(charter_sync::sync(a, &repo, &p.want())).await?;
    let git = charter_sync::git::Git::work(Some(work));
    let hp = charter_sync::HeadPush::prepare(
        a,
        &repo,
        &p.cfg,
        &p.keys,
        view,
        &git,
        &[upd("refs/heads/main")],
    )?;
    Box::pin(hp.run()).await
}

/// Extends `p`'s head of the repo from `by`'s wallet, as keepalive or a mirror would.
pub async fn extend_head(net: &Net, by: &Peer, p: &Peer) {
    let f = Pred::And(vec![
        observe::kind(Kind::Head),
        observe::b32("ag.nid", &p.keys.nid()),
    ]);
    let heads = observe::all(net, f).await.expect("heads");
    let ops = heads
        .iter()
        .map(|o| charter_store::extend_op(o.key.into(), by.cfg.control_lifetime))
        .collect();
    charter_sync::write::send(net, &by.keys.wallet(), &by.cfg.pipeline, vec![ops])
        .await
        .expect("extend");
}
