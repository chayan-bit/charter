//! Reading a private repo's group state from Arkiv: join from the one Welcome for one of
//! this member's key packages that an admin attested, then apply attested admin commits in
//! Arkiv order, one epoch at a time. Also decides, for any rid, whether it is public,
//! private and readable, or private and closed (`E_NOT_MEMBER`).

use std::collections::BTreeSet;

use charter_core::{AttrValue, ErrorCode, IdBody, Nid, Observed, Rid};
use charter_store::{Arkiv, Pred, Query};
use charter_sync::{Config, Keys, Result, SyncError, observe, view};

use crate::auth::{self, CTX_COMMIT, CTX_WELCOME, att_msg, bad, open_att, verifies};
use crate::keyring::{FORMAT, Keyring, Role, not_member};
use crate::mls::Mls;
use crate::msg::{
    T_COMMIT, T_GROUP, T_PUB, T_WELCOME, bytes, find, open_box, rid_pred, tag, u64_of,
};
use crate::seal::{Key, blind};
use crate::{Sealed, entity};

/// This member's MLS state and up-to-date keyring for `rid` (`None`: not a member).
pub async fn refresh<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
) -> Result<(Mls, Option<Keyring>)> {
    let mls = Mls::open(&cfg.home, keys.nid())?;
    let mut ring = Keyring::load(&cfg.home, rid)?;
    if ring.is_none() {
        ring = join(a, &mls, rid).await?;
    }
    if let Some(r) = ring.as_mut() {
        apply_commits(a, &mls, r).await?;
        check_history(r)?;
        r.save(&cfg.home)?;
        mls.save()?;
    }
    Ok((mls, ring))
}

/// Joins `rid`'s group. Every Welcome for one of our key packages is evaluated; exactly one
/// distinct group must verify ([`verify_welcome`]), so a Welcome someone else made for a
/// published key package can neither win a race nor hide the real one.
async fn join<A: Arkiv>(a: &A, mls: &Mls, rid: &Rid) -> Result<Option<Keyring>> {
    let mut rows = vec![];
    for kp in &mls.kps {
        let found = find(a, T_WELCOME, vec![observe::b32("ag.kp", kp)]).await?;
        rows.extend(found.into_iter().map(|w| (*kp, w)));
    }
    let mut ok: Vec<(usize, (Keyring, Vec<Observed>))> = vec![];
    let mut refused = None;
    for (i, (kp, w)) in rows.iter().enumerate() {
        let snap = mls.snapshot()?;
        let verdict = verify_welcome(a, mls, rid, kp, w).await;
        mls.restore(snap)?;
        match verdict {
            Ok(Some(ring)) => ok.push((i, ring)),
            Ok(None) => {}
            Err(e) => refused = Some(e),
        }
    }
    let groups: BTreeSet<(Vec<u8>, u64)> = ok
        .iter()
        .map(|(_, (r, _))| (r.group.clone(), r.current().map_or(0, |c| c.0)))
        .collect();
    if groups.len() > 1 {
        return Err(SyncError::coded(
            ErrorCode::Equivocation,
            "several admin-attested welcomes for your key package name different groups; \
             ask the repo's admins which is real before you join",
        ));
    }
    let Some((i, (mut ring, _))) = ok.into_iter().next() else {
        return refused.map_or(Ok(None), Err);
    };
    let body = bytes(&rows[i].1, "ag.body").unwrap_or_default();
    let g = mls.finish(mls.stage(body)?)?;
    ring.group = g.group_id().as_slice().to_vec();
    // Not handed to the clone as prefetched rows: the clone must read them itself, so its
    // memo records the identity page and later fetches replay it instead of reading again.
    Ok(Some(ring))
}

