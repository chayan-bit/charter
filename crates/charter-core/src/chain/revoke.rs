//! Device revocations (docs/p5/head-model.md section 15): which delegated devices an
//! identity revoked, and the body each one's state is frozen at.

use std::collections::BTreeMap;

use super::head::{Opened, to_head};
use super::{Device, DeviceHead, IdentityChain, PeerInfo, open_all};
use crate::attrs::AttrValue;
use crate::types::{Hash32, Nid, Observed, Rid};
use crate::{
    Entity, HeadBody, Plane, REVOKE_LAYOUT_VERSION, RevokeBody, VerifyError, ZERO,
    open_head_calldata, verify_entity,
};

/// A valid revoke record of one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    pub device: Nid,
    /// The last valid body: its seq, hash and landing tx; `h` zero when there is none.
    pub seq: u64,
    pub h: Hash32,
    pub tx: Hash32,
    /// `h` of the record itself.
    pub record: Hash32,
    /// `$createdAt` of the record.
    pub block: u64,
}

/// Whether an `anchor`-kind row is a revoke record (`ag.v = 3`).
pub fn is_revoke_row(o: &Observed) -> bool {
    o.attr("ag.v") == Some(&AttrValue::U64(REVOKE_LAYOUT_VERSION))
}

/// Valid revoke records of `peer` for `rid` among `observed`, the first created per device:
/// signed by the identity and created by one of its control addresses, so a device key can
/// never revoke (head-model 15.1).
pub fn revocations(rid: &Rid, peer: &PeerInfo, observed: &[Observed]) -> BTreeMap<Nid, Revocation> {
    let mut out = BTreeMap::new();
    for e in open_all::<RevokeBody>(observed).ok {
        let valid = e.envelope.signer == peer.binds.nid
            && e.body.rid == *rid
            && peer
                .binds
                .creator_ok(Plane::Control, &e.creator, e.position.block)
            && e.envelope.verify(&peer.pk).is_ok();
        if valid {
            out.entry(e.body.device).or_insert(Revocation {
                device: e.body.device,
                seq: e.body.seq,
                h: e.body.h,
                tx: e.body.tx,
                record: e.h,
                block: e.position.block,
            });
        }
    }
    out
}

/// The body revocation `r` freezes `dev` at, from a calldata `row` of the tx it names, which
/// landed in block `landing` (the caller checked the tx came from `C_d` and succeeded). The
/// body must be signed by the device, for this repo and identity, with the named seq and
/// hash.
pub fn accept_cut_head(
    rid: &Rid,
    nid: &Nid,
    dev: &Device,
    identity: &IdentityChain,
    r: &Revocation,
    row: &Observed,
    landing: u64,
) -> Result<DeviceHead, VerifyError> {
    let bad = |what: &str| VerifyError::BadRefs(format!("revoked device's cut body: {what}"));
    if r.h == ZERO {
        return Err(bad("the record names no body"));
    }
    let row = Observed {
        block: landing,
        ..row.clone()
    };
    let opened = match verify_entity::<HeadBody>(&row, &dev.pk) {
        Ok(e) => Opened::v2(e),
        Err(_) => {
            let (envelope, body, h) = open_head_calldata(&row).map_err(|e| bad(&e.to_string()))?;
            envelope.verify(&dev.pk).map_err(|_| bad("signature"))?;
            let v4 = body.refs_table.is_some();
            let e = Entity {
                position: row.position(),
                creator: row.creator,
                expires_at: row.expires_at,
                h,
                envelope_bytes: match row.attr("ag.body") {
                    Some(AttrValue::Bytes(b)) => b.clone(),
                    _ => return Err(bad("no body")),
                },
                envelope,
                body,
            };
            let o = match v4 {
                true => Opened::v4(Entity {
                    position: e.position,
                    creator: e.creator,
                    expires_at: e.expires_at,
                    h: e.h,
                    envelope_bytes: e.envelope_bytes,
                    envelope: e.envelope,
                    body: crate::HeadBodyV4(e.body),
                }),
                false => Opened::v3(e),
            };
            o.ok_or_else(|| bad("unknown codec"))?
        }
    };
    let e = &opened.e;
    let ok = e.envelope.signer == dev.nid
        && e.h == r.h
        && e.body.seq == r.seq
        && e.body.rid == *rid
        && e.body.nid == *nid
        && identity.contains(&e.body.id_head);
    if !ok {
        return Err(bad("not the body the record names"));
    }
    Ok(to_head(opened, dev, landing))
}
