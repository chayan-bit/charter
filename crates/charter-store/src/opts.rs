//! `QueryOpts` (limit, offset, select), `QueryPage`, and typed reads of returned attributes.

use alloy_primitives::{B256, hex};
use arkiv_rpc_types::EntityData;
use serde_json::{Map, Value as Json, json};

use crate::StoreError;
use crate::query::MAX_LIMIT;

/// Which attributes to return.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum Attrs {
    #[default]
    None,
    All,
    Named(Vec<String>),
}

/// The `select` projection. Default: the key only, as on the node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Select {
    pub key: bool,
    pub owner: bool,
    pub creator: bool,
    pub created_at: bool,
    pub updated_at: bool,
    pub expires_at: bool,
    pub creation_flags: bool,
    pub attrs: Attrs,
    pub count: bool,
}

impl Default for Select {
    fn default() -> Self {
        Self {
            key: true,
            owner: false,
            creator: false,
            created_at: false,
            updated_at: false,
            expires_at: false,
            creation_flags: false,
            attrs: Attrs::None,
            count: false,
        }
    }
}

/// `arkiv_query` options. `limit` is 1..=100; there are no cursors (design 4.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryOpts {
    pub limit: u32,
    pub offset: u64,
    pub select: Select,
}

impl Default for QueryOpts {
    fn default() -> Self {
        Self {
            limit: MAX_LIMIT,
            offset: 0,
            select: Select::default(),
        }
    }
}

impl QueryOpts {
    /// Only the total match count.
    pub fn count_only() -> Self {
        Self {
            limit: 1,
            offset: 0,
            select: Select {
                key: false,
                count: true,
                ..Select::default()
            },
        }
    }

    /// Key plus the named attributes.
    pub fn with_attrs(names: &[&str]) -> Self {
        let attrs = Attrs::Named(names.iter().map(|s| s.to_string()).collect());
        Self {
            select: Select {
                attrs,
                ..Select::default()
            },
            ..Self::default()
        }
    }

    /// The JSON options object, after validating the limit.
    pub fn to_json(&self) -> Result<Json, StoreError> {
        if self.limit == 0 || self.limit > MAX_LIMIT {
            return Err(StoreError::InvalidQuery(format!(
                "limit {} outside 1..={MAX_LIMIT}",
                self.limit
            )));
        }
        let s = &self.select;
        let mut sel = Map::new();
        for (name, on) in [
            ("key", s.key),
            ("owner", s.owner),
            ("creator", s.creator),
            ("createdAt", s.created_at),
            ("updatedAt", s.updated_at),
            ("expiresAt", s.expires_at),
            ("creationFlags", s.creation_flags),
            ("count", s.count),
        ] {
            if on {
                sel.insert(name.into(), Json::Bool(true));
            }
        }
        match &s.attrs {
            Attrs::None => {}
            Attrs::All => {
                sel.insert("attributes".into(), Json::Bool(true));
            }
            Attrs::Named(ns) => {
                let m: Map<String, Json> =
                    ns.iter().map(|n| (n.clone(), Json::Bool(true))).collect();
                sel.insert("attributes".into(), Json::Object(m));
            }
        }
        let mut o = json!({ "select": sel, "limit": format!("{:#x}", self.limit) });
        if self.offset > 0 {
            o["offset"] = Json::String(format!("{:#x}", self.offset));
        }
        Ok(o)
    }
}

/// One page of query results.
#[derive(Debug, Clone, Default)]
pub struct QueryPage {
    /// Block the node evaluated the query at.
    pub block_number: u64,
    /// Total matches, when `select.count` was set.
    pub count: Option<u64>,
    pub rows: Vec<EntityData>,
}

/// Typed reads of returned attributes. `None` when absent, of another type, or malformed:
/// attributes are untrusted hints, checked again against signed bodies upstream.
pub trait RowExt {
    fn attr_json(&self, name: &str, ty: &str) -> Option<&Json>;

    fn attr_u64(&self, name: &str) -> Option<u64> {
        let s = self.attr_json(name, "u64")?.as_str()?;
        u64::from_str_radix(s.strip_prefix("0x")?, 16).ok()
    }
    fn attr_str(&self, name: &str) -> Option<&str> {
        self.attr_json(name, "str")?.as_str()
    }
    fn attr_bytes(&self, name: &str) -> Option<Vec<u8>> {
        hex::decode(self.attr_json(name, "bytes")?.as_str()?).ok()
    }
    /// A `bytes32` attribute (GolemDB also reports `key`-typed values here, conformance Q2).
    fn attr_b256(&self, name: &str) -> Option<B256> {
        let j = self
            .attr_json(name, "bytes32")
            .or_else(|| self.attr_json(name, "key"))?;
        j.as_str()?.parse().ok()
    }
}

impl RowExt for EntityData {
    fn attr_json(&self, name: &str, ty: &str) -> Option<&Json> {
        self.attributes
            .as_ref()?
            .iter()
            .find(|a| a.name == name && a.value_type == ty)
            .map(|a| &a.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_serialise_like_the_spike_client() {
        let o = QueryOpts {
            offset: 200,
            ..QueryOpts::with_attrs(&["ag.i", "ag.body"])
        };
        let j = o.to_json().expect("valid");
        assert_eq!(j["limit"], "0x64");
        assert_eq!(j["offset"], "0xc8");
        assert_eq!(j["select"]["key"], true);
        assert_eq!(j["select"]["attributes"]["ag.body"], true);
        let c = QueryOpts::count_only().to_json().expect("valid");
        assert_eq!(c["select"], json!({ "count": true }));
        assert!(c.get("offset").is_none());
        let all = QueryOpts {
            select: Select {
                attrs: Attrs::All,
                creator: true,
                owner: true,
                created_at: true,
                expires_at: true,
                creation_flags: true,
                ..Select::default()
            },
            ..QueryOpts::default()
        };
        let j = all.to_json().expect("valid");
        assert_eq!(j["select"]["attributes"], true);
        assert_eq!(j["select"]["createdAt"], true);
    }

    #[test]
    fn limit_must_be_one_to_one_hundred() {
        for bad in [0, 101] {
            let o = QueryOpts {
                limit: bad,
                ..QueryOpts::default()
            };
            assert_eq!(o.to_json().map_err(|e| e.code()), Err("E_INTERNAL"));
        }
    }

    #[test]
    fn typed_attribute_reads() {
        let row: EntityData = serde_json::from_value(json!({
            "key": format!("0x{}", "00".repeat(32)),
            "attributes": [
                { "name": "ag.i", "type": "u64", "value": "0x11" },
                { "name": "ag.kind", "type": "str", "value": "chunk" },
                { "name": "ag.body", "type": "bytes", "value": "0x0102" },
                { "name": "ag.pack", "type": "bytes32", "value": format!("0x{}", "ab".repeat(32)) },
                { "name": "ag.k", "type": "key", "value": format!("0x{}", "cd".repeat(32)) },
            ]
        }))
        .expect("row");
        assert_eq!(row.attr_u64("ag.i"), Some(17));
        assert_eq!(row.attr_str("ag.kind"), Some("chunk"));
        assert_eq!(row.attr_bytes("ag.body"), Some(vec![1, 2]));
        assert_eq!(row.attr_b256("ag.pack"), Some(B256::repeat_byte(0xab)));
        assert_eq!(row.attr_b256("ag.k"), Some(B256::repeat_byte(0xcd)));
        assert_eq!(row.attr_u64("ag.kind"), None, "wrong type");
        assert_eq!(row.attr_u64("missing"), None);
    }
}
