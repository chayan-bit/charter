//! Identifier aliases and the observed-entity input type the sync layer feeds to core.

pub use charter_crypto::Hash32;
pub use charter_crypto::evm::Address;

use crate::attrs::{AttrValue, Attribute};

/// Peer id, `H("nid", u16be(alg) || pk)`.
pub type Nid = Hash32;
/// Repo id, `H("id", root envelope bytes)`.
pub type Rid = Hash32;

pub const ZERO: Hash32 = [0u8; 32];

/// Arkiv creation order: creating transaction (block, then tx index), then entity key
/// within one transaction (design section 3.2). `Ord` is that total order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Position {
    pub block: u64,
    pub tx: u32,
    pub key: Hash32,
}

/// One entity as returned by an Arkiv query, before any verification.
///
/// `block` is `$createdAt`; `tx` is any value that orders entities created in the same
/// block in creation order (the tx index, or the rank in a creation-ordered query result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub key: Hash32,
    pub creator: Address,
    pub block: u64,
    pub tx: u32,
    pub expires_at: u64,
    /// `$updatedAt`: the last patch or extend (equal to `block` when never mutated).
    pub updated_at: u64,
    pub attributes: Vec<Attribute>,
}

impl Observed {
    pub fn position(&self) -> Position {
        Position {
            block: self.block,
            tx: self.tx,
            key: self.key,
        }
    }

    pub fn attr(&self, name: &str) -> Option<&AttrValue> {
        self.attributes
            .iter()
            .find(|a| a.name == name)
            .map(|a| &a.value)
    }
}

/// Sorts observed entities into creation order (arrival order never matters).
pub fn in_creation_order(observed: &[Observed]) -> Vec<&Observed> {
    let mut v: Vec<&Observed> = observed.iter().collect();
    v.sort_by_key(|o| o.position());
    v
}
