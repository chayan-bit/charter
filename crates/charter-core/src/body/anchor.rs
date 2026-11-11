use super::{Body, cbor_codec, require, seq_prev_ok};
use crate::attrs::Attribute;
use crate::chunk::{CHUNK_SIZE, MAX_PACK_SIZE, chunk_count};
use crate::types::{Hash32, Nid, Rid};
use crate::{FormatError, Kind, field};

/// `anchor`: one link of a peer's chain, fixing its refs head.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct AnchorBody {
    #[cbor(n(1), with = "minicbor::bytes")]
    pub rid: Rid,
    #[n(2)]
    pub seq: u64,
    /// `h` of anchor `seq - 1`; zero iff `seq = 0`.
    #[cbor(n(3), with = "minicbor::bytes")]
    pub prev: Hash32,
    /// `h` of this push's refs entity.
    #[cbor(n(4), with = "minicbor::bytes")]
    pub refs: Hash32,
    /// `h` of manifests added by this push.
    #[cbor(n(5), with = "crate::cbor::hashes")]
    pub manifests: Vec<Hash32>,
    /// `h` of the identity revision the writer verified.
    #[cbor(n(6), with = "minicbor::bytes")]
    pub id_head: Hash32,
    /// Head block number seen when signing.
    #[n(7)]
    pub at: u64,
}

impl Body for AnchorBody {
    const KIND: Kind = Kind::Anchor;
    cbor_codec!();

    fn validate(&self) -> Result<(), FormatError> {
        require(
            seq_prev_ok(self.seq, &self.prev),
            field("anchor", "prev", "zero iff seq = 0"),
        )
    }

    fn extra_attributes(&self, signer: &Nid, h: &Hash32) -> Vec<Attribute> {
        vec![
            Attribute::b32("ag.rid", self.rid),
            Attribute::b32("ag.nid", *signer),
            Attribute::b32("ag.h", *h),
            Attribute::u64("ag.seq", self.seq),
            Attribute::u64("ag.at", self.at),
        ]
    }
}

/// `(nid, anchor h)` whose packs must be present first.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(array)]
pub struct Require {
    #[cbor(n(0), with = "minicbor::bytes")]
    pub nid: Nid,
    #[cbor(n(1), with = "minicbor::bytes")]
    pub anchor: Hash32,
}

/// `manifest`: one pack and its chunk hashes.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct ManifestBody {
    #[cbor(n(1), with = "minicbor::bytes")]
    pub rid: Rid,
    /// SHA-256 of the stored pack bytes.
    #[cbor(n(2), with = "minicbor::bytes")]
    pub pack_id: Hash32,
    /// Git pack trailer: 20 B SHA-1 or 32 B SHA-256.
    #[cbor(n(3), with = "minicbor::bytes")]
    pub trailer: Vec<u8>,
    #[n(4)]
    pub size: u64,
    #[n(5)]
    pub chunk_size: u64,
    /// 32 x ceil(size / chunk size) bytes.
    #[cbor(n(6), with = "minicbor::bytes")]
    pub chunk_hashes: Vec<u8>,
    #[n(7)]
    pub requires: Vec<Require>,
    /// 1 = self-contained git pack; 2 = thin (reserved); 3 = brotli stream of a
    /// self-contained pack (format v3: `pack_id`, `size`, chunks describe the stored bytes).
    #[n(8)]
    pub pack_kind: u64,
}

impl ManifestBody {
    /// Expected SHA-256 of chunk `i`, if in range.
    pub fn chunk_hash(&self, i: u64) -> Option<&[u8]> {
        let start = usize::try_from(i).ok()?.checked_mul(32)?;
        self.chunk_hashes.get(start..start.checked_add(32)?)
    }
}

impl Body for ManifestBody {
    const KIND: Kind = Kind::Manifest;
    cbor_codec!();

    fn validate(&self) -> Result<(), FormatError> {
        require(
            matches!(self.trailer.len(), 20 | 32),
            field("manifest", "trailer", "must be 20 or 32 bytes"),
        )?;
        require(
            (1..=MAX_PACK_SIZE).contains(&self.size),
            field("manifest", "size", "must be 1..104,857,600"),
        )?;
        require(
            self.chunk_size == CHUNK_SIZE as u64,
            field("manifest", "chunk_size", "must be 65,536 in P1"),
        )?;
        require(
            self.chunk_hashes.len() as u64 == 32 * chunk_count(self.size),
            field("manifest", "chunk_hashes", "must be 32 x chunk count bytes"),
        )?;
        match self.pack_kind {
            // 3: format v3, the brotli stream of a self-contained pack (head-model 13.1).
            1 | 3 => Ok(()),
            2 => Err(FormatError::Unsupported {
                what: "pack kind",
                value: 2,
            }),
            _ => Err(field("manifest", "pack_kind", "unknown value")),
        }
    }

    fn extra_attributes(&self, signer: &Nid, h: &Hash32) -> Vec<Attribute> {
        vec![
            Attribute::b32("ag.rid", self.rid),
            Attribute::b32("ag.nid", *signer),
            Attribute::b32("ag.h", *h),
            Attribute::b32("ag.pack", self.pack_id),
        ]
    }
}
