//! Composite (ML-DSA-65 + Ed25519) signatures that tie MLS state to identities, so nothing
//! an MLS leaf, a lockbox or a grant claims rests on an unauthenticated label
//! (docs/p2/private-repos.md "Threat model"):
//!
//! - leaf binding: a nid signs its MLS signature key; an admin checks it before an add;
//! - attestation: the admin who made a commit or welcome signs its bytes and lockbox;
//! - admin certificate: an admin vouches for a new admin, rooted in the root identity;
//! - grant: a root delegate signs a published epoch grant.

use std::collections::{BTreeMap, BTreeSet};

use charter_core::{ErrorCode, Hash32, Nid, Rid};
use charter_crypto::{Alg, PublicKey, Sig};
use charter_store::Arkiv;
use charter_sync::{Keys, Result, SyncError};

use crate::keyring::{Keyring, Role};
use crate::seal::{Key, aad, open, seal};

pub const CTX_LEAF: &[u8] = b"charter-mls-leaf";
pub const CTX_COMMIT: &[u8] = b"charter-mls-commit";
pub const CTX_WELCOME: &[u8] = b"charter-mls-welcome";
pub const CTX_ADMIN: &[u8] = b"charter-mls-admin";
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

fn leaf_msg(sig_key: &[u8], nid: &Nid) -> Vec<u8> {
    [sig_key, nid.as_slice()].concat()
}

/// This member's signature over its MLS signature key.
pub fn sign_leaf(keys: &Keys, sig_key: &[u8]) -> Result<Vec<u8>> {
    sign(keys, CTX_LEAF, &leaf_msg(sig_key, &keys.nid()))
}

/// Whether `sig` by the key `pk` (which must be `nid`'s) binds `sig_key` to `nid`.
pub fn leaf_bound(nid: &Nid, pk: &[u8], sig: &[u8], sig_key: &[u8]) -> bool {
    pk_of(nid, pk).is_some_and(|pk| verifies(&pk, CTX_LEAF, &leaf_msg(sig_key, nid), sig))
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

fn cert_msg(rid: &Rid, nid: &Nid) -> Vec<u8> {
    [rid.as_slice(), nid.as_slice()].concat()
}

/// This admin's certificate for `nid` as a new admin: hex `signer nid || signature`.
pub fn sign_cert(keys: &Keys, rid: &Rid, nid: &Nid) -> Result<String> {
    let sig = sign(keys, CTX_ADMIN, &cert_msg(rid, nid))?;
    Ok(hex::encode([keys.nid().as_slice(), &sig].concat()))
}

/// Admins whose authority chains to `roots` (the root identity's delegates) through the
/// certificates in `ring`; looks up each signer's key once.
pub async fn admins<A: Arkiv>(a: &A, ring: &Keyring, roots: &[Nid]) -> Result<BTreeSet<Nid>> {
    let mut ok: BTreeSet<Nid> = roots.iter().copied().collect();
    let certs: BTreeMap<Nid, (Nid, Vec<u8>)> = ring
        .certs
        .iter()
        .filter_map(|(n, c)| {
            let n: Nid = hex::decode(n).ok()?.try_into().ok()?;
            let c = hex::decode(c).ok()?;
            let (signer, sig) = c.split_at_checked(32)?;
            Some((n, (signer.try_into().ok()?, sig.to_vec())))
        })
        .collect();
    let mut keys: BTreeMap<Nid, Option<PublicKey>> = BTreeMap::new();
    loop {
        let mut grew = false;
        for (n, (signer, sig)) in &certs {
            if ok.contains(n) || !ok.contains(signer) {
                continue;
            }
            if !keys.contains_key(signer) {
                keys.insert(*signer, charter_sync::cob::peer_key_of(a, signer).await?);
            }
            let pk = keys.get(signer).and_then(Option::as_ref);
            if pk.is_some_and(|pk| verifies(pk, CTX_ADMIN, &cert_msg(&ring.rid, n), sig)) {
                ok.insert(*n);
                grew = true;
            }
        }
        if !grew {
            return Ok(ok);
        }
    }
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
    fn a_leaf_binding_only_verifies_for_its_own_nid_and_key() {
        let keys = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([1; 32]));
        let other = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([2; 32]));
        let sig = sign_leaf(&keys, b"mls key").expect("sign");
        let pk = keys.pk().as_bytes();
        assert!(leaf_bound(&keys.nid(), pk, &sig, b"mls key"));
        assert!(!leaf_bound(&keys.nid(), pk, &sig, b"other key"));
        assert!(
            !leaf_bound(&other.nid(), pk, &sig, b"mls key"),
            "pk is not that nid's"
        );
        assert!(!leaf_bound(
            &other.nid(),
            other.pk().as_bytes(),
            &sig,
            b"mls key"
        ));
    }
}
