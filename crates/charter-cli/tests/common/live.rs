//! Shared helpers for the live Rabdi tests: the throwaway key, running the real binaries
//! with an isolated home, and sweeping leftover funds back.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use alloy_signer_local::PrivateKeySigner;
use charter_core::{Kind, Observed, Rid};
use charter_store::{
    Address, Arkiv, B256, Operation, PipelineConfig, Query, QueryOpts, QueryPage, RABDI_CHAIN_ID,
    Receipt, RpcArkiv, RpcConfig, SignedTx, StoreError, TxHash, TxInfo, TxPipeline, U256,
};
use charter_sync::{Keys, observe};

/// `CHARTER_RABDI` must be `1` for a live test to run.
pub fn enabled() -> bool {
    let on = std::env::var("CHARTER_RABDI").as_deref() == Ok("1");
    if !on {
        eprintln!("skipped: set CHARTER_RABDI=1");
    }
    on
}

pub struct Secrets {
    pub key: PrivateKeySigner,
    pub access: Option<String>,
}

/// Reads `PRIVATE_KEY` and optional `ARKIV_ACCESS_KEY` from `spike/arkiv-measure/.env`.
pub fn secrets() -> Secrets {
    let env = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../spike/arkiv-measure/.env");
    let text = std::fs::read_to_string(&env).expect("spike/arkiv-measure/.env is readable");
    let get = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name)?.strip_prefix('='))
            .map(|v| v.trim().trim_matches('"').to_owned())
            .filter(|v| !v.is_empty())
    };
    let key = get("PRIVATE_KEY")
        .and_then(|k| k.parse::<PrivateKeySigner>().ok())
        .unwrap_or_else(|| panic!("PRIVATE_KEY in .env is missing or malformed"));
    Secrets {
        key,
        access: get("ARKIV_ACCESS_KEY"),
    }
}

/// Connects to Rabdi and refuses anything but its testnet chain id.
pub fn connect(s: &Secrets) -> RpcArkiv {
    let rpc = charter_cli::block_on(RpcArkiv::connect(RpcConfig::rabdi(s.access.clone())))
        .expect("runtime")
        .expect("connect to Rabdi");
    let chain = charter_cli::block_on(rpc.chain_id())
        .expect("rt")
        .expect("chain id");
    assert_eq!(chain, RABDI_CHAIN_ID, "testnet only");
    assert_eq!(chain, 7_794_095);
    rpc
}

pub fn balance(rpc: &RpcArkiv, a: charter_store::Address) -> U256 {
    charter_cli::block_on(rpc.balance(a))
        .expect("rt")
        .expect("balance")
}

/// `RpcArkiv` 429 waits seen in the binaries' stderr so far.
pub static RATE_LIMITED: AtomicUsize = AtomicUsize::new(0);

/// `program` in `dir` with `home` as `$HOME`, the binaries on `PATH`, `lifetime` blocks
/// for every entity and git config isolated from the machine's.
pub fn command(
    home: &Path,
    access: &Option<String>,
    lifetime: &str,
    program: &str,
    dir: &Path,
    args: &[&str],
) -> Command {
    let bins = PathBuf::from(env!("CARGO_BIN_EXE_git-remote-arkiv"));
    let path = format!(
        "{}:{}",
        bins.parent().expect("dir").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut c = Command::new(program);
    c.current_dir(dir)
        .env("PATH", path)
        .env("HOME", home)
        .env("CHARTER_HOME", home.join(".charter"))
        .env_remove("CHARTER_BACKEND")
        .env("CHARTER_DATA_LIFETIME", lifetime)
        .env("CHARTER_CONTROL_LIFETIME", lifetime)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    if program == "git" {
        c.args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
        ]);
    }
    c.args(args);
    match access {
        Some(k) => c.env("ARKIV_ACCESS_KEY", k),
        None => c.env_remove("ARKIV_ACCESS_KEY"),
    };
    c
}

/// Echoes a child's stderr and counts its 429 waits.
pub fn note_stderr(stderr: &str) {
    eprint!("{stderr}");
    RATE_LIMITED.fetch_add(stderr.matches("rate limited").count(), Ordering::Relaxed);
}

