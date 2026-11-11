//! Composite (ML-DSA-65 + Ed25519) signatures that tie MLS state to identities, so nothing
//! an MLS leaf, a lockbox or a grant claims rests on an unauthenticated label
//! (docs/p2/private-repos.md "Threat model"):
//!
//! - leaf binding: a nid signs its MLS signature key; an admin checks it before an add;
//! - welcome attestation: the admin who made a welcome signs it and its lockbox, sealed;
//! - commit attestation: the admin who made a commit signs it, its lockbox, the new epoch's
//!   admin set and an epoch authenticator, readable by anyone holding the generation-0 id
//!   key, so joiners and grant readers can replay who was admin when from the root;
//! - grant: a root delegate signs a published epoch grant.

use std::collections::BTreeSet;

use charter_core::{ErrorCode, Hash32, Nid, Rid};
use charter_crypto::{Alg, PublicKey, Sig};
use charter_sync::{Keys, Result, SyncError};

use crate::keyring::Role;
use crate::seal::{Key, aad, blind, open, seal};

pub const CTX_LEAF: &[u8] = b"charter-mls-leaf-device";
pub const CTX_COMMIT: &[u8] = b"charter-mls-commit";
pub const CTX_WELCOME: &[u8] = b"charter-mls-welcome";
pub const CTX_GRANT: &[u8] = b"charter-mls-grant";
/// AAD kind of the sealed attestation; reserved, so no entity can be sealed under it.
pub const ATT_KIND: &str = "att";

pub fn bad(msg: impl Into<String>) -> SyncError {
    SyncError::coded(ErrorCode::BadSig, msg.into())
}

fn sig_of(b: &[u8]) -> Option<Sig> {
    Sig::from_bytes(Alg::MlDsa65Ed25519, b).ok()
}

/// `pk` from its bytes, if it is the key `nid` names.
pub fn pk_of(nid: &Nid, bytes: &[u8]) -> Option<PublicKey> {
    PublicKey::from_bytes(Alg::MlDsa65Ed25519, bytes)
        .ok()
        .filter(|pk| pk.nid() == *nid)
}

pub fn verifies(pk: &PublicKey, ctx: &[u8], msg: &[u8], sig: &[u8]) -> bool {
    sig_of(sig).is_some_and(|s| pk.verify(msg, ctx, &s).is_ok())
}

fn sign(keys: &Keys, ctx: &[u8], msg: &[u8]) -> Result<Vec<u8>> {
    keys.sk()
        .sign(msg, ctx)
        .map(|s| s.as_bytes().to_vec())
        .map_err(|e| SyncError::coded(ErrorCode::Internal, format!("sign: {e:?}")))
}

fn leaf_msg(sig_key: &[u8], nid: &Nid, device: &Nid) -> Vec<u8> {
    [sig_key, nid.as_slice(), device.as_slice()].concat()
}

/// This member's signature over its MLS signature key and the device (head-model device
/// nid) the leaf is for, so a revoke of that device names exactly this leaf.
pub fn sign_leaf(keys: &Keys, sig_key: &[u8], device: &Nid) -> Result<Vec<u8>> {
    sign(keys, CTX_LEAF, &leaf_msg(sig_key, &keys.nid(), device))
}

/// Whether `sig` by the key `pk` (which must be `nid`'s) binds `sig_key` to `nid`'s device
/// `device`.
pub fn leaf_bound(nid: &Nid, pk: &[u8], sig: &[u8], sig_key: &[u8], device: &Nid) -> bool {
    pk_of(nid, pk).is_some_and(|pk| verifies(&pk, CTX_LEAF, &leaf_msg(sig_key, nid, device), sig))
}

/// What an admin attests for a commit (`kp = None`) or a welcome for key package `kp`.
pub fn att_msg(rid: &Rid, epoch: u64, body: &[u8], lockbox: &[u8], kp: Option<&Hash32>) -> Vec<u8> {
    [
        rid.as_slice(),
        &epoch.to_be_bytes(),
        &charter_crypto::sha256(body),
        &charter_crypto::sha256(lockbox),
        kp.map_or(&[0u8; 32], |k| k),
    ]
    .concat()
}

/// `nid || signature` sealed under the new epoch's key.
pub fn seal_att(
    keys: &Keys,
    ctx: &[u8],
    key: &Key,
    rid: &Rid,
    epoch: u64,
    msg: &[u8],
) -> Result<Vec<u8>> {
    let att = [keys.nid().as_slice(), &sign(keys, ctx, msg)?].concat();
    seal(key, &aad(rid, ATT_KIND, epoch), &att)
}

/// The signer nid and signature of a sealed attestation.
pub fn open_att(key: &Key, rid: &Rid, epoch: u64, sealed: &[u8]) -> Option<(Nid, Vec<u8>)> {
    let att = open(key, &aad(rid, ATT_KIND, epoch), sealed)?;
    let (nid, sig) = att.split_at_checked(32)?;
    Some((nid.try_into().ok()?, sig.to_vec()))
}

