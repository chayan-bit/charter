//! Live P1 gate on Rabdi (design 8.3, 8.5, 8.6) through the real binaries: blobs of
//! 1 MiB noise pushed with `git push arkiv::`, a second small commit pushed, a fresh
//! `git clone arkiv::` in a second home compared byte for byte, then the replay checks of
//! the sync gate (old anchor and refs re-posted from a foreign address and from the peer's
//! own control address; a view hiding the newest anchor gives `E_STALE_HEAD`).
//!
//! Pays from the throwaway testnet key in `spike/arkiv-measure/.env`, which it never prints.
//! A zero-spend preflight prices the run from `eth_estimateGas` under the rolling top-up
//! rule (design 4.2 step 6) and picks 10 MiB, or 8 MiB (the fake gate's CI size) when 10
//! does not fit a 0.60 tGLM wallet with a 0.05 margin, else stops before any send.
//! The fresh wallet gets 0.59 tGLM, which bounds the spend under the 0.60 cap; a push is
//! killed if the base fee rises so far that the projected spend would breach the cap.
//! Every entity lives 1 day; leftover funds are swept back even on panic.
//!
//! `CHARTER_RABDI=1 cargo test --release -p charter-cli --test rabdi_gate -- --ignored --nocapture`
#![cfg(unix)]

mod common;

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use charter_core::{AnchorBody, Kind, Observed, RefsBody, Rid, chunk_count};
use charter_store::{Address, Arkiv, B256, Operation, PipelineConfig, RpcArkiv, U256};
use charter_sync::Keys;
use charter_sync::write::{chunk_op, entity_op};
use common::live::*;

const CAP_WEI: u128 = 600_000_000_000_000_000; // 0.6 tGLM net spend
const FUND_WEI: u128 = 590_000_000_000_000_000; // 0.59 tGLM to the fresh wallet
const MARGIN_WEI: u128 = 50_000_000_000_000_000; // 0.05 tGLM under the cap
const LIFETIME: &str = "43200"; // blocks: 1 day at 2 s for every entity
const LIFETIME_BLOCKS: u64 = 43_200;
const FILE_BYTES: usize = 1 << 20;
/// Gas outside the chunks: init, two anchors, refs and manifests, the second push, the
/// replays and the top-up transfers (itemized about 3M at a 1 day lifetime).
const OVERHEAD_GAS: u64 = 5_000_000;
/// Largest control tx (init's peer + bind + id), for the control address's reserve.
const CONTROL_TX_GAS: u64 = 3_000_000;

/// What the preflight settled on.
struct Plan {
    files: u64,
    gas: u64,
}

#[test]
#[ignore = "live Rabdi: needs CHARTER_RABDI=1 and the funded throwaway key"]
fn live_rabdi_gate() {
    if !enabled() {
        return;
    }
    let s = secrets();
    let rpc = connect(&s);
    let before = balance(&rpc, s.key.address());
    assert!(
        before >= U256::from(CAP_WEI),
        "throwaway key holds less than the cap"
    );
    let plan = preflight(&rpc, s.key.address());

    let tmp = tempfile::tempdir().expect("tmp");
    let (alice, bob) = (tmp.path().join("alice"), tmp.path().join("bob"));
    [&alice, &bob]
        .iter()
        .for_each(|d| std::fs::create_dir_all(d).expect("home"));
    run(
        &alice,
        &s.access,
        LIFETIME,
        env!("CARGO_BIN_EXE_chart"),
        &alice,
        &["keys", "init"],
    );
    let keys = Keys::require(&alice.join(".charter")).expect("keys");
    transfer(&rpc, &s.key, keys.wallet().address(), U256::from(FUND_WEI));

    // `chart init` makes no local cache, so the rid comes back from the gate itself.
    let rid = std::cell::Cell::new(None);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        gate(&rpc, &s, &keys, &plan, tmp.path(), &rid)
    }));

    let mut swept = rids(&alice);
    swept.extend(rid.get().filter(|r| !swept.contains(r)));
    sweep(&rpc, &keys, &swept, s.key.address());
    let after = balance(&rpc, s.key.address());
    let spent = before.saturating_sub(after);
    eprintln!(
        "gate: net spend {} tGLM, throwaway now holds {} tGLM, {} rate-limit waits in the binaries",
        charter_cli::units::fmt_glm(spent),
        charter_cli::units::fmt_glm(after),
        RATE_LIMITED.load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(spent < U256::from(CAP_WEI), "spent over the cap");
    if let Err(p) = result {
        std::panic::resume_unwind(p);
    }
}

