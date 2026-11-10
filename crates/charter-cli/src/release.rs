//! `chart release`: publish a signed release record and verify one (docs/p3/releases.md).
//! The op carries only names, hashes, sizes and locations; the bytes live on any mirror.

use std::path::{Path, PathBuf};

use charter_core::{Hash32, Rid};
use charter_store::Arkiv;
use charter_sync::program::proofs::sha256_hex;
use charter_sync::release::{self, fetch, plain_name};
use charter_sync::{Config, Keys, Repo, Want, sync};
use clap::{Args, Subcommand};
use serde_json::Value;

use crate::cmd::rid_of;
use crate::{CliError, Result};

pub use charter_sync::release::RELEASE_TYPE;

#[derive(Debug, Subcommand)]
pub enum ReleaseCmd {
    /// Hash the artifacts and post one signed `xyz.arkiv.release` op; prints its id.
    Publish(PublishArgs),
    /// Refuse unless the pinned program says the release is valid, then download every
    /// artifact and check its sha256 and size.
    Verify(VerifyArgs),
}

#[derive(Debug, Args)]
pub struct PublishArgs {
    #[arg(long)]
    pub repo: String,
    #[arg(long)]
    pub tag: String,
    /// The released commit (hex oid).
    #[arg(long)]
    pub commit: String,
    /// `<name>=<path-or-url>[@<target>]`; a URL is downloaded to hash it and becomes a location.
    #[arg(long = "artifact", required = true)]
    pub artifacts: Vec<String>,
    /// `<name>=<url>`: another place the artifact can be downloaded from.
    #[arg(long = "location")]
    pub locations: Vec<String>,
    #[arg(long)]
    pub notes: Option<String>,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    #[arg(long)]
    pub repo: String,
    #[arg(long)]
    pub tag: String,
    /// Write each verified artifact here as `<name>`.
    #[arg(long)]
    pub download_dir: Option<PathBuf>,
}

impl ReleaseCmd {
    pub fn rid(&self) -> Result<Rid> {
        rid_of(match self {
            Self::Publish(p) => &p.repo,
            Self::Verify(v) => &v.repo,
        })
    }
}

fn usage(s: impl Into<String>) -> CliError {
    CliError::Usage(s.into())
}

/// Hashes the artifacts and posts the release op.
pub async fn publish<A: Arkiv>(a: &A, cfg: &Config, p: &PublishArgs) -> Result<Hash32> {
    let keys = Keys::require(&cfg.home)?;
    let p = release::Publish {
        rid: rid_of(&p.repo)?,
        tag: p.tag.clone(),
        commit: p.commit.clone(),
        artifacts: p.artifacts.clone(),
        locations: p.locations.clone(),
        notes: p.notes.clone(),
    };
    Ok(release::publish(a, cfg, &keys, &p).await?)
}

/// Downloads `art` from its first working location; Ok is the line to print.
async fn check(art: &Value, dir: Option<&Path>) -> std::result::Result<String, String> {
    let name = art["name"]
        .as_str()
        .filter(|n| plain_name(n))
        .ok_or("unsafe artifact name")?;
    let (sha, size) = (art["sha256"].as_str().unwrap_or(""), art["size"].as_u64());
    let locations = art["locations"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    for loc in locations.iter().filter_map(Value::as_str) {
        let bytes = match fetch(loc).await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("{name}: {loc}: {e}");
                continue;
            }
        };
        let got = sha256_hex(&bytes);
        if got != sha || Some(bytes.len() as u64) != size {
            return Err(format!(
                "MISMATCH {name} at {loc}: sha256 {got} size {}",
                bytes.len()
            ));
        }
        if let Some(d) = dir {
            std::fs::write(d.join(name), &bytes).map_err(|e| format!("{name}: {e}"))?;
        }
        return Ok(format!("OK {name} {sha} {} from {loc}", bytes.len()));
    }
    Err(format!("{name}: no location could be downloaded"))
}

/// Checks the release against the pinned program's state, then every artifact.
pub async fn verify<A: Arkiv>(a: &A, cfg: &Config, rid: &Rid, v: &VerifyArgs) -> Result<()> {
    let view = sync(a, &Repo::open(&cfg.home, rid)?, &Want::default()).await?;
    let e = view
        .program
        .as_ref()
        .ok_or_else(|| CliError::Flagged("no program pinned".into()))?;
    let state: Value =
        serde_json::from_str(&e.outcome.state_json).map_err(|e| usage(e.to_string()))?;
    let r = &state["releases"][&v.tag];
    if !r.is_object() {
        return Err(CliError::Flagged(format!(
            "no release {} in the program state",
            v.tag
        )));
    }
    if r["valid"] != true {
        let missing: Vec<&str> = r["missing"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        return Err(CliError::Flagged(format!(
            "release {} is not valid: needs {}",
            v.tag,
            missing.join("; ")
        )));
    }
    println!(
        "release {} at {}: valid under the pinned program",
        v.tag,
        r["commit"].as_str().unwrap_or("")
    );
    if let Some(d) = &v.download_dir {
        std::fs::create_dir_all(d).map_err(|e| usage(format!("{}: {e}", d.display())))?;
    }
    let mut bad = 0;
    for art in r["artifacts"].as_array().into_iter().flatten() {
        match check(art, v.download_dir.as_deref()).await {
            Ok(line) => println!("{line}"),
            Err(why) => {
                eprintln!("{why}");
                bad += 1;
            }
        }
    }
    match bad {
        0 => Ok(()),
        n => Err(CliError::Flagged(format!(
            "{n} artifact(s) failed verification"
        ))),
    }
}

pub async fn run<A: Arkiv>(a: &A, cfg: &Config, cmd: &ReleaseCmd) -> Result<()> {
    match cmd {
        ReleaseCmd::Publish(p) => {
            println!("{}", charter_sync::ids::hex32(&publish(a, cfg, p).await?))
        }
        ReleaseCmd::Verify(v) => verify(a, cfg, &cmd.rid()?, v).await?,
    }
    Ok(())
}