/// What only holders of epoch `epoch`'s key can compute: an authenticator of the key.
pub fn epoch_auth(key: &Key, rid: &Rid) -> Hash32 {
    blind(key, "epoch", rid)
}

/// `ag.ct` of every format-2 commit: found by anyone holding the generation-0 id key (every
/// member and joiner, checked against the root identity), whatever later generations hold.
pub fn commits_tag(gen0: &Key, rid: &Rid) -> Hash32 {
    blind(gen0, "commits", rid)
}

/// The key a commit attestation's admin set is sealed under: holders of the generation-0
/// id key, who already see each commit's signer, read it; anyone else sees ciphertext.
fn admins_key(gen0: &Key, rid: &Rid) -> Key {
    blind(gen0, "admin-set", rid)
}

const ADM_KIND: &str = "adm";

/// A commit's public attestation (`ag.att`): `signer || epoch auth || u32be(len) || admin
/// set sealed under [`admins_key`] || sig`, opened: the new epoch's admins (hex nids).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAtt {
    pub signer: Nid,
    pub auth: Hash32,
    pub admins: BTreeSet<String>,
    pub sig: Vec<u8>,
}

fn admin_bytes(admins: &BTreeSet<String>) -> Vec<u8> {
    admins
        .iter()
        .filter_map(|n| hex::decode(n).ok())
        .flatten()
        .collect()
}

fn commit_msg(
    rid: &Rid,
    epoch: u64,
    body: &[u8],
    lockbox: &[u8],
    eauth: &Hash32,
    admins: &BTreeSet<String>,
) -> Vec<u8> {
    [
        att_msg(rid, epoch, body, lockbox, None),
        eauth.to_vec(),
        admin_bytes(admins),
    ]
    .concat()
}

/// This admin's public attestation of commit `body` with `lockbox`, making epoch `epoch`
/// (key `key`) with admin set `admins`; `gen0` is the repo's generation-0 id key.
#[allow(clippy::too_many_arguments)]
pub fn sign_commit(
    keys: &Keys,
    rid: &Rid,
    gen0: &Key,
    epoch: u64,
    key: &Key,
    body: &[u8],
    lockbox: &[u8],
    admins: &BTreeSet<String>,
) -> Result<Vec<u8>> {
    let eauth = epoch_auth(key, rid);
    let msg = commit_msg(rid, epoch, body, lockbox, &eauth, admins);
    let sig = sign(keys, CTX_COMMIT, &msg)?;
    let sealed = seal(
        &admins_key(gen0, rid),
        &aad(rid, ADM_KIND, epoch),
        &admin_bytes(admins),
    )?;
    let len = u32::try_from(sealed.len())
        .map_err(|_| SyncError::coded(ErrorCode::Internal, "admin set too large"))?;
    Ok([
        keys.nid().as_slice(),
        &eauth,
        &len.to_be_bytes(),
        &sealed,
        &sig,
    ]
    .concat())
}

/// The attestation of a commit for epoch `epoch` of `rid`, its admin set opened with the
/// generation-0 id key `gen0`.
pub fn parse_commit_att(b: &[u8], gen0: &Key, rid: &Rid, epoch: u64) -> Option<CommitAtt> {
    let (signer, rest) = b.split_at_checked(32)?;
    let (eauth, rest) = rest.split_at_checked(32)?;
    let (len, rest) = rest.split_at_checked(4)?;
    let len = usize::try_from(u32::from_be_bytes(len.try_into().ok()?)).ok()?;
    let (sealed, sig) = rest.split_at_checked(len)?;
    let set = open(&admins_key(gen0, rid), &aad(rid, ADM_KIND, epoch), sealed)?;
    if !set.len().is_multiple_of(32) {
        return None;
    }
    Some(CommitAtt {
        signer: signer.try_into().ok()?,
        auth: eauth.try_into().ok()?,
        admins: set.chunks(32).map(hex::encode).collect(),
        sig: sig.to_vec(),
    })
}

/// Whether `att` is a valid signature by its signer (key `pk`) over that commit row.
pub fn commit_signed(
    pk: &PublicKey,
    att: &CommitAtt,
    rid: &Rid,
    epoch: u64,
    body: &[u8],
    lockbox: &[u8],
) -> bool {
    let msg = commit_msg(rid, epoch, body, lockbox, &att.auth, &att.admins);
    verifies(pk, CTX_COMMIT, &msg, &att.sig)
}

/// A published epoch grant: `id_key || key`, signed by a root delegate, with the roles of
/// that epoch (so readers apply the same role rules as members). Legacy grants are the bare
/// 64 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedGrant {
    pub id_key: Key,
    pub key: Key,
    /// `(signer public key, signature)`; `None` on a legacy grant.
    pub sig: Option<(Vec<u8>, Vec<u8>)>,
    pub roles: Vec<(Nid, Role)>,
}

