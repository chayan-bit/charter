//! The sealing selector: the backend to use for one repo, plain for a public repo or
//! [`Sealed`] for a private repo this machine can read. The CLI and the UI write worker pick
//! every repo-scoped backend through [`for_repo`]; charter-sync's `E_UNSEALED_PRIVATE` guard
//! catches any caller that does not.

use charter_core::Rid;
use charter_store::{
    Address, Arkiv, EntityData, MinedTx, Operation, Query, QueryOpts, QueryPage, Quota, Receipt,
    SignedTx, StoreError, TxHash, TxInfo, U256,
};
use charter_sync::{Config, Keys, Result};

use crate::arkiv::Sealed;
use crate::join::ring_for;
use crate::keyring::Keyring;

/// `a` itself, or `a` sealed under a private repo's keyring.
pub enum ForRepo<'a, A> {
    Plain(&'a A),
    Sealed(Box<Sealed<'a, A>>),
}

/// The backend for `rid` (see [`ring_for`]): costs a `stat` on a public repo already seen,
/// and `E_NOT_MEMBER` for a private repo this machine cannot read.
pub async fn for_repo<'a, A: Arkiv>(
    a: &'a A,
    cfg: &Config,
    keys: Option<&Keys>,
    rid: &Rid,
) -> Result<ForRepo<'a, A>> {
    Ok(match ring_for(a, cfg, keys, rid).await? {
        None => ForRepo::Plain(a),
        Some(ring) => ForRepo::Sealed(Box::new(Sealed::new(a, ring))),
    })
}

/// [`for_repo`] from the keyring this machine last saved for `rid`, with no group refresh
/// (no query): for pricing a write, where only sizes and this member's binds matter. The
/// send that follows calls [`for_repo`], so it seals under the keys current at send time.
pub async fn for_repo_cached<'a, A: Arkiv>(
    a: &'a A,
    cfg: &Config,
    keys: Option<&Keys>,
    rid: &Rid,
) -> Result<ForRepo<'a, A>> {
    match Keyring::load(&cfg.home, rid)? {
        Some(ring) => Ok(ForRepo::Sealed(Box::new(Sealed::new(a, ring)))),
        None => for_repo(a, cfg, keys, rid).await,
    }
}

macro_rules! both {
    ($self:ident, $b:ident => $e:expr) => {
        match $self {
            ForRepo::Plain($b) => $e,
            ForRepo::Sealed($b) => $e,
        }
    };
}

impl<A: Arkiv> Arkiv for ForRepo<'_, A> {
    fn quota(&self) -> Option<Quota> {
        both!(self, b => b.quota())
    }
    async fn chain_id(&self) -> Result<u64, StoreError> {
        both!(self, b => b.chain_id().await)
    }
    async fn head(&self) -> Result<u64, StoreError> {
        both!(self, b => b.head().await)
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        both!(self, b => b.base_fee().await)
    }
    async fn head_fee(&self) -> Result<(u64, u128), StoreError> {
        both!(self, b => b.head_fee().await)
    }
    fn heads_ws(&self) -> Option<charter_store::heads::WsConfig> {
        both!(self, b => b.heads_ws())
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> Result<QueryPage, StoreError> {
        both!(self, b => b.query(q, o).await)
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        both!(self, b => b.entity_nonce(c).await)
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        both!(self, b => b.balance(a).await)
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        both!(self, b => b.tx_count(a).await)
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        both!(self, b => b.tx_count_mined(a).await)
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        both!(self, b => b.estimate(from, ops).await)
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        both!(self, b => b.send(tx).await)
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        both!(self, b => b.receipt(h).await)
    }
    fn seal_ops(&self, ops: Vec<Operation>) -> Result<Vec<Operation>, StoreError> {
        both!(self, b => b.seal_ops(ops))
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        both!(self, b => b.transaction(h).await)
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<MinedTx>, StoreError> {
        both!(self, b => b.mined_tx(h).await)
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<MinedTx>, StoreError> {
        both!(self, b => b.block_txs(n).await)
    }
    async fn tx_at(&self, n: u64, i: u32) -> Result<Option<MinedTx>, StoreError> {
        both!(self, b => b.tx_at(n, i).await)
    }
    fn seals(&self) -> bool {
        both!(self, b => b.seals())
    }
    fn seals_for(&self, rid: &[u8; 32]) -> bool {
        both!(self, b => b.seals_for(rid))
    }
    fn open_rows(&self, rows: Vec<EntityData>) -> Vec<EntityData> {
        both!(self, b => b.open_rows(rows))
    }
}
