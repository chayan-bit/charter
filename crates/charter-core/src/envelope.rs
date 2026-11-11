//! Signed envelope (design section 2.1) and size caps.

use crate::types::{Hash32, Nid};
use crate::{FormatError, Kind, VerifyError, field};
use charter_crypto::{PublicKey, SecretKey, Sig, h};
use minicbor::{Decoder, Encoder};

/// Maximum `ag.body` value, envelope included.
pub const AG_BODY_MAX: usize = 65_536;
/// Envelope framing besides body and signature: array header, v, kind, enc,
/// `bstr .size 32` signer, and 3-byte headers for body and sig.
pub const ENVELOPE_OVERHEAD: usize = 44;

const ENVELOPE_VERSION: u64 = 1;
const ENC_PLAINTEXT: u64 = 0;
const SIG_PREFIX: &[u8] = b"arkiv-git/v1/sig"; // protocol constant: unchanged by the Charter rename

/// Largest body that fits in one entity for a given signature length.
pub const fn max_body_len(sig_len: usize) -> usize {
    AG_BODY_MAX - sig_len - ENVELOPE_OVERHEAD
}

/// `[v = 1, kind, enc = 0, signer, body, sig]`. `v` and `enc` are implicit: any other
/// value fails decoding with [`FormatError::Unsupported`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub kind: Kind,
    pub signer: Nid,
    pub body: Vec<u8>,
    pub sig: Vec<u8>,
}

/// `"arkiv-git/v1/sig" || 0x00 || u8(kind) || u8(enc) || signer || body`.
pub fn sig_msg(kind: Kind, signer: &Nid, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(SIG_PREFIX.len() + 3 + 32 + body.len());
    m.extend_from_slice(SIG_PREFIX);
    m.extend_from_slice(&[0, kind.code(), ENC_PLAINTEXT as u8]);
    m.extend_from_slice(signer);
    m.extend_from_slice(body);
    m
}

/// Content hash of a signed entity: `H(kind_name, envelope_bytes)`.
pub fn content_hash(kind: Kind, envelope_bytes: &[u8]) -> Hash32 {
    h(kind.name(), envelope_bytes)
}

impl Envelope {
    /// Signs `body` (canonical CBOR of the kind's body) as `kind`.
    pub fn sign(sk: &SecretKey, kind: Kind, body: Vec<u8>) -> Result<Self, FormatError> {
        let pk = sk.public_key();
        let max = max_body_len(pk.alg().sig_len());
        if body.len() > max {
            let len = body.len();
            return Err(match kind {
                Kind::Refs | Kind::Head | Kind::HeadV3 => FormatError::RefsTooLarge { len, max },
                _ => FormatError::BodyTooLarge { len, max },
            });
        }
        if kind.unsigned() {
            return Err(field(kind.name(), "kind", "this kind carries no envelope"));
        }
        let signer = pk.nid();
        let sig = sk.sign(&sig_msg(kind, &signer, &body), kind.sig_ctx())?;
        Ok(Self {
            kind,
            signer,
            body,
            sig: sig.as_bytes().to_vec(),
        })
    }

    pub fn encode(&self) -> Vec<u8> {
        self.try_encode()
            .unwrap_or_else(|_| unreachable!("encoding into a Vec cannot fail"))
    }

    fn try_encode(&self) -> Result<Vec<u8>, minicbor::encode::Error<core::convert::Infallible>> {
        let mut e = Encoder::new(Vec::with_capacity(self.body.len() + self.sig.len() + 48));
        e.array(6)?
            .u64(ENVELOPE_VERSION)?
            .u8(self.kind.code())?
            .u64(ENC_PLAINTEXT)?
            .bytes(&self.signer)?
            .bytes(&self.body)?
            .bytes(&self.sig)?;
        Ok(e.into_writer())
    }

    /// Decodes canonical envelope bytes. Unknown `v` or `enc` gives `Unsupported`.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        if bytes.len() > AG_BODY_MAX {
            return Err(FormatError::BodyTooLarge {
                len: bytes.len(),
                max: AG_BODY_MAX,
            });
        }
        let env = decode_fields(&mut Decoder::new(bytes)).map_err(|e| match e {
            Decoded::Format(f) => f,
            Decoded::Cbor(c) => FormatError::Cbor(c.to_string()),
        })?;
        if env.encode() != bytes {
            return Err(FormatError::NonCanonical);
        }
        Ok(env)
    }

    /// Checks `signer == nid(pk)` and the composite signature over [`sig_msg`].
    pub fn verify(&self, pk: &PublicKey) -> Result<(), VerifyError> {
        if pk.nid() != self.signer {
            return Err(VerifyError::SignerMismatch);
        }
        let sig = Sig::from_bytes(pk.alg(), &self.sig)?;
        pk.verify(
            &sig_msg(self.kind, &self.signer, &self.body),
            self.kind.sig_ctx(),
            &sig,
        )?;
        Ok(())
    }
}

enum Decoded {
    Format(FormatError),
    Cbor(minicbor::decode::Error),
}

impl From<minicbor::decode::Error> for Decoded {
    fn from(e: minicbor::decode::Error) -> Self {
        Decoded::Cbor(e)
    }
}

fn decode_fields(d: &mut Decoder<'_>) -> Result<Envelope, Decoded> {
    let len = d.array()?;
    let v = d.u64()?;
    if v != ENVELOPE_VERSION {
        return Err(Decoded::Format(FormatError::Unsupported {
            what: "envelope version",
            value: v,
        }));
    }
    let kind_code = d.u64()?;
    let enc = d.u64()?;
    if enc != ENC_PLAINTEXT {
        return Err(Decoded::Format(FormatError::Unsupported {
            what: "envelope enc",
            value: enc,
        }));
    }
    if len != Some(6) {
        return Err(Decoded::Format(field(
            "envelope",
            "array",
            "must have 6 items",
        )));
    }
    let kind = Kind::from_code(kind_code)
        .filter(|k| !k.unsigned())
        .ok_or(Decoded::Format(field(
            "envelope",
            "kind",
            "unknown kind code",
        )))?;
    let signer: Nid = d
        .bytes()?
        .try_into()
        .map_err(|_| Decoded::Format(field("envelope", "signer", "must be 32 bytes")))?;
    let body = d.bytes()?.to_vec();
    let sig = d.bytes()?.to_vec();
    if d.position() != d.input().len() {
        return Err(Decoded::Format(FormatError::TrailingBytes));
    }
    Ok(Envelope {
        kind,
        signer,
        body,
        sig,
    })
}