/// The keyring a Welcome hands over, if it is for us and verifies: its lockbox opens under
/// its epoch key, and its attestation is signed over the Welcome and lockbox by an admin
/// whose authority chains to the root identity (whose content hash is the rid). `Ok(None)`:
/// not for us; an error: for us but unverifiable.
async fn verify_welcome<A: Arkiv>(
    a: &A,
    mls: &Mls,
    rid: &Rid,
    kp: &charter_core::Hash32,
    w: &Observed,
) -> Result<Option<(Keyring, Vec<Observed>)>> {
    let (Some(body), Some(bx)) = (bytes(w, "ag.body"), bytes(w, "ag.box")) else {
        return Ok(None);
    };
    let Ok(staged) = mls.stage(body) else {
        return Ok(None);
    };
    let (epoch, key) = mls.staged_key(&staged, rid)?;
    let Some(mut ring) = open_box(rid, epoch, &key, bx) else {
        return Ok(None);
    };
    let Some((signer, sig)) = bytes(w, "ag.att").and_then(|s| open_att(&key, rid, epoch, s)) else {
        return Err(SyncError::coded(
            ErrorCode::UnsupportedVersion,
            "this welcome carries no admin attestation (it was made by charter before 0.5.1 \
             or by someone else); ask an admin on 0.5.1 or later to add you again",
        ));
    };
    let pk = charter_sync::cob::peer_key_of(a, &signer).await?;
    let msg = att_msg(rid, epoch, body, bx, Some(kp));
    if !pk.is_some_and(|pk| verifies(&pk, CTX_WELCOME, &msg, &sig)) {
        return Err(bad("welcome attestation does not verify"));
    }
    ring.epochs.insert(epoch, key);
    ring.rotated.insert(epoch, w.block);
    let roles = ring.roles.clone();
    ring.record_roles(epoch, &roles);
    let Some((roots, ids)) = root_rows(a, &ring).await? else {
        return Err(bad(
            "the welcome's keys do not open the repo's root identity",
        ));
    };
    let admins = auth::admins(a, &ring, &roots).await?;
    if !admins.contains(&signer) || ring.role(&signer) != Some(Role::Admin) {
        return Err(bad(
            "welcome attested by someone who is not an admin of this repo",
        ));
    }
    Ok(Some((ring, ids)))
}

/// The root identity's delegates and the identity rows `ring` opens, or `None` when no
/// row opens to the root (content hash = rid, revision 0).
pub async fn root_rows<A: Arkiv>(
    a: &A,
    ring: &Keyring,
) -> Result<Option<(Vec<Nid>, Vec<Observed>)>> {
    let rows = observe::all(
        &Sealed::new(a, ring.clone()),
        view::identity_filter(&ring.rid),
    )
    .await?;
    Ok(root_of(&ring.rid, &rows).map(|d| (d, rows)))
}

fn root_of(rid: &Rid, rows: &[Observed]) -> Option<Vec<Nid>> {
    rows.iter().find_map(|o| {
        charter_core::open::<IdBody>(o)
            .ok()
            .filter(|e| e.h == *rid && e.body.is_root())
            .map(|e| e.body.delegates)
    })
}

/// Applies attested admin commits for the next epochs; any other commit is skipped and
/// undone. Stops once this member is removed: it then holds no key for later epochs.
async fn apply_commits<A: Arkiv>(a: &A, mls: &Mls, ring: &mut Keyring) -> Result<()> {
    let mut g = mls.load(&ring.group)?;
    loop {
        let gens = ring.id_keys().count();
        let rows = find(a, T_COMMIT, vec![rid_pred(ring)]).await?;
        derive_history(ring, &rows);
        for c in &rows {
            if !g.is_active() {
                break;
            }
            let next = g.epoch().as_u64() + 1;
            let (Some(body), Some(bx)) = (bytes(c, "ag.body"), bytes(c, "ag.box")) else {
                continue;
            };
            if u64_of(c, "ag.ep") != Some(next) {
                continue;
            }
            let snap = mls.snapshot()?;
            let admins = ring.clone();
            let is_admin = |n: &Nid| admins.role(n) == Some(Role::Admin);
            let Ok(Some(sender)) = mls.process(&mut g, body, is_admin) else {
                mls.restore(snap)?;
                g = mls.load(&ring.group)?;
                continue;
            };
            if !g.is_active() {
                break;
            }
            let (epoch, key) = mls.epoch_key(&g, &ring.rid)?;
            let newer = open_box(&ring.rid, epoch, &key, bx);
            if newer.is_none() || !commit_attested(a, ring, &sender, epoch, &key, c).await? {
                mls.restore(snap)?;
                g = mls.load(&ring.group)?;
                continue;
            }
            ring.epochs.insert(epoch, key);
            ring.rotated.insert(epoch, c.block);
            if let Some(newer) = newer {
                ring.merge(&newer);
            }
            let roles = ring.roles.clone();
            ring.record_roles(epoch, &roles);
        }
        // A removal started a new id-key generation: its later commits use it.
        if ring.id_keys().count() == gens || !g.is_active() {
            return Ok(());
        }
    }
}

