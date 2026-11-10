//! Repo programs (docs/p3/programs.md): a Rhai script pinned by hash in the identity doc,
//! folded deterministically over Arkiv-ordered ops and ref updates. Pure, no I/O.

mod checkpoint;
mod engine;
mod fold;
mod value;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use checkpoint::{CHECKPOINT_VERSION, Checkpoint};
pub use value::cbor_from_json;

/// `ancestry(a, b)`: `Some(true)` if commit `a` is an ancestor of (or equal to) commit `b`,
/// `Some(false)` if not, `None` if the caller cannot tell (objects missing).
pub type Ancestry<'a> = &'a dyn Fn(&[u8], &[u8]) -> Option<bool>;

/// The only engine tag this build runs.
pub const ENGINE_RHAI_V1: &str = "rhai/1";
/// Tag of `H(tag, source)` for program pins.
pub const HASH_TAG: &str = "program";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProgramError {
    #[error("unknown program engine {0:?} (only {ENGINE_RHAI_V1})")]
    UnknownEngine(String),
    #[error("program does not compile: {0}")]
    Compile(String),
    #[error("program has no `fn init(repo)`")]
    NoInit,
    #[error("program init failed: {0}")]
    Init(String),
    #[error("event at Arkiv order {0} arrives after a later one")]
    OutOfOrder(u64),
    #[error("program state is not serializable: {0}")]
    State(String),
}

/// Identity facts handed to `init`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoInfo {
    #[serde(with = "hex::serde")]
    pub rid: [u8; 32],
    #[serde(with = "value::hexes")]
    pub delegates: Vec<[u8; 32]>,
    pub quorum: u64,
    pub name: String,
}

/// A verified COB op, flattened (mirrors `VerifiedOp` + `CobOpBody` from the COB branch).
/// `Serialize` is the canonical form checkpoints hash (the payload as raw bytes).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProgramOp {
    #[serde(with = "hex::serde")]
    pub id: [u8; 32],
    #[serde(with = "hex::serde")]
    pub author: [u8; 32],
    #[serde(with = "hex::serde", default)]
    pub rid: [u8; 32],
    #[serde(with = "hex::serde")]
    pub object: [u8; 32],
    #[serde(rename = "type")]
    pub type_name: String,
    pub action: String,
    #[serde(with = "value::hexes", default)]
    pub parents: Vec<[u8; 32]>,
    /// Canonical CBOR; in JSON input it is written as a plain JSON value.
    #[serde(
        deserialize_with = "value::json_payload",
        serialize_with = "hex::serde::serialize"
    )]
    pub payload: Vec<u8>,
    #[serde(default)]
    pub ts_ms: u64,
    pub arkiv_order: u64,
    /// Client-side verification results of the op's proof attachments, in payload order;
    /// scripts see them as `op.proofs` and never see receipt bytes.
    #[serde(default)]
    pub proofs: Vec<ProofCheck>,
}

/// One verified proof attachment (docs/p3/programs.md, "Proofs"). `ok` means the receipt
/// verified against the pinned `image_id` and its journal equals `public` exactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofCheck {
    pub kind: String,
    pub image_id: String,
    pub ok: bool,
    /// The attachment's claimed public statement (trust it only when `ok`).
    pub public: serde_json::Value,
}

/// One ref move by one author; `old` is empty when the ref is new, `new` empty on delete.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefUpdate {
    #[serde(with = "hex::serde")]
    pub author: [u8; 32],
    #[serde(rename = "ref")]
    pub name: String,
    #[serde(with = "hex::serde", default)]
    pub old: Vec<u8>,
    #[serde(with = "hex::serde")]
    pub new: Vec<u8>,
    pub arkiv_order: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    Op(ProgramOp),
    Push(RefUpdate),
}

impl Event {
    pub fn order(&self) -> u64 {
        match self {
            Self::Op(o) => o.arkiv_order,
            Self::Push(p) => p.arkiv_order,
        }
    }
}

/// Result of a fold. `state_json` has sorted map keys, so it is byte-identical across clients.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outcome {
    pub state_json: String,
    pub accepted: Vec<RefUpdate>,
    pub rejected: Vec<(RefUpdate, String)>,
    pub obligations: Vec<serde_json::Value>,
    pub logs: Vec<String>,
    /// Last accepted head per ref after the fold (the seed plus every accepted push).
    #[serde(skip)]
    pub heads: BTreeMap<String, Vec<u8>>,
}

/// Which optional ABI functions the script defines.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Abi {
    on_op: bool,
    check_push: bool,
    on_push: bool,
    obligations: bool,
}

