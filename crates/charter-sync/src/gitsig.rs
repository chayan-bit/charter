//! Git commit signatures (issue #61, docs/p3/cob-ops.md "Signing keys"): SSH signatures
//! (OpenSSH PROTOCOL.sshsig, Ed25519 keys) are verified in process, with no `ssh-keygen` or
//! `gpg` on the read path, and a signing-key claim op names the Charter identity of a key.
//!
//! A claim is an `xyz.arkiv.signing-key` create op whose payload is `{ key, sig }`: `key` an
//! `ssh-ed25519` public key line, `sig` an armored SSH signature by that key over
//! [`claim_message`] of the op's author under the namespace [`CLAIM_NAMESPACE`]. The op's own
//! composite signature says who claims; `sig` proves the claimer holds the key, so nobody can
//! claim someone else's key. The first valid claim of a key in Arkiv order wins; a `revoke`
//! op by the claimer on that object drops it.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use charter_core::{Nid, VerifiedOp};
use sha2::{Digest, Sha256, Sha512};

use crate::cob::json::to_json;
use crate::ids::hex32;

/// COB type of signing-key claims.
pub const CLAIM_TYPE: &str = "xyz.arkiv.signing-key";
/// SSH signature namespace of a claim's proof of possession.
pub const CLAIM_NAMESPACE: &str = "charter-signing-key";
/// Namespace git signs commits under (`gpg.format = ssh`).
const GIT_NAMESPACE: &[u8] = b"git";
const MAGIC: &[u8] = b"SSHSIG";
const ED25519: &[u8] = b"ssh-ed25519";
/// Armored signatures past this are refused before decoding (an Ed25519 one is about 300 B).
const MAX_ARMOR: usize = 4096;
/// Key lines past this are refused (an Ed25519 key line is about 100 B plus its comment).
const MAX_KEY_LINE: usize = 1024;

/// What a claim's SSH signature signs: the claimer's peer id.
pub fn claim_message(nid: &Nid) -> String {
    format!("charter signing key for {}\n", hex32(nid))
}

/// An Ed25519 SSH public key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SshKey(pub [u8; 32]);

impl SshKey {
    /// `ssh-ed25519 <base64> [comment]`; any other key type is `None`.
    pub fn parse(line: &str) -> Option<Self> {
        if line.len() > MAX_KEY_LINE {
            return None;
        }
        let mut it = line.split_whitespace();
        (it.next()? == "ssh-ed25519").then_some(())?;
        Self::from_blob(&STANDARD.decode(it.next()?).ok()?)
    }

    fn from_blob(blob: &[u8]) -> Option<Self> {
        let mut r = Reader(blob);
        (r.string()? == ED25519).then_some(())?;
        let k = r.string()?.try_into().ok()?;
        r.0.is_empty().then_some(Self(k))
    }

    fn blob(&self) -> Vec<u8> {
        let mut b = vec![];
        put(&mut b, ED25519);
        put(&mut b, &self.0);
        b
    }

    /// `SHA256:<base64>`, as `ssh-keygen -l` prints it.
    pub fn fingerprint(&self) -> String {
        format!(
            "SHA256:{}",
            STANDARD_NO_PAD.encode(Sha256::digest(self.blob()))
        )
    }
}

fn put(b: &mut Vec<u8>, s: &[u8]) {
    b.extend_from_slice(&u32::try_from(s.len()).unwrap_or(u32::MAX).to_be_bytes());
    b.extend_from_slice(s);
}

/// SSH wire format: `uint32` and length-prefixed `string`s.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        (self.0.len() >= n).then(|| {
            let (a, b) = self.0.split_at(n);
            self.0 = b;
            a
        })
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.u32()?).ok()?;
        self.take(n)
    }
}

/// Why an SSH signature was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SigError {
    /// Not an SSH signature Charter checks (another key type, or not SSHSIG at all).
    Unsupported,
    /// Malformed, another namespace, or the signature does not verify.
    Bad,
}

/// Checks an armored SSH signature over `msg` under `namespace`; returns its key.
pub fn verify_sshsig(armored: &str, namespace: &[u8], msg: &[u8]) -> Result<SshKey, SigError> {
    let armor = armored
        .trim()
        .strip_prefix("-----BEGIN SSH SIGNATURE-----")
        .and_then(|s| s.strip_suffix("-----END SSH SIGNATURE-----"))
        .ok_or(SigError::Unsupported)?;
    // After the armor check, so a large OpenPGP or X.509 signature is "not checked", not bad.
    if armored.len() > MAX_ARMOR {
        return Err(SigError::Bad);
    }
    let body: String = armor.split_whitespace().collect();
    let raw = STANDARD.decode(body).map_err(|_| SigError::Bad)?;
    let mut r = Reader(&raw);
    let bad = SigError::Bad;
    (r.take(6) == Some(MAGIC) && r.u32() == Some(1))
        .then_some(())
        .ok_or(bad)?;
    let (pk, ns, reserved, hash, sig) = (
        r.string().ok_or(bad)?,
        r.string().ok_or(bad)?,
        r.string().ok_or(bad)?,
        r.string().ok_or(bad)?,
        r.string().ok_or(bad)?,
    );
    if !r.0.is_empty() || ns != namespace {
        return Err(bad);
    }
    let key = SshKey::from_blob(pk).ok_or(SigError::Unsupported)?;
    let digest = match hash {
        b"sha512" => Sha512::digest(msg).to_vec(),
        b"sha256" => Sha256::digest(msg).to_vec(),
        _ => return Err(bad),
    };
    let mut sr = Reader(sig);
    (sr.string() == Some(ED25519)).then_some(()).ok_or(bad)?;
    let sig: &[u8; 64] = sr.string().ok_or(bad)?.try_into().map_err(|_| bad)?;
    let mut signed = MAGIC.to_vec();
    for s in [ns, reserved, hash, &digest] {
        put(&mut signed, s);
    }
    match charter_crypto::ed25519_verify(&key.0, &signed, sig) {
        true => Ok(key),
        false => Err(bad),
    }
}

