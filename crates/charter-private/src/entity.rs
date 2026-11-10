//! Sealing at the Arkiv wire boundary. A create op carrying `ag.rid = rid` loses its
//! leaking attributes (`ag.body`, `ag.rid`, `ag.name`, `ag.lang`, `ag.lic`) into one sealed
//! `ag.body`, gains `ag.rid = HMAC(rid)`, `ag.name = HMAC(name)` and a plaintext `ag.ep`;
//! reading reverses it, so core verification sees the original entity unchanged.

use arkiv_bindings::{Attribute, Create, Ident32, OP_CREATE, OP_PATCH, Operation, Patch};
use arkiv_rpc_types::{AttributeEntry, EntityData};
use charter_core::{ErrorCode, Hash32};
use charter_store::{AttributeValue, Field, Pred, Value};
use charter_sync::{Result, SyncError};

use crate::keyring::Keyring;
use crate::seal::{aad, blind, open, seal};

/// Plaintext epoch attribute: readers pick the key by it.
pub const EPOCH: &str = "ag.ep";
/// Always sealed when present; any other `bytes32` attribute equal to the rid is too
/// (the root identity's `ag.h`, revision 1's `ag.parent`).
pub const HIDDEN: [&str; 5] = ["ag.body", "ag.rid", "ag.name", "ag.lang", "ag.lic"];

fn word(name: &str) -> Result<charter_store::B256> {
    Ident32::encode(name)
        .map(Ident32::into_word)
        .map_err(|e| internal(&format!("{name}: {e}")))
}

/// The attribute name an `Ident32` word encodes (left-aligned ASCII, zero padded).
fn name_of(w: &charter_store::B256) -> Result<String> {
    let n = w.iter().position(|b| *b == 0).unwrap_or(32);
    String::from_utf8(w[..n].to_vec()).map_err(|_| internal("attribute name"))
}

fn internal(msg: &str) -> SyncError {
    SyncError::coded(ErrorCode::Internal, msg.to_owned())
}

fn attr(name: &str, v: AttributeValue, indexed: bool) -> Result<Attribute> {
    Attribute::from_value(
        Ident32::encode(name).map_err(|e| internal(&e.to_string()))?,
        &v,
        indexed,
    )
    .map_err(|e| internal(&e.to_string()))
}

/// `[u8 name len][name][u8 tag][u32be len][value]` per hidden attribute.
fn encode_hidden(items: &[(String, AttributeValue)]) -> Result<Vec<u8>> {
    let mut out = vec![];
    for (name, v) in items {
        let (tag, bytes) = match v {
            AttributeValue::Str(s) => (0u8, s.as_bytes().to_vec()),
            AttributeValue::U64(n) => (1, n.to_be_bytes().to_vec()),
            AttributeValue::Bytes(b) => (2, b.clone()),
            AttributeValue::Bytes32(b) => (3, b.to_vec()),
            other => return Err(internal(&format!("{name}: cannot seal {other:?}"))),
        };
        out.push(u8::try_from(name.len()).map_err(|_| internal("name"))?);
        out.extend_from_slice(name.as_bytes());
        out.push(tag);
        out.extend_from_slice(
            &u32::try_from(bytes.len())
                .map_err(|_| internal("len"))?
                .to_be_bytes(),
        );
        out.extend_from_slice(&bytes);
    }
    Ok(out)
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Option<&'a [u8]> {
    let (head, rest) = b.split_at_checked(n)?;
    *b = rest;
    Some(head)
}

