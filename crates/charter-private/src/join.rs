//! Reading a private repo's group state from Arkiv: join from a Welcome addressed to one
//! of this member's key packages, then apply admin commits in Arkiv order, one epoch at a
//! time. Also decides, for any rid, whether it is public, private and readable, or
//! private and closed (`E_NOT_MEMBER`).

use std::collections::BTreeMap;

use charter_core::{AttrValue, IdBody, Observed, Rid};
use charter_store::Arkiv;
use charter_sync::{Config, Keys, Result, SyncError, observe, view};

use crate::keyring::{Keyring, Role, not_member};
use crate::mls::Mls;
use crate::msg::{
    T_COMMIT, T_GROUP, T_PUB, T_WELCOME, blinded_rid, bytes, find, open_box, tag, u64_of,
};

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
        r.save(&cfg.home)?;
        mls.save()?;
    }
    Ok((mls, ring))
}

/// Joins `rid`'s group from the first Welcome for one of our key packages that opens.
async fn join<A: Arkiv>(a: &A, mls: &Mls, rid: &Rid) -> Result<Option<Keyring>> {
    for kp in &mls.kps {
        for w in find(a, T_WELCOME, vec![observe::b32("ag.kp", kp)]).await? {
            let (Some(body), Some(bx)) = (bytes(&w, "ag.body"), bytes(&w, "ag.box")) else {
                continue;
            };
            let Ok(g) = mls.join(body) else { continue };
            let (epoch, key) = mls.epoch_key(&g, rid)?;
            if let Some(mut ring) = open_box(rid, epoch, &key, bx) {
                ring.epochs.insert(epoch, key);
                ring.group = g.group_id().as_slice().to_vec();
                return Ok(Some(ring));
            }
        }
    }
    Ok(None)
}

/// Applies admin commits for the next epochs; a commit whose sender is no admin is skipped.
/// Stops once this member is removed: it then holds no key for later epochs.
async fn apply_commits<A: Arkiv>(a: &A, mls: &Mls, ring: &mut Keyring) -> Result<()> {
    let mut g = mls.load(&ring.group)?;
    let filter = vec![observe::b32("ag.rid", &blinded_rid(ring))];
    for c in find(a, T_COMMIT, filter).await? {
        if !g.is_active() {
            break;
        }
        let next = g.epoch().as_u64() + 1;
        let (Some(body), Some(bx)) = (bytes(&c, "ag.body"), bytes(&c, "ag.box")) else {
            continue;
        };
        if u64_of(&c, "ag.ep") != Some(next) {
            continue;
        }
        let admins = ring.clone();
        let is_admin = |n: &_| admins.role(n) == Some(Role::Admin);
        if !matches!(mls.process(&mut g, body, is_admin), Ok(true)) || !g.is_active() {
            continue;
        }
        let (epoch, key) = mls.epoch_key(&g, &ring.rid)?;
        ring.epochs.insert(epoch, key);
        if let Some(newer) = open_box(&ring.rid, epoch, &key, bx) {
            ring.merge(newer);
        }
    }
    Ok(())
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
    if ids.iter().any(|o| o.attr("ag.kind") == Some(&AttrValue::Str("id".into()))) {
        view::prefetched_identity(rid, ids);
        return Ok(None);
    }
    let by_tag = || vec![observe::b32("ag.tag", &tag(rid))];
    if find(a, T_GROUP, by_tag()).await?.is_empty() {
        return Ok(None);
    }
    published(rid, &find(a, T_PUB, by_tag()).await?)
        .map(Some)
        .ok_or_else(|| not_member(rid))
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

/// A read-only keyring from published `id_key || content key` grants, if any.
fn published(rid: &Rid, grants: &[charter_core::Observed]) -> Option<Keyring> {
    let mut epochs = BTreeMap::new();
    let mut id_key = None;
    for g in grants {
        let (Some(ep), Some(b)) = (u64_of(g, "ag.ep"), bytes(g, "ag.body")) else {
            continue;
        };
        let (Ok(id), Ok(k)) = (
            b[..b.len().min(32)].try_into(),
            b[b.len().min(32)..].try_into(),
        ) else {
            continue;
        };
        id_key.get_or_insert(id);
        epochs.insert(ep, k);
    }
    Some(Keyring {
        rid: *rid,
        id_key: id_key?,
        epochs,
        roles: BTreeMap::new(),
        group: vec![],
    })
}
