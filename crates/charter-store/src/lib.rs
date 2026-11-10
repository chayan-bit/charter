//! charter-store: the boundary to Arkiv (design 7.1-7.3).
//!
//! [`Arkiv`] is the trait; [`RpcArkiv`] talks to a node over JSON-RPC, `MemArkiv` (feature
//! `fake`) is an in-memory model of one. [`TxPipeline`] and the paging helpers are generic
//! over the trait, so tests run the same code the real client runs.

use std::future::Future;

pub use alloy_primitives::{Address, B256, TxHash, U256};
pub use arkiv_bindings::Operation;
pub use arkiv_rpc_types::EntityData;

mod error;
#[cfg(feature = "fake")]
pub mod fake;
pub mod heads;
mod ops;
mod opts;
pub mod paging;
pub mod pipeline;
mod query;
pub mod quota;
pub mod rpc;
mod tx;

pub use error::StoreError;
pub use ops::{AttributeValue, ENTITY_FLAGS, create_op, derive_key, extend_op, random_salt};
pub use opts::{Attrs, QueryOpts, QueryPage, RowExt, Select};
pub use pipeline::{PipelineConfig, TxPipeline};
pub use query::{
    Field, MAX_AND_OR_DEPTH, MAX_LIMIT, MAX_PREDICATES, MAX_QUERY_BYTES, OrderBy, OrderType, Pred,
    Query, Value, attr,
};
pub use quota::Quota;
pub use rpc::{RpcArkiv, RpcConfig};
pub use tx::{
    ARKIV_ADDRESS, BLOCK_GAS_LIMIT, MAX_RAW_TX_BYTES, RABDI_CHAIN_ID, RABDI_RPC, Receipt, SignedTx,
    TX_GAS_CAP, TxInfo, TxParams, decode_revert, execute_calldata, op_calldata_bytes,
};

/// Everything Charter asks of Arkiv (design 7.2).
///
/// Methods return `impl Future + Send` rather than being `async fn`, so callers may
/// spawn the futures on a multi-threaded runtime; implementors still write `async fn`.
pub trait Arkiv: Send + Sync {
    fn chain_id(&self) -> impl Future<Output = Result<u64, StoreError>> + Send;
    fn head(&self) -> impl Future<Output = Result<u64, StoreError>> + Send;
    /// Base fee of the latest block, in wei.
    fn base_fee(&self) -> impl Future<Output = Result<u128, StoreError>> + Send;
    fn query(
        &self,
        q: &Query,
        opts: &QueryOpts,
    ) -> impl Future<Output = Result<QueryPage, StoreError>> + Send;
    /// Entities `creator` ever created, live or not.
    fn entity_nonce(
        &self,
        creator: Address,
    ) -> impl Future<Output = Result<u64, StoreError>> + Send;
    fn balance(&self, a: Address) -> impl Future<Output = Result<U256, StoreError>> + Send;
    /// Next nonce for `a`, counting txs the node holds as pending.
    fn tx_count(&self, a: Address) -> impl Future<Output = Result<u64, StoreError>> + Send;
    /// Txs from `a` in mined blocks only (`latest`), unlike [`Arkiv::tx_count`].
    fn tx_count_mined(&self, a: Address) -> impl Future<Output = Result<u64, StoreError>> + Send;
    /// Gas `execute(ops)` from `from` would use, or the decoded revert.
    fn estimate(
        &self,
        from: Address,
        ops: &[Operation],
    ) -> impl Future<Output = Result<u64, StoreError>> + Send;
    /// The RPC's latest report of its rate quota; `None` when it sends none (the fake).
    fn quota(&self) -> Option<Quota> {
        None
    }
    /// Submit an `execute(ops)` call or a plain transfer.
    fn send(&self, tx: SignedTx) -> impl Future<Output = Result<TxHash, StoreError>> + Send;
    fn receipt(
        &self,
        h: TxHash,
    ) -> impl Future<Output = Result<Option<Receipt>, StoreError>> + Send;
    /// Rewrites one tx's ops before they are priced and signed. Identity by default;
    /// P2 private repos seal create ops here (`charter-private`). Must be idempotent.
    fn seal_ops(&self, ops: Vec<Operation>) -> Result<Vec<Operation>, StoreError> {
        Ok(ops)
    }
    /// Sender, recipient and value of a tx, pending or mined; `None` if unknown.
    fn transaction(
        &self,
        h: TxHash,
    ) -> impl Future<Output = Result<Option<TxInfo>, StoreError>> + Send;
}
