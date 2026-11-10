//! `chart` subcommands (design 3.5, 5, 6, 7.1). Output is plain text on stdout.

use alloy_signer_local::PrivateKeySigner;
use charter_core::{IdBody, Rid};
use charter_store::{Arkiv, U256};
use charter_sync::identity;
use charter_sync::ids::{hex32, parse_id, parse_url, to_text, unhex32};
use charter_sync::keepalive::{self, KeepOpts};
use charter_sync::search::{SearchQuery, search};
use charter_sync::state::State;
use charter_sync::{Config, Keys, Repo};
use clap::{Args, Parser, Subcommand};

use crate::program::{self, ProgramCmd};
use crate::units::{blocks, fmt_glm, glm};
use crate::{CliError, Result, backend};

#[derive(Debug, Parser)]
#[command(name = "chart", version, about = "Git repositories on Arkiv")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Manage this machine's identity seed.
    #[command(subcommand)]
    Keys(KeysCmd),
    /// Create a repo with this peer as its single delegate.
    Init(InitArgs),
    /// Private repos: key packages, members and roles.
    #[command(subcommand)]
    Member(crate::member::MemberCmd),
    /// Repo identity: show, propose a revision, accept one.
    #[command(subcommand)]
    Id(IdCmd),
    /// Extend everything a repo needs before it expires.
    Keepalive(KeepArgs),
    /// Find repos by annotation, verified against their identity heads.
    Search(SearchArgs),
    /// Per-peer heads, expiry outlook and equivocation notes.
    Status { repo: String },
    /// Print the wallet address, its balance and an estimate of what a push needs.
    Fund(FundArgs),
    /// Repo programs: pin one in the identity doc, or dry-run one locally.
    #[command(subcommand)]
    Program(ProgramCmd),
    /// Collaborative-object ops: post one, list verified ones.
    #[command(subcommand)]
    Op(crate::op::OpCmd),
    /// Signed release records: publish one, verify one against the pinned program.
    #[command(subcommand)]
    Release(crate::release::ReleaseCmd),
    /// Timelock encryption on drand quicknet: seal now, anyone opens at time T.
    #[command(subcommand)]
    Timelock(crate::timelock::TimelockCmd),
    /// Accountable escrow: init, appoint, deposit, run the agent, audit.
    #[command(subcommand)]
    Escrow(crate::escrow::EscrowCmd),
    /// Zero-knowledge receipts for sealed patches (offline).
    #[command(subcommand)]
    Zk(crate::zk::ZkCmd),
    /// Local read-only web UI over the repos in `$CHARTER_HOME`; prints one URL to open.
    Ui(UiArgs),
}

#[derive(Debug, Args)]
pub struct UiArgs {
    /// Also sync this repo (`arkiv::<rid>` or a rid), fetching it first if it is not cached.
    pub repo: Option<String>,
    /// Port on 127.0.0.1 (default: a free one).
    #[arg(long, default_value_t = 0)]
    pub port: u16,
}

#[derive(Debug, Subcommand)]
pub enum KeysCmd {
    Init,
    Show,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long, default_value = "")]
    pub description: String,
    #[arg(long, default_value = "main")]
    pub default_branch: String,
    #[arg(long)]
    pub lang: Option<String>,
    #[arg(long)]
    pub license: Option<String>,
    /// Seal everything on Arkiv; members share keys through an MLS group.
    #[arg(long)]
    pub private: bool,
}

#[derive(Debug, Subcommand)]
pub enum IdCmd {
    Show {
        repo: String,
    },
    Propose(ProposeArgs),
    /// Accept revision `rev` (its hex `h`, as `id show` / `id propose` print it).
    Accept {
        repo: String,
        rev: String,
    },
}

#[derive(Debug, Args)]
pub struct ProposeArgs {
    pub repo: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub default_branch: Option<String>,
    #[arg(long = "add-delegate")]
    pub add: Vec<String>,
    #[arg(long = "remove-delegate")]
    pub remove: Vec<String>,
    #[arg(long)]
    pub threshold: Option<u64>,
    #[arg(long)]
    pub lang: Option<String>,
    #[arg(long)]
    pub license: Option<String>,
}

#[derive(Debug, Args)]
pub struct KeepArgs {
    pub repo: String,
    #[arg(long, default_value = "90d", value_parser = blocks)]
    pub horizon: u64,
    #[arg(long, default_value = "30d", value_parser = blocks)]
    pub renew_below: u64,
    /// Upper bound in tGLM.
    #[arg(long, value_parser = glm)]
    pub max_spend: Option<U256>,
    #[arg(long)]
    pub dry_run: bool,
    /// File holding a hex EVM private key to pay with (default: this machine's wallet).
    #[arg(long)]
    pub evm_key: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    #[arg(long, conflicts_with = "prefix")]
    pub name: Option<String>,
    #[arg(long)]
    pub prefix: Option<String>,
    #[arg(long)]
    pub lang: Option<String>,
    #[arg(long)]
    pub license: Vec<String>,
    #[arg(long)]
    pub peer: Option<String>,
    #[arg(long)]
    pub recent: Option<u64>,
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub limit: u32,
}

