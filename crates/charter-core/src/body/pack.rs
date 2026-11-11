//! A pack a device head names, in either head format: v2 lists each piece's tx and
//! SHA-256 (head-model 4.2), v3 lists runs of `(block, index)` and a codec (13.1).

use super::head::{HeadPack, PieceRef};
use super::head_v3::{PackV3, PieceRun};
use crate::FormatError;
use crate::codec::Codec;
use crate::types::Hash32;

/// Where a pack's pieces sit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PackPieces {
    /// Format v2: one entry per piece (`tx = 0`: the tx carrying the listing body).
    Txs(Vec<PieceRef>),
    /// Format v3: runs of consecutive txs (`PieceRun::HERE`: the tx carrying the body).
    Runs(Vec<PieceRun>),
}

/// One pack of a head: SHA-256 of its stored bytes, their codec and where they sit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pack {
    pub pack_id: Hash32,
    pub codec: Codec,
    pub pieces: PackPieces,
}

impl Pack {
    pub fn from_v2(p: &HeadPack) -> Self {
        Self {
            pack_id: p.pack_id,
            codec: Codec::Raw,
            pieces: PackPieces::Txs(p.pieces.clone()),
        }
    }

    /// Fails only on an unknown codec, which a validated v3 body never has.
    pub fn from_v3(p: &PackV3) -> Result<Self, FormatError> {
        Ok(Self {
            pack_id: p.pack_id,
            codec: p.codec()?,
            pieces: PackPieces::Runs(p.runs.clone()),
        })
    }

    /// The v3 form of a pack listed by runs; `None` for a v2 pack.
    pub fn to_v3(&self) -> Option<PackV3> {
        match &self.pieces {
            PackPieces::Runs(runs) => Some(PackV3 {
                pack_id: self.pack_id,
                codec: self.codec as u8,
                runs: runs.clone(),
            }),
            PackPieces::Txs(_) => None,
        }
    }

    /// Pieces (v2) or piece txs (v3) the pack occupies; what the head's piece limit counts.
    pub fn piece_count(&self) -> usize {
        match &self.pieces {
            PackPieces::Txs(p) => p.len(),
            PackPieces::Runs(r) => r.iter().map(|r| r.count as usize).sum(),
        }
    }

    /// Whether some piece sits in the tx carrying the listing body.
    pub fn has_inline(&self) -> bool {
        match &self.pieces {
            PackPieces::Txs(p) => p.iter().any(|x| x.tx == crate::ZERO),
            PackPieces::Runs(r) => r.contains(&PieceRun::HERE),
        }
    }
}
