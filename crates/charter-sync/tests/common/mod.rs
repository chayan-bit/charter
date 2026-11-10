//! Shared fixtures for the sync integration tests: an auto-mining `MemArkiv` wrapper with a
//! send-dropping fault, peers with their own `$CHARTER_HOME`, and real git work repos.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use alloy_signer_local::PrivateKeySigner;
use charter_core::{Hash32, Rid};
use charter_crypto::MasterSeed;
use charter_store::fake::MemArkiv;
use charter_store::{
    Address, Arkiv, Operation, Query, QueryOpts, QueryPage, Receipt, SignedTx, StoreError, TxHash,
    TxInfo, U256,
};
use charter_sync::identity::{self, RepoMeta};
use charter_sync::{Config, Keys, Listing, Push, Repo, Update, View, Want};

pub const CHAIN: u64 = 7;

/// `MemArkiv` that mines one block whenever a receipt is polled and not found, and can
/// accept sends without ever mining them (a lossy RPC edge).
pub struct Net {
    pub mem: Arc<MemArkiv>,
    /// `(sender, sends still let through)`: later sends from `sender` are dropped.
    drop: Mutex<Option<(Address, u64)>>,
    /// `(from block, wei)`: the base fee is held at `wei` once the head reaches the block.
    fee: Mutex<Option<(u64, u128)>>,
}

impl Net {
    pub fn new() -> Self {
        Self {
            mem: Arc::new(MemArkiv::new(CHAIN)),
            drop: Mutex::new(None),
            fee: Mutex::new(None),
        }
    }

    /// Accept but never mine every send from `sender` after the next `after` ones.
    pub fn drop_sends_from(&self, sender: Address, after: u64) {
        *self.drop.lock().expect("lock") = Some((sender, after));
    }

    /// Hold the base fee at `wei` from block `from` on (a fee spike mid-push).
    pub fn hold_base_fee(&self, from: u64, wei: u128) {
        *self.fee.lock().expect("lock") = Some((from, wei));
    }

    pub fn heal(&self) {
        *self.drop.lock().expect("lock") = None;
        *self.fee.lock().expect("lock") = None;
    }

    pub fn fund(&self, a: Address) {
        self.mem.fund(a, U256::from(10u128.pow(24)));
    }

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
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.mem.transaction(h).await
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<charter_store::MinedTx>, StoreError> {
        self.mem.mined_tx(h).await
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<charter_store::MinedTx>, StoreError> {
        self.mem.block_txs(n).await
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.mem.estimate(from, ops).await
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        let dropped = match self.drop.lock().expect("lock").as_mut() {
            Some((s, left)) if *s == tx.from() => match *left {
                0 => true,
                _ => {
                    *left -= 1;
                    false
                }
            },
            _ => false,
        };
        if dropped {
            return Ok(tx.hash());
        }
        self.mem.send(tx).await
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        if let Some(r) = self.mem.receipt(h).await? {
            return Ok(Some(r));
        }
        let n = self.mem.mine();
        let held = *self.fee.lock().expect("lock");
        if let Some((_, wei)) = held.filter(|(from, _)| n + 1 >= *from) {
            self.mem.set_base_fee(wei);
        }
        self.mem.receipt(h).await
    }
}

/// A peer: its own home directory, config and keys (wallet funded).
pub struct Peer {
    pub home: tempfile::TempDir,
    pub cfg: Config,
    pub keys: Keys,
}

impl Peer {
    pub fn new(net: &Net, seed: u8) -> Self {
        let home = tempfile::tempdir().expect("home");
        let mut cfg = Config::for_fake(home.path().to_path_buf());
        cfg.pipeline.resend_after_blocks = 3;
        let keys = Keys::from_seed(MasterSeed::from_bytes([seed; 32]));
        net.fund(keys.wallet().address());
        Self { home, cfg, keys }
    }

    pub fn repo(&self, rid: &Rid) -> Repo {
        Repo::open(self.home.path(), rid).expect("repo")
    }

    pub fn want(&self) -> Want {
        Want {
            must: [self.keys.nid()].into(),
            ..Want::default()
        }
    }
}

pub fn meta(name: &str) -> RepoMeta {
    RepoMeta {
        name: name.into(),
        description: "test repo".into(),
        default_branch: "main".into(),
        lang: Some("rust".into()),
        license: Some("MIT".into()),
    }
}

pub async fn init(net: &Net, p: &Peer, name: &str) -> Rid {
    identity::init(net, &p.cfg, &p.keys, &meta(name))
        .await
        .expect("init")
}

/// Fetch sync including the peer itself, then prepare a push of `updates`.
pub async fn prepare<'a>(
    net: &'a Net,
    p: &'a Peer,
    repo: &'a Repo,
    work: &Path,
    updates: &[Update],
) -> charter_sync::Result<Push<'a, Net>> {
    let view = charter_sync::sync(net, repo, &p.want()).await?;
    Push::prepare(
        net,
        repo,
        &p.cfg,
        &p.keys,
        view,
        &charter_sync::git::Git::work(Some(work)),
        updates,
    )
}

