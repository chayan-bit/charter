//! Fetch and clone (design 4.3): verified view, pack download and `index-pack`, the
//! connectivity check, then the commit (marks first, then namespace refs, then applied
//! packs). Nothing reaches namespace refs before every check passed for that peer.

use std::collections::{BTreeMap, BTreeSet};

use charter_core::{ErrorCode, Hash32, IdMark, Nid, RefsBody, canonical_refs};
use charter_store::Arkiv;

use crate::git::RefOp;
use crate::ids::{hex32, to_text};
use crate::repo::{Repo, WARN_EXPIRY_BLOCKS, namespace};
use crate::state::State;
use crate::view::{self, View, Want};
use crate::{Result, SyncError, packs, warn};

/// Runs the fetch sync for `repo`; returns the verified view it committed.
pub async fn sync<A: Arkiv>(a: &A, repo: &Repo, want: &Want) -> Result<View> {
    use charter_store::trace::span;
    let _t = span(|| "sync".into());
    let mut state = State::load(&repo.state_path())?;
    let t = span(|| "sync.view".into());
    let mut view = view::build(a, &repo.rid, &state, want).await?;
    drop(t);
    warn_expiry(&view);
    let must: BTreeSet<Nid> = view.delegates().iter().chain(&want.must).copied().collect();
    let t = span(|| "sync.download".into());
    let applied = apply_all(a, repo, &mut view, &state, &must).await?;
    drop(t);
    let t = span(|| "sync.checks".into());
    check_connected(repo, &mut view, &must)?;
    check_bases(repo, &mut view, &must)?;
    drop(t);
    // After the packs are in the cache, so the program sees real git ancestry.
    let t = span(|| "sync.program".into());
    view.program = crate::program::enforce(a, &view, repo.git()).await?;
    drop(t);
    let _c = span(|| "sync.commit".into());
    commit(repo, &view, &mut state, applied)?;
    Ok(view)
}

fn skip_or_fail(view: &mut View, must: &BTreeSet<Nid>, nid: &Nid, e: SyncError) -> Result<()> {
    if must.contains(nid) {
        return Err(e);
    }
    warn(&format!("skipping peer {}: {e}", to_text(nid)));
    view.peers.remove(nid);
    Ok(())
}

/// Packs and manifests newly applied by this fetch.
#[derive(Debug, Default)]
pub struct Applied {
    pub packs: BTreeSet<Hash32>,
    pub manifests: BTreeSet<Hash32>,
}

async fn apply_all<A: Arkiv>(
    a: &A,
    repo: &Repo,
    view: &mut View,
    state: &State,
    must: &BTreeSet<Nid>,
) -> Result<Applied> {
    let mut applied = Applied::default();
    let mut todo: Vec<Download> = vec![];
    let nids: Vec<Nid> = view.peers.keys().copied().collect();
    for nid in nids {
        match download_peer(a, repo, view, &nid, state, &applied, &todo).await {
            Ok((known, fetched)) => {
                applied.manifests.extend(known);
                todo.extend(fetched);
            }
            Err(e) => skip_or_fail(view, must, &nid, e)?,
        }
    }
    let failed = index_until_stuck(repo, todo, &mut applied);
    for (nid, e) in failed {
        // A v2 head may list a pack whose thin bases left with a later snapshot: no live
        // ref needs it then, and the connectivity check below decides (head-model 4.4).
        if view
            .peers
            .get(&nid)
            .is_some_and(|p| p.head.is_none() && p.v2.is_some())
        {
            // shortcut: such a pack is downloaded again on every fetch; record it if that shows up in cost.
            warn(&format!(
                "peer {}: a listed pack did not index ({e}); checking live refs",
                to_text(&nid)
            ));
            continue;
        }
        if view.peers.contains_key(&nid) {
            skip_or_fail(view, must, &nid, e)?;
        }
    }
    Ok(applied)
}

