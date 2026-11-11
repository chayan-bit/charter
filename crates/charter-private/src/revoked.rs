//! Removing a revoked device's MLS leaf (docs/p5/head-model.md 15.7). Each leaf names its
//! device (`nid || device` credential, bound by the member's signature at add), so an
//! identity's revoke record maps to exactly one leaf. A refresh reads the repo's revoke
//! records in its commit query for free but never sends; an admin's explicit write
//! ([`remove_revoked`]) commits the removal of every leaf a valid record names, which
//! rotates the epoch key and the id-key generation like a member removal.

use charter_core::{DEVICE_SEQ, Kind, Nid, Observed, is_revoke_row, revocations};
use charter_store::{Arkiv, EntityData, Operation, Pred};
use charter_sync::{Config, Keys, Result, observe};
use openmls::prelude::LeafNodeIndex;

use crate::Sealed;
use crate::entity::open_row_unchecked;
use crate::group::{Pre, commit_ops, finish, next_keys, prepare, ready};
use crate::join::refresh_in;
use crate::keyring::{Keyring, Role, not_member};
use crate::lock::lock;
use crate::mls::Mls;
use crate::msg::rid_pred;

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

/// Leaves of `g` whose member and device a revoke-layout row among `rows` names, by their
/// plaintext attributes alone (anyone can write such a row): `(leaf, nid, device)`. Costs
/// nothing; [`pending`] checks the records.
pub(crate) fn candidates(
    g: &openmls::prelude::MlsGroup,
    rows: &[EntityData],
) -> Vec<(LeafNodeIndex, Nid, Nid)> {
    let leaves = Mls::device_leaves(g);
    let mut out: Vec<(LeafNodeIndex, Nid, Nid)> = vec![];
    for o in observe::to_observed(rows.to_vec()) {
        let (Some(nid), Some(dev)) = (nid_attr(&o, "ag.nid"), nid_attr(&o, "ag.dev")) else {
            continue;
        };
        let Some((leaf, ..)) = leaves.iter().find(|(_, n, d)| *n == nid && *d == Some(dev)) else {
            continue;
        };
        if is_revoke_row(&o) && !out.iter().any(|c| c.0 == *leaf) {
            out.push((*leaf, nid, dev));
        }
    }
    out
}

/// The [`candidates`] whose device a valid revoke record among `rows` names, by core's own
/// rule ([`charter_core::revocations`]: signed by the leaf's member for this repo and
/// created by one of its bound control addresses). Empty while this member's own device is
/// revoked (it must not act). Costs one peer lookup (key and binds, sent together) per
/// revoking identity with a leaf still in the group.
pub(crate) async fn pending<A: Arkiv>(
    a: &A,
    ring: &Keyring,
    g: &openmls::prelude::MlsGroup,
    rows: &[EntityData],
) -> Result<Vec<LeafNodeIndex>> {
    let cands = candidates(g, rows);
    let sealed = Sealed::new(a, ring.clone());
    let mut out = vec![];
    let nids: std::collections::BTreeSet<Nid> = cands.iter().map(|c| c.1).collect();
    for nid in nids {
        let Some(peer) = charter_sync::view::resolve_peer(&sealed, &ring.rid, &nid).await? else {
            continue;
        };
        let opened: Vec<EntityData> = rows
            .iter()
            .map(|r| open_row_unchecked(ring, r.clone()))
            .collect();
        let revoked = revocations(&ring.rid, &peer, &observe::to_observed(opened));
        for (leaf, _, dev) in cands.iter().filter(|c| c.1 == nid) {
            if !revoked.contains_key(dev) {
                continue;
            }
            if *leaf == g.own_leaf_index() {
                return Ok(vec![]);
            }
            out.push(*leaf);
        }
    }
    Ok(out)
}

/// Admin only: commits the removal of every leaf whose device was revoked ([`pending`]);
/// returns how many. The commit stays pending until a refresh sees it first on chain
/// ([`crate::group::finish`]); if another admin's removal lands first, that one counts.
/// Costs nothing unless a revoke names a leaf still in the group; then the peer lookups,
/// the add and remove path's queries and one commit tx.
pub(crate) async fn remove<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    mls: &mut Mls,
    ring: &Keyring,
    rows: &[EntityData],
) -> Result<usize> {
    let Some((next, pre, commit, n)) = removal(a, cfg, keys, mls, ring, rows).await? else {
        return Ok(0);
    };
    finish(a, cfg, keys, mls, next, pre, commit, None).await?;
    Ok(n)
}

/// The removal commit [`remove`] would send, with the keyring after it; `None` when there is
/// nothing to remove or this identity is not an admin. Leaves the commit pending in `mls`
/// (in memory only: [`finish`] saves it).
async fn removal<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    mls: &Mls,
    ring: &Keyring,
    rows: &[EntityData],
) -> Result<Option<(Keyring, Pre, Vec<u8>, usize)>> {
    if rows.is_empty() || ring.role(&keys.nid()) != Some(Role::Admin) {
        return Ok(None);
    }
    let mut g = ready(a, mls, ring).await?;
    let leaves = pending(a, ring, &g, rows).await?;
    if leaves.is_empty() {
        return Ok(None);
    }
    let pre = prepare(a, cfg, keys, ring).await?;
    let commit = mls.remove_leaves(&mut g, &leaves)?;
    let mut next = ring.clone();
    let (epoch, key, id, _) = next_keys(mls, &g, &ring.rid)?;
    next.epochs.insert(epoch, key);
    next.id_rot.insert(epoch, id);
    Ok(Some((next, pre, commit, leaves.len())))
}

/// `chart member sync`, `chart device revoke` and the UI's priced write: an admin removes
/// the MLS leaf of every device a valid revoke record of this repo names, in one commit
/// (one tx from its control address, a new epoch key and id-key generation). Returns how
/// many leaves it removed; 0 sends nothing. Refreshes first, under the home lock.
pub async fn remove_revoked<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &charter_core::Rid,
) -> Result<usize> {
    let _held = lock(&cfg.home).await?;
    let (mut mls, ring, rows) = refresh_in(a, cfg, keys, rid).await?;
    let ring = ring.ok_or_else(|| not_member(rid))?;
    // Boxed: the commit future is large, and this sits under the CLI's and UI's own futures.
    Box::pin(remove(a, cfg, keys, &mut mls, &ring, &rows)).await
}

/// The tx [`remove_revoked`] would send now, for pricing it; `None` when it would send
/// nothing. Sends nothing and saves no commit (the one it builds is dropped).
pub async fn removal_ops<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &charter_core::Rid,
) -> Result<Option<Vec<Operation>>> {
    let _held = lock(&cfg.home).await?;
    let (mls, ring, rows) = refresh_in(a, cfg, keys, rid).await?;
    let ring = ring.ok_or_else(|| not_member(rid))?;
    let Some((next, pre, commit, _)) = Box::pin(removal(a, cfg, keys, &mls, &ring, &rows)).await?
    else {
        return Ok(None);
    };
    Ok(Some(commit_ops(cfg, keys, next, pre, commit, None)?.1))
}

/// What a device revoke did to the private repo's group, as this machine sees it after a
/// refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeafState {
    /// No leaf names the device any more; it cannot open what is sealed from `epoch` on.
    Removed { epoch: u64 },
    /// Its leaf is still in the group until an admin removes it (`chart member sync`).
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