#[derive(Debug, Args)]
pub struct FundArgs {
    /// Size of the push to estimate for, in bytes.
    #[arg(long, default_value_t = 0)]
    pub bytes: u64,
    /// Credit this many tGLM to the wallet on the local fake chain (`mem:` backend only).
    #[arg(long, value_parser = glm)]
    pub dev: Option<U256>,
}

pub(crate) fn rid_of(repo: &str) -> Result<Rid> {
    let url = repo.strip_prefix("arkiv::").unwrap_or(repo);
    Ok(parse_url(url)?.rid)
}

fn home() -> std::path::PathBuf {
    Config::default_home()
}

fn keys_cmd(cmd: &KeysCmd) -> Result<()> {
    let keys = match cmd {
        KeysCmd::Init => Keys::init(&home())?,
        KeysCmd::Show => Keys::require(&home())?,
    };
    println!("nid:    {}", to_text(&keys.nid()));
    println!("wallet: {}", keys.wallet().address());
    Ok(())
}

fn print_identity(rid: &Rid, chain: &charter_core::IdentityChain) {
    let head = chain.head();
    let b = &head.body;
    println!("rid:            {}", to_text(rid));
    println!("url:            arkiv::{}", to_text(rid));
    println!("revision:       {} ({})", b.rev, hex32(&head.h));
    println!("name:           {}", b.name);
    println!("description:    {}", b.description);
    println!("default branch: {}", b.default_branch);
    println!("delegates:      {}", identity::delegates_text(&b.delegates));
    println!("threshold:      {}", b.threshold);
    println!("lang:           {}", b.lang.as_deref().unwrap_or("-"));
    println!("license:        {}", b.license.as_deref().unwrap_or("-"));
    if let Some(p) = &b.program {
        println!("program:        {} {}", p.engine, hex32(&p.hash));
    }
}

fn edit(args: &ProposeArgs) -> Result<impl FnOnce(&IdBody) -> IdBody + use<>> {
    let add = args
        .add
        .iter()
        .map(|s| parse_id(s))
        .collect::<Result<Vec<_>, _>>()?;
    let remove = args
        .remove
        .iter()
        .map(|s| parse_id(s))
        .collect::<Result<Vec<_>, _>>()?;
    let (name, description, branch) = (
        args.name.clone(),
        args.description.clone(),
        args.default_branch.clone(),
    );
    let (threshold, lang, license) = (args.threshold, args.lang.clone(), args.license.clone());
    Ok(move |h: &IdBody| {
        let mut b = h.clone();
        b.name = name.unwrap_or(b.name);
        b.description = description.unwrap_or(b.description);
        b.default_branch = branch.unwrap_or(b.default_branch);
        b.delegates.extend(add);
        b.delegates.retain(|d| !remove.contains(d));
        b.threshold = threshold.unwrap_or(b.threshold);
        b.lang = lang.or(b.lang);
        b.license = license.or(b.license);
        b
    })
}

async fn id_cmd<A: Arkiv>(a: &A, cfg: &Config, cmd: &IdCmd) -> Result<()> {
    let st = State::default();
    match cmd {
        IdCmd::Show { repo } => {
            let rid = rid_of(repo)?;
            print_identity(&rid, &identity::show(a, &rid, &st).await?);
        }
        IdCmd::Propose(args) => {
            let (rid, keys) = (rid_of(&args.repo)?, Keys::require(&cfg.home)?);
            let h = identity::propose(a, cfg, &keys, &rid, &st, edit(args)?).await?;
            println!("proposed revision {}", hex32(&h));
            print_identity(&rid, &identity::show(a, &rid, &st).await?);
        }
        IdCmd::Accept { repo, rev } => {
            let (rid, keys) = (rid_of(repo)?, Keys::require(&cfg.home)?);
            identity::accept(a, cfg, &keys, &rid, &unhex32(rev)?).await?;
            print_identity(&rid, &identity::show(a, &rid, &st).await?);
        }
    }
    Ok(())
}

fn evm_key(path: &std::path::Path) -> Result<PrivateKeySigner> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))?;
    text.trim()
        .parse::<PrivateKeySigner>()
        .map_err(|_| CliError::Usage(format!("{}: not a hex EVM private key", path.display())))
}

