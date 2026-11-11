//! Opening an observed entity: attributes, envelope, body, content hash, signature.

use crate::attrs::{AttrValue, Attribute, sorted};
use crate::types::{Address, Hash32, Observed, Position};
use crate::{Body, Envelope, FormatError, VerifyError, content_hash, field};
use charter_crypto::PublicKey;

/// `ag.v`, the entity layout version.
pub const ENTITY_LAYOUT_VERSION: u64 = 1;

/// A decoded signed entity whose attributes match its envelope and body.
#[derive(Debug, Clone)]
pub struct Entity<B> {
    pub position: Position,
    pub creator: Address,
    pub expires_at: u64,
    pub envelope: Envelope,
    pub body: B,
    /// `H(kind_name, envelope bytes)`.
    pub h: Hash32,
    pub envelope_bytes: Vec<u8>,
}

/// Every attribute of a signed entity, sorted as Arkiv requires.
pub fn entity_attributes<B: Body>(env: &Envelope, env_bytes: &[u8], body: &B) -> Vec<Attribute> {
    let h = content_hash(B::KIND, env_bytes);
    let mut v = vec![
        Attribute::str("ag.kind", B::AG_KIND),
        Attribute::u64("ag.v", B::AG_V),
        Attribute::new("ag.body", AttrValue::Bytes(env_bytes.to_vec())),
    ];
    v.extend(body.extra_attributes(&env.signer, &h));
    sorted(v)
}

fn kind_and_bytes<B: Body>(obs: &Observed) -> Result<&[u8], FormatError> {
    match obs.attr("ag.kind") {
        Some(AttrValue::Str(k)) if k == B::AG_KIND => {}
        other => {
            return Err(FormatError::KindMismatch {
                expected: B::AG_KIND,
                found: format!("{other:?}"),
            });
        }
    }
    match obs.attr("ag.v") {
        Some(AttrValue::U64(v)) if *v == B::AG_V => {}
        Some(AttrValue::U64(v)) => {
            return Err(FormatError::Unsupported {
                what: "entity layout version",
                value: *v,
            });
        }
        _ => return Err(field("entity", "ag.v", "missing or not u64")),
    }
    match obs.attr("ag.body") {
        Some(AttrValue::Bytes(b)) => Ok(b),
        _ => Err(field("entity", "ag.body", "missing or not bytes")),
    }
}

/// Decodes and checks everything except the signature.
pub fn open<B: Body>(obs: &Observed) -> Result<Entity<B>, FormatError> {
    let bytes = kind_and_bytes::<B>(obs)?;
    let envelope = Envelope::decode(bytes)?;
    if envelope.kind != B::KIND {
        return Err(FormatError::KindMismatch {
            expected: B::KIND.name(),
            found: envelope.kind.name().to_owned(),
        });
    }
    let body = B::decode(&envelope.body)?;
    if sorted(obs.attributes.clone()) != entity_attributes(&envelope, bytes, &body) {
        return Err(FormatError::AttributeMismatch);
    }
    Ok(Entity {
        position: obs.position(),
        creator: obs.creator,
        expires_at: obs.expires_at,
        h: content_hash(B::KIND, bytes),
        envelope_bytes: bytes.to_vec(),
        envelope,
        body,
    })
}

/// [`open`] plus the signature check under `pk`.
pub fn verify_entity<B: Body>(obs: &Observed, pk: &PublicKey) -> Result<Entity<B>, VerifyError> {
    let e = open::<B>(obs)?;
    e.envelope.verify(pk)?;
    Ok(e)
}