/// A compiled repo program.
#[derive(Debug, Clone)]
pub struct Program {
    ast: rhai::AST,
    abi: Abi,
}

impl Program {
    /// Compiles `source` for `engine_tag`; it must define `fn init(repo)`.
    pub fn load(engine_tag: &str, source: &str) -> Result<Self, ProgramError> {
        if engine_tag != ENGINE_RHAI_V1 {
            return Err(ProgramError::UnknownEngine(engine_tag.into()));
        }
        let ast = engine::compile(source)?;
        let has = |name: &str, arity: usize| {
            ast.iter_functions()
                .any(|f| f.name == name && f.params.len() == arity)
        };
        if !has("init", 1) {
            return Err(ProgramError::NoInit);
        }
        let abi = Abi {
            on_op: has("on_op", 2),
            check_push: has("check_push", 2),
            on_push: has("on_push", 2),
            obligations: has("obligations", 1),
        };
        Ok(Self { ast, abi })
    }

    /// `H("program", source)`, the hash an identity doc pins.
    pub fn hash(source: &str) -> [u8; 32] {
        charter_crypto::h(HASH_TAG, source.as_bytes())
    }

    /// Runs `init`, then every event in Arkiv order, then `obligations`.
    pub fn fold(
        &self,
        repo: RepoInfo,
        events: impl IntoIterator<Item = Event>,
    ) -> Result<Outcome, ProgramError> {
        fold::run(self, &repo, BTreeMap::new(), events, &|_, _| None)
    }

    /// Like [`Program::fold`], but the accepted heads start at `seed` (ref name to oid)
    /// instead of empty, e.g. the canonical heads when the program was pinned.
    pub fn fold_from(
        &self,
        repo: RepoInfo,
        seed: BTreeMap<String, Vec<u8>>,
        events: impl IntoIterator<Item = Event>,
    ) -> Result<Outcome, ProgramError> {
        fold::run(self, &repo, seed, events, &|_, _| None)
    }

    /// [`Program::fold_with`] over `events`, resuming from `cache` when it is a checkpoint
    /// of this exact program, repo, seed and a prefix of these exact events (see
    /// [`Checkpoint`]); otherwise folds from the start. Returns the outcome and a
    /// checkpoint after all of `events` (before `obligations`), when one can be kept.
    pub fn fold_cached(
        &self,
        source: &str,
        repo: RepoInfo,
        seed: BTreeMap<String, Vec<u8>>,
        events: &[Event],
        ancestry: Ancestry<'_>,
        cache: Option<&Checkpoint>,
    ) -> Result<(Outcome, Option<Checkpoint>), ProgramError> {
        let program = Self::hash(source);
        let hash = |evs: &[Event]| checkpoint::hash_all(evs).map_err(ProgramError::State);
        let usable = cache.filter(|c| {
            c.v == CHECKPOINT_VERSION
                && c.program == program
                && c.repo == repo
                && c.seed == seed
                && usize::try_from(c.events).is_ok_and(|n| n <= events.len())
        });
        let prefix = match usable {
            Some(c) => Some((c, c.events as usize, hash(&events[..c.events as usize])?)),
            None => None,
        };
        let resumed = prefix.filter(|(c, _, h)| *h == c.events_hash);
        let (mut f, from, from_hash) = match resumed {
            Some((c, n, h)) => (fold::Folder::resume(self, c, ancestry), n, h),
            None => {
                let f = fold::Folder::start(self, &repo, seed.clone(), ancestry)?;
                (f, 0, [0; 32])
            }
        };
        let events_hash = events[from..]
            .iter()
            .try_fold(from_hash, checkpoint::chain)
            .map_err(ProgramError::State)?;
        f.events(events[from..].iter().cloned())?;
        let base = Checkpoint {
            v: CHECKPOINT_VERSION,
            program,
            repo,
            seed,
            events: events.len() as u64,
            events_hash,
            last_order: 0,
            state: checkpoint::Val::Unit,
            heads: BTreeMap::new(),
            accepted: vec![],
            rejected: vec![],
            logs: vec![],
            spent: 0,
        };
        let cp = f.checkpoint(base);
        Ok((f.finish()?, cp))
    }

    /// Like [`Program::fold_from`], with `ff` decided by `ancestry` (git ancestry of the
    /// accepted head and the new oid) whenever it can tell.
    pub fn fold_with(
        &self,
        repo: RepoInfo,
        seed: BTreeMap<String, Vec<u8>>,
        events: impl IntoIterator<Item = Event>,
        ancestry: Ancestry<'_>,
    ) -> Result<Outcome, ProgramError> {
        fold::run(self, &repo, seed, events, ancestry)
    }
}
