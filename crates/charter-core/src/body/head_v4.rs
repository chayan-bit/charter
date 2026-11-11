//! Format v4 head (docs/p5/head-model.md section 14): a v3 head whose refs overflow into a
//! ref table, stored as bulk pieces and named by hash from field 12 of the signed body.
//! Stored as `ag.kind = 'head'` with `ag.v = 4`, which v0.5.0 meets as an unknown layout
//! version and stops on.

use std::collections::BTreeMap;

use super::{Body, require};
use crate::attrs::Attribute;
use crate::codec::Codec;
use crate::types::{Hash32, Nid};
use crate::{FormatError, HeadBody, HeadBodyV3, HeadRef, Kind, PieceRun, field};

/// `ag.v` of a format v4 head.
pub const HEAD_V4_LAYOUT_VERSION: u64 = 4;
/// Most piece txs one ref table may span (4 MiB stored).
pub const MAX_TABLE_PIECES: usize = 64;
/// Cap on a decoded ref table, so a signed table bomb fails instead of filling memory.
pub const MAX_TABLE_SIZE: u64 = 16 << 20;

/// Field 12 of a v4 body: where the ref table's stored bytes are, their SHA-256 and codec,
/// and the stamp that table entries with `stamp = 0` take (0: this body's landing block).
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(array)]
pub struct RefTable {
    #[cbor(n(0), with = "minicbor::bytes")]
    pub table_id: Hash32,
    #[n(1)]
    pub codec: u8,
    #[n(2)]
    pub runs: Vec<PieceRun>,
    #[n(3)]
    pub stamp: u64,
}

impl RefTable {
    pub fn codec(&self) -> Result<Codec, FormatError> {
        Codec::from_code(self.codec)
    }

    /// Bulk piece txs only (never the head tx), at most [`MAX_TABLE_PIECES`], a known codec
    /// and a stamp no later than `at`.
    fn validate(&self, at: u64) -> Result<(), FormatError> {
        self.codec()?;
        let runs_ok = !self.runs.is_empty()
            && self
                .runs
                .iter()
                .all(|r| r.count >= 1 && r.block != 0 && r.index.checked_add(r.count).is_some());
        require(runs_ok, field("head", "refs_table", "bad run"))?;
        let txs: usize = self.runs.iter().map(|r| r.count as usize).sum();
        require(
            txs <= MAX_TABLE_PIECES,
            field("head", "refs_table", "too many pieces"),
        )?;
        require(
            self.stamp <= at,
            field("head", "refs_table", "stamp after at"),
        )
    }

    /// The table this body reuses, its `stamp = 0` made concrete at `landing`.
    pub fn resolved(&self, landing: u64) -> Self {
        Self {
            stamp: if self.stamp == 0 { landing } else { self.stamp },
            ..self.clone()
        }
    }

    /// Table entries as they count for this body: `stamp = 0` takes the table's stamp.
    pub fn stamped(&self, table: &[HeadRef]) -> Vec<HeadRef> {
        table
            .iter()
            .map(|e| HeadRef {
                stamp: if e.stamp == 0 { self.stamp } else { e.stamp },
                ..e.clone()
            })
            .collect()
    }
}

/// Canonical CBOR of a ref table, before the codec.
pub fn encode_table(refs: &[HeadRef]) -> Vec<u8> {
    crate::cbor::to_vec(&refs.to_vec())
}

/// A decoded table: canonical CBOR with strictly ascending names (the other entry rules are
/// checked on the effective refs, [`effective_refs`]).
pub fn decode_table(bytes: &[u8]) -> Result<Vec<HeadRef>, FormatError> {
    let refs: Vec<HeadRef> = crate::cbor::decode_canonical(bytes)?;
    require(
        refs.windows(2)
            .all(|w| w[0].name.as_bytes() < w[1].name.as_bytes()),
        field("head", "refs_table", "names must be strictly ascending"),
    )?;
    Ok(refs)
}

