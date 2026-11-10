//! Format v2 bodies (docs/p5/head-model.md section 4): the identity's delegation of one
//! device, and the device's head.

use super::refs::ref_name_ok;
use super::{Body, cbor_codec, require};
use crate::attrs::Attribute;
use crate::types::{Address, Hash32, Nid, Rid};
use crate::{FormatError, Kind, ZERO, field};
use charter_crypto::{Alg, PublicKey};

/// Entity layout version of the device delegation: v0.3.0 stops on it (head-model 4.1).
pub const DEVICE_LAYOUT_VERSION: u64 = 2;
/// Most pieces one head may list before a snapshot is due (head-model 4.4).
pub const MAX_HEAD_PIECES: usize = 256;

/// `device`: identity `N` delegates one device key and its per-repo addresses.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct DeviceBody {
    #[cbor(n(1), with = "minicbor::bytes")]
    pub rid: Rid,
    #[n(2)]
    pub alg: u16,
    #[cbor(n(3), with = "minicbor::bytes")]
    pub pk: Vec<u8>,
    /// `C_d`: creates the head and inline pieces.
    #[cbor(n(4), with = "minicbor::bytes")]
    pub control: Address,
    /// `B_d`: creates bulk pieces and snapshot chunks and manifests.
    #[cbor(n(5), with = "minicbor::bytes")]
    pub bulk: Address,
}

impl DeviceBody {
    pub fn public_key(&self) -> Result<PublicKey, FormatError> {
        let alg = Alg::from_id(self.alg).ok_or(field("device", "alg", "unknown algorithm"))?;
        Ok(PublicKey::from_bytes(alg, &self.pk)?)
    }
}

impl Body for DeviceBody {
    const KIND: Kind = Kind::Device;
    const AG_KIND: &'static str = Kind::Anchor.name();
    const AG_V: u64 = DEVICE_LAYOUT_VERSION;
    cbor_codec!();

    fn validate(&self) -> Result<(), FormatError> {
        let alg = Alg::from_id(self.alg).ok_or(field("device", "alg", "unknown algorithm"))?;
        require(
            self.pk.len() == alg.pk_len(),
            field("device", "pk", "length does not match algorithm"),
        )?;
        require(
            self.control != self.bulk,
            field("device", "bulk", "must differ from control"),
        )
    }

    fn extra_attributes(&self, signer: &Nid, h: &Hash32) -> Vec<Attribute> {
        vec![
            Attribute::b32("ag.rid", self.rid),
            Attribute::b32("ag.nid", *signer),
            Attribute::b32("ag.h", *h),
            Attribute::u64("ag.seq", 0),
        ]
    }
}

/// One ref entry of a head: an empty `oid` is a tombstone; `stamp = 0` is the block the
/// body carrying it landed in.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(array)]
pub struct HeadRef {
    #[n(0)]
    pub name: String,
    #[cbor(n(1), with = "minicbor::bytes")]
    pub oid: Vec<u8>,
    #[n(2)]
    pub counter: u64,
    #[n(3)]
    pub stamp: u64,
}

impl HeadRef {
    pub fn is_tombstone(&self) -> bool {
        self.oid.is_empty()
    }
}

/// One piece: the tx that created it (zero = the tx carrying this body) and its SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(array)]
pub struct PieceRef {
    #[cbor(n(0), with = "minicbor::bytes")]
    pub tx: Hash32,
    #[cbor(n(1), with = "minicbor::bytes")]
    pub sha256: Hash32,
}

/// A pack (the concatenation of its pieces) and its SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(array)]
pub struct HeadPack {
    #[cbor(n(0), with = "minicbor::bytes")]
    pub pack_id: Hash32,
    #[n(1)]
    pub pieces: Vec<PieceRef>,
}

/// `head`: one device's current refs, snapshot and packs since it.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct HeadBody {
    #[cbor(n(1), with = "minicbor::bytes")]
    pub rid: Rid,
    /// The identity this device writes for.
    #[cbor(n(2), with = "minicbor::bytes")]
    pub nid: Nid,
    #[n(3)]
    pub seq: u64,
    #[cbor(n(4), with = "minicbor::bytes")]
    pub id_head: Hash32,
    /// Latest block seen just before signing; the body lands after it.
    #[n(5)]
    pub at: u64,
    /// `entityNonce(C_d)` after the tx carrying this body, minus 1 (the head itself).
    #[n(6)]
    pub pieces_created: u64,
    /// Strictly ascending by name.
    #[n(7)]
    pub refs: Vec<HeadRef>,
    /// `h` of each snapshot manifest.
    #[cbor(n(8), with = "crate::cbor::hashes")]
    pub snapshot: Vec<Hash32>,
    /// Packs since the snapshot, in apply order.
    #[n(9)]
    pub packs: Vec<HeadPack>,
    /// The tx that carried this device's previous body; zero at seq 0.
    #[cbor(n(10), with = "minicbor::bytes")]
    pub prev_tx: Hash32,
}

impl HeadBody {
    /// Pieces listed across all packs.
    pub fn piece_count(&self) -> usize {
        self.packs.iter().map(|p| p.pieces.len()).sum()
    }

    pub fn entry(&self, name: &str) -> Option<&HeadRef> {
        self.refs
            .binary_search_by(|e| e.name.as_str().cmp(name))
            .ok()
            .map(|i| &self.refs[i])
    }
}

impl Body for HeadBody {
    const KIND: Kind = Kind::Head;
    cbor_codec!();

    fn validate(&self) -> Result<(), FormatError> {
        require(
            self.refs
                .windows(2)
                .all(|w| w[0].name.as_bytes() < w[1].name.as_bytes()),
            field("head", "refs", "names must be strictly ascending"),
        )?;
        require(
            self.refs.iter().all(|e| {
                ref_name_ok(&e.name) && e.counter >= 1 && matches!(e.oid.len(), 0 | 20 | 32)
            }),
            field("head", "refs", "invalid name, counter or oid length"),
        )?;
        require(
            (self.seq == 0) == (self.prev_tx == ZERO),
            field("head", "prev_tx", "zero iff seq = 0"),
        )?;
        require(
            self.piece_count() <= MAX_HEAD_PIECES
                && self.packs.iter().all(|p| !p.pieces.is_empty()),
            field("head", "packs", "empty pack or too many pieces"),
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
