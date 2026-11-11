//! The v0.4.0 head acceptance (bda7571 `crates/charter-core/src/chain/head.rs`
//! `valid_head`, `fail_unsupported` and `open_all` of `chain/mod.rs`, and the nonce check),
//! copied verbatim onto the public API it used. `open`, `HeadBody` and `check_head_nonce`
//! are unchanged since bda7571, so this is exactly what a v0.4.0 client runs on a head row.

use std::collections::BTreeSet;

use charter_core::{
    Address, Device, Entity, FormatError, Hash32, HeadBody, HeadNonce, IdentityChain, Nid,
    Observed, Rid, VerifyError, check_head_nonce, in_creation_order, open,
};

struct Opened {
    ok: Vec<Entity<HeadBody>>,
    unsupported: Vec<(Address, Hash32)>,
}

fn open_all(observed: &[Observed]) -> Opened {
    let mut ok = vec![];
    let mut unsupported = vec![];
    for o in in_creation_order(observed) {
        match open::<HeadBody>(o) {
            Ok(e) => ok.push(e),
            Err(FormatError::Unsupported { .. }) => unsupported.push((o.creator, o.key)),
            Err(_) => {}
        }
    }
    Opened { ok, unsupported }
}

fn fail_unsupported(
    unsupported: &[(Address, Hash32)],
    control: &BTreeSet<Address>,
) -> Result<(), VerifyError> {
    match unsupported.iter().find(|(c, _)| control.contains(c)) {
        Some((_, key)) => Err(VerifyError::UnsupportedVersion { key: *key }),
        None => Ok(()),
    }
}

fn valid_head(
    rid: &Rid,
    nid: &Nid,
    dev: &Device,
    identity: &IdentityChain,
    observed: &[Observed],
) -> Result<Option<Entity<HeadBody>>, VerifyError> {
    let opened = open_all(observed);
    fail_unsupported(&opened.unsupported, &BTreeSet::from([dev.control]))?;
    let mut valid: Vec<Entity<HeadBody>> = opened
        .ok
        .into_iter()
        .filter(|e| {
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

/// v0.4.0 `accept_device_head` up to its mark check: the head v0.4.0 accepts, or its error.
pub fn accept_device_head(
    rid: &Rid,
    nid: &Nid,
    dev: &Device,
    identity: &IdentityChain,
    observed: &[Observed],
    nonce: &HeadNonce,
) -> Result<Option<Entity<HeadBody>>, VerifyError> {
    let head = valid_head(rid, nid, dev, identity, observed)?;
    check_head_nonce(dev, head.as_ref().map(|e| &e.body), nonce)?;
    Ok(head)
}