/// Runs [`command`] to completion, asserting success; returns stdout.
pub fn run(
    home: &Path,
    access: &Option<String>,
    lifetime: &str,
    program: &str,
    dir: &Path,
    args: &[&str],
) -> String {
    let out = command(home, access, lifetime, program, dir, args)
        .output()
        .expect("spawn");
    note_stderr(&String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{program} {args:?} failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Sends `value` from `from` to `to` and asserts it landed.
pub fn transfer(rpc: &RpcArkiv, from: &PrivateKeySigner, to: charter_store::Address, value: U256) {
    charter_cli::block_on(async {
        let chain = rpc.chain_id().await.expect("chain");
        let mut p = TxPipeline::new(rpc, from.clone(), chain, PipelineConfig::default());
        let r = p.transfer(to, value).await.expect("transfer");
        assert!(r.success);
    })
    .expect("rt");
}

/// Repo ids cached under `home`'s `$CHARTER_HOME`.
pub fn rids(home: &Path) -> Vec<charter_core::Rid> {
    std::fs::read_dir(home.join(".charter/repos"))
        .map(|d| {
            d.flatten()
                .filter_map(|e| charter_sync::ids::unhex32(&e.file_name().to_string_lossy()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Moves what is left on the wallet and on every repo's control and bulk address back to
/// `to`, keeping only the gas of the sweep transfer itself.
pub fn sweep(rpc: &RpcArkiv, keys: &Keys, rids: &[charter_core::Rid], to: charter_store::Address) {
    charter_cli::block_on(async {
        let chain = rpc.chain_id().await.expect("chain");
        let base = rpc.base_fee().await.expect("fee");
        let mut signers = vec![keys.wallet()];
        for rid in rids {
            signers.push(keys.control(rid));
            signers.push(keys.bulk(rid));
        }
        let cfg = PipelineConfig::default();
        let reserve = U256::from(30_000u64) * U256::from(2 * base + cfg.tip);
        for signer in signers.into_iter().rev() {
            let have = rpc.balance(signer.address()).await.expect("balance");
            if have <= reserve {
                continue;
            }
            let mut p = TxPipeline::new(rpc, signer, chain, cfg.clone());
            if let Err(e) = p.transfer(to, have - reserve).await {
                eprintln!("sweep failed: {}", e.code());
            }
        }
    })
    .expect("rt");
}

/// Deterministic incompressible bytes (same generator as the sync gate).
pub fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

/// Live entities of `kind` carrying `ag.rid = rid`, in creation order.
pub fn entities(rpc: &RpcArkiv, rid: &Rid, kind: Kind) -> Vec<Observed> {
    let f = charter_store::Pred::And(vec![observe::kind(kind), observe::b32("ag.rid", rid)]);
    charter_cli::block_on(observe::all(rpc, f))
        .expect("rt")
        .expect("query")
}

pub fn seq_of(o: &Observed) -> u64 {
    match o.attr("ag.seq") {
        Some(charter_core::AttrValue::U64(s)) => *s,
        other => panic!("no seq: {other:?}"),
    }
}

/// The local proxy of design 8.5 step 4: Rabdi with one entity withheld from every query
/// and `entityNonce` of `control` lowered to match.
pub struct Hiding<'a> {
    pub inner: &'a RpcArkiv,
    pub key: B256,
    pub control: Address,
    pub nonce: u64,
}

impl Arkiv for Hiding<'_> {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        self.inner.chain_id().await
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.inner.head().await
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.inner.base_fee().await
    }
    async fn query(&self, q: &Query, o: &QueryOpts) -> Result<QueryPage, StoreError> {
        let mut page = self.inner.query(q, o).await?;
        page.rows.retain(|r| r.key != Some(self.key));
        if let Some(c) = page.count.as_mut() {
            // A count-only page has no rows, so probe the keys to see if the hidden one counts.
            // shortcut: probes the first 100 matches only, enough for one repo's control set.
            let keys = self.inner.query(q, &QueryOpts::default()).await?;
            if keys.rows.iter().any(|r| r.key == Some(self.key)) {
                *c -= 1;
            }
        }
        Ok(page)
    }
    async fn entity_nonce(&self, c: Address) -> Result<u64, StoreError> {
        match c == self.control {
            true => Ok(self.nonce),
            false => self.inner.entity_nonce(c).await,
        }
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
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.inner.estimate(from, ops).await
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        self.inner.send(tx).await
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        self.inner.receipt(h).await
    }
}