/// Prices the run from a live chunk estimate without sending anything: predicted spend at
/// the base fee plus the bulk window's and control's max-fee reserves must leave the margin
/// under the cap; 10 MiB if it does, else 8 MiB, else stop.
fn preflight(rpc: &RpcArkiv, from: Address) -> Plan {
    let probe = chunk_op(
        &[0; 32],
        &[0; 32],
        0,
        &vec![0xa5; charter_core::CHUNK_SIZE],
        LIFETIME_BLOCKS,
    )
    .expect("probe");
    let (chunk_gas, base_fee) = charter_cli::block_on(async {
        (
            rpc.estimate(from, &[probe]).await.expect("estimate"),
            rpc.base_fee().await.expect("fee"),
        )
    })
    .expect("rt");
    let pc = PipelineConfig::default();
    let glm = charter_cli::units::fmt_glm;
    for files in [10, 8] {
        // The pack is the blobs plus a few hundred bytes of trees, commits and header.
        let chunks = chunk_count(files * FILE_BYTES as u64 + 4_096);
        let gas = chunks * chunk_gas + OVERHEAD_GAS;
        let spend = U256::from(gas) * U256::from(base_fee + pc.tip);
        let reserve =
            pc.reserve(chunks, chunk_gas, base_fee) + pc.reserve(1, CONTROL_TX_GAS, base_fee);
        let need = spend + reserve;
        eprintln!(
            "gate preflight {files} MiB: {chunks} chunks x {chunk_gas} gas + {OVERHEAD_GAS} = {gas} \
             gas at base fee {base_fee} wei: spend {} + reserve {} = {} tGLM",
            glm(spend),
            glm(reserve),
            glm(need)
        );
        if need + U256::from(MARGIN_WEI) <= U256::from(CAP_WEI) {
            return Plan { files, gas };
        }
    }
    panic!("even 8 MiB does not fit the budget; stopping before any send");
}

