//! Composite ML-DSA-65 + Ed25519 signatures, `id-MLDSA65-Ed25519-SHA512`, as specified by
//! RFC 10073 (published from draft-ietf-lamps-pq-composite-sigs-19), section 4:
//!
//! ```text
//! M'        = Prefix || Label || len(ctx) || ctx || SHA-512(M)
//! mldsaSig  = ML-DSA-65.Sign(mldsaSK, M', ctx = Label)      (hedged / randomized)
//! tradSig   = Ed25519.Sign(tradSK, M')
//! signature = mldsaSig || tradSig;  pk = mldsaPK || tradPK;  sk = mldsaSeed || ed25519 seed
//! ```
//!
//! Valid only if both halves verify (Ed25519 with strict verification).
//!
//! Reuse survey (2026-11-06): crates.io and GitHub code search for
//! `CompositeAlgorithmSignatures2025` found about 30 Rust implementations. The strongest,
//! `darkbio-crypto` 0.20.1 (`xdsa`, BSD-3, single company), hard-codes an empty composite
//! `ctx`, so it cannot carry our per-object context, and pulls DER/PEM/serde surface we do
//! not need; the rest are application-internal modules or older draft revisions (e.g.
//! QUBIP/aurora at draft-13). The construction is about 60 lines over `ml-dsa` and
//! `ed25519-dalek`, so we own it and pin it with the RFC's published vectors.

use crate::{Alg, CryptoError, PK_LEN, SECRET_LEN, SIG_LEN};
use ed25519_dalek::Signer as _;
use ml_dsa::{EncodedSignature, EncodedVerifyingKey, MlDsa65};
use sha2::{Digest, Sha512};
use zeroize::{ZeroizeOnDrop, Zeroizing};

const PREFIX: &[u8; 32] = b"CompositeAlgorithmSignatures2025";
const LABEL: &[u8] = b"COMPSIG-MLDSA65-Ed25519-SHA512";
const ML_PK_LEN: usize = 1952;
const ML_SIG_LEN: usize = 3309;

fn message_representative(msg: &[u8], ctx: &[u8]) -> Result<Vec<u8>, CryptoError> {
    let ctx_len = u8::try_from(ctx.len()).map_err(|_| CryptoError::ContextTooLong)?;
    let mut m = Vec::with_capacity(PREFIX.len() + LABEL.len() + 1 + ctx.len() + 64);
    m.extend_from_slice(PREFIX);
    m.extend_from_slice(LABEL);
    m.push(ctx_len);
    m.extend_from_slice(ctx);
    m.extend_from_slice(&Sha512::digest(msg));
    Ok(m)
}

/// A composite public key.
#[derive(Clone)]
pub struct PublicKey {
    bytes: Box<[u8]>,
    ml: ml_dsa::VerifyingKey<MlDsa65>,
    ed: ed25519_dalek::VerifyingKey,
}

impl PublicKey {
    pub fn from_bytes(alg: Alg, bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != alg.pk_len() {
            return Err(CryptoError::BadLength);
        }
        let (ml, ed) = bytes.split_at(ML_PK_LEN);
        let ml_enc =
            EncodedVerifyingKey::<MlDsa65>::try_from(ml).map_err(|_| CryptoError::BadLength)?;
        let ed: &[u8; 32] = ed.try_into().map_err(|_| CryptoError::BadLength)?;
        let ed = ed25519_dalek::VerifyingKey::from_bytes(ed).map_err(|_| CryptoError::BadKey)?;
        Ok(Self {
            bytes: bytes.into(),
            ml: ml_dsa::VerifyingKey::decode(&ml_enc),
            ed,
        })
    }

    pub fn alg(&self) -> Alg {
        Alg::MlDsa65Ed25519
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `nid = H("nid", u16be(alg) || pk)` (design section 2.1).
    pub fn nid(&self) -> crate::Hash32 {
        let mut x = Vec::with_capacity(2 + self.bytes.len());
        x.extend_from_slice(&self.alg().id().to_be_bytes());
        x.extend_from_slice(&self.bytes);
        crate::h("nid", &x)
    }

    /// Verifies `sig` over `msg` with application context `ctx`; both halves must verify.
    pub fn verify(&self, msg: &[u8], ctx: &[u8], sig: &Sig) -> Result<(), CryptoError> {
        let m = message_representative(msg, ctx).map_err(|_| CryptoError::BadSignature)?;
        let (ml_sig, ed_sig) = sig.bytes.split_at(ML_SIG_LEN);
        let ml_enc =
            EncodedSignature::<MlDsa65>::try_from(ml_sig).map_err(|_| CryptoError::BadSignature)?;
        let ml_sig =
            ml_dsa::Signature::<MlDsa65>::decode(&ml_enc).ok_or(CryptoError::BadSignature)?;
        let ed_sig: &[u8; 64] = ed_sig.try_into().map_err(|_| CryptoError::BadSignature)?;
        let ed_sig = ed25519_dalek::Signature::from_bytes(ed_sig);
        let ml_ok = self.ml.verify_with_context(&m, LABEL, &ml_sig);
        let ed_ok = self.ed.verify_strict(&m, &ed_sig).is_ok();
        if ml_ok && ed_ok {
            Ok(())
        } else {
            Err(CryptoError::BadSignature)
        }
    }
}

impl PartialEq for PublicKey {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}
impl Eq for PublicKey {}

impl core::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "PublicKey(nid={})", hex_prefix(&self.nid()))
    }
}

