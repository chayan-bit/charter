//! Live checks against Rabdi. `#[ignore]`d and gated: run with
//! `CHARTER_RABDI=1 cargo test -p charter-store --test rabdi -- --ignored --test-threads=1`.
//! Read-only tests need no funds. The funded test uses only the throwaway key in
//! `spike/arkiv-measure/.env`, never prints it, keeps lifetimes at or under 1 day and
//! asserts it spent under 0.01 tGLM.

mod racing;

use std::collections::HashMap;
use std::time::Duration;

use alloy_signer_local::PrivateKeySigner;
use charter_store::{
    Address, Arkiv, AttributeValue as V, Attrs, Field, PipelineConfig, Pred, Query, QueryOpts,
    RABDI_CHAIN_ID, RABDI_RPC, RowExt, RpcArkiv, RpcConfig, Select, StoreError, TxPipeline, U256,
    Value, attr, create_op, derive_key, extend_op, random_salt,
};
use serde_json::{Value as Json, json};

/// The spike's throwaway account; it has created entities, so its nonce is > 0.
const SPIKE: &str = "0x201b7FB60f8f4B221afCEcEcAeeB6d09Af42f15A";
const HOUR: u64 = 1_800;
const MAX_SPEND_WEI: u128 = 10_000_000_000_000_000; // 0.01 tGLM

fn enabled() -> bool {
    let on = std::env::var("CHARTER_RABDI").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set CHARTER_RABDI=1");
    }
    on
}

/// `KEY=value` lines of the spike's `.env`; values are never printed.
fn spike_env() -> HashMap<String, String> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../spike/arkiv-measure/.env"
    );
    let text = std::fs::read_to_string(path).expect("spike/arkiv-measure/.env (see its README)");
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().trim_matches('"').to_string()))
        .collect()
}

async fn rpc() -> RpcArkiv {
    let key = spike_env().remove("ARKIV_ACCESS_KEY");
    RpcArkiv::connect(RpcConfig::rabdi(key))
        .await
        .expect("connect to Rabdi")
}

/// One raw `arkiv_query`, for inputs the typed client refuses to build.
async fn raw_query(q: &str, opts: Json) -> Json {
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "arkiv_query", "params": [q, opts] });
    let resp = reqwest::Client::new()
        .post(RABDI_RPC)
        .json(&body)
        .send()
        .await
        .expect("post");
    resp.json().await.expect("json")
}

#[tokio::test]
#[ignore = "live Rabdi; set CHARTER_RABDI=1"]
async fn rabdi_read_only() {
    if !enabled() {
        return;
    }
    let wrong = RpcConfig {
        chain_id: 1,
        ..RpcConfig::rabdi(None)
    };
    let err = RpcArkiv::connect(wrong).await.expect_err("chain guard");
    assert_eq!(
        err,
        StoreError::WrongChain {
            expected: 1,
            got: RABDI_CHAIN_ID
        }
    );
    let a = rpc().await;
    assert!(a.head().await.expect("head") > 0);
    assert!(
        a.base_fee().await.expect("base fee") >= 1_000_000_000,
        "1 gwei floor"
    );
    let spike: Address = SPIKE.parse().expect("addr");
    assert!(a.entity_nonce(spike).await.expect("entityNonce") > 0);
    // Queries spend the per-IP cost budget; when it is gone the node answers 429 and
    // the client must surface E_RATE_LIMITED with the server's Retry-After, not hang.
    let q = Query::from(Pred::eq(Field::Owner, spike));
    match a.query(&q, &QueryOpts::count_only()).await {
        Ok(page) => assert!(page.count.is_some() && page.block_number > 0, "{page:?}"),
        Err(StoreError::RateLimited { retry_after }) => {
            eprintln!("query budget exhausted, Retry-After {retry_after:?}; skipping query checks");
            assert!(retry_after.is_some());
            return;
        }
        Err(e) => panic!("{e:?}"),
    }
    let over = raw_query("*", json!({ "limit": "0x65" })).await;
    assert!(
        over.get("error").is_some(),
        "limit 101 must be refused: {over}"
    );
    let not = raw_query("NOT (ag.kind = str('x'))", json!({})).await;
    assert!(not.get("error").is_some(), "NOT must be refused: {not}");
    let ne = raw_query("ag.kind != str('x')", json!({})).await;
    assert!(ne.get("error").is_some(), "!= must be refused: {ne}");
}

