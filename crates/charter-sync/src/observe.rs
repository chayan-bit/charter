//! Reading Arkiv: query results become core [`Observed`] entities (attributes plus creator,
//! creation position and expiry), paged per design 4.8, and the completeness observation
//! of design 3.3.

use std::collections::BTreeSet;

use charter_core::{
    Address, AttrValue, Attribute, CompletenessObs, ErrorCode, Hash32, Kind, Observed,
};
use charter_store::paging::{keyset, offset_all};
use charter_store::{Arkiv, Attrs, B256, EntityData, Field, Pred, Query, QueryOpts, Select, attr};

use crate::{Result, SyncError};

/// `entityNonce` retries before `E_INCONSISTENT_VIEW` (design 3.3 step 4).
const COMPLETENESS_RETRIES: usize = 3;
/// OR lists stay under the node's 64-predicate limit with room for the other terms.
pub const MAX_OR_TERMS: usize = 48;

/// Key, creator, `$createdAt`, `$expiresAt` and every attribute.
pub fn full_select() -> Select {
    Select {
        key: true,
        creator: true,
        created_at: true,
        updated_at: true,
        expires_at: true,
        attrs: Attrs::All,
        ..Select::default()
    }
}

fn attribute(name: &str, ty: &str, v: &serde_json::Value) -> Option<Attribute> {
    let s = v.as_str()?;
    let value = match ty {
        "str" => AttrValue::Str(s.to_owned()),
        "u64" => AttrValue::U64(u64::from_str_radix(s.strip_prefix("0x")?, 16).ok()?),
        "bytes" => AttrValue::Bytes(hex::decode(s.strip_prefix("0x")?).ok()?),
        // GolemDB reports `bytes32` and `key` as one type (conformance Q2).
        "bytes32" | "key" => {
            AttrValue::Bytes32(hex::decode(s.strip_prefix("0x")?).ok()?.try_into().ok()?)
        }
        _ => return None,
    };
    Some(Attribute::new(name, value))
}

/// Rows to observed entities; `tx` is the rank in the (creation-ordered or seq-then-creation
/// ordered) result, which orders entities created in one block. Rows missing a field the
/// rules need are dropped; an unknown attribute type makes the entity fail its schema check.
pub fn to_observed(rows: Vec<EntityData>) -> Vec<Observed> {
    rows.into_iter()
        .enumerate()
        .filter_map(|(rank, r)| {
            let attributes = r
                .attributes
                .unwrap_or_default()
                .iter()
                .map(|a| attribute(&a.name, &a.value_type, &a.value))
                .collect::<Option<Vec<_>>>()?;
            Some(Observed {
                key: r.key?.0,
                creator: r.creator?.into_array(),
                block: r.created_at?,
                updated_at: r.updated_at.or(r.created_at)?,
                tx: u32::try_from(rank).unwrap_or(u32::MAX),
                expires_at: r.expires_at?,
                attributes,
            })
        })
        .collect()
}

pub fn kind(k: Kind) -> Pred {
    Pred::eq(attr("ag.kind"), k.name())
}

pub fn b32(name: &str, h: &Hash32) -> Pred {
    Pred::eq(attr(name), B256::from(*h))
}

/// `$creator = a OR ...`; callers never pass an empty set (an empty OR matches nothing).
pub fn creators(set: &BTreeSet<Address>) -> Pred {
    Pred::Or(
        set.iter()
            .map(|a| Pred::eq(Field::Creator, charter_store::Address::from(*a)))
            .collect(),
    )
}

/// Every match in creation order (offset paging with the count check).
pub async fn all<A: Arkiv>(a: &A, filter: Pred) -> Result<Vec<Observed>> {
    let rows = offset_all(a, &Query::from(filter), full_select()).await?;
    Ok(to_observed(rows))
}