/// A downloaded, verified pack waiting for `index-pack`.
#[derive(Debug)]
struct Download {
    nid: Nid,
    manifest: Hash32,
    pack_id: Hash32,
    tmp: std::path::PathBuf,
}

/// Downloads one peer's missing packs in anchor order; returns the manifests whose packs
/// are already in the cache and the downloads still to index.
async fn download_peer<A: Arkiv>(
    a: &A,
    repo: &Repo,
    view: &View,
    nid: &Nid,
    state: &State,
    applied: &Applied,
    queued: &[Download],
) -> Result<(Vec<Hash32>, Vec<Download>)> {
    let (mut known, mut fetched) = (vec![], vec![]);
    let v2 = view.peers.get(nid).and_then(|p| p.v2.as_ref());
    if let Some(v2) = v2.filter(|v| v.heads().next().is_some()) {
        let in_cache = |id: &Hash32| {
            state.applied_packs.contains(&hex32(id))
                || state.applied_manifests.contains(&hex32(id))
                || applied.packs.contains(id)
                || queued.iter().any(|d| d.pack_id == *id)
        };
        for d in crate::pieces::download_v2(a, repo, &view.rid, v2, &in_cache).await? {
            fetched.push(Download {
                nid: *nid,
                manifest: d.0,
                pack_id: d.1,
                tmp: d.2,
            });
        }
        return Ok((known, fetched));
    }
    let Some(head) = view.peers.get(nid).and_then(|p| p.head.as_ref()) else {
        return Ok((known, fetched));
    };
    let bulk = view.peers[nid].info.binds.bulk_set();
    for (h, m) in &head.manifests {
        let in_cache =
            state.applied_packs.contains(&hex32(&m.pack_id)) || applied.packs.contains(&m.pack_id);
        if in_cache {
            known.push(*h);
            continue;
        }
        let tmp = repo.tmp().join(format!("{}.pack", hex32(&m.pack_id)));
        // The same pack named twice is downloaded once; the second entry follows the first.
        let in_flight = queued
            .iter()
            .chain(&fetched)
            .any(|d: &Download| d.pack_id == m.pack_id);
        if in_flight {
            fetched.push(Download {
                nid: *nid,
                manifest: *h,
                pack_id: m.pack_id,
                tmp,
            });
            continue;
        }
        if let Err(e) = packs::download(a, &view.rid, m, &bulk, &tmp).await {
            let _ = std::fs::remove_file(&tmp);
            fetched.iter().for_each(|d: &Download| {
                let _ = std::fs::remove_file(&d.tmp);
            });
            return Err(e);
        }
        fetched.push(Download {
            nid: *nid,
            manifest: *h,
            pack_id: m.pack_id,
            tmp,
        });
    }
    Ok((known, fetched))
}

/// Indexes downloads in rounds: a thin pack whose delta bases arrive in another peer's pack
/// fails until that pack is in, so failures are retried while any round makes progress.
/// Returns the last error of every download that never indexed.
fn index_until_stuck(
    repo: &Repo,
    mut todo: Vec<Download>,
    applied: &mut Applied,
) -> Vec<(Nid, SyncError)> {
    loop {
        let before = todo.len();
        let mut failed = vec![];
        for d in std::mem::take(&mut todo) {
            if applied.packs.contains(&d.pack_id) {
                applied.manifests.insert(d.manifest);
                continue;
            }
            match repo.git().index_pack(&d.tmp) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&d.tmp);
                    applied.packs.insert(d.pack_id);
                    applied.manifests.insert(d.manifest);
                }
                Err(e) => failed.push((d, e)),
            }
        }
        if failed.is_empty() {
            return vec![];
        }
        if failed.len() == before {
            return failed
                .into_iter()
                .map(|(d, e)| {
                    let _ = std::fs::remove_file(&d.tmp);
                    (d.nid, e)
                })
                .collect();
        }
        todo = failed.into_iter().map(|(d, _)| d).collect();
    }
}

