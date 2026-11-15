//! `chart program`: pin a repo program in the identity doc, or dry-run one locally.

use std::path::{Path, PathBuf};

use charter_core::{ProgramPin, Rid};
use charter_program::{Event, Outcome, Program, RepoInfo, engine_for};
use charter_store::Arkiv;
use charter_sync::ids::to_text;
use charter_sync::{Config, Repo, Want, sync};
use clap::{Args, Subcommand};
use serde::Deserialize;

use crate::{CliError, Result};

#[derive(Debug, Subcommand)]
pub enum ProgramCmd {
    /// Publish the program source as an op, then propose an identity revision pinning it.
    Pin(PinArgs),
    /// Fold the pinned program over the repo: state, heads, rejected pushes, obligations.
    Status {
        #[arg(long)]
        repo: String,
    },
    /// Fold a local events file through the program and print the outcome as JSON.
    Check(CheckArgs),
}

#[derive(Debug, Args)]
pub struct PinArgs {
    #[arg(long)]
    pub repo: String,
    #[arg(long)]
    pub file: PathBuf,
}

#[derive(Debug, Args)]
pub struct CheckArgs {
    #[arg(long)]
    pub file: PathBuf,
    /// JSON `{ "repo": RepoInfo, "events": [Event] }` (docs/p3/programs.md).
    #[arg(long)]
    pub events: PathBuf,
}

#[derive(Deserialize)]
struct EventsFile {
    repo: RepoInfo,
    events: Vec<Event>,
}

fn read(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).map_err(|e| CliError::Usage(format!("{}: {e}", path.display())))
}

/// Loads (so a broken program is never pinned) and returns the pin and source of `path`.
pub fn pin_for(path: &Path) -> Result<(ProgramPin, String)> {
    let src = read(path)?;
    let engine = engine_for(&src);
    Program::load(engine, &src).map_err(|e| CliError::Usage(e.to_string()))?;
    let pin = ProgramPin {
        engine: engine.into(),
        hash: Program::hash(&src),
    };
    Ok((pin, src))
}

/// `program status`: the enforced fold of the repo's pinned program. It fetches into the
/// local cache first, because fast-forwards are decided by git ancestry.
pub async fn status<A: Arkiv>(a: &A, cfg: &Config, rid: &Rid) -> Result<()> {
    let repo = Repo::open(&cfg.home, rid)?;
    let view = sync(a, &repo, &Want::default()).await?;
    let Some(e) = &view.program else {
        println!("no program pinned");
        return Ok(());
    };
    let o = &e.outcome;
    if let Some(p) = &view.identity.head().body.program {
        println!("program: {} {}", p.engine, hex::encode(p.hash));
    }
    println!("state: {}", o.state_json);
    for (name, oid) in &o.heads {
        println!("head: {name} {}", hex::encode(oid));
    }
    for (u, why) in &o.rejected {
        let (old, new) = (hex::encode(&u.old), hex::encode(&u.new));
        let by = to_text(&u.author);
        println!("rejected: {} {old}..{new} by {by}: {why}", u.name);
    }
    for ob in &o.obligations {
        println!("obligation: {ob}");
    }
    Ok(())
}

/// Runs the dry-run fold for `program check`.
pub fn check(args: &CheckArgs) -> Result<Outcome> {
    let src = read(&args.file)?;
    let prog = Program::load(engine_for(&src), &src).map_err(|e| CliError::Usage(e.to_string()))?;
    let input: EventsFile = serde_json::from_str(&read(&args.events)?)
        .map_err(|e| CliError::Usage(format!("{}: {e}", args.events.display())))?;
    prog.fold(input.repo, input.events)
        .map_err(|e| CliError::Usage(e.to_string()))
}

/// The outcome as pretty JSON, with the state inlined rather than as a string.
pub fn outcome_json(o: &Outcome) -> Result<String> {
    let state: serde_json::Value =
        serde_json::from_str(&o.state_json).map_err(|e| CliError::Usage(e.to_string()))?;
    let v = serde_json::json!({
        "state": state,
        "accepted": o.accepted,
        "rejected": o.rejected,
        "obligations": o.obligations,
        "logs": o.logs,
    });
    serde_json::to_string_pretty(&v).map_err(|e| CliError::Usage(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn programs() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../programs")
    }

    #[test]
    fn pin_hashes_the_exact_source() {
        let path = programs().join("ci-attested.rhai");
        let (pin, src) = pin_for(&path).expect("pin");
        assert_eq!(src, read(&path).expect("src"));
        assert_eq!(pin.engine, "rhai/1");
        assert_eq!(pin.hash, Program::hash(&read(&path).expect("src")));
    }

    #[test]
    fn check_folds_an_events_file() {
        let dir = tempfile::tempdir().expect("tmp");
        let events = dir.path().join("events.json");
        let (a1, a2, oid) = ("a1".repeat(32), "a2".repeat(32), "01".repeat(20));
        let attest = |n: u64, who: &str| {
            serde_json::json!({"event": "op", "id": format!("{n:064x}"), "author": who,
                "object": "40".repeat(32), "type": "xyz.arkiv.attest", "action": "ci",
                "payload": {"commit": oid, "result": "pass"}, "arkiv_order": n})
        };
        let doc = serde_json::json!({
            "repo": {"rid": "ee".repeat(32), "delegates": [a1], "quorum": 1, "name": "demo"},
            "events": [attest(1, &a1), attest(2, &a2),
                {"event": "push", "author": a1, "ref": "refs/heads/main", "new": oid, "arkiv_order": 3}]
        });
        std::fs::write(&events, doc.to_string()).expect("write");
        let args = CheckArgs {
            file: programs().join("ci-attested.rhai"),
            events,
        };
        let o = check(&args).expect("check");
        assert_eq!(o.accepted.len(), 1);
        assert!(outcome_json(&o).expect("json").contains("\"state\""));
    }
}