async fn keepalive_cmd<A: Arkiv>(a: &A, cfg: &Config, k: &KeepArgs) -> Result<()> {
    let signer = match &k.evm_key {
        Some(p) => evm_key(p)?,
        None => Keys::require(&cfg.home)?.wallet(),
    };
    let repo = Repo::open(&cfg.home, &rid_of(&k.repo)?)?;
    let opts = KeepOpts {
        horizon: k.horizon,
        renew_below: k.renew_below,
        max_spend: k.max_spend,
        dry_run: k.dry_run,
    };
    let r = keepalive::keepalive(a, &repo, cfg, &signer, &opts).await?;
    let verb = if k.dry_run {
        "would extend"
    } else {
        "extended"
    };
    println!(
        "keep set: {} entities; {verb} {} in {} txs",
        r.kept, r.extended, r.txs
    );
    println!("gas used: {}; spent: {} tGLM", r.gas, fmt_glm(r.spent));
    if let Some(m) = r.min_expiry_blocks {
        println!(
            "minimum expiry: {} days",
            m / charter_sync::repo::BLOCKS_PER_DAY
        );
    }
    Ok(())
}

async fn search_cmd<A: Arkiv>(a: &A, s: &SearchArgs) -> Result<()> {
    let q = SearchQuery {
        name: s.name.clone(),
        prefix: s.prefix.clone(),
        lang: s.lang.clone(),
        licenses: s.license.clone(),
        peer: s.peer.as_deref().map(parse_id).transpose()?,
        recent_days: s.recent,
        limit: s.limit,
    };
    if q == (SearchQuery {
        limit: s.limit,
        ..SearchQuery::default()
    }) {
        return Err(CliError::Usage(
            "give at least one of --name, --prefix, --lang, --license, --peer, --recent".into(),
        ));
    }
    for h in search(a, &q).await? {
        let b = &h.head;
        let (lang, lic) = (
            b.lang.as_deref().unwrap_or("-"),
            b.license.as_deref().unwrap_or("-"),
        );
        println!(
            "arkiv::{}  {}  {lang}  {lic}  {}",
            to_text(&h.rid),
            b.name,
            b.description
        );
    }
    Ok(())
}

async fn status_cmd<A: Arkiv>(a: &A, cfg: &Config, repo: &str) -> Result<()> {
    let repo = Repo::open(&cfg.home, &rid_of(repo)?)?;
    let (view, lines) = keepalive::status(a, &repo, cfg).await?;
    println!(
        "repo {} ({})",
        view.identity.head().body.name,
        to_text(&view.rid)
    );
    for l in lines {
        let role = if l.delegate { "delegate" } else { "peer" };
        let head = l
            .head
            .map_or("no head".into(), |(s, b)| format!("seq {s} at block {b}"));
        let exp = l
            .min_expiry_days
            .map_or("-".into(), |d| format!("{d} days"));
        println!(
            "{} {role}: {head}; min expiry {exp}; {} expiring within 14 days",
            to_text(&l.nid),
            l.expiring_14d
        );
    }
    for e in &view.equivocations {
        println!(
            "E_EQUIVOCATION peer {} seq {}: entities {}, {}",
            to_text(&e.nid),
            e.seq,
            hex32(&e.keys[0]),
            hex32(&e.keys[1])
        );
    }
    Ok(())
}

async fn fund_cmd<A: Arkiv>(a: &A, cfg: &Config, f: &FundArgs) -> Result<()> {
    let keys = Keys::require(&cfg.home)?;
    let wallet = keys.wallet().address();
    let have = a.balance(wallet).await?;
    let probe = [0u8; 32];
    let setup = charter_sync::write::estimate(
        a,
        keys.control_address(&probe),
        &charter_sync::push_send::first_write_ops(&keys, &probe, cfg)?,
    )
    .await?;
    let chunk = charter_sync::write::chunk_op(
        &probe,
        &probe,
        0,
        &vec![0xa5; charter_core::CHUNK_SIZE],
        cfg.data_lifetime,
    )?;
    let per_chunk = charter_sync::write::estimate(a, keys.bulk_address(&probe), &[chunk]).await?;
    let chunks = f.bytes.div_ceil(charter_core::CHUNK_SIZE as u64);
    // Setup covers peer + bind + id; three more control/bulk entities (anchor, refs, manifest).
    let gas = setup * 2 + per_chunk * chunks;
    // Predicted spend plus one window of in-flight max fees (design 4.2 step 6).
    let base_fee = a.base_fee().await?;
    let need = U256::from(gas) * U256::from(base_fee + cfg.pipeline.tip)
        + cfg.pipeline.window_reserve(per_chunk.max(setup), base_fee);
    println!("wallet:  {wallet}");
    println!("balance: {} tGLM", fmt_glm(have));
    println!(
        "estimated need for init plus a {}-byte push: {} tGLM",
        f.bytes,
        fmt_glm(need)
    );
    println!("send testnet funds to the wallet address yourself; Charter makes no transfers here");
    Ok(())
}

