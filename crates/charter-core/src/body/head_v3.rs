//! Format v3 head (docs/p5/head-model.md section 13): the v2 head with pieces named by
//! runs of `(block, tx index)` and a codec per pack. Stored as `ag.kind = 'head'` with
//! `ag.v = 3`, which v0.4.0 meets as an unknown layout version and stops on.

use super::{Body, cbor_codec, require};
use crate::attrs::{AttrValue, Attribute, sorted};
use crate::codec::Codec;
use crate::types::{Hash32, Nid, Observed, Rid};
use crate::{Envelope, FormatError, HeadBody, HeadRef, Kind, MAX_HEAD_PIECES, content_hash, field};

/// `ag.v` of a format v3 head. v0.4.0 accepts only 1 for `ag.kind = 'head'`.
pub const HEAD_V3_LAYOUT_VERSION: u64 = 3;

/// `count` consecutive txs of block `block` from index `index`, each carrying its piece
/// creates in op order. `block = 0` is the tx carrying this body (`index = 0`, `count = 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(array)]
pub struct PieceRun {
    #[n(0)]
    pub block: u64,
    #[n(1)]
    pub index: u32,
    #[n(2)]
    pub count: u32,
}

impl PieceRun {
    pub const HERE: PieceRun = PieceRun {
        block: 0,
        index: 0,
        count: 1,
    };
}

/// One pack: SHA-256 of its stored bytes (the concatenated pieces), its codec, its runs.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(array)]
pub struct PackV3 {
    #[cbor(n(0), with = "minicbor::bytes")]
    pub pack_id: Hash32,
    #[n(1)]
    pub codec: u8,
    #[n(2)]
    pub runs: Vec<PieceRun>,
}

impl PackV3 {
    pub fn codec(&self) -> Result<Codec, FormatError> {
        Codec::from_code(self.codec)
    }

    /// Txs this pack's pieces sit in.
    pub fn tx_count(&self) -> usize {
        self.runs.iter().map(|r| r.count as usize).sum()
    }
}

/// Runs of `(block, tx index)` positions in order: consecutive indexes of one block merge.
pub fn runs(positions: &[(u64, u32)]) -> Vec<PieceRun> {
    let mut out: Vec<PieceRun> = vec![];
    for &(block, index) in positions {
        match out.last_mut() {
            Some(r) if r.block == block && block != 0 && r.index + r.count == index => r.count += 1,
            _ => out.push(PieceRun {
                block,
                index,
                count: 1,
            }),
        }
    }
    out
}

/// `head` v3: fields 1 to 8, 10 and 11 as in [`HeadBody`]; field 9 lists [`PackV3`].
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct HeadBodyV3 {
    #[cbor(n(1), with = "minicbor::bytes")]
    pub rid: Rid,
    #[cbor(n(2), with = "minicbor::bytes")]
    pub nid: Nid,
    #[n(3)]
    pub seq: u64,
    #[cbor(n(4), with = "minicbor::bytes")]
    pub id_head: Hash32,
    #[n(5)]
    pub at: u64,
    #[n(6)]
    pub pieces_created: u64,
    #[n(7)]
    pub refs: Vec<HeadRef>,
    #[cbor(n(8), with = "crate::cbor::hashes")]
    pub snapshot: Vec<Hash32>,
    #[n(9)]
    pub packs: Vec<PackV3>,
    #[cbor(n(10), with = "minicbor::bytes")]
    pub prev_tx: Hash32,
    #[n(11)]
    pub object_hash: u8,
}

impl HeadBodyV3 {
    /// The fields both formats share, as a v2 body without packs: what the merge, the
    /// rollback check and the program fold read, so a v3 timeline folds as v2 would.
    pub fn shared(&self) -> HeadBody {
        HeadBody {
            rid: self.rid,
            nid: self.nid,
            seq: self.seq,
            id_head: self.id_head,
            at: self.at,
            pieces_created: self.pieces_created,
            refs: self.refs.clone(),
            snapshot: self.snapshot.clone(),
            packs: vec![],
            prev_tx: self.prev_tx,
            object_hash: self.object_hash,
        }
    }

