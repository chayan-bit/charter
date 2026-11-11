//! Device revoke record (docs/p5/head-model.md section 15): the identity names a device's
//! last valid head body; every later body of that device is ignored.

use super::{Body, cbor_codec, require};
use crate::attrs::Attribute;
use crate::types::{Hash32, Nid, Rid};
use crate::{DEVICE_SEQ, FormatError, Kind, ZERO, field};

/// `ag.v` of a revoke record, stored as an `anchor` so v0.5.0 stops on it (head-model 15.6).
pub const REVOKE_LAYOUT_VERSION: u64 = 3;

/// `revoke`: identity `N` revokes device `device`; its body `h` (seq `seq`, carried by tx
/// `tx`) is the last that stays valid, or none when `h` is zero.
#[derive(Debug, Clone, PartialEq, Eq, minicbor::Encode, minicbor::Decode)]
#[cbor(map)]
pub struct RevokeBody {
    #[cbor(n(1), with = "minicbor::bytes")]
    pub rid: Rid,
    #[cbor(n(2), with = "minicbor::bytes")]
    pub device: Nid,
    #[n(3)]
    pub seq: u64,
    #[cbor(n(4), with = "minicbor::bytes")]
    pub h: Hash32,
    #[cbor(n(5), with = "minicbor::bytes")]
    pub tx: Hash32,
}

impl Body for RevokeBody {
    const KIND: Kind = Kind::Revoke;
    const AG_KIND: &'static str = Kind::Anchor.name();
    const AG_V: u64 = REVOKE_LAYOUT_VERSION;
    cbor_codec!();

    fn validate(&self) -> Result<(), FormatError> {
        require(
            (self.h == ZERO) == (self.tx == ZERO),
            field("revoke", "tx", "set exactly when h is"),
        )?;
        require(
            self.h != ZERO || self.seq == 0,
            field("revoke", "seq", "0 when no body stays valid"),
        )
    }

    fn extra_attributes(&self, signer: &Nid, h: &Hash32) -> Vec<Attribute> {
        vec![
            Attribute::b32("ag.rid", self.rid),
            Attribute::b32("ag.nid", *signer),
            Attribute::b32("ag.dev", self.device),
            Attribute::b32("ag.h", *h),
            // Like a delegation: every incremental anchor read of an old client meets it.
            Attribute::u64("ag.seq", DEVICE_SEQ),
        ]
    }
}