fn gate(
    rpc: &RpcArkiv,
    s: &Secrets,
    keys: &Keys,
    plan: &Plan,
    tmp: &Path,
    rid_out: &std::cell::Cell<Option<Rid>>,
) {
    let (alice, bob) = (&tmp.join("alice"), &tmp.join("bob"));
    let run = |home: &Path, program: &str, dir: &Path, args: &[&str]| {
        common::live::run(home, &s.access, LIFETIME, program, dir, args)
    };
    let files = plan.files;
    let created = run(
        alice,
        env!("CARGO_BIN_EXE_chart"),
        alice,
        &["init", "--name", "gate"],
    );
    let url = created
        .lines()
        .find_map(|l| l.strip_prefix("url: "))
        .expect("url")
        .to_owned();
    eprintln!("gate repo {url}");
    let rid = charter_sync::ids::parse_id(url.strip_prefix("arkiv::").expect("arkiv:: url"))
        .expect("rid");
    rid_out.set(Some(rid));
    let work = tmp.join("w");
    run(
        alice,
        "git",
        tmp,
        &["init", "-q", "-b", "main", &work.to_string_lossy()],
    );
    for i in 0..files {
        std::fs::write(
            work.join(format!("blob-{i:03}")),
            noise(1_000 + i, FILE_BYTES),
        )
        .expect("write");
    }
    run(alice, "git", &work, &["add", "-A"]);
    run(alice, "git", &work, &["commit", "-q", "-m", "feat: noise"]);

    let push = || {
        command(
            alice,
            &s.access,
            LIFETIME,
            "git",
            &work,
            &["push", "-q", &url, "main"],
        )
    };
    measured_push(
        rpc,
        keys,
        &rid,
        &format!("A ({files} MiB)"),
        plan.gas,
        tmp,
        push(),
    );
    let chunks = entities(rpc, &rid, Kind::Chunk).len();
    eprintln!("gate: {chunks} chunks live on Rabdi");
    assert!(chunks as u64 >= files * 16, "1 MiB is 16 chunks of 64 KiB");

    std::fs::write(work.join("B"), b"b").expect("write");
    run(alice, "git", &work, &["add", "-A"]);
    run(alice, "git", &work, &["commit", "-q", "-m", "feat: B"]);
    measured_push(rpc, keys, &rid, "B (small)", plan.gas, tmp, push());
    let head_b = run(alice, "git", &work, &["rev-parse", "HEAD"]);

    // Replay A's anchor and refs from a foreign address (the throwaway key, no peer of the
    // repo) and A's anchor from alice's own control address, both after B.
    let control = keys.control_address(&rid);
    let anchors = entities(rpc, &rid, Kind::Anchor);
    let anchor_a = anchors
        .iter()
        .find(|o| seq_of(o) == 0 && o.creator == control.into_array())
        .expect("A");
    let refs_a = entities(rpc, &rid, Kind::Refs)[0].clone();
    let replay_anchor = entity_op::<AnchorBody>(&body_of(anchor_a), LIFETIME_BLOCKS).expect("op");
    let replay_refs = entity_op::<RefsBody>(&body_of(&refs_a), LIFETIME_BLOCKS).expect("op");
    send_ops(rpc, &s.key, None, vec![replay_anchor.clone(), replay_refs]);
    send_ops(
        rpc,
        &keys.control(&rid),
        Some(&keys.wallet()),
        vec![replay_anchor],
    );
    eprintln!("gate: replayed A's anchor and refs (foreign) and A's anchor (own control)");

    let dest = tmp.join("clone");
    let t = Instant::now();
    run(
        bob,
        "git",
        tmp,
        &["clone", "-q", &url, &dest.to_string_lossy()],
    );
    eprintln!("gate: fresh clone of {files} MiB took {:.1?}", t.elapsed());
    assert_eq!(
        run(bob, "git", &dest, &["rev-parse", "HEAD"]),
        head_b,
        "replay rejected: clone gets B"
    );
    assert_eq!(
        run(bob, "git", &dest, &["ls-tree", "-r", "HEAD"]),
        run(alice, "git", &work, &["ls-tree", "-r", "HEAD"])
    );
    run(bob, "git", &dest, &["fsck", "--full", "--strict"]);
    for i in [0, files / 2, files - 1] {
        let name = format!("blob-{i:03}");
        let same = std::fs::read(dest.join(&name)).expect("read") == noise(1_000 + i, FILE_BYTES);
        assert!(same, "{name} differs");
    }
    eprintln!("gate: clone matches HEAD, tree and bytes; fsck clean");

    stale_head_check(rpc, &bob.join(".charter"), &rid);
}

/// Design 8.5 step 4, last check: a client holding the mark at B (an honest sync first)
/// served a view that withholds B's anchor, with `entityNonce` lowered to match, sees only
/// A and its replays and must fail with `E_STALE_HEAD` rather than roll back. Read-only.
fn stale_head_check(rpc: &RpcArkiv, home: &Path, rid: &Rid) {
    let anchor_b = entities(rpc, rid, Kind::Anchor)
        .into_iter()
        .find(|o| seq_of(o) == 1)
        .expect("B's anchor (only the peer's control key wrote seq 1)");
    let control = Address::from(anchor_b.creator);
    let repo = charter_sync::Repo::open(home, rid).expect("repo");
    let want = charter_sync::Want::default();
    charter_cli::block_on(charter_sync::sync(rpc, &repo, &want))
        .expect("rt")
        .expect("honest sync sets the mark at B");
    let truth = charter_cli::block_on(rpc.entity_nonce(control))
        .expect("rt")
        .expect("nonce");
    let hiding = Hiding {
        inner: rpc,
        key: B256::from(anchor_b.key),
        control,
        nonce: truth - 1,
    };
    let stale = charter_cli::block_on(charter_sync::sync(&hiding, &repo, &want)).expect("rt");
    let code = stale.err().map(|e| e.code().as_str());
    assert_eq!(code, Some("E_STALE_HEAD"));
    eprintln!("gate: withholding view gives E_STALE_HEAD");
}

