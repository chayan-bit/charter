//! Fixtures: an auto-mining `MemArkiv`, a repo with one delegate, an appointed escrow and
//! a payer that deposits through the real transfer path.
#![allow(dead_code)]

use alloy_signer_local::PrivateKeySigner;
use charter_core::{Hash32, Rid};
use charter_crypto::MasterSeed;
use charter_escrow::ops::{self, DEPOSIT, DepositPayload};
use charter_escrow::{Agent, AuditReport, EscrowKeys, FixedObligations, Flag, Obligation};
use charter_store::fake::MemArkiv;
use charter_store::{
    Address, Arkiv, Operation, Query, QueryOpts, QueryPage, Receipt, SignedTx, StoreError, TxHash,
    TxInfo, TxPipeline, U256,
};
use charter_sync::identity::{self, RepoMeta};
use charter_sync::{Config, Keys};

pub const CHAIN: u64 = 7;
pub const TGLM: u128 = 1_000_000_000_000_000_000;

/// `MemArkiv` that mines one block when a receipt is polled while txs are pending.
pub struct Net {
    pub mem: MemArkiv,
}

impl Net {
    pub fn mine(&self, n: u64) {
        (0..n).for_each(|_| {
            self.mem.mine();
        });
    }
}

impl Arkiv for Net {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        self.mem.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.mem.head().await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.mem.base_fee().await
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> Result<QueryPage, StoreError> {
        self.mem.query(q, o).await
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        self.mem.entity_nonce(c).await
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.mem.balance(a).await
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.mem.tx_count(a).await
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.mem.tx_count_mined(a).await
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.mem.estimate(from, ops).await
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        self.mem.send(tx).await
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        if self.mem.pending() > 0 && self.mem.receipt(h).await?.is_none() {
            self.mem.mine();
        }
        self.mem.receipt(h).await
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.mem.transaction(h).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.mem.mined_tx(h).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<charter_store::MinedTx>, StoreError> {
        self.mem.block_txs(n).await
    }
}

pub struct Peer {
    pub home: tempfile::TempDir,
    pub cfg: Config,
    pub keys: Keys,
}

impl Peer {
    pub fn new(net: &Net, seed: u8) -> Self {
        let home = tempfile::tempdir().expect("home");
        let cfg = Config::for_fake(home.path().to_path_buf());
        let keys = Keys::from_seed(MasterSeed::from_bytes([seed; 32]));
        net.mem
            .fund(keys.wallet().address(), U256::from(1000 * TGLM));
        Self { home, cfg, keys }
    }
}

/// A repo, its delegate, an appointed escrow (with a 1 tGLM gas float) and a payer.
pub struct World {
    pub net: Net,
    pub delegate: Peer,
    pub payer: Peer,
    /// The escrow agent's home (journal) and config.
    pub agent: Peer,
    pub escrow: EscrowKeys,
    /// The payout key, kept so tests can make the escrow misbehave.
    pub payout: PrivateKeySigner,
    pub rid: Rid,
    pub object: Hash32,
}

impl World {
    pub async fn new() -> Self {
        let net = Net {
            mem: MemArkiv::new(CHAIN),
        };
        let (delegate, payer, agent) = (Peer::new(&net, 1), Peer::new(&net, 2), Peer::new(&net, 9));
        let payout = PrivateKeySigner::random();
        let escrow = EscrowKeys::new(
            Keys::from_seed(MasterSeed::from_bytes([9; 32])),
            payout.clone(),
        );
        net.mem.fund(escrow.address(), U256::from(TGLM));
        let meta = RepoMeta {
            name: "escrowed".into(),
            description: String::new(),
            default_branch: "main".into(),
            lang: None,
            license: None,
        };
        let rid = Box::pin(identity::init(&net, &delegate.cfg, &delegate.keys, &meta))
            .await
            .expect("init");
        let object = ops::appoint(
            &net,
            &delegate.cfg,
            &delegate.keys,
            &rid,
            &escrow.id.nid(),
            escrow.address(),
        )
        .await
        .expect("appoint");
        Self {
            net,
            delegate,
            payer,
            agent,
            escrow,
            payout,
            rid,
            object,
        }
    }

    /// Sends `value` from `from` to `to` through `TxPipeline::transfer`; returns the tx.
    pub async fn transfer(&self, from: &PrivateKeySigner, to: Address, value: u128) -> TxHash {
        let mut p = TxPipeline::new(
            &self.net,
            from.clone(),
            CHAIN,
            self.payer.cfg.pipeline.clone(),
        );
        Box::pin(p.transfer(to, U256::from(value)))
            .await
            .expect("transfer")
            .tx_hash
    }

    /// Posts a deposit op claiming `amount` for `purpose` against `tx`.
    pub async fn claim(
        &self,
        tx: TxHash,
        amount: u128,
        purpose: &str,
        expiry: Option<u64>,
    ) -> Hash32 {
        let p = DepositPayload {
            tx,
            amount: amount.to_string(),
            purpose: purpose.into(),
            expiry,
        };
        let (cfg, keys) = (&self.payer.cfg, &self.payer.keys);
        ops::post(&self.net, cfg, keys, &self.rid, self.object, DEPOSIT, &p)
            .await
            .expect("deposit op")
    }

    /// The payer sends `amount` to the escrow and claims it.
    pub async fn deposit(&self, amount: u128, purpose: &str, expiry: Option<u64>) -> TxHash {
        let tx = self
            .transfer(&self.payer.keys.wallet(), self.escrow.address(), amount)
            .await;
        self.claim(tx, amount, purpose, expiry).await;
        tx
    }

    pub fn agent<'a>(&'a self, src: &'a FixedObligations) -> Agent<'a, Net, FixedObligations> {
        Agent {
            a: &self.net,
            cfg: &self.agent.cfg,
            keys: &self.escrow,
            rid: self.rid,
            source: src,
        }
    }

    pub async fn audit(&self, src: &FixedObligations, grace: u64) -> AuditReport {
        charter_escrow::audit(&self.net, &self.rid, src, grace)
            .await
            .expect("audit")
    }

    pub async fn balance(&self, a: Address) -> U256 {
        self.net.balance(a).await.expect("balance")
    }

    pub async fn head(&self) -> u64 {
        self.net.head().await.expect("head")
    }
}

pub fn pay(key: &str, to: Address, amount: u128, purpose: &str) -> Obligation {
    Obligation {
        kind: "pay".into(),
        to: to.to_string(),
        amount: amount.to_string(),
        reason: "test".into(),
        key: key.into(),
        purpose: purpose.into(),
        since_block: 0,
    }
}

/// Runs a test body on 8 MiB stacks: in debug builds the write path's nested futures plus
/// ML-DSA verification need more than libtest's 2 MiB thread stack.
pub fn big(f: impl std::future::Future<Output = ()> + Send + 'static) {
    const STACK: usize = 8 << 20;
    let t = std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(STACK)
                .build()
                .expect("runtime")
                .block_on(f)
        });
    if let Err(p) = t.expect("spawn").join() {
        std::panic::resume_unwind(p);
    }
}

pub fn has(flags: &[Flag], pred: impl Fn(&Flag) -> bool) -> bool {
    flags.iter().any(pred)
}
