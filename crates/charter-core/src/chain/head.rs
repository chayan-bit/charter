//! Format v2 verification (docs/p5/head-model.md sections 4 to 7): device delegations,
//! device heads with the rollback check, and the merge of device heads into an identity's
//! refs.

use std::collections::{BTreeMap, BTreeSet};

use super::{IdentityChain, PeerInfo, fail_unsupported, open_all};
use crate::attrs::AttrValue;
use crate::marks::PeerMark;
use crate::types::{Address, Hash32, Nid, Observed, Position, Rid};
use crate::{
    DEVICE_LAYOUT_VERSION, DeviceBody, Entity, HeadBody, HeadBodyV3, HeadBodyV4, HeadRef, Pack,
    Plane, RefEntry, RefTable, RefsBody, VerifyError,
};
use charter_crypto::PublicKey;

/// A verified device delegation of one identity.
#[derive(Debug, Clone)]
pub struct Device {
    /// The device nid (signer of its head).
    pub nid: Nid,
    pub pk: PublicKey,
    pub control: Address,
    pub bulk: Address,
    pub h: Hash32,
    pub position: Position,
}

/// Whether an `anchor`-kind row is a v2 device delegation (`ag.v = 2`).
pub fn is_device_row(o: &Observed) -> bool {
    o.attr("ag.v") == Some(&AttrValue::U64(DEVICE_LAYOUT_VERSION))
}

/// Valid delegations of `peer` for `rid` among `observed`, first-created per device.
pub fn devices(rid: &Rid, peer: &PeerInfo, observed: &[Observed]) -> Vec<Device> {
    let mut seen = BTreeSet::new();
    open_all::<DeviceBody>(observed)
        .ok
        .into_iter()
        .filter(|e| {
            e.envelope.signer == peer.binds.nid
                && e.body.rid == *rid
                && peer
                    .binds
                    .creator_ok(Plane::Control, &e.creator, e.position.block)
                && e.envelope.verify(&peer.pk).is_ok()
        })
        .filter_map(|e| {
            let pk = e.body.public_key().ok()?;
            seen.insert(pk.nid()).then(|| Device {
                nid: pk.nid(),
                pk,
                control: e.body.control,
                bulk: e.body.bulk,
                h: e.h,
                position: e.position,
            })
        })
        .collect()
}

/// `entityNonce(C_d)` read before and after the heads query (head-model section 6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadNonce {
    pub address: Address,
    pub before: u64,
    pub after: u64,
}

/// The wire format of a device head body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadFormat {
    /// `ag.v = 1`, envelope kind `head` (head-model 4.2).
    V2,
    /// `ag.v = 3`, envelope kind `head3` (head-model 13).
    V3,
    /// `ag.v = 4`, envelope kind `head4`: a v3 head with a ref table (head-model 14).
    V4,
}

/// An accepted device head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceHead {
    pub device: Nid,
    pub control: Address,
    pub key: Hash32,
    /// The fields both formats share; `body.packs` is empty for a v3 head, read [`Self::packs`].
    /// For a v4 head `body.refs` is field 7 only until the reader overlays the table
    /// (`effective_refs`, head-model 14.4); charter-sync does that before any merge.
    pub body: HeadBody,
    /// The ref table of a v4 head.
    pub refs_table: Option<RefTable>,
    pub format: HeadFormat,
    /// The packs since the snapshot, in either format.
    pub packs: Vec<Pack>,
    pub h: Hash32,
    pub envelope_bytes: Vec<u8>,
    /// `$createdAt` of the head entity.
    pub created_at: u64,
    /// `$updatedAt`: the last patch or extend, an upper bound for the body's landing block.
    pub updated_at: u64,
}

impl DeviceHead {
    pub fn mark(&self) -> PeerMark {
        PeerMark {
            seq: self.body.seq,
            hash: self.h,
            block: self.updated_at,
        }
    }
}

/// A head row opened in either format: the shared fields as a v2 body, and its packs.
pub(super) struct Opened {
    pub(super) e: Entity<HeadBody>,
    format: HeadFormat,
    packs: Vec<Pack>,
    refs_table: Option<RefTable>,
}

impl Opened {
    pub(super) fn v2(e: Entity<HeadBody>) -> Self {
        let packs = e.body.packs.iter().map(Pack::from_v2).collect();
        Self {
            e,
            format: HeadFormat::V2,
            packs,
            refs_table: None,
        }
    }

