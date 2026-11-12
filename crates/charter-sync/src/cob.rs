//! Collaborative-object ops (docs/p3/cob-ops.md): [`post`] writes one signed op from the
//! author's control address; [`list`] queries ops and returns only the verified ones.

pub mod json;

use std::collections::{BTreeMap, BTreeSet};

use charter_core::{
    AttrValue, Body, CobOpBody, Hash32, Kind, Nid, Observed, Rid, VerifiedOp, peer_doc, verify_ops,
};
use charter_store::{Arkiv, Pred, attr};

use crate::keys::Keys;
use crate::observe::{all, b32, kind};
use crate::repo::Config;
use crate::write::{entity_op, sign};
use crate::{Result, identity};

/// Signs and writes `body` (control plane, control lifetime); returns the op id. A writer
/// with no bind for the repo gets its `peer` and `bind_0` in the same tx.
pub async fn post<A: Arkiv>(a: &A, cfg: &Config, keys: &Keys, body: &CobOpBody) -> Result<Hash32> {
    body.validate()?;
    let s = sign(keys.sk(), body)?;
    let op = entity_op::<CobOpBody>(&s.bytes, cfg.control_lifetime)?;
    identity::with_bind(a, cfg, keys, &body.rid, op).await?;
    Ok(s.h)
}

/// Validates and signs one op; its `h` is the op id, so a later op can name it as parent
/// before it is sent (signatures are randomized: send these exact bytes, never re-sign).
pub fn sign_op(keys: &Keys, body: &CobOpBody) -> Result<crate::write::Signed> {
    body.validate()?;
    sign(keys.sk(), body)
}

/// Writes signed ops of `rid` in order, one tx each, back to back on one nonce pipeline
/// without waiting for each receipt (docs/p5/speed-and-cost.md 2.2 item 7). A writer with
/// no bind gets its `peer` and `bind_0` in the first tx. Returns the op ids in order.
pub async fn post_signed<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    rid: &Rid,
    signed: &[crate::write::Signed],
) -> Result<Vec<Hash32>> {
    if signed.is_empty() {
        return Ok(vec![]);
    }
    let ops = signed
        .iter()
        .map(|s| entity_op::<CobOpBody>(&s.bytes, cfg.control_lifetime))
        .collect::<Result<Vec<_>>>()?;
    identity::with_bind_all(a, cfg, keys, rid, ops).await?;
    Ok(signed.iter().map(|s| s.h).collect())
}

/// [`post_signed`] for independent ops of one repo (none names another as parent).
pub async fn post_many<A: Arkiv>(
    a: &A,
    cfg: &Config,
    keys: &Keys,
    bodies: &[CobOpBody],
) -> Result<Vec<Hash32>> {
    let Some(rid) = bodies.first().map(|b| b.rid) else {
        return Ok(vec![]);
    };
    if bodies.iter().any(|b| b.rid != rid) {
        return Err(crate::SyncError::coded(
            charter_core::ErrorCode::Internal,
            "post_many: ops of more than one repo",
        ));
    }
    let signed = bodies
        .iter()
        .map(|b| sign_op(keys, b))
        .collect::<Result<Vec<_>>>()?;
    post_signed(a, cfg, keys, &rid, &signed).await
}

/// Verified ops of `rid`, optionally of one object and one type, in Arkiv creation order.
pub async fn list<A: Arkiv>(
    a: &A,
    rid: &Rid,
    object: Option<&Hash32>,
    type_name: Option<&str>,
) -> Result<Vec<VerifiedOp>> {
    let mut terms = vec![kind(Kind::Cob), b32("ag.rid", rid)];
    terms.extend(object.map(|o| b32("ag.cob", o)));
    terms.extend(type_name.map(|t| Pred::eq(attr("ag.type"), t)));
    let observed = all(a, Pred::And(terms)).await?;
    let keys = peer_keys(a, &observed).await?;
    Ok(verify_ops(rid, &keys, &observed))
}

/// Keys of every `ag.nid` named in `observed` that has a valid self-signed `peer` document.
pub(crate) async fn peer_keys<A: Arkiv>(
    a: &A,
    observed: &[Observed],
) -> Result<BTreeMap<Nid, charter_crypto::PublicKey>> {
    peer_keys_with(a, observed, &BTreeMap::new()).await
}