/// Matches of `AND(base, ag.h = one of hashes)`, split into OR groups under the limit.
pub async fn by_hashes<A: Arkiv>(a: &A, base: Pred, hashes: &[Hash32]) -> Result<Vec<Observed>> {
    let mut out = vec![];
    for group in hashes.chunks(MAX_OR_TERMS) {
        let any = Pred::Or(group.iter().map(|h| b32("ag.h", h)).collect());
        out.extend(all(a, Pred::And(vec![base.clone(), any])).await?);
    }
    Ok(out)
}

/// Every match ordered by the `u64` attribute `name` (keyset paging from 0).
pub async fn by_seq<A: Arkiv>(a: &A, filter: Pred, name: &str) -> Result<Vec<Observed>> {
    let rows = keyset(a, &filter, name, 0, full_select()).await?;
    Ok(to_observed(rows))
}

/// Live entities created by `address`.
pub async fn live_count<A: Arkiv>(a: &A, address: &Address) -> Result<u64> {
    let q = Query::from(creators(&BTreeSet::from([*address])));
    let page = a.query(&q, &QueryOpts::count_only()).await?;
    page.count
        .ok_or_else(|| SyncError::coded(ErrorCode::Rpc, "count missing from query result"))
}

/// `entityNonce`, live count, `entityNonce`; retried while the two nonces differ.
pub async fn completeness<A: Arkiv>(a: &A, address: &Address) -> Result<CompletenessObs> {
    let evm = charter_store::Address::from(*address);
    let mut obs = None;
    for _ in 0..=COMPLETENESS_RETRIES {
        let nonce_before = a.entity_nonce(evm).await?;
        let live = live_count(a, address).await?;
        let nonce_after = a.entity_nonce(evm).await?;
        let o = CompletenessObs {
            address: *address,
            nonce_before,
            live,
            nonce_after,
        };
        if nonce_before == nonce_after {
            return Ok(o);
        }
        obs = Some(o);
    }
    obs.ok_or_else(|| SyncError::coded(ErrorCode::InconsistentView, "no observation"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rows_convert_with_rank_and_typed_attributes() {
        let row = |key: u8, extra: serde_json::Value| -> EntityData {
            let mut attrs = vec![
                json!({"name": "ag.kind", "type": "str", "value": "peer"}),
                json!({"name": "ag.v", "type": "u64", "value": "0x1"}),
                json!({"name": "ag.body", "type": "bytes", "value": "0x0102"}),
                json!({"name": "ag.h", "type": "key", "value": format!("0x{}", "ab".repeat(32))}),
            ];
            attrs.push(extra);
            serde_json::from_value(json!({
                "key": format!("0x{}", hex::encode([key; 32])),
                "creator": format!("0x{}", "11".repeat(20)),
                "createdAt": "0x5",
                "expiresAt": "0x9",
                "attributes": attrs,
            }))
            .expect("row")
        };
        let ok =
            json!({"name": "ag.nid", "type": "bytes32", "value": format!("0x{}", "cd".repeat(32))});
        let unknown = json!({"name": "ag.x", "type": "bool", "value": true});
        let obs = to_observed(vec![row(1, ok.clone()), row(2, unknown), row(3, ok)]);
        assert_eq!(
            obs.len(),
            2,
            "the row with an unknown attribute type is dropped"
        );
        assert_eq!((obs[0].tx, obs[1].tx), (0, 2));
        assert_eq!(obs[0].block, 5);
        assert_eq!(obs[0].expires_at, 9);
        assert_eq!(obs[0].attr("ag.v"), Some(&AttrValue::U64(1)));
        assert_eq!(obs[0].attr("ag.body"), Some(&AttrValue::Bytes(vec![1, 2])));
        assert_eq!(obs[0].attr("ag.h"), Some(&AttrValue::Bytes32([0xab; 32])));
        let missing: EntityData = serde_json::from_value(json!({})).expect("row");
        assert!(to_observed(vec![missing]).is_empty());
    }
}