/// A composite signature, `SIG_LEN` bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct Sig {
    bytes: Box<[u8]>,
}

impl Sig {
    pub fn from_bytes(alg: Alg, bytes: &[u8]) -> Result<Self, CryptoError> {
        if bytes.len() != alg.sig_len() {
            return Err(CryptoError::BadLength);
        }
        Ok(Self {
            bytes: bytes.into(),
        })
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl core::fmt::Debug for Sig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Sig({} B)", self.bytes.len())
    }
}

/// A composite secret key. Both component keys zeroize on drop.
#[derive(Clone)]
pub struct SecretKey {
    ml_seed: Zeroizing<[u8; 32]>,
    ml: ml_dsa::ExpandedSigningKey<MlDsa65>,
    ed: ed25519_dalek::SigningKey,
}

impl ZeroizeOnDrop for SecretKey {}

impl SecretKey {
    /// From the RFC 10073 private key encoding `mldsaSeed (32) || ed25519 seed (32)`.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        let bytes: &[u8; SECRET_LEN] = bytes.try_into().map_err(|_| CryptoError::BadLength)?;
        let (ml, ed) = bytes.split_at(32);
        let ml: [u8; 32] = ml.try_into().map_err(|_| CryptoError::BadLength)?;
        let ml = Zeroizing::new(ml);
        let ed: &[u8; 32] = ed.try_into().map_err(|_| CryptoError::BadLength)?;
        Ok(Self {
            ml: ml_dsa::ExpandedSigningKey::from_seed(&(*ml).into()),
            ml_seed: ml,
            ed: ed25519_dalek::SigningKey::from_bytes(ed),
        })
    }

    pub fn to_bytes(&self) -> Zeroizing<[u8; SECRET_LEN]> {
        let mut out = Zeroizing::new([0u8; SECRET_LEN]);
        out[..32].copy_from_slice(&*self.ml_seed);
        out[32..].copy_from_slice(&self.ed.to_bytes());
        out
    }

    pub fn public_key(&self) -> PublicKey {
        let ml = self.ml.verifying_key();
        let mut bytes = Vec::with_capacity(PK_LEN);
        bytes.extend_from_slice(&ml.encode());
        bytes.extend_from_slice(self.ed.verifying_key().as_bytes());
        PublicKey {
            bytes: bytes.into(),
            ml,
            ed: self.ed.verifying_key(),
        }
    }

    /// Hedged composite signature over `msg` with application context `ctx` (<= 255 B).
    pub fn sign(&self, msg: &[u8], ctx: &[u8]) -> Result<Sig, CryptoError> {
        let m = message_representative(msg, ctx)?;
        let ml_sig = self
            .ml
            .sign_randomized(&m, LABEL, &mut getrandom::SysRng)
            .map_err(|_| CryptoError::SignFailed)?;
        let ed_sig = self.ed.sign(&m);
        let mut bytes = Vec::with_capacity(SIG_LEN);
        bytes.extend_from_slice(&ml_sig.encode());
        bytes.extend_from_slice(&ed_sig.to_bytes());
        Ok(Sig {
            bytes: bytes.into(),
        })
    }
}

impl core::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("SecretKey(..)")
    }
}

fn hex_prefix(b: &[u8]) -> String {
    b.iter().take(6).map(|x| format!("{x:02x}")).collect()
}

/// A plain Ed25519 check (RFC 8032, strict), for keys outside Charter's composite identity
/// such as a git SSH signing key; `false` for a malformed key.
pub fn ed25519_verify(pk: &[u8; 32], msg: &[u8], sig: &[u8; 64]) -> bool {
    ed25519_dalek::VerifyingKey::from_bytes(pk).is_ok_and(|k| {
        k.verify_strict(msg, &ed25519_dalek::Signature::from_bytes(sig))
            .is_ok()
    })
}