    pub(super) fn v4(e: Entity<HeadBodyV4>) -> Option<Self> {
        let Entity {
            position,
            creator,
            expires_at,
            envelope,
            body,
            h,
            envelope_bytes,
        } = e;
        let v3 = Entity {
            position,
            creator,
            expires_at,
            envelope,
            body: body.0,
            h,
            envelope_bytes,
        };
        let refs_table = v3.body.refs_table.clone();
        Self::v3(v3).map(|o| Self {
            format: HeadFormat::V4,
            refs_table,
            ..o
        })
    }

    /// `None` only for a codec a validated body cannot carry.
    pub(super) fn v3(e: Entity<HeadBodyV3>) -> Option<Self> {
        let packs = e
            .body
            .packs
            .iter()
            .map(Pack::from_v3)
            .collect::<Result<_, _>>()
            .ok()?;
        let body = e.body.shared();
        Some(Self {
            e: Entity {
                position: e.position,
                creator: e.creator,
                expires_at: e.expires_at,
                envelope: e.envelope,
                body,
                h: e.h,
                envelope_bytes: e.envelope_bytes,
            },
            format: HeadFormat::V3,
            packs,
            refs_table: None,
        })
    }
}

pub(super) fn to_head(o: Opened, dev: &Device, updated_at: u64) -> DeviceHead {
    let e = o.e;
    DeviceHead {
        device: dev.nid,
        control: dev.control,
        key: e.position.key,
        h: e.h,
        created_at: e.position.block,
        updated_at,
        envelope_bytes: e.envelope_bytes,
        body: e.body,
        format: o.format,
        packs: o.packs,
        refs_table: o.refs_table,
    }
}

/// The rollback check: `C_d` created exactly the head plus `pieces_created` pieces.
pub fn check_head_nonce(
    dev: &Device,
    head: Option<&HeadBody>,
    obs: &HeadNonce,
) -> Result<(), VerifyError> {
    if obs.before != obs.after || obs.address != dev.control {
        return Err(VerifyError::InconsistentView);
    }
    let recorded = head
        .map(|h| h.pieces_created.saturating_add(1))
        .unwrap_or(0);
    if obs.before != recorded {
        return Err(VerifyError::HeadRollback {
            device: dev.nid,
            address: dev.control,
            nonce: obs.before,
            recorded,
        });
    }
    Ok(())
}

/// The one validly signed head of `dev` among `observed`, in either format, without the
/// nonce check.
fn valid_head(
    rid: &Rid,
    nid: &Nid,
    dev: &Device,
    identity: &IdentityChain,
    observed: &[Observed],
) -> Result<Option<Opened>, VerifyError> {
    let v2 = open_all::<HeadBody>(observed);
    let v3 = open_all::<HeadBodyV3>(observed);
    let v4 = open_all::<HeadBodyV4>(observed);
    // A head a newer client wrote (no layout version this client reads): say so instead of
    // reporting a missing head.
    let unknown: Vec<(Address, Hash32)> = v2
        .unsupported
        .iter()
        .filter(|u| v3.unsupported.contains(u) && v4.unsupported.contains(u))
        .copied()
        .collect();
    fail_unsupported(&unknown, &BTreeSet::from([dev.control]))?;
    let opened = v2
        .ok
        .into_iter()
        .map(Opened::v2)
        .chain(v3.ok.into_iter().filter_map(Opened::v3))
        .chain(v4.ok.into_iter().filter_map(Opened::v4));
    let mut valid: Vec<Opened> = opened
        .filter(|Opened { e, .. }| {
            e.envelope.signer == dev.nid
                && e.creator == dev.control
                && e.body.rid == *rid
                && e.body.nid == *nid
                && identity.contains(&e.body.id_head)
                && e.envelope.verify(&dev.pk).is_ok()
        })
        .collect();
    if valid.len() > 1 {
        return Err(VerifyError::DuplicateHead {
            device: dev.nid,
            count: valid.len(),
        });
    }
    Ok(valid.pop())
}

/// The local high-water mark (head-model 5.2 rule 5), then the accepted head.
fn after_mark(
    o: Opened,
    dev: &Device,
    observed: &[Observed],
    mark: Option<&PeerMark>,
) -> Result<DeviceHead, VerifyError> {
    let e = &o.e;
    if let Some(m) = mark {
        if e.body.seq < m.seq {
            return Err(VerifyError::StaleHead {
                seq: Some(e.body.seq),
                mark: m.seq,
            });
        }
        if e.body.seq == m.seq && e.h != m.hash {
            return Err(VerifyError::HistoryRewritten { seq: m.seq });
        }
    }
    let up = observed
        .iter()
        .find(|o| o.key == e.position.key)
        .map_or(e.position.block, |r| r.updated_at);
    Ok(to_head(o, dev, up))
}

