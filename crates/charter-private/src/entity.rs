//! Sealing at the Arkiv wire boundary. A create op carrying `ag.rid = rid` loses its
//! leaking attributes (`ag.body`, `ag.rid`, `ag.name`, `ag.lang`, `ag.lic`, and `ag.pack`
//! in a migrated repo) into one sealed `ag.body`, gains `ag.rid = HMAC(rid)`,
//! `ag.name = HMAC(name)`, `ag.pack = HMAC(pack)` and a plaintext `ag.ep`; reading reverses
//! it, so core verification sees the original entity unchanged.
//!
//! Opening also decides whether the row may count at all: a row sealed under an epoch that
//! has ended must have landed before that end (plus [`GRACE_BLOCKS`]), and a signed row's
//! author must have held a role then (write for git state). A removed member keeps the old
//! keys, so without this it could keep writing rows that members accept.

use arkiv_bindings::{Attribute, Create, Ident32, OP_CREATE, OP_PATCH, Operation, Patch};
use arkiv_rpc_types::{AttributeEntry, EntityData};
use charter_core::{ErrorCode, Hash32};
use charter_store::{AttributeValue, Field, Pred, Value};
use charter_sync::{Result, SyncError};

use crate::keyring::{FORMAT, Keyring, Role};
use crate::msg::{BOX_KIND, STOP_V, T_STOP};
use crate::seal::{aad, blind, open, seal};

/// Plaintext epoch attribute: readers pick the key by it.
pub const EPOCH: &str = "ag.ep";
/// Always sealed when present; any other `bytes32` attribute equal to the rid is too
/// (the root identity's `ag.h`, revision 1's `ag.parent`).
pub const HIDDEN: [&str; 5] = ["ag.body", "ag.rid", "ag.name", "ag.lang", "ag.lic"];
/// Sealed too in a migrated repo: the plain SHA-256 of a pack links a private copy to a
/// public one and confirms guessed content.
pub const PACK: &str = "ag.pack";
/// AAD kinds of the lockbox and the attestation: never sealed or opened as an entity.
const RESERVED: [&str; 2] = [BOX_KIND, crate::auth::ATT_KIND];
/// Blocks after an epoch ended in which rows sealed under it still count: a push that read
/// the keyring just before a membership change lands within this (as head bodies land
/// within 64 blocks of `at`, docs/p5/head-model.md 5.4).
pub const GRACE_BLOCKS: u64 = 64;
/// Kinds that carry git state: their author needs the write role.
const WRITE_KINDS: [&str; 5] = ["anchor", "refs", "manifest", "device", "head"];
/// Kinds whose `ag.nid` is a device key, not a member (docs/p5/head-model.md 4.1): a head
/// names its member in `ag.nid` (the device in `ag.dev`), so only a snapshot manifest is.
const DEVICE_SIGNED: [&str; 1] = ["manifest"];

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

/// Seals one create or patch op of `ring`'s repo; an op naming another repo's rid is
/// refused, and every other op, and an op already sealed, is returned unchanged (sealing is
/// idempotent, as `Arkiv::seal_ops` requires). A v2 head patch carries every attribute, so
/// it seals exactly like a create (head-model 9).
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
            match seal_attrs(ring, &p.mutations)? {
                Some(attrs) => Ok(Operation::patch(p.entityKey, attrs)),
                // A body without `ag.rid` and `ag.kind` (a format v3 minimal head patch)
                // cannot be sealed: refuse it rather than send it in plaintext.
                None if has(&p.mutations, "ag.body")? && !has(&p.mutations, EPOCH)? => Err(
                    internal("a patch without ag.rid and ag.kind cannot be sealed"),
                ),
                None => Ok(op),
            }
        }
        _ => Ok(op),
    }
}

fn has(given: &[Attribute], name: &str) -> Result<bool> {
    let w = word(name)?;
    Ok(given.iter().any(|a| a.name == w))
}