/// Whether commit row `c` carries `sender`'s attestation of its bytes and lockbox. A commit
/// without one is accepted only while the repo is still in the legacy format.
async fn commit_attested<A: Arkiv>(
    a: &A,
    ring: &Keyring,
    sender: &Nid,
    epoch: u64,
    key: &Key,
    c: &Observed,
) -> Result<bool> {
    let (body, bx) = (
        bytes(c, "ag.body").unwrap_or_default(),
        bytes(c, "ag.box").unwrap_or_default(),
    );
    let Some((signer, sig)) = bytes(c, "ag.att").and_then(|s| open_att(key, &ring.rid, epoch, s))
    else {
        return Ok(ring.format < FORMAT);
    };
    if signer != *sender {
        return Ok(false);
    }
    let pk = charter_sync::cob::peer_key_of(a, &signer).await?;
    let msg = att_msg(&ring.rid, epoch, body, bx, None);
    Ok(pk.is_some_and(|pk| verifies(&pk, CTX_COMMIT, &msg, &sig)))
}

/// Where each epoch held started, from commit rows already read: the earliest row naming
/// that epoch whose lockbox opens under its key (nobody holds that key before the commit
/// lands). Also fills the role history of a keyring written before 0.5.1 from those
/// lockboxes. Costs no query.
fn derive_history(ring: &mut Keyring, rows: &[Observed]) {
    for c in rows {
        let Some(ep) = u64_of(c, "ag.ep") else {
            continue;
        };
        if ring.rotated.contains_key(&ep) {
            continue;
        }
        let opened = match (ring.epochs.get(&ep), bytes(c, "ag.box")) {
            (Some(k), Some(bx)) => match open_box(&ring.rid, ep, k, bx) {
                Some(b) => Some(b),
                None => continue,
            },
            (Some(_), None) => continue,
            (None, _) => None, // a grant reader without this epoch: earliest row bounds it
        };
        ring.rotated.insert(ep, c.block);
        if let Some(b) = opened
            && !ring.role_log.contains_key(&ep)
        {
            ring.record_roles(ep, &b.roles);
        }
    }
    let Some(first) = ring.epochs.keys().next().copied() else {
        return;
    };
    if ring.role_log.range(..=first).next().is_none() {
        // A legacy keyring's first epoch: its admins are the earliest admins on record.
        let seed: std::collections::BTreeMap<String, Role> = match ring.role_log.values().next() {
            Some(m) => m
                .iter()
                .filter(|(_, r)| **r == Some(Role::Admin))
                .map(|(n, _)| (n.clone(), Role::Admin))
                .collect(),
            None => ring.roles.clone(),
        };
        ring.record_roles(first, &seed);
    }
}

/// Fails loudly when some epoch's end cannot be placed (its commit expired before this
/// keyring learned it), instead of reading that epoch with no bound on late writes.
fn check_history(ring: &Keyring) -> Result<()> {
    let (Some(first), Ok((cur, _))) = (ring.epochs.keys().next(), ring.current()) else {
        return Ok(());
    };
    let missing: Vec<u64> = (first + 1..=cur)
        .filter(|e| !ring.rotated.contains_key(e))
        .collect();
    match missing.is_empty() {
        true => Ok(()),
        false => Err(SyncError::coded(
            ErrorCode::NotLive,
            format!(
                "cannot place the start of epochs {missing:?} of this private repo (their \
                 commits expired); ask an admin to remove and re-add you"
            ),
        )),
    }
}