/// Like [`peer_keys`], taking keys already verified in `known` (the sync view resolves
/// delegates and identity signers) and looking up the rest concurrently.
pub(crate) async fn peer_keys_with<A: Arkiv>(
    a: &A,
    observed: &[Observed],
    known: &BTreeMap<Nid, charter_core::PeerInfo>,
) -> Result<BTreeMap<Nid, charter_crypto::PublicKey>> {
    let nids: BTreeSet<Nid> = observed
        .iter()
        .filter_map(|o| match o.attr("ag.nid") {
            Some(AttrValue::Bytes32(n)) => Some(*n),
            _ => None,
        })
        .collect();
    let mut keys: BTreeMap<Nid, charter_crypto::PublicKey> = nids
        .iter()
        .filter_map(|n| known.get(n).map(|p| (*n, p.pk.clone())))
        .collect();
    let missing: Vec<Nid> = nids.into_iter().filter(|n| !keys.contains_key(n)).collect();
    let found =
        futures_util::future::try_join_all(missing.iter().map(|nid| peer_key_of(a, nid))).await?;
    for (nid, pk) in missing.into_iter().zip(found) {
        if let Some(pk) = pk {
            keys.insert(nid, pk);
        }
    }
    Ok(keys)
}

/// Aliases of every peer document this process verified, by peer; filled as keys are
/// looked up, so naming peers costs no query of its own.
static ALIASES: std::sync::Mutex<BTreeMap<Nid, String>> = std::sync::Mutex::new(BTreeMap::new());

/// The alias the last verified peer document of `nid` carried; `None` if it was never
/// looked up in this process or it set none.
pub fn alias_of(nid: &Nid) -> Option<String> {
    let m = ALIASES.lock().ok()?;
    m.get(nid).filter(|a| !a.is_empty()).cloned()
}

fn remembered(
    nid: &Nid,
    doc: Option<(charter_crypto::PublicKey, String)>,
) -> Option<charter_crypto::PublicKey> {
    let (pk, alias) = doc?;
    if let Ok(mut m) = ALIASES.lock() {
        m.insert(*nid, alias);
    }
    Some(pk)
}

/// The key of `nid` from its self-signed `peer` documents: the first document alone when
/// it verifies (any valid one gives the same key, since `nid` hashes it), else all of them.
/// Peer documents carry no `ag.rid`, so a busy peer has many; this reads one row.
pub async fn peer_key_of<A: Arkiv>(a: &A, nid: &Nid) -> Result<Option<charter_crypto::PublicKey>> {
    let filter = Pred::And(vec![kind(Kind::Peer), b32("ag.nid", nid)]);
    let one = charter_store::QueryOpts {
        limit: 1,
        offset: 0,
        select: crate::observe::full_select(),
    };
    let page = a
        .query(&charter_store::Query::from(filter.clone()), &one)
        .await?;
    if let Some(pk) = remembered(nid, peer_doc(nid, &crate::observe::to_observed(page.rows))) {
        return Ok(Some(pk));
    }
    Ok(remembered(nid, peer_doc(nid, &all(a, filter).await?)))
}

/// New verified ops of `rid`: what [`list_since`] found at or after a block.
#[derive(Debug, Clone, Default)]
pub struct Since {
    /// Ops created at or after `from_block`, in Arkiv creation order.
    pub ops: Vec<VerifiedOp>,
    /// Highest creation block among all op entities seen, accepted or not.
    pub last_block: Option<u64>,
}

/// Verified ops of `rid` created at or after `from_block`, given every op accepted before
/// it (`prior`, in Arkiv order). Appending the result to `prior` gives the same ops in the
/// same order as a full [`list`]: blocks are atomic, so a block is either wholly before
/// `from_block` or wholly in this result, and `Observed.tx` ranks only ever order entities
/// of one block within one query. Only the new ops' signers are looked up.
///
/// `$createdAt` is not queryable on Arkiv, so the op query itself still pages every op;
/// the saving is in peer lookups and verification.
pub async fn list_since<A: Arkiv>(
    a: &A,
    rid: &Rid,
    from_block: u64,
    prior: &[VerifiedOp],
) -> Result<Since> {
    let observed = all(a, Pred::And(vec![kind(Kind::Cob), b32("ag.rid", rid)])).await?;
    let last_block = observed.iter().map(|o| o.block).max();
    let fresh: Vec<Observed> = observed
        .into_iter()
        .filter(|o| o.block >= from_block)
        .collect();
    let keys = peer_keys(a, &fresh).await?;
    let ops = charter_core::verify_ops_after(rid, &keys, prior, &fresh);
    Ok(Since { ops, last_block })
}