fn role_byte(r: Role) -> u8 {
    match r {
        Role::Read => 0,
        Role::Write => 1,
        Role::Admin => 2,
    }
}

fn grant_msg(rid: &Rid, epoch: u64, id_key: &Key, key: &Key, roles: &[(Nid, Role)]) -> Vec<u8> {
    let mut m = [rid.as_slice(), &epoch.to_be_bytes(), id_key, key].concat();
    for (n, r) in roles {
        m.extend_from_slice(n);
        m.push(role_byte(*r));
    }
    m
}

/// `id_key || key || pk || sig || (nid || role)*`.
pub fn encode_grant(
    keys: &Keys,
    rid: &Rid,
    epoch: u64,
    id_key: &Key,
    key: &Key,
    roles: &[(Nid, Role)],
) -> Result<Vec<u8>> {
    let msg = grant_msg(rid, epoch, id_key, key, roles);
    let sig = sign(keys, CTX_GRANT, &msg)?;
    let mut b = [id_key.as_slice(), key, keys.pk().as_bytes(), &sig].concat();
    b.extend_from_slice(&msg[32 + 8 + 64..]);
    Ok(b)
}

pub fn decode_grant(b: &[u8]) -> Option<SignedGrant> {
    let (id_key, rest) = b.split_at_checked(32)?;
    let (key, rest) = rest.split_at_checked(32)?;
    let (id_key, key) = (id_key.try_into().ok()?, key.try_into().ok()?);
    if rest.is_empty() {
        return Some(SignedGrant {
            id_key,
            key,
            sig: None,
            roles: vec![],
        });
    }
    let alg = Alg::MlDsa65Ed25519;
    let (pk, rest) = rest.split_at_checked(alg.pk_len())?;
    let (sig, rest) = rest.split_at_checked(alg.sig_len())?;
    if !rest.len().is_multiple_of(33) {
        return None;
    }
    let roles = rest
        .chunks(33)
        .map(|c| {
            let role = match c[32] {
                0 => Role::Read,
                1 => Role::Write,
                2 => Role::Admin,
                _ => return None,
            };
            Some((c[..32].try_into().ok()?, role))
        })
        .collect::<Option<Vec<_>>>()?;
    Some(SignedGrant {
        id_key,
        key,
        sig: Some((pk.to_vec(), sig.to_vec())),
        roles,
    })
}

/// Whether `g` is signed by one of `delegates` for `rid`'s epoch `epoch`.
pub fn grant_signed(g: &SignedGrant, rid: &Rid, epoch: u64, delegates: &[Nid]) -> bool {
    let Some((pk, sig)) = &g.sig else {
        return false;
    };
    let msg = grant_msg(rid, epoch, &g.id_key, &g.key, &g.roles);
    delegates
        .iter()
        .any(|d| pk_of(d, pk).is_some_and(|pk| verifies(&pk, CTX_GRANT, &msg, sig)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_round_trip_and_reject_other_signers_or_epochs() {
        let keys = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([1; 32]));
        let other = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([2; 32]));
        let roles = [([7; 32], Role::Write), ([8; 32], Role::Admin)];
        let b = encode_grant(&keys, &[3; 32], 4, &[5; 32], &[6; 32], &roles).expect("grant");
        let g = decode_grant(&b).expect("decode");
        assert_eq!(
            (g.id_key, g.key, g.roles.clone()),
            ([5; 32], [6; 32], roles.to_vec())
        );
        assert!(grant_signed(&g, &[3; 32], 4, &[keys.nid()]));
        assert!(!grant_signed(&g, &[3; 32], 5, &[keys.nid()]), "other epoch");
        assert!(
            !grant_signed(&g, &[3; 32], 4, &[other.nid()]),
            "not a delegate"
        );
        let legacy = decode_grant(&[1; 64]).expect("legacy");
        assert!(legacy.sig.is_none() && !grant_signed(&legacy, &[3; 32], 4, &[keys.nid()]));
        assert!(decode_grant(&b[..100]).is_none());
    }

    #[test]
    fn a_leaf_binding_only_verifies_for_its_own_nid_key_and_device() {
        let keys = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([1; 32]));
        let other = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([2; 32]));
        let dev = [7; 32];
        let sig = sign_leaf(&keys, b"mls key", &dev).expect("sign");
        let pk = keys.pk().as_bytes();
        assert!(leaf_bound(&keys.nid(), pk, &sig, b"mls key", &dev));
        assert!(!leaf_bound(&keys.nid(), pk, &sig, b"other key", &dev));
        assert!(
            !leaf_bound(&keys.nid(), pk, &sig, b"mls key", &[8; 32]),
            "another device"
        );
        assert!(
            !leaf_bound(&other.nid(), pk, &sig, b"mls key", &dev),
            "pk is not that nid's"
        );
        let theirs = other.pk().as_bytes();
        assert!(!leaf_bound(&other.nid(), theirs, &sig, b"mls key", &dev));
    }
}
