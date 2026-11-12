//! `chart signing-key`: claim an SSH signing key for this identity in a repo, and check a
//! commit's git signature against the repo's claims (issue #61, `charter_sync::gitsig`).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use charter_core::{CobOpBody, ErrorCode, ZERO};
use charter_store::Arkiv;
use charter_sync::gitsig::{self, CLAIM_NAMESPACE, CLAIM_TYPE, CommitSig, SshKey};
use charter_sync::ids::{hex32, to_text};
use charter_sync::{Config, Keys, Repo, SyncError, cob};
use clap::Subcommand;
use serde_json::json;

use crate::cmd::rid_of;
use crate::op::now_ms;
use crate::{CliError, Result};

#[derive(Debug, Subcommand)]
pub enum SigningCmd {
    /// Prove with `ssh-keygen` that this identity holds an Ed25519 SSH key and post the
    /// claim, so commits it signs show as signed by you; prints the claim op id.
    Claim {
        #[arg(long)]
        repo: String,
        /// The private key file (its `.pub` must sit next to it), or a `.pub` file whose
        /// private key is in `ssh-agent`.
        #[arg(long)]
        key: PathBuf,
    },
    /// Check one cached commit's git signature against the repo's claims.
    Verify {
        #[arg(long)]
        repo: String,
        /// Full commit id (40 or 64 hex digits).
        commit: String,
    },
}

impl SigningCmd {
    pub fn rid(&self) -> Result<charter_core::Rid> {
        match self {
            Self::Claim { repo, .. } | Self::Verify { repo, .. } => rid_of(repo),
        }
    }
}

fn usage(m: impl Into<String>) -> CliError {
    CliError::Usage(m.into())
}

/// The public key line of `key` (a `.pub` file, or the one next to a private key).
fn public_line(key: &Path) -> Result<String> {
    let pubf = match key.extension().is_some_and(|e| e == "pub") {
        true => key.to_path_buf(),
        false => PathBuf::from(format!("{}.pub", key.display())),
    };
    let line =
        std::fs::read_to_string(&pubf).map_err(|e| usage(format!("{}: {e}", pubf.display())))?;
    let line = line.trim().to_owned();
    SshKey::parse(&line)
        .map(|_| line)
        .ok_or_else(|| usage(format!("{}: not an ssh-ed25519 key", pubf.display())))
}

/// `ssh-keygen -Y sign` of `msg` with `key` under the claim namespace.
fn ssh_sign(key: &Path, msg: &[u8]) -> Result<String> {
    let mut c = Command::new("ssh-keygen")
        .args(["-q", "-Y", "sign", "-n", CLAIM_NAMESPACE, "-f"])
        .arg(key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| usage(format!("ssh-keygen: {e}")))?;
    c.stdin
        .take()
        .ok_or_else(|| usage("ssh-keygen: no stdin"))?
        .write_all(msg)
        .map_err(|e| usage(format!("ssh-keygen: {e}")))?;
    let out = c
        .wait_with_output()
        .map_err(|e| usage(format!("ssh-keygen: {e}")))?;
    match out.status.success() {
        true => String::from_utf8(out.stdout).map_err(|e| usage(format!("ssh-keygen: {e}"))),
        false => Err(usage(format!(
            "ssh-keygen: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))),
    }
}

async fn claim<A: Arkiv>(a: &A, cfg: &Config, repo: &str, key: &Path) -> Result<()> {
    let (rid, keys) = (rid_of(repo)?, Keys::require(&cfg.home)?);
    let line = public_line(key)?;
    let msg = gitsig::claim_message(&keys.nid());
    let sig = ssh_sign(key, msg.as_bytes())?;
    // Checked here as every reader will check it, so a bad claim is never paid for.
    let signed = gitsig::verify_sshsig(&sig, CLAIM_NAMESPACE.as_bytes(), msg.as_bytes());
    if signed.ok() != SshKey::parse(&line) {
        return Err(usage(
            "ssh-keygen signed with another key than the claimed one",
        ));
    }
    let body = CobOpBody {
        rid,
        type_name: CLAIM_TYPE.into(),
        object: ZERO,
        parents: vec![],
        action: "create".into(),
        payload: cob::json::to_cbor(&json!({"key": line, "sig": sig}))?,
        ts_ms: now_ms(),
    };
    let id = cob::post(a, cfg, &keys, &body).await?;
    println!("{}", hex32(&id));
    Ok(())
}

async fn verify<A: Arkiv>(a: &A, cfg: &Config, repo: &str, commit: &str) -> Result<()> {
    let hex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(hex) {
        return Err(usage(format!(
            "{commit:?} is not a full lowercase commit id"
        )));
    }
    let rid = rid_of(repo)?;
    let raw = Repo::open(&cfg.home, &rid)?
        .git()
        .run(&["cat-file", "commit", commit], b"")?;
    let claims = gitsig::claims(&cob::list(a, &rid, None, Some(CLAIM_TYPE)).await?);
    match gitsig::commit_sig(&raw, &claims) {
        CommitSig::Unsigned => println!("unsigned"),
        CommitSig::Unchecked(kind) => println!("signed with {kind}: not checked"),
        CommitSig::Good {
            fingerprint,
            signer: Some(n),
        } => println!("verified: signed by {} with {fingerprint}", to_text(&n)),
        CommitSig::Good { fingerprint, .. } => {
            println!("valid signature by {fingerprint}, a key no one in this repo claimed")
        }
        CommitSig::Bad => {
            let msg = format!("the git signature of {commit} does not verify");
            return Err(SyncError::coded(ErrorCode::BadSig, msg).into());
        }
    }
    Ok(())
}

pub async fn run<A: Arkiv>(a: &A, cfg: &Config, cmd: &SigningCmd) -> Result<()> {
    match cmd {
        SigningCmd::Claim { repo, key } => claim(a, cfg, repo, key).await,
        SigningCmd::Verify { repo, commit } => verify(a, cfg, repo, commit).await,
    }
}
