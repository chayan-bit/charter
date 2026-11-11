//! MLS entities on Arkiv (`ag.kind = mls`, `ag.t` names the type) and the sealed lockbox.
//!
//! - `kp`: a key package, from the member's wallet; `ag.h = sha256(bytes)` is its id.
//! - `group`: the private-repo marker, `ag.tag = H(rid)`; its body is the suite code point.
//! - `commit`: from an admin's control address, `ag.rid = HMAC(rid)`, `ag.ep` = new epoch.
//! - `welcome`: `ag.kp` = the joiner's key package id, `ag.ep` = the joining epoch.
//! - `pub`: a published epoch grant (`id_key || content key`), `ag.tag = H(rid)`.
//!
//! Commits and welcomes carry `ag.box`: the keyring sealed under the new epoch key, and
//! `ag.att`: the committing admin's attestation of both, sealed under the same key.
//! A migrated repo also has one `stop` row (see [`stop_op`]).

use charter_core::{AttrValue, Hash32, Kind, Observed, Rid};
use charter_store::{Arkiv, AttributeValue, Operation, Pred, attr, create_op, random_salt};
use charter_sync::observe;
use charter_sync::{Result, SyncError};

use crate::keyring::Keyring;
use crate::seal::{Key, aad, blind, open, seal};

pub const T_KP: &str = "kp";
pub const T_GROUP: &str = "group";
pub const T_COMMIT: &str = "commit";
pub const T_WELCOME: &str = "welcome";
pub const T_PUB: &str = "pub";
/// AAD kind of the lockbox; reserved, so no entity can be sealed under it.
pub const BOX_KIND: &str = "box";
/// `ag.t` of the migration stopper.
pub const T_STOP: &str = "stop";
/// `ag.v` of the stopper: no client supports it, so a pre-0.5.1 client fails closed.
pub const STOP_V: u64 = 0x5354_4f50;

/// Public handle of a private repo for anyone who knows its rid.
pub fn tag(rid: &Rid) -> Hash32 {
    charter_crypto::h("private-tag", rid)
}

/// The blinded rid of what epoch `epoch` seals (a commit to epoch `e` uses `e - 1`, the
/// epoch every member that must apply it is in).
pub fn blinded_rid_at(ring: &Keyring, epoch: u64) -> Hash32 {
    blind(ring.id_key_at(epoch), "rid", &ring.rid)
}

/// `ag.rid` equal to the rid blinded under any id-key generation `ring` holds.
pub fn rid_pred(ring: &Keyring) -> Pred {
    let mut terms: Vec<Pred> = vec![];
    for k in ring.id_keys() {
        let t = observe::b32("ag.rid", &blind(k, "rid", &ring.rid));
        if !terms.contains(&t) {
            terms.push(t);
        }
    }
    match terms.len() {
        1 => terms.remove(0),
        _ => Pred::Or(terms),
    }
}

/// The migration stopper: an identity row under the generation-0 blinded rid with an
/// `ag.v` no client supports. Sent from a root delegate's control address, it makes a
/// pre-0.5.1 member fail with `E_UNSUPPORTED_VERSION` instead of reading a repo whose
/// commits it can no longer verify; 0.5.1 drops it before core sees it.
pub fn stop_op(ring: &Keyring, lifetime: u64) -> Result<Operation> {
    let attrs = [
        ("ag.kind", AttributeValue::Str(Kind::Id.name().into()), true),
        ("ag.v", AttributeValue::U64(STOP_V), false),
        ("ag.t", AttributeValue::Str(T_STOP.into()), true),
        (
            "ag.rid",
            AttributeValue::Bytes32(blind(&ring.id_key, "rid", &ring.rid)),
            true,
        ),
    ];
    Ok(create_op(&attrs, lifetime, random_salt())?)
}

pub fn op(
    t: &str,
    extra: Vec<(&str, AttributeValue, bool)>,
    body: Vec<u8>,
    lifetime: u64,
) -> Result<Operation> {
    let mut attrs = vec![
        (
            "ag.kind",
            AttributeValue::Str(Kind::Mls.name().into()),
            true,
        ),
        (
            "ag.v",
            AttributeValue::U64(charter_core::ENTITY_LAYOUT_VERSION),
            false,
        ),
        ("ag.t", AttributeValue::Str(t.into()), true),
        ("ag.body", AttributeValue::Bytes(body), false),
    ];
    attrs.extend(extra);
    Ok(create_op(&attrs, lifetime, random_salt())?)
}

pub fn b32(v: Hash32) -> AttributeValue {
    AttributeValue::Bytes32(v)
}

/// Live `mls` entities of type `t` matching `extra`, in creation order.
pub async fn find<A: Arkiv>(a: &A, t: &str, extra: Vec<Pred>) -> Result<Vec<Observed>> {
    let mut all = vec![observe::kind(Kind::Mls), Pred::eq(attr("ag.t"), t)];
    all.extend(extra);
    observe::all(a, Pred::And(all)).await
}

pub fn bytes<'a>(o: &'a Observed, name: &str) -> Option<&'a [u8]> {
    match o.attr(name) {
        Some(AttrValue::Bytes(b)) => Some(b),
        _ => None,
    }
}

pub fn u64_of(o: &Observed, name: &str) -> Option<u64> {
    match o.attr(name) {
        Some(AttrValue::U64(n)) => Some(*n),
        _ => None,
    }
}

/// The keyring sealed under `key`, the key of `epoch`.
pub fn seal_box(ring: &Keyring, epoch: u64, key: &Key) -> Result<Vec<u8>> {
    let mut json = serde_json::to_vec(ring)
        .map_err(|e| SyncError::coded(charter_core::ErrorCode::Internal, e.to_string()))?;
    let sealed = seal(key, &aad(&ring.rid, BOX_KIND, epoch), &json);
    zeroize::Zeroize::zeroize(&mut json);
    sealed
}

pub fn open_box(rid: &Rid, epoch: u64, key: &Key, sealed: &[u8]) -> Option<Keyring> {
    let mut json = open(key, &aad(rid, BOX_KIND, epoch), sealed)?;
    let ring = serde_json::from_slice::<Keyring>(&json).ok();
    zeroize::Zeroize::zeroize(&mut json);
    ring.filter(|r| r.rid == *rid)
}