/// Inverse of [`encode_hidden`], straight to the JSON row form the read path parses.
fn decode_hidden(mut b: &[u8]) -> Option<Vec<AttributeEntry>> {
    let mut out = vec![];
    while !b.is_empty() {
        let n = usize::from(*take(&mut b, 1)?.first()?);
        let name = std::str::from_utf8(take(&mut b, n)?).ok()?.to_owned();
        let tag = *take(&mut b, 1)?.first()?;
        let len = u32::from_be_bytes(take(&mut b, 4)?.try_into().ok()?);
        let v = take(&mut b, usize::try_from(len).ok()?)?;
        let (ty, value) = match tag {
            0 => ("str", std::str::from_utf8(v).ok()?.to_owned()),
            1 => (
                "u64",
                format!("0x{:x}", u64::from_be_bytes(v.try_into().ok()?)),
            ),
            2 => ("bytes", format!("0x{}", hex::encode(v))),
            3 => ("bytes32", format!("0x{}", hex::encode(v))),
            _ => return None,
        };
        out.push(AttributeEntry {
            name,
            value_type: ty.into(),
            value: value.into(),
        });
    }
    Some(out)
}

/// Seals one create or patch op of `ring`'s repo; every other op, and an op already sealed,
/// is returned unchanged (sealing is idempotent, as `Arkiv::seal_ops` requires). A v2 head
/// patch carries every attribute, so it seals exactly like a create (head-model 9).
pub fn seal_op(ring: &Keyring, op: Operation) -> Result<Operation> {
    match op.operation {
        OP_CREATE => {
            let c: Create = op.payload_of().map_err(|e| internal(&e.to_string()))?;
            Ok(match seal_attrs(ring, &c.attributes)? {
                Some(attrs) => {
                    Operation::create(c.salt, c.expiresAt, c.minLifetime, c.creationFlags, attrs)
                }
                None => op,
            })
        }
        OP_PATCH => {
            let p: Patch = op.payload_of().map_err(|e| internal(&e.to_string()))?;
            Ok(match seal_attrs(ring, &p.mutations)? {
                Some(attrs) => Operation::patch(p.entityKey, attrs),
                None => op,
            })
        }
        _ => Ok(op),
    }
}

/// The sealed attribute list, or `None` when the attributes are not an unsealed entity
/// of `ring`'s repo.
fn seal_attrs(ring: &Keyring, given: &[Attribute]) -> Result<Option<Vec<Attribute>>> {
    let find = |n: &str| -> Result<Option<AttributeValue>> {
        let w = word(n)?;
        match given.iter().find(|a| a.name == w) {
            Some(a) => a.to_value().map_err(|e| internal(&e.to_string())),
            None => Ok(None),
        }
    };
    let (Some(AttributeValue::Bytes32(rid)), None) = (find("ag.rid")?, find(EPOCH)?) else {
        return Ok(None);
    };
    let Some(AttributeValue::Str(kind)) = find("ag.kind")? else {
        return Ok(None);
    };
    if rid != ring.rid {
        return Ok(None);
    }
    let hidden_words = HIDDEN.map(word).into_iter().collect::<Result<Vec<_>>>()?;
    let mut hidden: Vec<(String, AttributeValue)> = vec![];
    let mut attrs: Vec<Attribute> = vec![];
    for a in given {
        let v = a.to_value().map_err(|e| internal(&e.to_string()))?;
        let name = HIDDEN
            .iter()
            .zip(&hidden_words)
            .find(|(_, w)| **w == a.name);
        match (name, v) {
            (Some((n, _)), Some(v)) => hidden.push(((*n).to_owned(), v)),
            (None, Some(AttributeValue::Bytes32(b))) if b == rid => {
                let n = name_of(&a.name)?;
                attrs.push(attr(
                    &n,
                    AttributeValue::Bytes32(blind(&ring.id_key, "rid", &rid)),
                    true,
                )?);
                hidden.push((n, AttributeValue::Bytes32(b)));
            }
            _ => attrs.push(a.clone()),
        }
    }
    let (epoch, key) = ring.current()?;
    let sealed = seal(key, &aad(&rid, &kind, epoch), &encode_hidden(&hidden)?)?;
    attrs.push(attr("ag.body", AttributeValue::Bytes(sealed), false)?);
    attrs.push(attr(
        "ag.rid",
        AttributeValue::Bytes32(blind(&ring.id_key, "rid", &rid)),
        true,
    )?);
    attrs.push(attr(EPOCH, AttributeValue::U64(epoch), true)?);
    if let Some((_, AttributeValue::Str(name))) = hidden.iter().find(|(n, _)| n == "ag.name") {
        let b = blind(&ring.id_key, "name", name.as_bytes());
        attrs.push(attr("ag.name", AttributeValue::Bytes32(b), true)?);
    }
    Ok(Some(attrs))
}