/// Accepts the head of `dev` (head-model 5.2) from the head rows of identity `nid`.
pub fn accept_device_head(
    rid: &Rid,
    nid: &Nid,
    dev: &Device,
    identity: &IdentityChain,
    observed: &[Observed],
    nonce: &HeadNonce,
    mark: Option<&PeerMark>,
) -> Result<Option<DeviceHead>, VerifyError> {
    let head = valid_head(rid, nid, dev, identity, observed)?;
    check_head_nonce(dev, head.as_ref().map(|o| &o.e.body), nonce)?;
    head.map(|e| after_mark(e, dev, observed, mark)).transpose()
}

/// [`accept_device_head`] on the machine that sent body `sent` (head-model 7.4): when the
/// current head is exactly that body and `C_d` created more than it records (a copied
/// device seed landed a second head tx), the head is accepted so the next push can sign a
/// body that accounts for the real nonce. A rollback still fails: an older body is not the
/// one this machine sent last, and the local mark rejects it.
#[allow(clippy::too_many_arguments)]
pub fn accept_own_device_head(
    rid: &Rid,
    nid: &Nid,
    dev: &Device,
    identity: &IdentityChain,
    observed: &[Observed],
    nonce: &HeadNonce,
    mark: Option<&PeerMark>,
    sent: &Hash32,
) -> Result<Option<DeviceHead>, VerifyError> {
    let err = match accept_device_head(rid, nid, dev, identity, observed, nonce, mark) {
        Err(
            e @ VerifyError::HeadRollback {
                nonce, recorded, ..
            },
        ) if nonce > recorded => e,
        other => return other,
    };
    match valid_head(rid, nid, dev, identity, observed)? {
        Some(o) if o.e.h == *sent && nonce.before == nonce.after => {
            after_mark(o, dev, observed, mark).map(Some)
        }
        _ => Err(err),
    }
}

/// The entry that holds a name, and the device whose head carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Winner {
    pub device: Nid,
    /// `stamp` resolved to a block when it took part in a tie, else as written.
    pub entry: HeadRef,
}

/// Every name of an identity and its winning entry (tombstones included).
pub type Merged = BTreeMap<String, Winner>;

fn stamp(device: &Nid, e: &HeadRef, landing: &BTreeMap<Nid, u64>) -> Option<u64> {
    match e.stamp {
        0 => landing.get(device).copied(),
        s => Some(s),
    }
}

/// Merges device heads (head-model 5.3): the larger counter wins; a tie between
/// different values goes to the earlier landing block, then the smaller device nid.
/// `landing` maps a device to the block its current body landed in; when a tie needs one
/// that is missing, the devices to resolve come back as the error.
pub fn merge(
    heads: &[(Nid, &HeadBody)],
    landing: &BTreeMap<Nid, u64>,
) -> Result<Merged, BTreeSet<Nid>> {
    let mut by_name: BTreeMap<&str, Vec<(Nid, &HeadRef)>> = BTreeMap::new();
    for (dev, body) in heads {
        for e in &body.refs {
            by_name.entry(e.name.as_str()).or_default().push((*dev, e));
        }
    }
    let (mut out, mut need) = (Merged::new(), BTreeSet::new());
    for (name, mut cands) in by_name {
        let top = cands.iter().map(|(_, e)| e.counter).max().unwrap_or(0);
        cands.retain(|(_, e)| e.counter == top);
        cands.sort_by_key(|(d, _)| *d);
        let differ = cands.iter().any(|(_, e)| e.oid != cands[0].1.oid);
        if !differ {
            let (device, e) = cands[0];
            out.insert(
                name.to_owned(),
                Winner {
                    device,
                    entry: e.clone(),
                },
            );
            continue;
        }
        let mut ranked = vec![];
        for (d, e) in &cands {
            match stamp(d, e, landing) {
                Some(s) => ranked.push((s, *d, *e)),
                None => _ = need.insert(*d),
            }
        }
        if let Some((s, device, e)) = ranked.into_iter().min_by_key(|(s, d, _)| (*s, *d)) {
            let entry = HeadRef {
                stamp: s,
                ..e.clone()
            };
            out.insert(name.to_owned(), Winner { device, entry });
        }
    }
    match need.is_empty() {
        true => Ok(out),
        false => Err(need),
    }
}

