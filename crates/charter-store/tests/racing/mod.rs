//! A backend racing this client on a shared wallet's nonce (issue #28), for the fake and
//! for the live Rabdi probe.
#![allow(dead_code)]

use alloy_signer_local::PrivateKeySigner;
use charter_store::{Arkiv, SignedTx, TxParams, U256};

/// What the racing device moves to its own address (issue #28).
pub const OTHER_TOP_UP: u128 = 10u128.pow(15);

/// A backend where another device of the same identity sends its own top-up from the
/// shared wallet right after this client read the wallet's nonce: both sign the same
/// nonce, and the other one reaches the node first (issue #28). Logs every refused send.
pub struct Racing<'a, A> {
    pub inner: &'a A,
    pub wallet: PrivateKeySigner,
    pub other: charter_store::Address,
    chain: u64,
    armed: std::sync::atomic::AtomicBool,
}

impl<'a, A: Arkiv> Racing<'a, A> {
    /// `other` stands for the racing device's own address, which its top-up funds.
    pub fn new(
        inner: &'a A,
        wallet: PrivateKeySigner,
        chain: u64,
        other: charter_store::Address,
    ) -> Self {
        Self {
            inner,
            wallet,
            other,
            chain,
            armed: true.into(),
        }
    }

    /// Races the next read of the wallet's nonce again.
    pub fn arm(&self) {
        self.armed.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

type R<T> = Result<T, charter_store::StoreError>;

impl<A: Arkiv> Arkiv for Racing<'_, A> {
    async fn chain_id(&self) -> R<u64> {
        self.inner.chain_id().await
    }
    async fn head(&self) -> R<u64> {
        self.inner.head().await
    }
    async fn base_fee(&self) -> R<u128> {
        self.inner.base_fee().await
    }
    async fn head_fee(&self) -> R<(u64, u128)> {
        self.inner.head_fee().await
    }
    async fn query(
        &self,
        q: &charter_store::Query,
        o: &charter_store::QueryOpts,
    ) -> R<charter_store::QueryPage> {
        self.inner.query(q, o).await
    }
    async fn entity_nonce(&self, c: charter_store::Address) -> R<u64> {
        self.inner.entity_nonce(c).await
    }
    async fn balance(&self, a: charter_store::Address) -> R<U256> {
        self.inner.balance(a).await
    }
    async fn tx_count(&self, a: charter_store::Address) -> R<u64> {
        let n = self.inner.tx_count(a).await?;
        let race = a == self.wallet.address()
            && self.armed.swap(false, std::sync::atomic::Ordering::SeqCst);
        if race {
            let tip = charter_store::PipelineConfig::default().tip;
            let p = TxParams {
                nonce: n,
                to: self.other,
                value: U256::from(OTHER_TOP_UP),
                input: Default::default(),
                gas_limit: 21_000,
                max_fee_per_gas: 4 * self.inner.base_fee().await? + tip,
                max_priority_fee_per_gas: tip,
            };
            let tx = SignedTx::sign(&self.wallet, self.chain, p)?;
            self.inner.send(tx).await?;
            eprintln!(
                "race: {} -> {} {OTHER_TOP_UP} wei at nonce {n}",
                self.wallet.address(),
                self.other
            );
        }
        Ok(n)
    }
    async fn tx_count_mined(&self, a: charter_store::Address) -> R<u64> {
        self.inner.tx_count_mined(a).await
    }
    async fn estimate(
        &self,
        from: charter_store::Address,
        ops: &[charter_store::Operation],
    ) -> R<u64> {
        self.inner.estimate(from, ops).await
    }
    async fn send(&self, tx: SignedTx) -> R<charter_store::TxHash> {
        let r = self.inner.send(tx).await;
        if let Err(e) = &r {
            eprintln!("send refused: {e}");
        }
        r
    }
    async fn receipt(&self, h: charter_store::TxHash) -> R<Option<charter_store::Receipt>> {
        self.inner.receipt(h).await
    }
    async fn transaction(&self, h: charter_store::TxHash) -> R<Option<charter_store::TxInfo>> {
        self.inner.transaction(h).await
    }
    async fn mined_tx(&self, h: charter_store::TxHash) -> R<Option<charter_store::MinedTx>> {
        self.inner.mined_tx(h).await
    }
    async fn block_txs(&self, n: u64) -> R<Vec<charter_store::MinedTx>> {
        self.inner.block_txs(n).await
    }
}