/// How to read `rid`: `None` for a public (or unknown) repo, a keyring for a private repo
/// this peer can read (as a member, or through published epoch grants), else `E_NOT_MEMBER`.
///
/// A verified public root is remembered on disk: `rid` hashes the root body, which records
/// the visibility, so a public repo can never turn private and later calls send no query.
pub async fn ring_for<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: Option<&Keys>,
    rid: &Rid,
) -> Result<Option<Keyring>> {
    if public_marker(&cfg.home, rid).exists() {
        return Ok(None);
    }
    let member = keys.filter(|_| matches!(Keyring::load(&cfg.home, rid), Ok(Some(_))));
    if let Some(keys) = member
        && let (_, Some(ring)) = refresh(a, cfg, keys, rid).await?
    {
        return Ok(Some(ring));
    }
    // The view's identity query: on a public repo the view reuses these rows.
    let ids = observe::all(a, view::identity_filter(rid)).await?;
    if ids.iter().any(|o| is_public_root(o, rid)) {
        remember_public(&cfg.home, rid)?;
        view::prefetched_identity(rid, ids);
        return Ok(None);
    }
    if let Some(keys) = keys.filter(|_| member.is_none())
        && let (_, Some(ring)) = refresh(a, cfg, keys, rid).await?
    {
        return Ok(Some(ring));
    }
    // A public identity wins, so a forged marker cannot close a public repo.
    if ids
        .iter()
        .any(|o| o.attr("ag.kind") == Some(&AttrValue::Str("id".into())))
    {
        view::prefetched_identity(rid, ids);
        return Ok(None);
    }
    // The marker and the grants in one query.
    let types = Pred::Or(
        [T_GROUP, T_PUB]
            .map(|t| Pred::eq(charter_store::attr("ag.t"), t))
            .to_vec(),
    );
    let marked = find_any(a, types, rid).await?;
    let is = |o: &Observed, t: &str| o.attr("ag.t") == Some(&AttrValue::Str(t.into()));
    if !marked.iter().any(|o| is(o, T_GROUP)) {
        return Ok(None);
    }
    let grants: Vec<Observed> = marked.into_iter().filter(|o| is(o, T_PUB)).collect();
    published(a, rid, &grants)
        .await?
        .map(Some)
        .ok_or_else(|| not_member(rid))
}

async fn find_any<A: Arkiv>(a: &A, types: Pred, rid: &Rid) -> Result<Vec<Observed>> {
    let f = Pred::And(vec![
        observe::kind(charter_core::Kind::Mls),
        types,
        observe::b32("ag.tag", &tag(rid)),
    ]);
    observe::all(a, f).await
}

/// The root identity of `rid` itself, saying public: its content hash is `rid`, so no other
/// row can pass (a forged `kind = id` row with this `ag.rid` does not).
fn is_public_root(o: &Observed, rid: &Rid) -> bool {
    charter_core::open::<IdBody>(o)
        .is_ok_and(|e| e.h == *rid && e.body.is_root() && e.body.visibility == 0)
}

fn public_marker(home: &std::path::Path, rid: &Rid) -> std::path::PathBuf {
    home.join("private")
        .join(format!("{}.public", hex::encode(rid)))
}

fn remember_public(home: &std::path::Path, rid: &Rid) -> Result<()> {
    let p = public_marker(home, rid);
    let dir = p.parent().unwrap_or(home);
    std::fs::create_dir_all(dir).map_err(|e| SyncError::io(dir.display(), e))?;
    std::fs::write(&p, b"").map_err(|e| SyncError::io(p.display(), e))
}

/// Most distinct id keys tried for the root in one query (junk grants cannot widen it).
const MAX_ID_CANDIDATES: usize = 16;

