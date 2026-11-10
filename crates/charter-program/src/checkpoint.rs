//! Incremental folds (docs/p5/speed-and-cost.md 2.1 item 2): a fold paused after some
//! events, kept so the next fold only runs the events after them.
//!
//! A checkpoint is used only when it provably equals a fresh fold up to that point: the
//! same program source, repo facts and seed heads, and the exact same events (a running
//! SHA-256 over their canonical JSON), and every git-ancestry question it asked was
//! answered (a `None` may turn into an answer once objects arrive, so such a fold is never
//! kept). The script state is kept with its exact Rhai types; a state holding anything
//! else (function pointers, shared or tagged values) is not checkpointed.

use std::collections::BTreeMap;

use rhai::{Array, Blob, Dynamic, Map};
use serde::{Deserialize, Serialize};

use crate::{Event, RefUpdate, RepoInfo};

/// Bump when the checkpoint layout or the fold semantics change: older ones are ignored.
pub const CHECKPOINT_VERSION: u32 = 1;

/// A fold paused after `events` events. Produced by [`crate::Program::fold_cached`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub v: u32,
    #[serde(with = "hex::serde")]
    pub program: [u8; 32],
    pub repo: RepoInfo,
    #[serde(with = "hexmap")]
    pub seed: BTreeMap<String, Vec<u8>>,
    /// How many events were folded, and the running hash over them.
    pub events: u64,
    #[serde(with = "hex::serde")]
    pub events_hash: [u8; 32],
    pub(crate) last_order: u64,
    pub(crate) state: Val,
    #[serde(with = "hexmap")]
    pub(crate) heads: BTreeMap<String, Vec<u8>>,
    pub(crate) accepted: Vec<RefUpdate>,
    pub(crate) rejected: Vec<(RefUpdate, String)>,
    pub(crate) logs: Vec<String>,
    pub(crate) spent: u64,
}

/// The running hash after `prev` and one more event.
pub(crate) fn chain(prev: [u8; 32], ev: &Event) -> Result<[u8; 32], String> {
    let j = serde_json::to_vec(ev).map_err(|e| e.to_string())?;
    let mut buf = Vec::with_capacity(32 + j.len());
    buf.extend_from_slice(&prev);
    buf.extend_from_slice(&j);
    Ok(charter_crypto::sha256(&buf))
}

/// The running hash over `events`.
pub(crate) fn hash_all<'a>(
    events: impl IntoIterator<Item = &'a Event>,
) -> Result<[u8; 32], String> {
    events.into_iter().try_fold([0; 32], chain)
}

/// A Rhai value with its exact type, as the fold's scripts can build it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", content = "v")]
pub(crate) enum Val {
    Unit,
    Bool(bool),
    Int(i64),
    Char(char),
    Str(String),
    Blob(#[serde(with = "hex::serde")] Vec<u8>),
    Array(Vec<Val>),
    Map(BTreeMap<String, Val>),
}

impl Val {
    /// `None` for anything a checkpoint could not restore exactly.
    pub(crate) fn of(d: &Dynamic) -> Option<Self> {
        if d.tag() != 0 || d.is_shared() || d.is_read_only() {
            return None;
        }
        if d.is_unit() {
            return Some(Self::Unit);
        }
        if let Ok(b) = d.as_bool() {
            return Some(Self::Bool(b));
        }
        if let Ok(i) = d.as_int() {
            return Some(Self::Int(i));
        }
        if let Ok(c) = d.as_char() {
            return Some(Self::Char(c));
        }
        if d.is_string() {
            return d.clone().into_string().ok().map(Self::Str);
        }
        if d.is_blob() {
            return d.clone().try_cast::<Blob>().map(Self::Blob);
        }
        if d.is_array() {
            let a = d.clone().try_cast::<Array>()?;
            return a
                .iter()
                .map(Self::of)
                .collect::<Option<_>>()
                .map(Self::Array);
        }
        if d.is_map() {
            let m = d.clone().try_cast::<Map>()?;
            return m
                .iter()
                .map(|(k, v)| Some((k.to_string(), Self::of(v)?)))
                .collect::<Option<_>>()
                .map(Self::Map);
        }
        None
    }

    pub(crate) fn to_dynamic(&self) -> Dynamic {
        match self {
            Self::Unit => Dynamic::UNIT,
            Self::Bool(b) => (*b).into(),
            Self::Int(i) => (*i).into(),
            Self::Char(c) => (*c).into(),
            Self::Str(s) => s.clone().into(),
            Self::Blob(b) => Dynamic::from_blob(b.clone()),
            Self::Array(a) => a.iter().map(Self::to_dynamic).collect::<Array>().into(),
            Self::Map(m) => m
                .iter()
                .map(|(k, v)| (k.as_str().into(), v.to_dynamic()))
                .collect::<Map>()
                .into(),
        }
    }
}

/// `BTreeMap<String, Vec<u8>>` with hex values.
mod hexmap {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(
        m: &BTreeMap<String, Vec<u8>>,
        s: S,
    ) -> Result<S::Ok, S::Error> {
        let h: BTreeMap<&String, String> = m.iter().map(|(k, v)| (k, hex::encode(v))).collect();
        h.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> Result<BTreeMap<String, Vec<u8>>, D::Error> {
        let h = BTreeMap::<String, String>::deserialize(d)?;
        h.into_iter()
            .map(|(k, v)| Ok((k, hex::decode(v).map_err(serde::de::Error::custom)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_round_trip_with_their_exact_types() {
        let mut m = Map::new();
        m.insert("blob".into(), Dynamic::from_blob(vec![0, 1, 255]));
        m.insert(
            "ints".into(),
            vec![Dynamic::from(1_i64), Dynamic::from(-2_i64)].into(),
        );
        m.insert("s".into(), "x".into());
        m.insert("c".into(), 'z'.into());
        m.insert("b".into(), true.into());
        m.insert("u".into(), Dynamic::UNIT);
        let d: Dynamic = m.into();
        let v = Val::of(&d).expect("encodable");
        let json = serde_json::to_string(&v).expect("json");
        let back: Val = serde_json::from_str(&json).expect("parse");
        assert_eq!(back, v);
        let again = back.to_dynamic();
        assert_eq!(Val::of(&again), Some(v));
        let blob = again.try_cast::<Map>().expect("map")["blob"].clone();
        assert!(blob.is_blob(), "a blob stays a blob, not an array of ints");
    }

    #[test]
    fn tagged_values_are_not_checkpointed() {
        let mut d = Dynamic::from(3_i64);
        d.set_tag(7);
        assert_eq!(Val::of(&d), None);
    }
}
