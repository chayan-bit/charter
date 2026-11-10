//! `TxPipeline` against `MemArkiv` with injected faults (design 4.4, 4.6, 8.3 "Pacing").
#![cfg(feature = "fake")]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering::SeqCst;

use alloy_signer_local::PrivateKeySigner;
use charter_store::fake::{MAX_LIFETIME, MemArkiv};
use charter_store::heads::WsConfig;
use charter_store::pipeline::Landed;
use charter_store::{
    ARKIV_ADDRESS, Address, Arkiv, B256, PipelineConfig, SignedTx, StoreError, TxParams,
    TxPipeline, U256, execute_calldata, extend_op,
};
use common::{CHAIN, DAY, blob, exec, funded, small, with_miner};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

fn setup() -> (Arc<MemArkiv>, PrivateKeySigner) {
    let fake = Arc::new(MemArkiv::new(CHAIN));
    let key = funded(&fake);
    (fake, key)
}

async fn run(
    fake: &Arc<MemArkiv>,
    key: &PrivateKeySigner,
    jobs: Vec<Vec<charter_store::Operation>>,
) -> Result<Vec<Landed>, StoreError> {
    let mut p = TxPipeline::new(&**fake, key.clone(), CHAIN, PipelineConfig::default());
    with_miner(fake.clone(), p.run(jobs)).await
}

/// Gas per block from the receipts of one run.
fn gas_by_block(landed: &[Landed]) -> BTreeMap<u64, (u64, usize)> {
    let mut m: BTreeMap<u64, (u64, usize)> = BTreeMap::new();
    for l in landed {
        let e = m.entry(l.receipt.block_number).or_default();
        e.0 += l.receipt.gas_used;
        e.1 += 1;
    }
    m
}

#[tokio::test(start_paused = true)]
async fn chunks_land_paced_at_40_percent_of_block_gas() {
    let (fake, key) = setup();
    let jobs: Vec<_> = (0..40).map(|i| vec![blob(65_536, DAY, i)]).collect();
    let landed = run(&fake, &key, jobs).await.expect("run");
    assert_eq!(landed.len(), 40);
    assert!(
        landed.windows(2).all(|w| w[0].job < w[1].job),
        "one receipt per job, in job order"
    );
    assert_eq!(fake.entity_nonce(key.address()).await.expect("n"), 40);
    let per_block = gas_by_block(&landed);
    for (n, (gas, _)) in &per_block {
        assert!(*gas <= 12_000_000, "block {n} carried {gas} gas of ours");
        assert_eq!(fake.block_gas_of(*n, key.address()), *gas);
    }
    let best = per_block.values().map(|(_, c)| *c).max().expect("blocks");
    assert_eq!(
        best, 4,
        "1-day 64 KiB chunks: 4 per block under the 12M budget"
    );
}

#[tokio::test(start_paused = true)]
async fn the_window_caps_small_txs_at_16_per_block() {
    let (fake, key) = setup();
    let jobs: Vec<_> = (0..40).map(|i| vec![small("w", i, i.into())]).collect();
    let landed = run(&fake, &key, jobs).await.expect("run");
    let per_block = gas_by_block(&landed);
    assert!(per_block.values().all(|(_, c)| *c <= 16));
    assert_eq!(per_block.values().map(|(_, c)| *c).max(), Some(16));
}

#[tokio::test(start_paused = true)]
async fn a_high_base_fee_halves_the_budget() {
    let (fake, key) = setup();
    fake.set_base_fee(20_000_000_000);
    let jobs: Vec<_> = (0..8).map(|i| vec![blob(65_536, DAY, i)]).collect();
    let landed = run(&fake, &key, jobs).await.expect("run");
    let first = gas_by_block(&landed).into_iter().next().expect("a block").1;
    assert!(first.0 <= 6_000_000 && first.1 == 2, "{first:?}");
}

#[tokio::test(start_paused = true)]
async fn dropped_txs_are_resent_until_they_land() {
    let (fake, key) = setup();
    fake.drop_sends(2, 1);
    let jobs: Vec<_> = (0..6).map(|i| vec![small("d", i, i.into())]).collect();
    let landed = run(&fake, &key, jobs).await.expect("run");
    assert_eq!(landed.len(), 6);
    assert_eq!(
        fake.entity_nonce(key.address()).await.expect("n"),
        6,
        "nothing landed twice"
    );
}

#[tokio::test(start_paused = true)]
async fn a_tx_dropped_on_every_resend_is_reported() {
    let (fake, key) = setup();
    fake.drop_next(100);
    let err = run(&fake, &key, vec![vec![small("d", 0, 0)]])
        .await
        .expect_err("dropped");
    assert!(matches!(err, StoreError::Dropped(_)), "{err:?}");
    assert_eq!(err.code(), "E_RPC");
}