/// The sealed attribute list, or `None` when the attributes are not an unsealed repo
/// entity; an error when they are another repo's.
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
    if rid != ring.rid {
        // Sending it as is would publish another repo's entity in the clear.
        return Err(internal(
            "this op belongs to another repo than the one this backend seals",
        ));
    }
    let Some(AttributeValue::Str(kind)) = find("ag.kind")? else {
        return Ok(None);
    };
    if RESERVED.contains(&kind.as_str()) {
        return Err(internal(&format!(
            "ag.kind {kind:?} is reserved in a private repo"
        )));
    }
    let (epoch, key) = ring.current()?;
    let id_key = ring.id_key_at(epoch);
    let mut names: Vec<&str> = HIDDEN.to_vec();
    if ring.format >= FORMAT {
        names.push(PACK);
    }
    let hidden_words = names.iter().map(|n| word(n)).collect::<Result<Vec<_>>>()?;
    let mut hidden: Vec<(String, AttributeValue)> = vec![];
    let mut attrs: Vec<Attribute> = vec![];
    for a in given {
        let v = a.to_value().map_err(|e| internal(&e.to_string()))?;
        let name = names.iter().zip(&hidden_words).find(|(_, w)| **w == a.name);
        match (name, v) {
            (Some((n, _)), Some(v)) => hidden.push(((*n).to_owned(), v)),
            (None, Some(AttributeValue::Bytes32(b))) if b == rid => {
                let n = name_of(&a.name)?;
                attrs.push(attr(
                    &n,
                    AttributeValue::Bytes32(blind(id_key, "rid", &rid)),
                    true,
                )?);
                hidden.push((n, AttributeValue::Bytes32(b)));
            }
            _ => attrs.push(a.clone()),
        }
    }
    let sealed = seal(key, &aad(&rid, &kind, epoch), &encode_hidden(&hidden)?)?;
    attrs.push(attr("ag.body", AttributeValue::Bytes(sealed), false)?);
    attrs.push(attr(
        "ag.rid",
        AttributeValue::Bytes32(blind(id_key, "rid", &rid)),
        true,
    )?);
    attrs.push(attr(EPOCH, AttributeValue::U64(epoch), true)?);
    for (n, v) in &hidden {
        let b = match (n.as_str(), v) {
            ("ag.name", AttributeValue::Str(name)) => blind(id_key, "name", name.as_bytes()),
            (PACK, AttributeValue::Bytes32(p)) => blind(id_key, "pack", p),
            _ => continue,
        };
        attrs.push(attr(n, AttributeValue::Bytes32(b), true)?);
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

/// Opens a sealed row of `ring`'s repo. Rows without `ag.ep`, rows whose epoch key this
/// member lacks, and rows [`admitted`] refuses come back unchanged: core then rejects them
/// as malformed, like any junk.
pub fn open_row(ring: &Keyring, row: EntityData) -> EntityData {
    try_open(ring, &row, true).unwrap_or(row)
}

/// [`open_row`] without [`admitted`]: only to find the root identity, whose content hash is
/// the rid, before the roles that admission needs are known.
pub fn open_row_unchecked(ring: &Keyring, row: EntityData) -> EntityData {
    try_open(ring, &row, false).unwrap_or(row)
}

/// The migration stopper ([`crate::msg::stop_op`]): only pre-0.6.0 clients must see it.
pub fn is_stopper(row: &EntityData) -> bool {
    entry(row, "ag.t") == Some(T_STOP)
        && entry(row, "ag.kind") == Some("id")
        && entry(row, "ag.v").and_then(|v| u64::from_str_radix(v.strip_prefix("0x")?, 16).ok())
            == Some(STOP_V)
}

fn try_open(ring: &Keyring, row: &EntityData, check: bool) -> Option<EntityData> {
    let epoch = u64::from_str_radix(entry(row, EPOCH)?.strip_prefix("0x")?, 16).ok()?;
    let blinded: Hash32 = hex_of(entry(row, "ag.rid")?)?.try_into().ok()?;
    if blinded != blind(ring.id_key_at(epoch), "rid", &ring.rid) {
        return None;
    }
    let kind = entry(row, "ag.kind")?;
    if RESERVED.contains(&kind) {
        return None;
    }
    let body = hex_of(entry(row, "ag.body")?)?;
    let plain = open(
        ring.epochs.get(&epoch)?,
        &aad(&ring.rid, kind, epoch),
        &body,
    )?;
    let hidden = decode_hidden(&plain)?;
    if check && !admitted(ring, row, epoch, kind) {
        return None;
    }
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

/// Whether a sealed row of epoch `epoch` may count: it landed before that epoch ended (plus
/// [`GRACE_BLOCKS`]), and its author (`ag.nid`, which core checks against the signature)
/// held a role in that epoch, write for git state. A read-only row lands at `$createdAt`;
/// a mutable one (a device head) may have been patched since, so `$updatedAt` counts,
/// unless an admin pinned its exact body when the epoch ended.
pub fn admitted(ring: &Keyring, row: &EntityData, epoch: u64, kind: &str) -> bool {
    in_time(ring, row, epoch) && role_ok(ring, row, epoch, kind)
}

fn in_time(ring: &Keyring, row: &EntityData, epoch: u64) -> bool {
    let Some(cut) = ring.cutoff(epoch) else {
        // No later epoch is known: only the newest epoch held may still be open.
        return ring.current().is_ok_and(|(cur, _)| epoch >= cur);
    };
    let readonly = row.creation_flags.is_some_and(|f| f.readonly);
    let landed = match readonly {
        true => row.created_at,
        false => row.updated_at.or(row.created_at),
    };
    if landed.is_some_and(|b| b <= cut.saturating_add(GRACE_BLOCKS)) {
        return true;
    }
    !readonly
        && entry(row, "ag.h")
            .and_then(hex_of)
            .is_some_and(|h| ring.pinned.contains(&hex::encode(h)))
}

fn role_ok(ring: &Keyring, row: &EntityData, epoch: u64, kind: &str) -> bool {
    let Some(nid) = entry(row, "ag.nid").and_then(hex_of) else {
        return true; // unsigned content (chunks, pieces): only a signed row can name it
    };
    let Ok(nid) = <[u8; 32]>::try_from(nid) else {
        return false;
    };
    let min = match WRITE_KINDS.contains(&kind) {
        true => Role::Write,
        false => Role::Read,
    };
    match ring.role_at(epoch, &nid) {
        Some(r) => r >= min,
        // A device key signs a snapshot manifest. It is content addressed and counts only
        // when an admitted head names it, so its signer's role does not matter.
        None => DEVICE_SIGNED.contains(&kind),
    }
}

/// A sealed head [`admitted`] refused only because `$updatedAt` passed its epoch's end plus
/// [`GRACE_BLOCKS`] while it was created in time and is not pinned: an extend, which anyone
/// may send, moves `$updatedAt` without changing the body, so the body may still have
/// landed in time. Returns the last block it may have landed in, for the caller to check
/// the body's landing transaction ([`crate::arkiv::Sealed`]).
pub fn late_head(ring: &Keyring, row: &EntityData) -> Option<u64> {
    let epoch = u64::from_str_radix(entry(row, EPOCH)?.strip_prefix("0x")?, 16).ok()?;
    let readonly = row.creation_flags.is_none_or(|f| f.readonly);
    if readonly || entry(row, "ag.kind") != Some("head") || !role_ok(ring, row, epoch, "head") {
        return None;
    }
    let bound = ring.cutoff(epoch)?.saturating_add(GRACE_BLOCKS);
    let pinned = entry(row, "ag.h")
        .and_then(hex_of)
        .is_some_and(|h| ring.pinned.contains(&hex::encode(h)));
    let late = row.created_at? <= bound && row.updated_at? > bound;
    (late && !pinned).then_some(bound)
}

/// `make(k)` for every id-key generation, as one predicate.
fn any_gen(ring: &Keyring, make: impl Fn(&crate::seal::Key) -> Pred) -> Pred {
    let mut terms: Vec<Pred> = vec![];
    for k in ring.id_keys() {
        let t = make(k);
        if !terms.contains(&t) {
            terms.push(t);
        }
    }
    match terms.len() {
        1 => terms.remove(0),
        _ => Pred::Or(terms),
    }
}

/// Rewrites `ag.rid = rid`, `ag.name = n` and `ag.pack = p` predicates to their blinded
/// values under every id-key generation; a pack also keeps its plain value for chunks sealed
/// before the migration (and by grant readers, who do not know the format).
pub fn blind_pred(ring: &Keyring, p: &Pred) -> Pred {
    let is = |f: &Field, n: &str| matches!(f, Field::Attr(a) if a == n);
    let b32 = |f: &Field, h: Hash32| Pred::Eq(f.clone(), Value::Bytes32(h.into()));
    match p {
        Pred::Eq(f @ Field::Attr(_), Value::Bytes32(v)) if v.0 == ring.rid => {
            any_gen(ring, |k| b32(f, blind(k, "rid", &ring.rid)))
        }
        Pred::Eq(f, Value::Str(n)) if is(f, "ag.name") => {
            any_gen(ring, |k| b32(f, blind(k, "name", n.as_bytes())))
        }
        Pred::Eq(f, Value::Bytes32(v)) if is(f, PACK) => Pred::Or(vec![
            p.clone(),
            any_gen(ring, |k| b32(f, blind(k, "pack", &v.0))),
        ]),
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

    fn ring() -> Keyring {
        let mut r = Keyring::new([1; 32], [2; 32], vec![7]);
        r.epochs = std::collections::BTreeMap::from([(1, [3; 32])]);
        r
    }

    #[test]
    fn a_patch_with_only_body_and_hash_is_refused_instead_of_sent_in_plaintext() {
        let minimal = charter_store::patch_op(
            charter_store::B256::repeat_byte(9),
            &[
                ("ag.body", AttributeValue::Bytes(vec![1, 2, 3]), false),
                ("ag.h", AttributeValue::Bytes32([4; 32]), true),
            ],
        )
        .expect("op");
        assert!(seal_op(&ring(), minimal).is_err());
        let full = charter_store::patch_op(
            charter_store::B256::repeat_byte(9),
            &[
                ("ag.kind", AttributeValue::Str("head".into()), true),
                ("ag.rid", AttributeValue::Bytes32([1; 32]), true),
                ("ag.body", AttributeValue::Bytes(vec![1, 2, 3]), false),
            ],
        )
        .expect("op");
        let sealed = seal_op(&ring(), full).expect("sealed");
        let again = seal_op(&ring(), sealed.clone()).expect("idempotent");
        assert_eq!(again.operationData, sealed.operationData);
    }

    #[test]
    fn an_op_of_another_repo_is_refused_instead_of_sent_in_plaintext() {
        let other = charter_store::create_op(
            &[
                ("ag.kind", AttributeValue::Str("anchor".into()), true),
                ("ag.rid", AttributeValue::Bytes32([8; 32]), true),
                ("ag.body", AttributeValue::Bytes(vec![1, 2, 3]), false),
            ],
            100,
            charter_store::random_salt(),
        )
        .expect("op");
        assert!(seal_op(&ring(), other).is_err());
    }

    fn row(nid: u8, created: u64, updated: u64, readonly: bool, h: u8) -> EntityData {
        let a = |n: &str, v: String| AttributeEntry {
            name: n.into(),
            value_type: "bytes32".into(),
            value: v.into(),
        };
        EntityData {
            created_at: Some(created),
            updated_at: Some(updated),
            creation_flags: Some(arkiv_rpc_types::CreationFlags {
                readonly,
                permissionless_extension: true,
                raw: 0,
            }),
            attributes: Some(vec![
                a("ag.nid", format!("0x{}", hex::encode([nid; 32]))),
                a("ag.h", format!("0x{}", hex::encode([h; 32]))),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn admission_checks_the_cutoff_the_pinned_heads_and_the_role_of_that_epoch() {
        let mut r = ring();
        r.epochs.insert(2, [4; 32]);
        r.rotated.insert(2, 100);
        r.record_roles(
            1,
            &[
                (hex::encode([5; 32]), Role::Write),
                (hex::encode([6; 32]), Role::Read),
            ]
            .into(),
        );
        r.record_roles(2, &[(hex::encode([6; 32]), Role::Read)].into());
        r.pinned.insert(hex::encode([9; 32]));
        let late = 100 + GRACE_BLOCKS + 1;
        assert!(
            admitted(&r, &row(5, 90, 90, true, 1), 1, "anchor"),
            "writer, in time"
        );
        assert!(
            admitted(&r, &row(5, 100 + GRACE_BLOCKS, 0, true, 1), 1, "anchor"),
            "grace"
        );
        assert!(
            !admitted(&r, &row(5, late, late, true, 1), 1, "anchor"),
            "after the cutoff"
        );
        assert!(
            !admitted(&r, &row(6, 90, 90, true, 1), 1, "anchor"),
            "a reader cannot write"
        );
        assert!(
            !admitted(&r, &row(7, 90, 90, true, 1), 1, "anchor"),
            "nor can an outsider"
        );
        assert!(
            !admitted(&r, &row(7, 90, 90, false, 1), 1, "head"),
            "a head names its member, and an outsider holds no role"
        );
        assert!(
            !admitted(&r, &row(6, 90, 90, false, 1), 1, "head"),
            "nor may a reader's head count"
        );
        assert!(
            admitted(&r, &row(7, 90, 90, true, 1), 1, "manifest"),
            "a device signs manifests"
        );
        assert!(
            admitted(&r, &row(6, 90, 90, true, 1), 1, "cob"),
            "a reader may comment"
        );
        assert!(
            !admitted(&r, &row(5, 90, late, false, 1), 1, "head"),
            "extended head, not pinned"
        );
        assert!(
            admitted(&r, &row(5, 90, late, false, 9), 1, "head"),
            "pinned head"
        );
        assert!(
            !admitted(&r, &row(5, 120, 120, true, 1), 2, "anchor"),
            "removed at epoch 2"
        );
        assert!(
            admitted(&r, &row(6, late, late, true, 1), 2, "cob"),
            "current epoch stays open"
        );
        r.epochs.insert(3, [5; 32]);
        assert!(
            !admitted(&r, &row(6, late, late, true, 1), 2, "cob"),
            "no cutoff, not newest"
        );
    }
}
