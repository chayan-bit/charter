//! Live Rabdi probe for the head model's rollback rule (docs/p5/head-model.md 6): a patch,
//! an extend and a reverted tx from a device control address `C_d` leave
//! `entityNonce(C_d)` unchanged, so only creates move it.
//!
//! Runs against an existing repo whose device (from `$CHARTER_HOME/device/seed`) has a
//! head; it re-patches the head with its own current body, which changes nothing a reader
//! trusts. Keys are loaded by the code and never printed.
//!
//! `CHARTER_RABDI=1 CHARTER_HOME=<home> CHARTER_PROBE_RID=<rid> cargo test -p charter-cli
//! --test rabdi_head_nonce -- --ignored --nocapture`

use charter_cli::backend::{Backend, connect};
use charter_core::{HeadBody, Kind};
use charter_store::{
    ARKIV_ADDRESS, Arkiv, B256, SignedTx, TxParams, U256, execute_calldata, extend_op, patch_op,
};
use charter_sync::observe::{all, b32, kind};
use charter_sync::write::{entity_patch_op, send};

#[test]
#[ignore = "live Rabdi: needs CHARTER_RABDI=1, CHARTER_HOME and CHARTER_PROBE_RID"]
fn patch_extend_and_revert_leave_entity_nonce_unchanged() {
    if std::env::var("CHARTER_RABDI").as_deref() != Ok("1") {
        return;
    }
    charter_cli::block_on(async {
        let (backend, cfg) = connect().await.expect("connect");
        let Backend::Rpc(a) = backend else {
            panic!("needs the Rabdi backend")
        };
        assert_eq!(
            a.chain_id().await.expect("chain"),
            7_794_095,
            "testnet only"
        );
        let rid_text = std::env::var("CHARTER_PROBE_RID").expect("CHARTER_PROBE_RID");
        let rid = charter_sync::ids::parse_url(rid_text.trim_start_matches("arkiv::"))
            .expect("rid")
            .rid;
        let dev = charter_sync::device::device_keys(&cfg.home).expect("device");
        let (c_d, signer) = (dev.control_address(&rid), dev.control(&rid));
        let filter = charter_store::Pred::And(vec![
            kind(Kind::Head),
            b32("ag.rid", &rid),
            b32("ag.dev", &dev.nid()),
        ]);
        let head = all(&a, filter).await.expect("query").pop().expect("a head");
        let env = match head.attr("ag.body") {
            Some(charter_core::AttrValue::Bytes(b)) => b.clone(),
            _ => panic!("head without a body"),
        };
        let nonce = || async { a.entity_nonce(c_d).await.expect("nonce") };
        let cfg = &cfg.pipeline;
        let n0 = nonce().await;

        let patch = entity_patch_op::<HeadBody>(head.key, &env).expect("op");
        let l = send(&a, &signer, cfg, vec![vec![patch]])
            .await
            .expect("patch");
        let n1 = nonce().await;
        // Past the current expiry, or the node refuses it (`ExpiryNotExtended`).
        let left = head.expires_at - a.head().await.expect("head");
        let extend = extend_op(B256::from(head.key), left + 2_000);
        let l2 = send(&a, &signer, cfg, vec![vec![extend]])
            .await
            .expect("extend");
        let n2 = nonce().await;
        // A patch of an entity that does not exist, sent without an estimate (which would
        // refuse it), so it is mined and reverts on chain.
        let missing = patch_op(B256::repeat_byte(0x5a), &[]).expect("op");
        let fee = a.base_fee().await.expect("fee") * 2 + cfg.tip;
        let params = TxParams {
            nonce: a.tx_count(c_d).await.expect("count"),
            to: ARKIV_ADDRESS,
            value: U256::ZERO,
            input: execute_calldata(&[missing]),
            gas_limit: 200_000,
            max_fee_per_gas: fee,
            max_priority_fee_per_gas: cfg.tip,
        };
        let tx = SignedTx::sign(&signer, 7_794_095, params).expect("sign");
        a.send(tx.clone()).await.expect("send");
        let receipt = loop {
            if let Some(r) = a.receipt(tx.hash()).await.expect("receipt") {
                break r;
            }
            tokio::time::sleep(cfg.poll.max(std::time::Duration::from_secs(1))).await;
        };
        let reverted = !receipt.success;
        let n3 = nonce().await;

        eprintln!(
            "entityNonce: start {n0}, after patch {n1} (gas {}), after extend {n2} (gas {}), \
             after revert {n3} (revert surfaced: {})",
            l[0].receipt.gas_used, l2[0].receipt.gas_used, reverted
        );
        assert!(reverted, "the patch of a missing entity reverts");
        assert_eq!((n1, n2, n3), (n0, n0, n0));
    })
    .expect("runtime");
}
