//! [`Sealed`]: an `Arkiv` adapter for one private repo. Writes are sealed in
//! `seal_ops` (the pipeline calls it before pricing and signing), queries are blinded,
//! and rows are opened before the sync layer sees them. Push, clone and every core rule
//! run unchanged on top of it.

use charter_store::{
    Address, Arkiv, Attrs, Operation, Query, QueryOpts, QueryPage, Receipt, SignedTx, StoreError,
    TxHash, TxInfo, U256,
};

use crate::entity::{EPOCH, HIDDEN, blind_pred, open_row, seal_op};
use crate::keyring::Keyring;

pub struct Sealed<'a, A> {
    pub inner: &'a A,
    pub ring: Keyring,
}

impl<'a, A: Arkiv> Sealed<'a, A> {
    pub fn new(inner: &'a A, ring: Keyring) -> Self {
        Self { inner, ring }
    }
}

impl<A: Arkiv> Arkiv for Sealed<'_, A> {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        self.inner.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.inner.head().await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.inner.base_fee().await
    }
    async fn head_fee(&self) -> Result<(u64, u128), StoreError> {
        self.inner.head_fee().await
    }
    fn heads_ws(&self) -> Option<charter_store::heads::WsConfig> {
        self.inner.heads_ws()
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> Result<QueryPage, StoreError> {
        let q = Query {
            filter: blind_pred(&self.ring, &q.filter),
            order: q.order.clone(),
        };
        let mut o = o.clone();
        // A named projection must still return what opening a sealed row needs.
        if let Attrs::Named(names) = &mut o.select.attrs
            && names.iter().any(|n| HIDDEN.contains(&n.as_str()))
        {
            names.extend(["ag.kind", "ag.rid", EPOCH].map(String::from));
        }
        let mut page = self.inner.query(&q, &o).await?;
        page.rows = page
            .rows
            .into_iter()
            .map(|r| open_row(&self.ring, r))
            .collect();
        Ok(page)
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        self.inner.entity_nonce(c).await
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.inner.balance(a).await
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.inner.tx_count(a).await
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.inner.tx_count_mined(a).await
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.inner.transaction(h).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.inner.mined_tx(h).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<charter_store::MinedTx>, StoreError> {
        self.inner.block_txs(n).await
    }
    async fn tx_at(&self, n: u64, i: u32) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.inner.tx_at(n, i).await
    }
    fn seals(&self) -> bool {
        true
    }
    fn open_rows(&self, rows: Vec<charter_store::EntityData>) -> Vec<charter_store::EntityData> {
        rows.into_iter().map(|r| open_row(&self.ring, r)).collect()
    }
    /// Estimates the sealed ops, so push cost predictions include padding.
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        let sealed = self.seal_ops(ops.to_vec())?;
        self.inner.estimate(from, &sealed).await
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        self.inner.send(tx).await
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        self.inner.receipt(h).await
    }
    fn seal_ops(&self, ops: Vec<Operation>) -> Result<Vec<Operation>, StoreError> {
        ops.into_iter()
            .map(|op| seal_op(&self.ring, op).map_err(|e| StoreError::InvalidOp(e.to_string())))
            .collect()
    }
}