fn oids(refs: &RefsBody) -> Vec<String> {
    refs.refs.iter().map(|e| hex::encode(&e.oid)).collect()
}

fn check_connected(repo: &Repo, view: &mut View, must: &BTreeSet<Nid>) -> Result<()> {
    let old: Vec<String> = repo
        .git()
        .refs("refs/namespaces/")?
        .into_iter()
        .map(|(_, o)| o)
        .collect();
    let nids: Vec<Nid> = view.peers.keys().copied().collect();
    for nid in nids {
        let Some(refs) = view.peers.get(&nid).and_then(|p| p.refs_body()) else {
            continue;
        };
        if !repo.git().connected(&oids(refs), &old) {
            let e = SyncError::coded(
                ErrorCode::MissingObjects,
                format!("refs of peer {} name objects not on Arkiv", to_text(&nid)),
            );
            skip_or_fail(view, must, &nid, e)?;
        }
    }
    Ok(())
}

/// A format v2 base is the writer's claim that `new` fast-forwards it, and the program fold
/// trusts it (head-model 8); every reader holding the objects of the live refs checks the
/// claim of each winning entry and refuses a head that lies (`E_BAD_REFS`).
fn check_bases(repo: &Repo, view: &mut View, must: &BTreeSet<Nid>) -> Result<()> {
    let nids: Vec<Nid> = view.peers.keys().copied().collect();
    for nid in nids {
        let Some(v2) = view.peers.get(&nid).and_then(|p| p.v2.as_ref()) else {
            continue;
        };
        let bad = v2.merged.iter().find(|(_, w)| {
            let e = &w.entry;
            !e.is_tombstone()
                && !e.base.is_empty()
                && e.base != e.oid
                && crate::program::is_ancestor(repo.git(), &e.base, &e.oid) != Some(true)
        });
        if let Some((name, w)) = bad {
            let e = SyncError::coded(
                ErrorCode::BadRefs,
                format!(
                    "peer {} signed {name} at {} with base {}, which is not its ancestor",
                    to_text(&nid),
                    hex::encode(&w.entry.oid),
                    hex::encode(&w.entry.base)
                ),
            );
            skip_or_fail(view, must, &nid, e)?;
        }
    }
    Ok(())
}

fn commit(repo: &Repo, view: &View, state: &mut State, applied: Applied) -> Result<()> {
    state.chain_id = Some(view.chain_id);
    let id = view.identity.head();
    let new_id = IdMark {
        rev: id.body.rev,
        hash: id.h,
    };
    let id_mark = match state.id_mark()? {
        Some(old) => old.advance(&new_id)?,
        None => new_id,
    };
    state.set_id_mark(&id_mark);
    for pv in view.peers.values() {
        commit_v2(state, pv)?;
    }
    for (nid, pv) in &view.peers {
        let Some(head) = &pv.head else { continue };
        let mark = match state.peer_mark(nid)? {
            Some(old) => old.advance(&head.mark)?,
            None => head.mark.clone(),
        };
        state.set_peer_mark(nid, &mark);
    }
    state.note_equivocations(&view.equivocations);
    state.save(&repo.state_path())?;
    for (nid, pv) in &view.peers {
        if let Some(refs) = pv.refs_body() {
            set_namespace(repo, nid, refs)?;
        }
    }
    state.applied_packs.extend(applied.packs.iter().map(hex32));
    state
        .applied_manifests
        .extend(applied.manifests.iter().map(hex32));
    state.save(&repo.state_path())
}

/// Device marks and landings of a v2 peer (head-model 6).
fn commit_v2(state: &mut State, pv: &crate::view::PeerView) -> Result<()> {
    let Some(v2) = &pv.v2 else { return Ok(()) };
    for (dev, head) in v2.heads() {
        let mark = match state.device_mark(&dev.nid)? {
            Some(old) => old.advance(&head.mark())?,
            None => head.mark(),
        };
        state.set_device_mark(&dev.nid, &mark);
    }
    for (h, l) in v2.landings.iter().filter(|(_, l)| l.block != u64::MAX) {
        state.landings.insert(hex32(h), *l);
    }
    Ok(())
}