    /// `shared`'s fields (its v2 packs ignored) with `packs`.
    pub fn from_shared(shared: &HeadBody, packs: Vec<PackV3>) -> Self {
        Self {
            rid: shared.rid,
            nid: shared.nid,
            seq: shared.seq,
            id_head: shared.id_head,
            at: shared.at,
            pieces_created: shared.pieces_created,
            refs: shared.refs.clone(),
            snapshot: shared.snapshot.clone(),
            packs,
            prev_tx: shared.prev_tx,
            object_hash: shared.object_hash,
        }
    }

    /// The v2 rules for everything but packs (refs, stamps, oid lengths, `prev_tx`).
    fn v2_rules(&self) -> Result<(), FormatError> {
        self.shared().validate()
    }
}

impl Body for HeadBodyV3 {
    const KIND: Kind = Kind::HeadV3;
    const AG_KIND: &'static str = Kind::Head.name();
    const AG_V: u64 = HEAD_V3_LAYOUT_VERSION;
    cbor_codec!();

    fn validate(&self) -> Result<(), FormatError> {
        self.v2_rules()?;
        let runs_ok = self.packs.iter().all(|p| {
            p.codec().is_ok()
                && !p.runs.is_empty()
                && p.runs
                    .iter()
                    .all(|r| r.count >= 1 && (r.block != 0 || *r == PieceRun::HERE))
        });
        require(runs_ok, field("head", "packs", "unknown codec or bad run"))?;
        let txs: usize = self.packs.iter().map(PackV3::tx_count).sum();
        require(
            txs <= MAX_HEAD_PIECES,
            field("head", "packs", "too many pieces"),
        )
    }

    fn extra_attributes(&self, signer: &Nid, h: &Hash32) -> Vec<Attribute> {
        vec![
            Attribute::b32("ag.rid", self.rid),
            Attribute::b32("ag.nid", self.nid),
            Attribute::b32("ag.dev", *signer),
            Attribute::b32("ag.h", *h),
        ]
    }
}

/// What a v3 head patch sets (head-model 13.2): the body and its hash only. Arkiv merges a
/// patch into the stored attributes, so `ag.kind`, `ag.v`, `ag.rid`, `ag.nid` and `ag.dev`
/// stay as the create (or the migrating full patch) wrote them.
pub fn head_patch_attributes(envelope_bytes: &[u8]) -> Vec<Attribute> {
    sorted(vec![
        Attribute::new("ag.body", AttrValue::Bytes(envelope_bytes.to_vec())),
        Attribute::b32("ag.h", content_hash(Kind::HeadV3, envelope_bytes)),
    ])
}

/// A v3 body read from calldata: a full row (create or migrating patch) or a minimal
/// patch row with only `ag.body` and `ag.h`. Checks `ag.h`, the envelope kind and the body
/// rules; the caller verifies the signature under the device key.
pub fn open_head_calldata(obs: &Observed) -> Result<(Envelope, HeadBodyV3, Hash32), FormatError> {
    let bytes = match obs.attr("ag.body") {
        Some(AttrValue::Bytes(b)) => b,
        _ => return Err(field("head", "ag.body", "missing or not bytes")),
    };
    let envelope = Envelope::decode(bytes)?;
    if envelope.kind != Kind::HeadV3 {
        return Err(FormatError::KindMismatch {
            expected: Kind::HeadV3.name(),
            found: envelope.kind.name().to_owned(),
        });
    }
    let body = HeadBodyV3::decode(&envelope.body)?;
    let h = content_hash(Kind::HeadV3, bytes);
    let minimal = sorted(obs.attributes.clone()) == head_patch_attributes(bytes);
    let full = sorted(obs.attributes.clone()) == crate::entity_attributes(&envelope, bytes, &body);
    require(minimal || full, FormatError::AttributeMismatch)?;
    Ok((envelope, body, h))
}