pub async fn push(
    net: &Net,
    p: &Peer,
    rid: &Rid,
    work: &Path,
    refs: &[&str],
) -> charter_sync::Result<()> {
    let repo = p.repo(rid);
    let updates: Vec<Update> = refs.iter().map(|r| upd(r)).collect();
    prepare(net, p, &repo, work, &updates).await?.run().await
}

pub fn upd(r: &str) -> Update {
    match r.strip_prefix(':') {
        Some(dst) => Update {
            src: None,
            dst: dst.into(),
        },
        None => Update {
            src: Some(r.into()),
            dst: r.into(),
        },
    }
}

/// Fresh-or-existing cache fetch, then the working repo `dest` gets the listed objects and
/// refs (what `git clone arkiv::<rid>` does through the helper).
pub async fn clone_into(
    net: &Net,
    p: &Peer,
    rid: &Rid,
    dest: &Path,
) -> charter_sync::Result<(View, Listing)> {
    let repo = p.repo(rid);
    let view = charter_sync::sync(net, &repo, &Want::default()).await?;
    let listing = charter_sync::listing(&view, None);
    if !dest.join(".git").exists() {
        git(
            dest.parent().expect("parent"),
            &["init", "-q", &dest.to_string_lossy()],
        );
    }
    let work = charter_sync::git::Git::work(Some(dest));
    let oids: Vec<String> = listing.refs.iter().map(|(_, o)| o.clone()).collect();
    work.fetch_pack_from(&repo.git_dir(), &oids)?;
    for (name, oid) in &listing.refs {
        git(dest, &["update-ref", name, oid]);
    }
    Ok((view, listing))
}

/// Runs git with a config isolated from the machine's (no global hooks or signing).
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A new working repo with no commits.
pub fn work_repo(parent: &Path, name: &str) -> PathBuf {
    let d = parent.join(name);
    git(parent, &["init", "-q", &d.to_string_lossy()]);
    d
}

/// Writes `files` (path, bytes) and commits them; returns the commit oid.
pub fn commit(work: &Path, files: &[(&str, &[u8])], msg: &str) -> String {
    for (p, b) in files {
        let f = work.join(p);
        std::fs::create_dir_all(f.parent().expect("parent")).expect("mkdir");
        std::fs::write(&f, b).expect("write");
    }
    git(work, &["add", "-A"]);
    git(work, &["commit", "-q", "--allow-empty", "-m", msg]);
    git(work, &["rev-parse", "HEAD"])
}

/// Deterministic incompressible bytes.
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

pub fn rev(dir: &Path, r: &str) -> String {
    git(dir, &["rev-parse", r])
}

/// A funded key that is no peer of any repo (third parties, replayers, sponsors).
pub fn outsider(net: &Net) -> PrivateKeySigner {
    let k = PrivateKeySigner::random();
    net.fund(k.address());
    k
}

pub fn code<T>(r: charter_sync::Result<T>) -> &'static str {
    match r {
        Ok(_) => "OK",
        Err(e) => e.code().as_str(),
    }
}

pub fn h32_hex(h: &Hash32) -> String {
    hex::encode(h)
}

pub fn fast() -> charter_store::PipelineConfig {
    charter_store::PipelineConfig {
        poll: std::time::Duration::ZERO,
        resend_after_blocks: 3,
        ..Default::default()
    }
}

/// Live entities of `kind` carrying `ag.rid = rid`, in creation order.
pub async fn entities(
    net: &Net,
    rid: &Rid,
    kind: charter_core::Kind,
) -> Vec<charter_core::Observed> {
    use charter_sync::observe;
    let f = charter_store::Pred::And(vec![observe::kind(kind), observe::b32("ag.rid", rid)]);
    observe::all(net, f).await.expect("query")
}

/// Every live entity created by `creator`.
pub async fn created_by(net: &Net, creator: Address) -> Vec<charter_core::Observed> {
    use charter_sync::observe;
    observe::all(net, observe::creators(&[creator.into_array()].into()))
        .await
        .expect("query")
}

pub fn body_of(o: &charter_core::Observed) -> Vec<u8> {
    match o.attr("ag.body") {
        Some(charter_core::AttrValue::Bytes(b)) => b.clone(),
        other => panic!("no body: {other:?}"),
    }
}

pub fn seq_of(o: &charter_core::Observed) -> u64 {
    match o.attr("ag.seq") {
        Some(charter_core::AttrValue::U64(s)) => *s,
        other => panic!("no seq: {other:?}"),
    }
}

/// Sends `ops` in one tx from `signer` and waits for it to land.
pub async fn send_ops(net: &Net, signer: &PrivateKeySigner, ops: Vec<Operation>) {
    charter_sync::write::send(net, signer, &fast(), vec![ops])
        .await
        .expect("send");
}

/// Extends each entity (permissionless) to `lifetime` blocks from now.
pub async fn extend(net: &Net, signer: &PrivateKeySigner, keys: &[Hash32], lifetime: u64) {
    for group in keys.chunks(40) {
        let ops = group
            .iter()
            .map(|k| charter_store::extend_op(charter_store::B256::from(*k), lifetime))
            .collect();
        send_ops(net, signer, ops).await;
    }
}