/// A read-only keyring from published grants that authenticate: the grant whose keys open
/// the root identity (content hash = rid), then grants signed by a root delegate. Costs the
/// identity query the view then skips, and one commit query that places epoch ends.
async fn published<A: Arkiv>(a: &A, rid: &Rid, grants: &[Observed]) -> Result<Option<Keyring>> {
    let cands: Vec<(u64, auth::SignedGrant)> = grants
        .iter()
        .filter_map(|o| {
            Some((
                u64_of(o, "ag.ep")?,
                auth::decode_grant(bytes(o, "ag.body")?)?,
            ))
        })
        .collect();
    let mut ids: Vec<Key> = vec![];
    for (_, g) in &cands {
        if !ids.contains(&g.id_key) && ids.len() < MAX_ID_CANDIDATES {
            ids.push(g.id_key);
        }
    }
    if ids.is_empty() {
        return Ok(None);
    }
    let blinded = ids
        .iter()
        .map(|k| observe::b32("ag.rid", &blind(k, "rid", rid)))
        .collect();
    let kinds = Pred::Or(
        [charter_core::Kind::Id, charter_core::Kind::IdAcc]
            .map(observe::kind)
            .to_vec(),
    );
    let q = Query::from(Pred::And(vec![Pred::Or(blinded), kinds]));
    let raw = charter_store::paging::offset_all(a, &q, observe::full_select()).await?;
    let Some((mut ring, roots)) = root_grant(rid, &cands, &raw) else {
        return Ok(None);
    };
    for (ep, g) in &cands {
        if ring.epochs.contains_key(ep) || !auth::grant_signed(g, rid, *ep, &roots) {
            continue;
        }
        ring.epochs.insert(*ep, g.key);
        if g.id_key != ring.id_key {
            ring.id_rot.insert(*ep, g.id_key);
        }
        let roles = g.roles.iter().map(|(n, r)| (hex::encode(n), *r)).collect();
        ring.record_roles(*ep, &roles);
    }
    let commits = find(a, T_COMMIT, vec![rid_pred(&ring)]).await?;
    derive_history(&mut ring, &commits);
    let rows = crate::arkiv::open_all(&ring, raw);
    view::prefetched_identity(rid, observe::to_observed(rows));
    Ok(Some(ring))
}

/// The keyring of the grant whose keys open the root identity (one a root delegate also
/// signed, if any), and the root's delegates.
fn root_grant(
    rid: &Rid,
    cands: &[(u64, auth::SignedGrant)],
    raw: &[charter_store::EntityData],
) -> Option<(Keyring, Vec<Nid>)> {
    let mut opening: Vec<(bool, Keyring, Vec<Nid>)> = cands
        .iter()
        .filter_map(|(ep, g)| {
            let (ring, roots) = root_ring(rid, *ep, g, raw)?;
            Some((auth::grant_signed(g, rid, *ep, &roots), ring, roots))
        })
        .collect();
    let best = opening
        .iter()
        .position(|(signed, _, _)| *signed)
        .unwrap_or(0);
    (!opening.is_empty()).then(|| {
        let (_, ring, roots) = opening.swap_remove(best);
        (ring, roots)
    })
}

fn root_ring(
    rid: &Rid,
    ep: u64,
    g: &auth::SignedGrant,
    raw: &[charter_store::EntityData],
) -> Option<(Keyring, Vec<Nid>)> {
    let mut ring = Keyring::new(*rid, g.id_key, vec![]);
    ring.epochs.insert(ep, g.key);
    let opened = raw
        .iter()
        .map(|r| entity::open_row_unchecked(&ring, r.clone()))
        .collect();
    let roots = root_of(rid, &observe::to_observed(opened))?;
    let mut roles: std::collections::BTreeMap<String, Role> = roots
        .iter()
        .map(|n| (hex::encode(n), Role::Admin))
        .collect();
    roles.extend(g.roles.iter().map(|(n, r)| (hex::encode(n), *r)));
    ring.record_roles(ep, &roles);
    Some((ring, roots))
}
