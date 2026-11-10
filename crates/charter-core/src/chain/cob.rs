//! Verification of collaborative-object ops (docs/p3/cob-ops.md).

use std::collections::{BTreeMap, BTreeSet};

use super::open_all;
use crate::types::{Hash32, Nid, Observed, Position, Rid};
use crate::{CobOpBody, Entity};
use charter_crypto::PublicKey;

/// An op whose signature, signer, repo, object and parents all check out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedOp {
    /// `H("cob", envelope bytes)`.
    pub id: Hash32,
    pub author: Nid,
    pub body: CobOpBody,
    /// Arkiv creation order; the only order programs may fold in.
    pub arkiv_order: Position,
}

impl VerifiedOp {
    /// The object id (the create op's id).
    pub fn object(&self) -> Hash32 {
        self.body.object_id(&self.id)
    }
}

/// Per object: its type and the ids of its accepted ops.
struct Object {
    type_name: String,
    ops: BTreeSet<Hash32>,
}

/// Accepted ops of `rid` in Arkiv creation order. An op is dropped (never an error) if it
/// fails decoding or the attribute check, belongs to another repo, its signer is not in
/// `keys`, its signature fails, it repeats an accepted id, or it is not a create op and its
/// object has no earlier accepted create op of the same type, or a parent is not an
/// earlier accepted op of the same object.
pub fn verify_ops(
    rid: &Rid,
    keys: &BTreeMap<Nid, PublicKey>,
    observed: &[Observed],
) -> Vec<VerifiedOp> {
    verify_ops_after(rid, keys, &[], observed)
}

/// Like [`verify_ops`], continuing after `prior` (ops this client accepted earlier, in Arkiv
/// order): follow-ups of their objects verify, and their ids are never returned again.
/// Keeps an incremental op cache equal to a full [`verify_ops`] of the whole history.
pub fn verify_ops_after(
    rid: &Rid,
    keys: &BTreeMap<Nid, PublicKey>,
    prior: &[VerifiedOp],
    observed: &[Observed],
) -> Vec<VerifiedOp> {
    let mut objects: BTreeMap<Hash32, Object> = BTreeMap::new();
    for p in prior {
        objects
            .entry(p.object())
            .or_insert_with(|| Object {
                type_name: p.body.type_name.clone(),
                ops: BTreeSet::new(),
            })
            .ops
            .insert(p.id);
    }
    let mut out = vec![];
    for e in open_all::<CobOpBody>(observed).ok {
        if accept(rid, keys, &e, &mut objects) {
            out.push(VerifiedOp {
                id: e.h,
                author: e.envelope.signer,
                body: e.body,
                arkiv_order: e.position,
            });
        }
    }
    out
}

fn accept(
    rid: &Rid,
    keys: &BTreeMap<Nid, PublicKey>,
    e: &Entity<CobOpBody>,
    objects: &mut BTreeMap<Hash32, Object>,
) -> bool {
    let signed = keys
        .get(&e.envelope.signer)
        .is_some_and(|pk| e.envelope.verify(pk).is_ok());
    if e.body.rid != *rid || !signed {
        return false;
    }
    if e.body.is_create() {
        if objects.contains_key(&e.h) {
            return false;
        }
        let ops = BTreeSet::from([e.h]);
        let type_name = e.body.type_name.clone();
        objects.insert(e.h, Object { type_name, ops });
        return true;
    }
    let Some(obj) = objects.get_mut(&e.body.object) else {
        return false;
    };
    let ok = obj.type_name == e.body.type_name
        && !obj.ops.contains(&e.h)
        && e.body.parents.iter().all(|p| obj.ops.contains(p));
    if ok {
        obj.ops.insert(e.h);
    }
    ok
}
