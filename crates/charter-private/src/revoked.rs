//! Removing a revoked device's MLS leaf (docs/p5/head-model.md 15.7). Each leaf names its
//! device (`nid || device` credential, bound by the member's signature at add), so an
//! identity's revoke record maps to exactly one leaf. An admin's refresh reads the repo's
//! revoke records in its commit query and commits the removal of every leaf a valid record
//! names, which rotates the epoch key and the id-key generation like a member removal.

use std::collections::BTreeMap;

use charter_core::{DEVICE_SEQ, Kind, Nid, Observed, RevokeBody, is_revoke_row};
use charter_store::{Arkiv, EntityData, Pred};
use charter_sync::{Config, Keys, Result, observe, warn};
use openmls::prelude::LeafNodeIndex;

use crate::entity::open_row_unchecked;
use crate::group::{finish, prepare};
use crate::join::{apply_commits, first_signed, nids, records};
use crate::keyring::{Keyring, Role};
use crate::mls::Mls;
use crate::msg::{T_COMMIT, bytes, find, rid_pred};

/// The repo's device records (delegations and revokes, `ag.seq = DEVICE_SEQ`), under any
/// id-key generation `ring` holds.
pub(crate) fn device_records(ring: &Keyring) -> Pred {
    Pred::And(vec![
        observe::kind(Kind::Anchor),
        Pred::eq(charter_store::attr("ag.seq"), DEVICE_SEQ),
        rid_pred(ring),
    ])
}

/// Whether a row of the commit query is a device record rather than a commit.
pub(crate) fn is_device_row(r: &EntityData) -> bool {
    r.attributes
        .iter()
        .flatten()
        .any(|a| a.name == "ag.kind" && a.value == "anchor")
}

fn nid_attr(o: &Observed, name: &str) -> Option<Nid> {
    match o.attr(name) {
        Some(charter_core::AttrValue::Bytes32(h)) => Some(*h),
        _ => None,
    }
}

/// Leaves of `g` whose device a valid revoke record among `rows` names: signed by the
/// leaf's member for this repo. Empty while this member's own device is revoked (it must
/// not act). Costs one key lookup per revoking identity with a leaf still in the group.
pub(crate) async fn pending<A: Arkiv>(
    a: &A,
    ring: &Keyring,
    g: &openmls::prelude::MlsGroup,
    rows: &[EntityData],
) -> Result<Vec<LeafNodeIndex>> {
    let leaves = Mls::device_leaves(g);
    let mut pks = BTreeMap::new();
    let mut out = vec![];
    for row in rows {
        let Some(o) = observe::to_observed(vec![row.clone()]).pop() else {
            continue;
        };
        let (Some(nid), Some(dev)) = (nid_attr(&o, "ag.nid"), nid_attr(&o, "ag.dev")) else {
            continue;
        };
        let Some(leaf) = leaves.iter().find(|(_, n, d)| *n == nid && *d == Some(dev)) else {
            continue;
        };
        if !is_revoke_row(&o) || out.contains(&leaf.0) {
            continue;
        }
        if let std::collections::btree_map::Entry::Vacant(e) = pks.entry(nid) {
            e.insert(charter_sync::cob::peer_key_of(a, &nid).await?);
        }
        let Some(pk) = &pks[&nid] else { continue };
        let Some(opened) = observe::to_observed(vec![open_row_unchecked(ring, row.clone())]).pop()
        else {
            continue;
        };
        let valid = charter_core::verify_entity::<RevokeBody>(&opened, pk).is_ok_and(|e| {
            e.envelope.signer == nid && e.body.rid == ring.rid && e.body.device == dev
        });
        if !valid {
            continue;
        }
        if leaf.0 == g.own_leaf_index() {
            return Ok(vec![]);
        }
        out.push(leaf.0);
    }
    Ok(out)
}

