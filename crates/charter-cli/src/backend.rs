//! Backend selection: `CHARTER_BACKEND=mem:<path>` is a file-persisted `MemArkiv` (tests,
//! demos; `CHARTER_MEM_POLL_MS` slows its receipt polls), anything else is Rabdi over
//! JSON-RPC with the optional `ARKIV_ACCESS_KEY`.

use std::path::PathBuf;

use charter_store::fake::FileMemArkiv;
use charter_store::{
    Address, Arkiv, Operation, Query, QueryOpts, QueryPage, Receipt, RpcArkiv, RpcConfig, SignedTx,
    StoreError, TxHash, TxInfo, U256,
};
use charter_sync::Config;
use charter_sync::repo::env_compat;

use crate::CliError;

/// Chain id of the file-backed fake.
pub const MEM_CHAIN_ID: u64 = 7;

pub enum Backend {
    Mem(Box<FileMemArkiv>),
    Rpc(RpcArkiv),
}

/// Where `CHARTER_BACKEND` points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Mem(PathBuf),
    Rabdi,
}

pub fn target(var: Option<&str>) -> Result<Target, CliError> {
    match var.filter(|v| !v.is_empty()) {
        None | Some("rabdi") => Ok(Target::Rabdi),
        Some(v) => match v.strip_prefix("mem:") {
            Some(p) if !p.is_empty() => Ok(Target::Mem(PathBuf::from(p))),
            _ => Err(CliError::Usage(format!(
                "CHARTER_BACKEND must be `mem:<path>` or `rabdi`, got {v:?}"
            ))),
        },
    }
}

/// Lifetime override in blocks from `var` or its pre-rename name `old` (test runs keep
/// data short-lived).
fn lifetime(var: &str, old: &str, default: u64) -> Result<u64, CliError> {
    match env_compat(var, old) {
        Some(v) => crate::units::blocks(&v.to_string_lossy())
            .map_err(|e| CliError::Usage(format!("{var}: {e}"))),
        None => Ok(default),
    }
}

/// `CHARTER_PACE`: `economy` paces big uploads at the block gas target (about half the
/// fee, about a third more wall time); unset, empty or `default` keeps full blocks.
fn economy(v: Option<&str>) -> Result<bool, CliError> {
    match v.unwrap_or_default() {
        "" | "default" => Ok(false),
        "economy" => Ok(true),
        other => Err(CliError::Usage(format!(
            "CHARTER_PACE: expected `default` or `economy`, got `{other}`"
        ))),
    }
}

/// `CHARTER_BACKEND`, else the pre-rename `ARKIV_GIT_BACKEND`.
fn backend_var() -> Option<String> {
    env_compat("CHARTER_BACKEND", "ARKIV_GIT_BACKEND").map(|v| v.to_string_lossy().into_owned())
}

/// Connects to the configured backend and builds the matching sync config.
pub async fn connect() -> Result<(Backend, Config), CliError> {
    let home = Config::default_home();
    let var = backend_var();
    let (backend, mut cfg) = match target(var.as_deref())? {
        Target::Mem(path) => {
            let mem = FileMemArkiv::open(&path, MEM_CHAIN_ID)?;
            let mut cfg = Config::for_fake(home);
            // The fake mines on the first receipt poll; a delay lets the UI e2e see a write
            // that is sent but not yet in a block.
            if let Some(ms) = std::env::var("CHARTER_MEM_POLL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
            {
                cfg.pipeline.poll = std::time::Duration::from_millis(ms);
            }
            (Backend::Mem(Box::new(mem)), cfg)
        }
        Target::Rabdi => {
            let key = std::env::var("ARKIV_ACCESS_KEY").ok();
            let rpc = RpcArkiv::connect(RpcConfig::rabdi(key)).await?;
            (Backend::Rpc(rpc), Config::new(home))
        }
    };
    cfg.data_lifetime = lifetime(
        "CHARTER_DATA_LIFETIME",
        "ARKIV_GIT_DATA_LIFETIME",
        cfg.data_lifetime,
    )?;
    cfg.control_lifetime = lifetime(
        "CHARTER_CONTROL_LIFETIME",
        "ARKIV_GIT_CONTROL_LIFETIME",
        cfg.control_lifetime,
    )?;
    let pace = std::env::var("CHARTER_PACE").ok();
    cfg.pipeline.economy = economy(pace.as_deref())?;
    Ok((backend, cfg))
}

fn dev_only() -> CliError {
    CliError::Usage(
        "fund --dev only works with CHARTER_BACKEND=mem:<path>; on Rabdi get tGLM from \
         hub.arkiv.network/faucet"
            .into(),
    )
}

/// Refuses before connecting unless `CHARTER_BACKEND` is `mem:<path>`.
pub fn require_mem() -> Result<(), CliError> {
    match target(backend_var().as_deref())? {
        Target::Mem(_) => Ok(()),
        Target::Rabdi => Err(dev_only()),
    }
}

/// Dev faucet: credits `wei` to `wallet` on the `mem:` fake chain and returns the new
/// balance. Any other backend refuses, since real chains are funded from a faucet.
pub async fn dev_fund(b: &Backend, wallet: Address, wei: U256) -> Result<U256, CliError> {
    let Backend::Mem(mem) = b else {
        return Err(dev_only());
    };
    mem.fund(wallet, wei)?;
    Ok(mem.balance(wallet).await?)
}

macro_rules! both {
    ($self:ident, $b:ident => $e:expr) => {
        match $self {
            Backend::Mem($b) => $e,
            Backend::Rpc($b) => $e,
        }
    };
}

impl Arkiv for Backend {
    fn quota(&self) -> Option<charter_store::Quota> {
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
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        both!(self, b => b.transaction(h).await)
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<charter_store::MinedTx>, StoreError> {
        both!(self, b => b.mined_tx(h).await)
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<charter_store::MinedTx>, StoreError> {
        both!(self, b => b.block_txs(n).await)
    }
    async fn tx_at(&self, n: u64, i: u32) -> Result<Option<charter_store::MinedTx>, StoreError> {
        both!(self, b => b.tx_at(n, i).await)
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_variable_selects_mem_or_rabdi() {
        assert_eq!(target(None).expect("default"), Target::Rabdi);
        assert_eq!(target(Some("")).expect("empty"), Target::Rabdi);
        assert_eq!(target(Some("rabdi")).expect("rabdi"), Target::Rabdi);
        assert_eq!(
            target(Some("mem:/x/log")).expect("mem"),
            Target::Mem(PathBuf::from("/x/log"))
        );
        assert!(target(Some("mem:")).is_err());
        assert!(target(Some("http://x")).is_err());
    }

    #[test]
    fn pace_variable_selects_default_or_economy() {
        assert!(!economy(None).expect("unset"));
        assert!(!economy(Some("")).expect("empty"));
        assert!(!economy(Some("default")).expect("default"));
        assert!(economy(Some("economy")).expect("economy"));
        assert!(economy(Some("cheap")).is_err());
    }
}