/// A raw commit object split into what git signs (the object without its `gpgsig` and
/// `gpgsig-sha256` headers, as git strips both) and the text of the first signature;
/// `None` for an unsigned commit.
pub fn split_commit(raw: &[u8]) -> Option<(Vec<u8>, String)> {
    let head_end = raw
        .windows(2)
        .position(|w| w == b"\n\n")
        .map_or(raw.len(), |i| i + 1);
    let (head, rest) = raw.split_at(head_end);
    let mut payload = Vec::with_capacity(raw.len());
    // `in_sig`: inside a signature header (dropped); `keep`: it is the one returned.
    let (mut sig, mut in_sig, mut keep) = (None::<Vec<u8>>, false, false);
    for line in head.split_inclusive(|b| *b == b'\n') {
        let value = line
            .strip_prefix(b"gpgsig ")
            .or_else(|| line.strip_prefix(b"gpgsig-sha256 "));
        if let Some(v) = value {
            (in_sig, keep) = (true, sig.is_none());
            if keep {
                sig = Some(v.to_vec());
            }
        } else if in_sig && line.first() == Some(&b' ') {
            if let Some(s) = sig.as_mut().filter(|_| keep) {
                s.extend_from_slice(&line[1..]);
            }
        } else {
            in_sig = false;
            payload.extend_from_slice(line);
        }
    }
    payload.extend_from_slice(rest);
    let sig = String::from_utf8(sig?).ok()?;
    Some((payload, sig))
}

/// Verified signing-key claims of a repo: key to claimer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Claims(BTreeMap<SshKey, Nid>);

impl Claims {
    pub fn signer(&self, key: &SshKey) -> Option<Nid> {
        self.0.get(key).copied()
    }

    /// Keys `nid` claimed, as fingerprints.
    pub fn of(&self, nid: &Nid) -> Vec<String> {
        self.0
            .iter()
            .filter(|(_, n)| *n == nid)
            .map(|(k, _)| k.fingerprint())
            .collect()
    }
}

/// The key a claim op proves, or `None` if its payload or proof is invalid.
fn claimed(o: &VerifiedOp) -> Option<SshKey> {
    let p = to_json(&o.body.payload).ok()?;
    let key = SshKey::parse(p["key"].as_str()?)?;
    let msg = claim_message(&o.author);
    let signed = verify_sshsig(
        p["sig"].as_str()?,
        CLAIM_NAMESPACE.as_bytes(),
        msg.as_bytes(),
    );
    (signed.ok()? == key).then_some(key)
}

/// Folds the claims of `ops` (verified, in Arkiv order).
pub fn claims(ops: &[VerifiedOp]) -> Claims {
    let mut by_obj: BTreeMap<[u8; 32], (SshKey, Nid)> = BTreeMap::new();
    let mut out = Claims::default();
    for o in ops.iter().filter(|o| o.body.type_name == CLAIM_TYPE) {
        if o.body.is_create() {
            if let Some(k) = claimed(o).filter(|k| !out.0.contains_key(k)) {
                out.0.insert(k, o.author);
                by_obj.insert(o.id, (k, o.author));
            }
        } else if o.body.action == "revoke"
            && by_obj
                .get(&o.object())
                .is_some_and(|(_, by)| *by == o.author)
            && let Some((k, _)) = by_obj.remove(&o.object())
        {
            // Removed from `by_obj` too, so a stale revoke of it can never drop a later claim.
            out.0.remove(&k);
        }
    }
    out
}

/// The signature state of one commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitSig {
    Unsigned,
    /// Signed in a way Charter does not check: `OpenPGP`, `X.509`, or an SSH key type
    /// other than Ed25519.
    Unchecked(&'static str),
    /// An SSH signature that does not verify.
    Bad,
    /// A valid SSH signature; `signer` is who claimed the key in this repo, if anyone.
    Good {
        fingerprint: String,
        signer: Option<Nid>,
    },
}

/// Checks the signature of the raw commit object `raw` against `claims`.
pub fn commit_sig(raw: &[u8], claims: &Claims) -> CommitSig {
    let Some((payload, sig)) = split_commit(raw) else {
        return CommitSig::Unsigned;
    };
    match verify_sshsig(&sig, GIT_NAMESPACE, &payload) {
        Ok(k) => CommitSig::Good {
            fingerprint: k.fingerprint(),
            signer: claims.signer(&k),
        },
        Err(SigError::Bad) => CommitSig::Bad,
        Err(SigError::Unsupported) if sig.contains("BEGIN PGP") => CommitSig::Unchecked("OpenPGP"),
        Err(SigError::Unsupported) if sig.contains("BEGIN SSH") => {
            CommitSig::Unchecked("SSH (not Ed25519)")
        }
        Err(SigError::Unsupported) => CommitSig::Unchecked("X.509"),
    }
}

#[cfg(test)]
mod tests;
