//! Reading mined `execute` txs back from calldata (docs/p5/head-model.md 5.4 and 8): a head
//! body's landing tx and pieces whose entities expired. Plus the two ops format v2 adds,
//! a patch and a create with explicit flags.

use alloy_primitives::{Address, B256, Bytes, TxHash, hex};
use alloy_sol_types::SolCall;
use arkiv_bindings::{
    Attribute, AttributeValue, Create, IEntityRegistry, Ident32, OP_CREATE, OP_PATCH, Operation,
    Patch,
};
use arkiv_rpc_types::{AttributeEntry, EntityData};

use crate::{ARKIV_ADDRESS, StoreError};

/// A mined tx with its calldata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinedTx {
    pub hash: TxHash,
    pub from: Address,
    pub to: Option<Address>,
    pub block: u64,
    /// Index in the block.
    pub index: u32,
    /// The sender's nonce it was signed at.
    pub nonce: u64,
    pub input: Bytes,
}

/// `(name, value, indexed)` attributes in wire form, sorted.
fn wire(attrs: &[(&str, AttributeValue, bool)]) -> Result<Vec<Attribute>, StoreError> {
    attrs
        .iter()
        .map(|(name, value, indexed)| {
            let id =
                Ident32::encode(name).map_err(|e| StoreError::InvalidOp(format!("{name}: {e}")))?;
            Attribute::from_value(id, value, *indexed)
                .map_err(|e| StoreError::InvalidOp(format!("{name}: {e}")))
        })
        .collect()
}

/// A create with explicit `flags` (the v2 head is `permissionlessExtension` only, so its
/// creator can patch it).
pub fn create_op_flags(
    attrs: &[(&str, AttributeValue, bool)],
    min_lifetime: u64,
    salt: u128,
    flags: u8,
) -> Result<Operation, StoreError> {
    Ok(Operation::create(
        salt,
        0,
        min_lifetime,
        flags,
        wire(attrs)?,
    ))
}

/// A patch that sets every attribute given (upserts; lifetime unchanged).
pub fn patch_op(
    key: B256,
    attrs: &[(&str, AttributeValue, bool)],
) -> Result<Operation, StoreError> {
    Ok(Operation::patch(key, wire(attrs)?))
}

fn name_of(w: &B256) -> Option<String> {
    let n = w.iter().position(|b| *b == 0).unwrap_or(32);
    String::from_utf8(w[..n].to_vec()).ok()
}

fn entry(a: &Attribute) -> Option<AttributeEntry> {
    let (ty, value) = match a.to_value().ok()?? {
        AttributeValue::Str(s) => ("str", s),
        AttributeValue::U64(n) => ("u64", format!("{n:#x}")),
        AttributeValue::Bytes(b) => ("bytes", format!("0x{}", hex::encode(b))),
        AttributeValue::Bytes32(b) => ("bytes32", format!("0x{}", hex::encode(b))),
        _ => return None,
    };
    Some(AttributeEntry {
        name: name_of(&a.name)?,
        value_type: ty.into(),
        value: value.into(),
    })
}

fn row(tx: &MinedTx, key: Option<B256>, attrs: &[Attribute]) -> EntityData {
    EntityData {
        key,
        creator: Some(tx.from),
        owner: Some(tx.from),
        created_at: Some(tx.block),
        updated_at: Some(tx.block),
        attributes: Some(attrs.iter().filter_map(entry).collect()),
        ..EntityData::default()
    }
}

/// The creates (`key: None`) and patches (`key` set) of an `execute` tx to the registry, as
/// query rows created by the sender in the tx's block, in op order. Anything else is empty.
pub fn tx_rows(tx: &MinedTx) -> Vec<EntityData> {
    if tx.to != Some(ARKIV_ADDRESS) {
        return vec![];
    }
    let Ok(call) = IEntityRegistry::executeCall::abi_decode(&tx.input) else {
        return vec![];
    };
    call.ops
        .iter()
        .filter_map(|op| match op.operation {
            OP_CREATE => op
                .payload_of::<Create>()
                .ok()
                .map(|c| row(tx, None, &c.attributes)),
            OP_PATCH => op
                .payload_of::<Patch>()
                .ok()
                .map(|p| row(tx, Some(p.entityKey), &p.mutations)),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::execute_calldata;

    #[test]
    fn creates_and_patches_come_back_as_rows() {
        let attrs = [
            ("ag.kind", AttributeValue::Str("piece".into()), true),
            ("ag.body", AttributeValue::Bytes(vec![1, 2, 3]), false),
            ("ag.v", AttributeValue::U64(1), false),
        ];
        let key = B256::repeat_byte(9);
        let ops = vec![
            create_op_flags(&attrs, 20, 7, 2).expect("create"),
            patch_op(key, &attrs).expect("patch"),
            crate::extend_op(key, 5),
        ];
        let tx = MinedTx {
            hash: TxHash::ZERO,
            from: Address::repeat_byte(1),
            to: Some(ARKIV_ADDRESS),
            block: 42,
            index: 3,
            nonce: 0,
            input: execute_calldata(&ops),
        };
        let rows = tx_rows(&tx);
        assert_eq!(rows.len(), 2);
        assert_eq!((rows[0].key, rows[1].key), (None, Some(key)));
        let a = rows[1].attributes.as_ref().expect("attrs");
        assert_eq!(a[0].name, "ag.body");
        assert_eq!(a[0].value, "0x010203");
        assert_eq!(a[2].value, "0x1");
        assert_eq!(rows[0].created_at, Some(42));
        let c: Create = ops[0].payload_of().expect("create");
        assert_eq!(c.creationFlags, 2);
        assert!(tx_rows(&MinedTx { to: None, ..tx }).is_empty());
    }
}