#[tokio::test(start_paused = true)]
async fn a_dropped_tx_below_others_is_plugged_so_they_land() {
    let (fake, key) = setup();
    // Sends 0 to 2 are nonces 0 to 2; nonce 0 and its three resends (sends 3 to 5, the
    // others are already known) are dropped, so nonces 1 and 2 wait on it.
    fake.drop_sends(0, 1);
    fake.drop_sends(3, 3);
    let jobs = (0..3).map(|i| vec![small("g", i, i.into())]).collect();
    let err = run(&fake, &key, jobs).await.expect_err("dropped");
    assert!(matches!(err, StoreError::Dropped(_)), "{err:?}");
    assert_eq!(fake.pending(), 0, "nothing left waiting on the gap");
    assert_eq!(fake.tx_count_mined(key.address()).await.expect("n"), 3);
    assert_eq!(fake.entity_nonce(key.address()).await.expect("n"), 2);
}

#[tokio::test(start_paused = true)]
async fn a_send_failing_below_others_is_plugged_then_reported() {
    let (fake, key) = setup();
    let spy = Spy {
        refuse: Some((1, 3.into())),
        ..Spy::new(fake.clone(), false)
    };
    let mut p = TxPipeline::new(&spy, key.clone(), CHAIN, PipelineConfig::default());
    let jobs = (0..5).map(|i| vec![small("s", i, i.into())]).collect();
    let err = with_miner(fake.clone(), p.run(jobs))
        .await
        .expect_err("send keeps failing");
    assert_eq!(err.code(), "E_RPC", "{err:?}");
    assert_eq!(fake.pending(), 0, "nonces 2 to 4 are not stranded");
    assert_eq!(fake.tx_count_mined(key.address()).await.expect("n"), 5);
    assert_eq!(fake.entity_nonce(key.address()).await.expect("n"), 4);
    // The next run starts from the node's nonce and works.
    with_miner(fake.clone(), p.run(vec![vec![small("s", 9, 9)]]))
        .await
        .expect("next run");
    assert_eq!(fake.tx_count_mined(key.address()).await.expect("n"), 6);
}

#[tokio::test(start_paused = true)]
async fn reverts_are_decoded() {
    let (fake, key) = setup();
    let err = run(&fake, &key, vec![vec![extend_op(B256::repeat_byte(9), 10)]])
        .await
        .expect_err("revert");
    assert!(
        matches!(&err, StoreError::Reverted { tx: None, reason } if reason == "EntityNotFound"),
        "{err:?}"
    );
    assert_eq!(err.code(), "E_INTERNAL");
    // A mined failure that does not reproduce on re-estimate is sent once more.
    fake.revert_next_at(0);
    let landed = run(&fake, &key, vec![vec![small("r", 0, 0)]])
        .await
        .expect("retried once");
    assert_eq!(landed.len(), 1);
    // A second failure of the same job is the error.
    let spy = Spy::new(fake.clone(), true);
    let mut p = TxPipeline::new(&spy, key.clone(), CHAIN, PipelineConfig::default());
    let err = with_miner(fake.clone(), p.run(vec![vec![small("r", 1, 1)]]))
        .await
        .expect_err("mined revert twice");
    assert!(
        matches!(&err, StoreError::Reverted { tx: Some(_), .. }),
        "{err:?}"
    );
}

/// `MemArkiv` that counts estimates and, when `revert`, makes every mined tx revert.
struct Spy {
    inner: Arc<MemArkiv>,
    revert: bool,
    estimates: std::sync::atomic::AtomicUsize,
    ws: Option<WsConfig>,
    /// Refuse this many sends of the tx with this nonce, as a flaky RPC would.
    refuse: Option<(u64, std::sync::atomic::AtomicU32)>,
}