/// The live refs of a merge as a refs body (SHA-1 repos, as P1 writes).
pub fn merged_refs(rid: &Rid, merged: &Merged) -> RefsBody {
    RefsBody {
        rid: *rid,
        object_hash: 1,
        refs: merged
            .iter()
            .filter(|(_, w)| !w.entry.is_tombstone())
            .map(|(name, w)| RefEntry {
                name: name.clone(),
                oid: w.entry.oid.clone(),
            })
            .collect(),
    }
}

/// One ref update of a push: `(name, new oid or None to delete, signed base)`.
pub type EntryUpdate = (String, Option<Vec<u8>>, Vec<u8>);

/// The ref entries of `me`'s next body (head-model 7.1 step 3).
///
/// `prev` is this device's current entries with `stamp = 0` already resolved (or the
/// v1 refs of a migrating identity with counter 1), `merged` the identity's merge including
/// them, `others` every name another device head of the identity lists, and `updates` the
/// pushed refs (`None` deletes) with the base each fast-forwards (empty if none).
pub fn next_entries(
    me: &Nid,
    prev: &[HeadRef],
    merged: &Merged,
    others: &BTreeSet<String>,
    updates: &[EntryUpdate],
) -> Vec<HeadRef> {
    let mut out: BTreeMap<String, HeadRef> = prev
        .iter()
        .filter(|e| merged.get(&e.name).is_none_or(|w| w.device == *me))
        // `base` only matters to the body that made the update (head-model 8).
        .map(|e| {
            let carried = HeadRef {
                base: vec![],
                ..e.clone()
            };
            (e.name.clone(), carried)
        })
        .collect();
    for (name, oid, base) in updates {
        let counter = merged.get(name).map(|w| w.entry.counter).unwrap_or(0) + 1;
        let oid = oid.clone().unwrap_or_default();
        out.insert(
            name.clone(),
            HeadRef {
                name: name.clone(),
                oid,
                counter,
                stamp: 0,
                base: base.clone(),
            },
        );
    }
    out.retain(|name, e| !e.is_tombstone() || others.contains(name));
    out.into_values().collect()
}