#[tokio::test]
#[ignore = "live Rabdi; set CHARTER_RABDI=1"]
async fn rabdi_timeouts_bound_every_call() {
    if !enabled() {
        return;
    }
    let cfg = RpcConfig {
        timeout: Duration::from_secs(5),
        max_rate_wait: Duration::ZERO,
        ..RpcConfig::rabdi(None)
    };
    let a = RpcArkiv::connect(cfg).await.expect("connect");
    let t = std::time::Instant::now();
    for _ in 0..5 {
        match a.head().await {
            Ok(_) | Err(StoreError::RateLimited { .. }) => {}
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(
        t.elapsed() < Duration::from_secs(30),
        "no call waits out a rate limit with max_rate_wait = 0"
    );
}

#[tokio::test]
#[ignore = "live Rabdi, spends < 0.01 tGLM; set CHARTER_RABDI=1"]
async fn rabdi_funded_create_query_extend() {
    if !enabled() {
        return;
    }
    let env = spike_env();
    let owner: PrivateKeySigner = env
        .get("PRIVATE_KEY")
        .expect("PRIVATE_KEY")
        .parse()
        .expect("key");
    let a = rpc().await;
    let me = owner.address();
    let start = a.balance(me).await.expect("balance");
    assert!(
        start > U256::from(MAX_SPEND_WEI),
        "fund {me} with at least 0.01 tGLM"
    );

    // Create 3 entities in 2 jobs; keys must match the client-side derivation.
    let kind = format!("ag-store-test-{:x}", random_salt() as u64);
    let salts = [random_salt(), random_salt(), random_salt()];
    let op = |i: usize| {
        let attrs = [
            ("ag.kind", V::Str(kind.clone()), true),
            ("ag.seq", V::U64(i as u64), true),
        ];
        create_op(&attrs, HOUR, salts[i]).expect("op")
    };
    let nonce0 = a.entity_nonce(me).await.expect("nonce");
    let mut p = TxPipeline::new(&a, owner.clone(), RABDI_CHAIN_ID, PipelineConfig::default());
    let landed = p
        .run(vec![vec![op(0), op(1)], vec![op(2)]])
        .await
        .expect("creates land");
    assert!(landed.iter().all(|l| l.receipt.success));
    let want: Vec<_> = (0..3)
        .map(|i| derive_key(RABDI_CHAIN_ID, me, nonce0 + i as u64, salts[i]))
        .collect();

    let sel = Select {
        expires_at: true,
        attrs: Attrs::Named(vec!["ag.seq".into()]),
        ..Select::default()
    };
    let q = Query::from(Pred::eq(attr("ag.kind"), kind.as_str()));
    let rows = a
        .query(
            &q,
            &QueryOpts {
                select: sel.clone(),
                ..QueryOpts::default()
            },
        )
        .await
        .expect("query")
        .rows;
    let mut got: Vec<_> = rows
        .iter()
        .map(|r| (r.attr_u64("ag.seq").expect("seq"), r.key.expect("key")))
        .collect();
    got.sort();
    assert_eq!(
        got.iter().map(|g| g.1).collect::<Vec<_>>(),
        want,
        "keys = keccak(chain, ARKIV, owner, nonce, salt)"
    );
    let before = rows
        .iter()
        .find(|r| r.key == Some(want[0]))
        .and_then(|r| r.expires_at)
        .expect("expiresAt");

    // Permissionless extend from a second, freshly funded throwaway key.
    let other = PrivateKeySigner::random();
    let gift = U256::from(300_000_000_000_000u128); // 0.0003 tGLM covers one extend
    assert!(
        p.transfer(other.address(), gift)
            .await
            .expect("transfer")
            .success
    );
    let mut q2 = TxPipeline::new(&a, other.clone(), RABDI_CHAIN_ID, PipelineConfig::default());
    let ext = q2
        .run(vec![vec![extend_op(want[0], 2 * HOUR)]])
        .await
        .expect("permissionless extend");
    let one = Query::from(Pred::eq(Field::Key, Value::Key(want[0])));
    let rows = a
        .query(
            &one,
            &QueryOpts {
                select: sel,
                ..QueryOpts::default()
            },
        )
        .await
        .expect("query")
        .rows;
    let after = rows[0].expires_at.expect("expiresAt");
    assert!(
        after > before && after <= ext[0].receipt.block_number + 2 * HOUR,
        "{before} -> {after}"
    );

    // The gift left in the discarded key is lost too, so count the owner's whole drop.
    let drop = start - a.balance(me).await.expect("balance");
    let gas = drop - gift + (gift - a.balance(other.address()).await.expect("b"));
    let tglm = |w: U256| f64::from(w) / 1e18;
    eprintln!(
        "owner balance fell {} tGLM (gas {} tGLM)",
        tglm(drop),
        tglm(gas)
    );
    assert!(drop < U256::from(MAX_SPEND_WEI));
}

/// What a funded live test may spend of `ARKIV_FUNDER_PRIVATE_KEY`: fees only, the rest is
/// swept back.
const COLLISION_BUDGET_WEI: u128 = 5_000_000_000_000_000; // 0.005 tGLM

/// Moves everything but one transfer's fee reserve from `from` back to `to`.
async fn sweep(a: &RpcArkiv, from: &PrivateKeySigner, to: Address) {
    let cfg = PipelineConfig::default();
    let have = a.balance(from.address()).await.expect("balance");
    let reserve = cfg.tx_reserve(21_000, U256::ZERO, a.base_fee().await.expect("fee"));
    if have <= reserve {
        return;
    }
    let value = have - reserve;
    TxPipeline::new(a, from.clone(), RABDI_CHAIN_ID, cfg)
        .transfers(vec![(to, value)])
        .await
        .expect("sweep");
    println!("transfer {} -> {to} {value} wei (sweep)", from.address());
}

/// Issue #28 on Rabdi: two devices of one identity top up from the shared wallet at the
/// same moment. The other device's transfer takes the wallet nonce this client read; the
/// prefund (wallet transfers) and the pipeline top-up both land anyway, with no nonce left
/// open. Every transfer is printed; the throwaway addresses are swept back to the funder.
///
/// `set -a; . ~/.config/charter/.env; set +a; CHARTER_RABDI=1 cargo test -p charter-store
/// --test rabdi rabdi_wallet_top_ups_survive_a_nonce_collision -- --ignored --nocapture`
#[tokio::test]
#[ignore = "live Rabdi: needs CHARTER_RABDI=1 and ARKIV_FUNDER_PRIVATE_KEY; spends fees only"]
async fn rabdi_wallet_top_ups_survive_a_nonce_collision() {
    if !enabled() {
        return;
    }
    let funder: PrivateKeySigner = std::env::var("ARKIV_FUNDER_PRIVATE_KEY")
        .expect("ARKIV_FUNDER_PRIVATE_KEY")
        .parse()
        .expect("key");
    let a = RpcArkiv::connect(RpcConfig::rabdi(std::env::var("ARKIV_ACCESS_KEY").ok()))
        .await
        .expect("connect to Rabdi");
    assert_eq!(
        a.chain_id().await.expect("chain"),
        RABDI_CHAIN_ID,
        "testnet only"
    );
    let cfg = PipelineConfig::default();
    let start = a.balance(funder.address()).await.expect("balance");
    let [wallet, dev_a, dev_b, dev_c] = std::array::from_fn(|_| PrivateKeySigner::random());
    let fund = U256::from(4_000_000_000_000_000u128); // 0.004 tGLM
    TxPipeline::new(&a, funder.clone(), RABDI_CHAIN_ID, cfg.clone())
        .transfers(vec![(wallet.address(), fund)])
        .await
        .expect("fund the wallet");
    println!(
        "transfer {} -> {} {fund} wei (fund)",
        funder.address(),
        wallet.address()
    );

    // The prefund path: wallet transfers, raced by device b's top-up.
    let racing = racing::Racing::new(&a, wallet.clone(), RABDI_CHAIN_ID, dev_b.address());
    let prefund = U256::from(500_000_000_000_000u128);
    let r = TxPipeline::new(&racing, wallet.clone(), RABDI_CHAIN_ID, cfg.clone())
        .transfers(vec![(dev_a.address(), prefund)])
        .await;
    println!(
        "transfer {} -> {} {prefund} wei (prefund): {r:?}",
        wallet.address(),
        dev_a.address()
    );
    r.expect("the prefund lands despite the collision");

    // The top-up path: device c's pipeline tops itself up from the wallet, raced again.
    racing.arm();
    let r = TxPipeline::new(&racing, dev_c.clone(), RABDI_CHAIN_ID, cfg.clone())
        .funded_by(wallet.clone())
        .transfers(vec![(wallet.address(), U256::from(1))])
        .await;
    println!(
        "pipeline of {} topped up from {}: {r:?}",
        dev_c.address(),
        wallet.address()
    );
    r.expect("the top-up lands despite the collision");

    let w = wallet.address();
    assert_eq!(
        a.tx_count(w).await.expect("pending"),
        a.tx_count_mined(w).await.expect("mined"),
        "no wallet nonce left open"
    );
    assert_eq!(
        a.balance(dev_b.address()).await.expect("b"),
        U256::from(2 * racing::OTHER_TOP_UP),
        "both racing top-ups landed"
    );
    for k in [&dev_a, &dev_b, &dev_c, &wallet] {
        sweep(&a, k, funder.address()).await;
    }
    let end = a.balance(funder.address()).await.expect("balance");
    let spent = start.saturating_sub(end);
    println!("spent {spent} wei of the funder");
    assert!(
        spent < U256::from(COLLISION_BUDGET_WEI),
        "spent {spent} wei"
    );
}