/// Re-runs only [`stale_head_check`] against a gate repo already on Rabdi
/// (`CHARTER_GATE_URL=arkiv::<rid>`, live while its 1 day lifetime lasts), from a fresh
/// home. Sends nothing; asserts the throwaway balance is unchanged.
#[test]
#[ignore = "live Rabdi, read-only: needs CHARTER_RABDI=1 and CHARTER_GATE_URL"]
fn live_rabdi_stale_head_recheck() {
    if !enabled() {
        return;
    }
    let url = std::env::var("CHARTER_GATE_URL").expect("CHARTER_GATE_URL");
    let rid =
        charter_sync::ids::parse_id(url.strip_prefix("arkiv::").unwrap_or(&url)).expect("rid");
    let s = secrets();
    let rpc = connect(&s);
    let before = balance(&rpc, s.key.address());
    let home = tempfile::tempdir().expect("home");
    stale_head_check(&rpc, home.path(), &rid);
    assert_eq!(balance(&rpc, s.key.address()), before, "read-only");
}

/// Runs the push `cmd` in its own process group, watching the base fee: if it climbs so
/// far that `plan_gas` at it would breach the cap, the whole group (git and the helper) is
/// killed and the test stops. Prints wall time, blocks, txs (nonce deltas of wallet,
/// control and bulk, top-ups included) and the base fee at start and peak.
fn measured_push(
    rpc: &RpcArkiv,
    keys: &Keys,
    rid: &Rid,
    label: &str,
    plan_gas: u64,
    tmp: &Path,
    mut cmd: Command,
) {
    let addrs = [
        keys.wallet().address(),
        keys.control_address(rid),
        keys.bulk_address(rid),
    ];
    let snap = || {
        charter_cli::block_on(async {
            let mut n = 0;
            for a in &addrs {
                n += rpc.tx_count(*a).await.expect("nonce");
            }
            let fee = rpc.base_fee().await.expect("fee");
            (rpc.head().await.expect("head"), n, fee)
        })
        .expect("rt")
    };
    let (b0, n0, fee0) = snap();
    let log_path = tmp.join("push.log");
    let log = std::fs::File::create(&log_path).expect("log");
    let t = Instant::now();
    let mut child = cmd
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .expect("spawn push");
    let mut peak = fee0;
    let status = loop {
        if let Some(status) = child.try_wait().expect("wait") {
            break status;
        }
        std::thread::sleep(Duration::from_secs(3));
        let Ok(Ok(fee)) = charter_cli::block_on(rpc.base_fee()) else {
            continue;
        };
        peak = peak.max(fee);
        if U256::from(plan_gas) * U256::from(fee) > U256::from(CAP_WEI) {
            let group = format!("-{}", child.id());
            let _ = Command::new("kill").args(["-TERM", &group]).status();
            let _ = child.wait();
            note_stderr(&std::fs::read_to_string(&log_path).unwrap_or_default());
            panic!(
                "base fee rose to {fee} wei: projected spend breaches the cap; push {label} killed"
            );
        }
    };
    let took = t.elapsed();
    note_stderr(&std::fs::read_to_string(&log_path).unwrap_or_default());
    assert!(status.success(), "push {label} failed");
    let (b1, n1, fee1) = snap();
    eprintln!(
        "gate: push {label} took {took:.1?}, {} blocks, {} txs, base fee start {fee0} peak {} wei",
        b1 - b0,
        n1 - n0,
        peak.max(fee1)
    );
}

fn body_of(o: &Observed) -> Vec<u8> {
    match o.attr("ag.body") {
        Some(charter_core::AttrValue::Bytes(b)) => b.clone(),
        other => panic!("no body: {other:?}"),
    }
}

/// Sends `ops` in one tx from `signer`, topped up from `wallet` when given.
fn send_ops(
    rpc: &RpcArkiv,
    signer: &alloy_signer_local::PrivateKeySigner,
    wallet: Option<&alloy_signer_local::PrivateKeySigner>,
    ops: Vec<Operation>,
) {
    let cfg = PipelineConfig::default();
    let sent = charter_cli::block_on(async {
        match wallet {
            Some(w) => charter_sync::write::send_funded(rpc, signer, w, &cfg, vec![ops]).await,
            None => charter_sync::write::send(rpc, signer, &cfg, vec![ops]).await,
        }
    });
    sent.expect("rt").expect("send");
}