async fn pin_cmd<A: Arkiv>(a: &A, cfg: &Config, p: &program::PinArgs) -> Result<()> {
    let (rid, keys, (pin, src)) = (
        rid_of(&p.repo)?,
        Keys::require(&cfg.home)?,
        program::pin_for(&p.file)?,
    );
    let op = charter_sync::program::publish(a, cfg, &keys, &rid, &pin.engine, &src).await?;
    println!("published program source in op {}", hex32(&op));
    let st = State::default();
    let h = identity::propose(a, cfg, &keys, &rid, &st, |b: &IdBody| IdBody {
        program: Some(pin),
        ..b.clone()
    })
    .await?;
    println!("proposed revision {}", hex32(&h));
    print_identity(&rid, &identity::show(a, &rid, &st).await?);
    Ok(())
}

async fn ui_cmd(a: backend::Backend, cfg: Config, u: &UiArgs) -> Result<()> {
    let want = u.repo.as_deref().map(rid_of).transpose()?;
    let opts = charter_ui::Options { port: u.port, want };
    charter_ui::serve(std::sync::Arc::new(a), cfg, opts)
        .await
        .map_err(|e| match e {
            charter_ui::UiError::Sync(e) => CliError::Sync(e),
            other => CliError::Sync(charter_sync::SyncError::io("chart ui", other)),
        })
}

/// Runs one parsed command.
pub fn run(cli: &Cli) -> Result<()> {
    match &cli.cmd {
        Cmd::Keys(k) => return keys_cmd(k),
        Cmd::Program(ProgramCmd::Check(c)) => {
            println!("{}", program::outcome_json(&program::check(c)?)?);
            return Ok(());
        }
        _ => {}
    }
    if let Cmd::Timelock(t) = &cli.cmd {
        return crate::timelock::run(t);
    }
    if let Cmd::Fund(FundArgs { dev: Some(_), .. }) = &cli.cmd {
        backend::require_mem()?;
    }
    if let Some(r) = match &cli.cmd {
        Cmd::Escrow(e) => crate::escrow::local(e, &home()),
        _ => None,
    } {
        return r;
    }
    if let Cmd::Zk(z) = &cli.cmd {
        return crate::zk::zk_cmd(z);
    }
    crate::block_on(async {
        let (a, cfg) = backend::connect().await?;
        match &cli.cmd {
            Cmd::Keys(_) | Cmd::Timelock(_) | Cmd::Zk(_) | Cmd::Program(ProgramCmd::Check(_)) => {
                Ok(())
            }
            Cmd::Program(ProgramCmd::Pin(p)) => {
                for_repo!(&a, &cfg, &rid_of(&p.repo)?, |b| pin_cmd(b, &cfg, p).await)
            }
            Cmd::Program(ProgramCmd::Status { repo }) => {
                let rid = rid_of(repo)?;
                for_repo!(&a, &cfg, &rid, |b| program::status(b, &cfg, &rid).await)
            }
            Cmd::Init(i) => crate::member::init_cmd(&a, &cfg, i).await,
            Cmd::Member(m) => crate::member::member_cmd(&a, &cfg, m).await,
            Cmd::Id(c) => id_cmd(&a, &cfg, c).await,
            Cmd::Keepalive(k) => keepalive_cmd(&a, &cfg, k).await,
            Cmd::Search(s) => search_cmd(&a, s).await,
            Cmd::Status { repo } => status_cmd(&a, &cfg, repo).await,
            Cmd::Fund(FundArgs { dev: Some(wei), .. }) => {
                let wallet = Keys::require(&cfg.home)?.wallet().address();
                let have = backend::dev_fund(&a, wallet, *wei).await?;
                println!("wallet:  {wallet}");
                println!("balance: {} tGLM (fake chain)", fmt_glm(have));
                Ok(())
            }
            Cmd::Fund(f) => fund_cmd(&a, &cfg, f).await,
            Cmd::Ui(u) => ui_cmd(a, cfg, u).await,
            Cmd::Op(o) => for_repo!(&a, &cfg, &o.rid()?, |b| crate::op::run(b, &cfg, o).await),
            Cmd::Release(r) => {
                for_repo!(&a, &cfg, &r.rid()?, |b| crate::release::run(b, &cfg, r)
                    .await)
            }
            // A private repo's escrow ops and obligations are sealed like its other ops.
            Cmd::Escrow(e) => match e.repo() {
                Some(r) => for_repo!(&a, &cfg, &rid_of(r)?, |b| {
                    crate::escrow::run(b, &cfg, e).await
                }),
                None => crate::escrow::run(&a, &cfg, e).await,
            },
        }
    })?
}