impl Spy {
    fn new(inner: Arc<MemArkiv>, revert: bool) -> Self {
        Self {
            inner,
            revert,
            estimates: Default::default(),
            ws: None,
            refuse: None,
        }
    }
    fn estimates(&self) -> usize {
        self.estimates.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Arkiv for Spy {
    fn heads_ws(&self) -> Option<WsConfig> {
        self.ws.clone()
    }
    async fn chain_id(&self) -> Result<u64, StoreError> {
        self.inner.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.inner.head().await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.inner.base_fee().await
    }
    async fn query(
        &self,
        q: &charter_store::Query,
        o: &charter_store::QueryOpts,
    ) -> Result<charter_store::QueryPage, StoreError> {
        self.inner.query(q, o).await
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
    async fn estimate(
        &self,
        from: Address,
        ops: &[charter_store::Operation],
    ) -> Result<u64, StoreError> {
        self.estimates
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.inner.estimate(from, ops).await
    }
    async fn send(&self, tx: SignedTx) -> Result<charter_store::TxHash, StoreError> {
        if self.revert {
            self.inner.revert_next_at(0);
        }
        if let Some((n, left)) = &self.refuse
            && tx.nonce() == *n
            && left
                .fetch_update(SeqCst, SeqCst, |l| l.checked_sub(1))
                .is_ok()
        {
            return Err(StoreError::Rpc("injected: connection reset".into()));
        }
        self.inner.send(tx).await
    }
    async fn receipt(
        &self,
        h: charter_store::TxHash,
    ) -> Result<Option<charter_store::Receipt>, StoreError> {
        self.inner.receipt(h).await
    }
    async fn transaction(
        &self,
        h: charter_store::TxHash,
    ) -> Result<Option<charter_store::TxInfo>, StoreError> {
        self.inner.transaction(h).await
    }
}

#[tokio::test(start_paused = true)]
async fn one_estimate_prices_every_chunk_of_a_shape() {
    let (fake, key) = setup();
    let spy = Spy::new(fake.clone(), false);
    let mut p = TxPipeline::new(&spy, key.clone(), CHAIN, PipelineConfig::default());
    let mut jobs: Vec<_> = (0..12).map(|i| vec![blob(65_536, DAY, i)]).collect();
    jobs.push(vec![blob(1_000, DAY, 99)]);
    let landed = with_miner(fake.clone(), p.run(jobs)).await.expect("run");
    assert_eq!(landed.len(), 13);
    assert_eq!(
        spy.estimates(),
        2,
        "one per shape: 64 KiB chunks and the tail"
    );
    assert!(landed.iter().all(|l| l.receipt.success));
    let off = PipelineConfig {
        reuse_estimates: false,
        ..PipelineConfig::default()
    };
    let spy = Spy::new(fake.clone(), false);
    let mut p = TxPipeline::new(&spy, key.clone(), CHAIN, off);
    let jobs: Vec<_> = (0..3).map(|i| vec![blob(65_536, DAY, 100 + i)]).collect();
    with_miner(fake.clone(), p.run(jobs)).await.expect("run");
    assert_eq!(spy.estimates(), 3, "reuse can be switched off");
}

#[tokio::test(start_paused = true)]
async fn bulk_keeps_two_blocks_of_gas_in_flight() {
    let (fake, key) = setup();
    let cfg = PipelineConfig::default().bulk();
    let mut p = TxPipeline::new(&*fake, key.clone(), CHAIN, cfg);
    let jobs: Vec<_> = (0..40).map(|i| vec![blob(65_536, DAY, i)]).collect();
    let (landed, t) = with_miner(fake.clone(), p.run_measured(jobs))
        .await
        .expect("run");
    assert_eq!((landed.len(), t.txs), (40, 40));
    assert!(t.bytes > 40 * 65_536 && t.mb_per_s() > 0.0);
    let best = gas_by_block(&landed).values().map(|(_, c)| *c).max();
    assert!(
        best > Some(4),
        "more than the 4 chunks per block the 40 percent pace allows: {best:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn jobs_over_the_byte_cap_are_split() {
    let (fake, key) = setup();
    let landed = run(
        &fake,
        &key,
        vec![(0..3).map(|i| blob(60_000, DAY, i)).collect()],
    )
    .await
    .expect("run");
    assert!(landed.len() >= 2 && landed.iter().all(|l| l.job == 0));
    assert_eq!(fake.entity_nonce(key.address()).await.expect("n"), 3);
}

#[tokio::test(start_paused = true)]
async fn jobs_over_the_gas_cap_are_split_and_single_ops_fail() {
    let (fake, key) = setup();
    let job: Vec<_> = (0..8).map(|i| blob(16_384, MAX_LIFETIME, i)).collect();
    let landed = run(&fake, &key, vec![job]).await.expect("run");
    assert!(landed.len() >= 2);
    assert_eq!(fake.entity_nonce(key.address()).await.expect("n"), 8);
    let err = run(&fake, &key, vec![vec![blob(126 * 1024, MAX_LIFETIME, 99)]])
        .await
        .expect_err("too big");
    assert_eq!(err.code(), "E_GAS_CAP");
}

#[tokio::test(start_paused = true)]
async fn a_stolen_nonce_is_reread() {
    let (fake, key) = setup();
    let mut p = TxPipeline::new(&*fake, key.clone(), CHAIN, PipelineConfig::default());
    with_miner(fake.clone(), p.run(vec![vec![small("n", 0, 0)]]))
        .await
        .expect("first");
    exec(&fake, &key, vec![small("n", 1, 1)]).await;
    with_miner(fake.clone(), p.run(vec![vec![small("n", 2, 2)]]))
        .await
        .expect("after re-read");
    assert_eq!(fake.entity_nonce(key.address()).await.expect("n"), 3);
}

#[tokio::test(start_paused = true)]
async fn transfers_and_insufficient_funds() {
    let (fake, key) = setup();
    let to = Address::repeat_byte(0x42);
    let mut p = TxPipeline::new(&*fake, key.clone(), CHAIN, PipelineConfig::default());
    assert_eq!(p.address(), key.address());
    let r = with_miner(fake.clone(), p.transfer(to, U256::from(1_000)))
        .await
        .expect("transfer");
    assert!(r.success);
    assert_eq!(fake.balance(to).await.expect("bal"), U256::from(1_000));
    let poor = PrivateKeySigner::random();
    let err = run(&fake, &poor, vec![vec![small("p", 0, 0)]])
        .await
        .expect_err("no funds");
    assert_eq!(err.code(), "E_INSUFFICIENT_FUNDS");
}

#[tokio::test]
async fn send_refuses_a_tx_already_too_large_for_the_rpc() {
    let fake = MemArkiv::new(CHAIN);
    let key = funded(&fake);
    let p = TxParams {
        nonce: 0,
        to: ARKIV_ADDRESS,
        value: U256::ZERO,
        input: execute_calldata(&[blob(131_000, DAY, 0)]),
        gas_limit: 6_000_000,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 0,
    };
    let err = fake
        .send(SignedTx::sign(&key, CHAIN, p).expect("sign"))
        .await
        .expect_err("too big");
    assert_eq!(err.code(), "E_TX_TOO_LARGE");
}

/// A `newHeads` server that mines `fake` and announces the block every 50 ms, for one
/// connection; returns its URL and how many subscriptions it served.
async fn mining_heads(fake: Arc<MemArkiv>) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("ws://{}", l.local_addr().expect("addr"));
    let subs = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = subs.clone();
    tokio::spawn(async move {
        let (s, _) = l.accept().await.expect("accept");
        let mut ws = tokio_tungstenite::accept_async(s).await.expect("handshake");
        let _ = ws.next().await;
        seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let ok = serde_json::json!({"jsonrpc": "2.0", "id": 1, "result": "0xab"});
        ws.send(Message::text(ok.to_string())).await.expect("reply");
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            fake.mine();
            let n = fake.head().await.expect("head");
            let v = serde_json::json!({"jsonrpc": "2.0", "method": "eth_subscription",
                "params": {"subscription": "0xab", "result": {"number": format!("{n:#x}")}}});
            if ws.send(Message::text(v.to_string())).await.is_err() {
                return;
            }
        }
    });
    (url, subs)
}

#[tokio::test]
async fn inclusion_is_awaited_on_new_heads() {
    let (fake, key) = setup();
    let (url, subs) = mining_heads(fake.clone()).await;
    let mut spy = Spy::new(fake.clone(), false);
    spy.ws = Some(WsConfig {
        url,
        access_key: None,
    });
    // A poll interval far longer than the test: only the socket can wake the pipeline.
    let cfg = PipelineConfig {
        poll: std::time::Duration::from_secs(600),
        ..PipelineConfig::default()
    };
    let mut p = TxPipeline::new(&spy, key.clone(), CHAIN, cfg);
    let jobs: Vec<_> = (0..20).map(|i| vec![small("h", i, i.into())]).collect();
    let landed = tokio::time::timeout(std::time::Duration::from_secs(30), p.run(jobs))
        .await
        .expect("woken by heads, not the timer")
        .expect("run");
    assert_eq!(landed.len(), 20);
    assert_eq!(subs.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[tokio::test(start_paused = true)]
async fn a_dead_socket_falls_back_to_polling() {
    let (fake, key) = setup();
    let mut spy = Spy::new(fake.clone(), false);
    // Nothing listens on port 9 of localhost: the subscription fails at once.
    spy.ws = Some(WsConfig {
        url: "ws://127.0.0.1:9".into(),
        access_key: None,
    });
    let mut p = TxPipeline::new(&spy, key.clone(), CHAIN, PipelineConfig::default());
    let landed = with_miner(fake.clone(), p.run(vec![vec![small("d", 0, 0)]]))
        .await
        .expect("polled");
    assert_eq!(landed.len(), 1);
}