fn entry<'a>(row: &'a EntityData, name: &str) -> Option<&'a str> {
    row.attributes
        .as_ref()?
        .iter()
        .find(|a| a.name == name)?
        .value
        .as_str()
}

fn hex_of(s: &str) -> Option<Vec<u8>> {
    hex::decode(s.strip_prefix("0x")?).ok()
}

/// Opens a sealed row of `ring`'s repo. Rows without `ag.ep`, or whose epoch key this
/// member lacks, come back unchanged: core then rejects them as malformed, like any junk.
pub fn open_row(ring: &Keyring, row: EntityData) -> EntityData {
    try_open(ring, &row).unwrap_or(row)
}

fn try_open(ring: &Keyring, row: &EntityData) -> Option<EntityData> {
    let epoch = u64::from_str_radix(entry(row, EPOCH)?.strip_prefix("0x")?, 16).ok()?;
    let blinded: Hash32 = hex_of(entry(row, "ag.rid")?)?.try_into().ok()?;
    if blinded != blind(&ring.id_key, "rid", &ring.rid) {
        return None;
    }
    let kind = entry(row, "ag.kind")?;
    let body = hex_of(entry(row, "ag.body")?)?;
    let plain = open(
        ring.epochs.get(&epoch)?,
        &aad(&ring.rid, kind, epoch),
        &body,
    )?;
    let hidden = decode_hidden(&plain)?;
    let mut attrs: Vec<AttributeEntry> = row
        .attributes
        .as_ref()?
        .iter()
        .filter(|a| {
            a.name != EPOCH && a.name != "ag.name" && !hidden.iter().any(|h| h.name == a.name)
        })
        .cloned()
        .collect();
    attrs.extend(hidden);
    Some(EntityData {
        attributes: Some(attrs),
        ..row.clone()
    })
}

/// Rewrites `ag.rid = rid` and `ag.name = n` predicates to their blinded values.
pub fn blind_pred(ring: &Keyring, p: &Pred) -> Pred {
    let is = |f: &Field, n: &str| matches!(f, Field::Attr(a) if a == n);
    match p {
        Pred::Eq(f @ Field::Attr(_), Value::Bytes32(v)) if v.0 == ring.rid => Pred::Eq(
            f.clone(),
            Value::Bytes32(blind(&ring.id_key, "rid", &ring.rid).into()),
        ),
        Pred::Eq(f, Value::Str(n)) if is(f, "ag.name") => Pred::Eq(
            f.clone(),
            Value::Bytes32(blind(&ring.id_key, "name", n.as_bytes()).into()),
        ),
        Pred::And(v) => Pred::And(v.iter().map(|q| blind_pred(ring, q)).collect()),
        Pred::Or(v) => Pred::Or(v.iter().map(|q| blind_pred(ring, q)).collect()),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_attributes_round_trip() {
        let items = [
            ("ag.rid", AttributeValue::Bytes32([1; 32])),
            ("ag.name", AttributeValue::Str("demo".into())),
            ("ag.body", AttributeValue::Bytes(vec![1, 2, 3])),
            ("ag.lic", AttributeValue::Str("MIT".into())),
        ]
        .map(|(n, v)| (n.to_owned(), v));
        let back = decode_hidden(&encode_hidden(&items).expect("enc")).expect("dec");
        assert_eq!(back.len(), 4);
        assert_eq!(back[1].value, serde_json::Value::from("demo"));
        assert_eq!(back[2].value, serde_json::Value::from("0x010203"));
        assert!(decode_hidden(&[3, b'a']).is_none());
    }
}
