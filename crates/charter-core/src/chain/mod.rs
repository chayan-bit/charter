//! Verification of chains: binds, identity, anchors, head acceptance (design section 3).

mod accept;
mod anchor;
mod bind;
mod cob;
mod head;
mod identity;

pub use accept::{
    AcceptedHead, CompletenessObs, HeadInputs, accept_head, check_completeness, check_control_set,
    check_requires, find_refs,
};
pub use anchor::{AnchorChain, AnchorLink, Start, anchor_chain};
pub use bind::{BindChain, BindLink, bind_chain};
pub use cob::{VerifiedOp, verify_ops, verify_ops_after};
pub use head::{
    Device, DeviceHead, EntryUpdate, HeadNonce, Merged, Winner, accept_device_head,
    check_head_nonce, devices, is_device_row, merge, merged_refs, next_entries, resolve_stamps,
};
pub use identity::{IdentityChain, Revision, identity_chain, maj};

use crate::entity::open;
use crate::types::{Address, Hash32, Nid, Observed};
use crate::{Body, Entity, FormatError, Kind, PeerBody, VerifyError};
use charter_crypto::PublicKey;
use std::collections::BTreeSet;

/// Evidence that a peer signed two conflicting entities at one seq (design 3.7).
/// Reported as `E_EQUIVOCATION` warnings; never changes the chain.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Equivocation {
    pub kind: Kind,
    pub nid: Nid,
    pub seq: u64,
    /// Entity keys: the chain's entity, then the conflicting one.
    pub keys: [Hash32; 2],
}

/// A peer's key and its verified bind chain for one repo.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub pk: PublicKey,
    pub binds: BindChain,
}

/// The public key of `nid` from the first valid self-signed peer document.
/// Every valid copy carries the same key, because `nid` commits to it.
pub fn peer_key(nid: &Nid, observed: &[Observed]) -> Option<PublicKey> {
    open_all::<PeerBody>(observed).ok.into_iter().find_map(|e| {
        let pk = e.body.public_key().ok()?;
        (pk.nid() == *nid && e.envelope.verify(&pk).is_ok()).then_some(pk)
    })
}

/// Decoded entities of one kind in creation order, plus entities whose version is unknown.
pub(crate) struct Opened<B> {
    pub ok: Vec<Entity<B>>,
    /// `(creator, key)` of entities with an unknown `ag.v`, envelope `v` or `enc`.
    pub unsupported: Vec<(Address, Hash32)>,
}

pub(crate) fn open_all<B: Body>(observed: &[Observed]) -> Opened<B> {
    let mut ok = vec![];
    let mut unsupported = vec![];
    for o in crate::in_creation_order(observed) {
        match open::<B>(o) {
            Ok(e) => ok.push(e),
            Err(FormatError::Unsupported { .. }) => unsupported.push((o.creator, o.key)),
            Err(_) => {}
        }
    }
    Opened { ok, unsupported }
}

/// `E_UNSUPPORTED_VERSION` for an unknown-version entity from a tracked control address;
/// the same entity from any other creator is ignored (design 2.1).
pub(crate) fn fail_unsupported(
    unsupported: &[(Address, Hash32)],
    control: &BTreeSet<Address>,
) -> Result<(), VerifyError> {
    match unsupported.iter().find(|(c, _)| control.contains(c)) {
        Some((_, key)) => Err(VerifyError::UnsupportedVersion { key: *key }),
        None => Ok(()),
    }
}

/// `(seq, h, key)` of a link or a valid candidate.
pub(crate) type SeqItem = (u64, Hash32, Hash32);

/// Fails with `E_CHAIN_GAP` if a valid candidate sits above the head (or exists with no
/// chain at all), and returns the equivocations: valid candidates at a linked seq with
/// different bytes than the link.
pub(crate) fn gap_and_equivocations(
    kind: Kind,
    nid: &Nid,
    links: &[SeqItem],
    candidates: &[SeqItem],
) -> Result<Vec<Equivocation>, VerifyError> {
    let head = links.last().map(|l| l.0);
    if let Some(c) = candidates.iter().find(|c| head.is_none_or(|h| c.0 > h)) {
        return Err(VerifyError::ChainGap { kind, seq: c.0 });
    }
    let mut eq: Vec<Equivocation> = candidates
        .iter()
        .filter_map(|c| {
            let link = links.iter().find(|l| l.0 == c.0)?;
            (link.1 != c.1).then_some(Equivocation {
                kind,
                nid: *nid,
                seq: c.0,
                keys: [link.2, c.2],
            })
        })
        .collect();
    eq.sort();
    Ok(eq)
}