/// `shared` (a v4 body's shared fields, field 7 inline) with its effective refs: the
/// stamped table overlaid by the inline entries by name, checked by every rule of 4.2.
pub fn effective_refs(
    shared: &HeadBody,
    t: &RefTable,
    table: &[HeadRef],
) -> Result<HeadBody, FormatError> {
    let mut all: BTreeMap<&str, HeadRef> = BTreeMap::new();
    let stamped = t.stamped(table);
    for e in stamped.iter().chain(&shared.refs) {
        all.insert(e.name.as_str(), e.clone());
    }
    let body = HeadBody {
        refs: all.into_values().collect(),
        ..shared.clone()
    };
    body.validate()?;
    Ok(body)
}

/// `head` v4: a [`HeadBodyV3`] that carries field 12, signed as its own kind so it can never
/// be read as a v3 body without its table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadBodyV4(pub HeadBodyV3);

impl Body for HeadBodyV4 {
    const KIND: Kind = Kind::HeadV4;
    const AG_KIND: &'static str = Kind::Head.name();
    const AG_V: u64 = HEAD_V4_LAYOUT_VERSION;

    fn encode(&self) -> Vec<u8> {
        crate::cbor::to_vec(&self.0)
    }

    fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let v = Self(crate::cbor::decode_canonical(bytes)?);
        v.validate()?;
        Ok(v)
    }

    fn validate(&self) -> Result<(), FormatError> {
        self.0.rules()?;
        let t = self.0.refs_table.as_ref().ok_or(field(
            "head",
            "refs_table",
            "a v4 body names a ref table",
        ))?;
        t.validate(self.0.at)
    }

    fn extra_attributes(&self, signer: &Nid, h: &Hash32) -> Vec<Attribute> {
        self.0.extra_attributes(signer, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str, oid: u8, stamp: u64) -> HeadRef {
        HeadRef {
            name: name.into(),
            oid: vec![oid; 20],
            counter: 1,
            stamp,
            base: vec![],
        }
    }

    fn table(stamp: u64) -> RefTable {
        RefTable {
            table_id: [7; 32],
            codec: 0,
            runs: vec![PieceRun {
                block: 5,
                index: 0,
                count: 1,
            }],
            stamp,
        }
    }

    fn shared(refs: Vec<HeadRef>) -> HeadBody {
        HeadBody {
            rid: [1; 32],
            nid: [2; 32],
            seq: 1,
            id_head: [3; 32],
            at: 100,
            pieces_created: 1,
            refs,
            snapshot: vec![],
            packs: vec![],
            prev_tx: [4; 32],
            object_hash: 1,
        }
    }

    #[test]
    fn inline_entries_override_the_table_and_zero_stamps_take_the_table_stamp() {
        let t = table(40);
        let rows = vec![e("refs/heads/a", 1, 0), e("refs/heads/b", 2, 30)];
        let body = effective_refs(&shared(vec![e("refs/heads/b", 9, 0)]), &t, &rows).expect("ok");
        assert_eq!(
            body.refs,
            vec![e("refs/heads/a", 1, 40), e("refs/heads/b", 9, 0)]
        );
    }

    #[test]
    fn a_table_round_trips_and_rejects_unsorted_names() {
        let rows = vec![e("refs/heads/a", 1, 0), e("refs/heads/b", 2, 3)];
        assert_eq!(decode_table(&encode_table(&rows)).expect("decode"), rows);
        let unsorted = vec![rows[1].clone(), rows[0].clone()];
        assert!(decode_table(&encode_table(&unsorted)).is_err());
    }

    #[test]
    fn a_table_stamp_after_at_or_a_run_in_the_head_tx_is_refused() {
        assert!(table(101).validate(100).is_err());
        let mut here = table(0);
        here.runs = vec![PieceRun::HERE];
        assert!(here.validate(100).is_err());
        assert!(table(0).validate(100).is_ok());
    }

    #[test]
    fn a_table_entry_stamped_after_at_fails_the_effective_rules() {
        let rows = vec![e("refs/heads/a", 1, 101)];
        assert!(effective_refs(&shared(vec![]), &table(0), &rows).is_err());
    }
}