/// Every `stamp = 0` of a body that landed in block `landing`, made concrete.
pub fn resolve_stamps(refs: &[HeadRef], landing: u64) -> Vec<HeadRef> {
    refs.iter()
        .map(|e| HeadRef {
            stamp: if e.stamp == 0 { landing } else { e.stamp },
            ..e.clone()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, oid: u8, counter: u64, stamp: u64) -> HeadRef {
        HeadRef {
            name: name.into(),
            oid: if oid == 0 { vec![] } else { vec![oid; 20] },
            counter,
            stamp,
            base: vec![],
        }
    }

    fn body(refs: Vec<HeadRef>) -> HeadBody {
        HeadBody {
            rid: [0; 32],
            nid: [0; 32],
            seq: 0,
            id_head: [0; 32],
            at: 0,
            pieces_created: 0,
            refs,
            snapshot: vec![],
            packs: vec![],
            prev_tx: [0; 32],
            object_hash: 1,
        }
    }

    const M: &str = "refs/heads/main";

    #[test]
    fn larger_counter_wins_over_a_stale_copy() {
        let (a, b) = ([1; 32], [2; 32]);
        let ha = body(vec![e(M, 5, 5, 100)]);
        let hb = body(vec![e(M, 6, 6, 0)]);
        let m = merge(&[(a, &ha), (b, &hb)], &BTreeMap::new()).expect("no tie");
        assert_eq!(m[M].device, b);
        assert_eq!(m[M].entry.oid, vec![6; 20]);
    }

    #[test]
    fn a_tie_goes_to_the_first_landed_and_needs_landing_blocks() {
        let (a, b) = ([9; 32], [2; 32]);
        let ha = body(vec![e(M, 5, 7, 0)]);
        let hb = body(vec![e(M, 6, 7, 0)]);
        let need = merge(&[(a, &ha), (b, &hb)], &BTreeMap::new()).expect_err("tie");
        assert_eq!(need, BTreeSet::from([a, b]));
        let landing = BTreeMap::from([(a, 100), (b, 101)]);
        let m = merge(&[(a, &ha), (b, &hb)], &landing).expect("resolved");
        assert_eq!(
            (m[M].device, m[M].entry.stamp),
            (a, 100),
            "earlier block wins"
        );
        let same = BTreeMap::from([(a, 100), (b, 100)]);
        let m = merge(&[(a, &ha), (b, &hb)], &same).expect("resolved");
        assert_eq!(m[M].device, b, "same block: smaller device nid");
    }

    #[test]
    fn equal_values_need_no_landing_and_different_refs_never_compete() {
        let (a, b) = ([1; 32], [2; 32]);
        let ha = body(vec![e(M, 5, 3, 0), e("refs/heads/x", 1, 1, 0)]);
        let hb = body(vec![e(M, 5, 3, 0), e("refs/heads/y", 2, 1, 0)]);
        let m = merge(&[(a, &ha), (b, &hb)], &BTreeMap::new()).expect("no conflict");
        assert_eq!(merged_refs(&[0; 32], &m).refs.len(), 3);
    }

    #[test]
    fn tombstones_hide_refs_and_are_dropped_once_nobody_lists_the_name() {
        let (a, b) = ([1; 32], [2; 32]);
        let hb = body(vec![e("refs/heads/x", 4, 1, 50)]);
        let merged = merge(&[(b, &hb)], &BTreeMap::new()).expect("merge");
        let others = BTreeSet::from(["refs/heads/x".to_owned()]);
        let next = next_entries(
            &a,
            &[],
            &merged,
            &others,
            &[("refs/heads/x".into(), None, vec![])],
        );
        assert_eq!(
            next,
            vec![e("refs/heads/x", 0, 2, 0)],
            "tombstone while b lists x"
        );
        let ha = body(next.clone());
        let m = merge(&[(a, &ha), (b, &hb)], &BTreeMap::new()).expect("merge");
        assert!(merged_refs(&[0; 32], &m).refs.is_empty(), "x is deleted");
        // b drops its superseded entry on its next push; then a drops the tombstone.
        let b_next = next_entries(
            &b,
            &hb.refs,
            &m,
            &BTreeSet::from(["refs/heads/x".into()]),
            &[],
        );
        assert!(b_next.is_empty());
        let a_next = next_entries(&a, &next, &m, &BTreeSet::new(), &[]);
        assert!(a_next.is_empty());
    }

    #[test]
    fn updates_take_the_next_counter_and_keep_own_winners() {
        let a = [1; 32];
        let prev = vec![e(M, 5, 3, 40), e("refs/tags/v1", 7, 1, 30)];
        let ha = body(prev.clone());
        let m = merge(&[(a, &ha)], &BTreeMap::new()).expect("merge");
        let next = next_entries(
            &a,
            &prev,
            &m,
            &BTreeSet::new(),
            &[(M.into(), Some(vec![8; 20]), vec![5; 20])],
        );
        let moved = HeadRef {
            base: vec![5; 20],
            ..e(M, 8, 4, 0)
        };
        assert_eq!(next, vec![moved, e("refs/tags/v1", 7, 1, 30)]);
        assert_eq!(resolve_stamps(&next, 77)[0].stamp, 77);
        assert_eq!(resolve_stamps(&next, 77)[1].stamp, 30);
    }

    fn dev() -> Device {
        let pk = charter_crypto::MasterSeed::from_bytes([3; 32])
            .signing_key()
            .public_key();
        Device {
            nid: pk.nid(),
            pk,
            control: [7; 20],
            bulk: [8; 20],
            h: [0; 32],
            position: Position {
                block: 1,
                tx: 0,
                key: [0; 32],
            },
        }
    }

    fn nonce(n: u64) -> HeadNonce {
        HeadNonce {
            address: [7; 20],
            before: n,
            after: n,
        }
    }

    #[test]
    fn rollback_check_accepts_the_current_head_and_rejects_an_older_one() {
        let d = dev();
        let mut h = body(vec![]);
        // Push 1 created the head and 1 piece; push 2 (refs-only) one marker piece; push 3 (big) 3 pieces.
        h.pieces_created = 5;
        assert!(
            check_head_nonce(&d, Some(&h), &nonce(6)).is_ok(),
            "current head"
        );
        h.pieces_created = 2;
        let err = check_head_nonce(&d, Some(&h), &nonce(6)).expect_err("rolled back head");
        assert_eq!(err.code(), crate::ErrorCode::ControlNotLive);
        h.pieces_created = 3;
        assert!(
            check_head_nonce(&d, Some(&h), &nonce(4)).is_ok(),
            "refs-only push: one marker piece"
        );
    }

    #[test]
    fn a_device_that_created_entities_without_a_live_head_fails_closed() {
        let d = dev();
        assert!(
            check_head_nonce(&d, None, &nonce(0)).is_ok(),
            "never pushed"
        );
        assert!(
            check_head_nonce(&d, None, &nonce(3)).is_err(),
            "head deleted or hidden"
        );
        let moving = HeadNonce {
            address: [7; 20],
            before: 3,
            after: 4,
        };
        assert_eq!(
            check_head_nonce(&d, None, &moving)
                .expect_err("raced")
                .code(),
            crate::ErrorCode::InconsistentView
        );
    }
}