/// Admin only: commits the removal of every leaf whose device was revoked ([`pending`]),
/// and returns the keyring after it. Costs nothing unless a revoke names a leaf still in
/// the group; then the add and remove path's queries, one commit tx, and one commit query
/// to see whether another admin's removal landed first. If it did, this one is undone and
/// that one applied (first commit wins). A failure is reported and undone, never fatal to
/// the read that triggered it.
pub(crate) async fn remove<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    mls: &Mls,
    ring: &Keyring,
    rows: &[EntityData],
) -> Result<Option<Keyring>> {
    if rows.is_empty() || ring.role(&keys.nid()) != Some(Role::Admin) {
        return Ok(None);
    }
    let mut g = mls.load(&ring.group)?;
    let leaves = pending(a, ring, &g, rows).await?;
    if leaves.is_empty() {
        return Ok(None);
    }
    let snap = mls.snapshot()?;
    let sent = async {
        let pre = prepare(a, cfg, keys, ring).await?;
        let commit = mls.remove_leaves(&mut g, &leaves)?;
        let mut next = ring.clone();
        let (epoch, key) = mls.epoch_key(&g, &ring.rid)?;
        next.epochs.insert(epoch, key);
        next.id_rot.insert(epoch, mls.id_key(&g, &ring.rid)?);
        let next = finish(a, cfg, keys, mls, next, pre, commit.clone(), None).await?;
        Ok::<_, charter_sync::SyncError>((next, epoch, commit))
    };
    let (next, epoch, commit) = match sent.await {
        Ok(s) => s,
        Err(e) => {
            mls.restore(snap)?;
            warn(&format!("removing a revoked device's MLS leaf failed: {e}"));
            return Ok(None);
        }
    };
    if first(a, ring, epoch, &commit).await? {
        return Ok(Some(next));
    }
    // Another admin's commit for this epoch landed first: take it instead of ours.
    mls.restore(snap)?;
    let mut theirs = ring.clone();
    apply_commits(a, mls, &mut theirs).await?;
    Ok(Some(theirs))
}

/// Whether `ours` is the first commit for `epoch` that an admin of the epoch before signed.
async fn first<A: Arkiv>(a: &A, ring: &Keyring, epoch: u64, ours: &[u8]) -> Result<bool> {
    let rows = find(a, T_COMMIT, vec![rid_pred(ring)]).await?;
    let by_ep = records(&rows, &ring.id_key, &ring.rid);
    let Some(cands) = by_ep.get(&epoch) else {
        return Ok(true);
    };
    let admins = nids(ring.adm_log.get(&(epoch - 1)));
    let won = first_signed(a, &mut BTreeMap::new(), &ring.rid, epoch, cands, &admins).await?;
    Ok(won.is_none_or(|(c, _)| bytes(c, "ag.body") == Some(ours)))
}

/// What a device revoke did to the private repo's group, as this machine sees it after a
/// refresh (which removes the leaf when this identity is an admin).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafState {
    /// No leaf names the device any more; it cannot open what is sealed from `epoch` on.
    Removed { epoch: u64 },
    /// Its leaf is still in the group until an admin's client next syncs the repo.
    Pending,
    /// No leaf names the device, but the identity has leaves made before leaves named
    /// their device: it may be one of them, so only removing the member closes it.
    Legacy,
}

/// The group state of `device` (of this identity) after a refresh of `rid`.
pub async fn leaf_state<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &charter_core::Rid,
    device: &Nid,
) -> Result<LeafState> {
    let (mls, ring) = crate::group::member_ring(a, cfg, keys, rid).await?;
    let leaves = Mls::device_leaves(&mls.load(&ring.group)?);
    let mine = |d: &Option<Nid>| leaves.iter().any(|(_, n, x)| *n == keys.nid() && x == d);
    Ok(match (mine(&Some(*device)), mine(&None)) {
        (true, _) => LeafState::Pending,
        (false, true) => LeafState::Legacy,
        (false, false) => LeafState::Removed {
            epoch: ring.current()?.0,
        },
    })
}