/// Moves `refs/namespaces/<nid>/` to exactly `refs`, in one transaction with old values.
pub fn set_namespace(repo: &Repo, nid: &Nid, refs: &RefsBody) -> Result<()> {
    let ns = namespace(nid);
    let current: BTreeMap<String, String> = repo.git().refs(&ns)?.into_iter().collect();
    let desired: BTreeMap<String, String> = refs
        .refs
        .iter()
        .map(|e| (format!("{ns}{}", e.name), hex::encode(&e.oid)))
        .collect();
    let mut ops = vec![];
    for (name, new) in &desired {
        let old = current.get(name).cloned();
        if old.as_ref() != Some(new) {
            ops.push(RefOp::Update {
                name: name.clone(),
                new: new.clone(),
                old,
            });
        }
    }
    for (name, old) in current.iter().filter(|(n, _)| !desired.contains_key(*n)) {
        ops.push(RefOp::Delete {
            name: name.clone(),
            old: old.clone(),
        });
    }
    repo.git().update_refs(&ops)
}

fn warn_expiry(view: &View) {
    let min = view
        .peers
        .values()
        .flat_map(|p| p.refs.iter().chain(&p.manifests).chain(&p.anchors))
        .map(|o| o.expires_at)
        .min();
    if let Some(m) = min.filter(|m| *m < view.head_block + WARN_EXPIRY_BLOCKS) {
        let days = m.saturating_sub(view.head_block) / crate::repo::BLOCKS_PER_DAY;
        warn(&format!(
            "an entity this repo needs expires in {days} days (block {m}); run `chart keepalive`"
        ));
    }
}

/// What the helper prints for `list`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listing {
    /// `(ref name, oid hex)`.
    pub refs: Vec<(String, String)>,
    /// Target of the `HEAD` symref, if that ref is listed.
    pub head: Option<String>,
}

/// Canonical refs (design 3.8; with a pinned program, its accepted heads), or one peer's verified refs for `arkiv://<rid>/<nid>`.
pub fn listing(view: &View, peer: Option<&Nid>) -> Listing {
    let id = &view.identity.head().body;
    let refs: Vec<(String, String)> = match peer {
        Some(nid) => view
            .peers
            .get(nid)
            .and_then(|p| p.refs_body())
            .map(oids_named)
            .unwrap_or_default(),
        None if view.program.is_some() => view
            .program
            .iter()
            .flat_map(|p| &p.outcome.heads)
            .map(|(n, o)| (n.clone(), hex::encode(o)))
            .collect(),
        None => {
            let votes: BTreeMap<Nid, RefsBody> = view
                .peers
                .iter()
                .filter_map(|(n, p)| p.refs_body().map(|r| (*n, r.clone())))
                .collect();
            let c = canonical_refs(id, &votes);
            c.no_quorum
                .iter()
                .for_each(|r| warn(&format!("no quorum for {r}")));
            c.refs
                .into_iter()
                .map(|(n, o)| (n, hex::encode(o)))
                .collect()
        }
    };
    let default = format!("refs/heads/{}", id.default_branch);
    let head = refs.iter().any(|(n, _)| *n == default).then_some(default);
    Listing { refs, head }
}

fn oids_named(refs: &RefsBody) -> Vec<(String, String)> {
    refs.refs
        .iter()
        .map(|e| (e.name.clone(), hex::encode(&e.oid)))
        .collect()
}

/// The local signer's verified refs (`list for-push`).
pub fn own_refs(view: &View, nid: &Nid) -> Vec<(String, String)> {
    view.peers
        .get(nid)
        .and_then(|p| p.refs_body())
        .map(oids_named)
        .unwrap_or_default()
}
