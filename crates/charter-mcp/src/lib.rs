//! chart-mcp: an MCP server that lets an AI agent work Charter repos (docs/p3/mcp.md).
//!
//! Every tool is one `chart` or `git` invocation; the CLI keeps all signing, fetching,
//! program and escrow logic, and its `E_*` error codes come back in the tool error as
//! `{code, message, exit, stdout}`. Keys never pass through tool arguments: the children
//! use the identity under `$CHARTER_HOME` and the backend from `$CHARTER_BACKEND`.

pub mod run;

use std::path::{Component, Path, PathBuf};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerConfig};
use rmcp::schemars::{self, JsonSchema};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::Deserialize;
use serde_json::Value;

pub use run::Runner;

type ToolResult = Result<String, String>;

const INSTRUCTIONS: &str = "Git repositories on Arkiv. A repo is named by its url \
`arkiv::<rid>` (or the bare rid). Find work with op_list (issues with a `bounty` that no \
patch names yet), clone with repo_clone, edit files in the clone, commit and push them with \
git_push, then attach_proof to post the patch with its receipt. program_status shows \
what the repo's pinned program decided; escrow_status audits payouts. Errors carry the \
stable Charter code (E_*).";

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RepoArg {
    /// Repo url `arkiv::<rid>` or bare rid.
    pub repo: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CloneArgs {
    /// Repo url `arkiv::<rid>` or bare rid.
    pub repo: String,
    /// Directory to clone into; must not exist yet.
    pub dir: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListArgs {
    /// Repo url `arkiv::<rid>` or bare rid.
    pub repo: String,
    /// Only ops of this type, e.g. `xyz.arkiv.issue`.
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// Only ops with this action, e.g. `create`.
    pub action: Option<String>,
    /// Only ops of this object (64-char hex id).
    pub object: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PostArgs {
    /// Repo url `arkiv::<rid>` or bare rid.
    pub repo: String,
    /// Object type, e.g. `xyz.arkiv.comment`.
    #[serde(rename = "type")]
    pub type_name: String,
    /// `create` starts a new object; any other action needs `object`.
    pub action: String,
    /// The object this op belongs to (64-char hex id), for actions other than `create`.
    pub object: Option<String>,
    /// Ids of earlier ops of the same object this op saw.
    #[serde(default)]
    pub parents: Vec<String>,
    /// Payload: a JSON object with no floats.
    pub payload: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AttachArgs {
    /// Repo url `arkiv::<rid>` or bare rid.
    pub repo: String,
    /// Op type; default `xyz.arkiv.patch`.
    #[serde(rename = "type")]
    pub type_name: Option<String>,
    /// Op action; default `create`.
    pub action: Option<String>,
    /// The object, for actions other than `create`.
    pub object: Option<String>,
    /// Ids of earlier ops of the same object.
    #[serde(default)]
    pub parents: Vec<String>,
    /// Payload object without `proofs`, e.g. `{"title": .., "head": <commit>, "issue": <id>}`.
    pub payload: Value,
    /// Path of the RISC Zero receipt file.
    pub receipt: PathBuf,
    /// Path of the receipt's public statement JSON.
    pub public: PathBuf,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PushArgs {
    /// Repo url `arkiv::<rid>` or bare rid.
    pub repo: String,
    /// The local clone (from repo_clone).
    pub dir: PathBuf,
    /// Branch to push `HEAD` to, e.g. `agent/1a2b3c4d`; it lands in this identity's namespace.
    pub branch: String,
    /// If set, first commit every change in `dir` with this message.
    pub message: Option<String>,
}

/// The Charter MCP server.
#[derive(Debug, Clone)]
pub struct ArkivGit {
    runner: Runner,
    tool_router: ToolRouter<Self>,
}

fn err(v: Value) -> String {
    v.to_string()
}

/// A refused tool argument, shaped like a CLI usage failure.
fn usage(msg: String) -> String {
    err(serde_json::json!({"code": "usage", "message": msg, "exit": null, "stdout": ""}))
}

/// `arkiv::<rid>` for a bare or prefixed rid (`z` plus base58btc), else a usage error: the
/// url reaches `git` as an argument.
fn url(repo: &str) -> Result<String, String> {
    let rid = repo.strip_prefix("arkiv::").unwrap_or(repo);
    let b58 = |c: char| c.is_ascii_alphanumeric() && !"0OIl".contains(c);
    match rid.len() > 1 && rid.starts_with('z') && rid.chars().all(b58) {
        true => Ok(format!("arkiv::{rid}")),
        false => Err(usage(format!("{repo:?} is not a repo url `arkiv::z...`"))),
    }
}

/// `p` as an absolute path inside the working area `work`. Tool arguments come from an agent
/// that may be prompt-injected, so a path that starts with `-` (git would read an option),
/// climbs with `..`, or resolves through symlinks outside the working area is refused.
pub fn confine(work: &Path, p: &Path) -> Result<PathBuf, String> {
    let bad = |why: &str| usage(format!("{}: {why}", p.display()));
    if p.as_os_str().is_empty() || p.to_string_lossy().starts_with('-') {
        return Err(bad("not a path"));
    }
    if p.components().any(|c| c == Component::ParentDir) {
        return Err(bad("`..` is not allowed"));
    }
    let root = work
        .canonicalize()
        .map_err(|e| bad(&format!("working area {}: {e}", work.display())))?;
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        root.join(p)
    };
    let real = abs
        .ancestors()
        .find(|a| a.exists())
        .and_then(|a| a.canonicalize().ok())
        .ok_or_else(|| bad("cannot be resolved"))?;
    match real.starts_with(&root) {
        true => Ok(abs),
        false => Err(bad(&format!("outside the working area {}", root.display()))),
    }
}

/// `--name=value`: a value can never be read as another option.
fn opt(name: &str, value: &str) -> String {
    format!("--{name}={value}")
}

fn op_args(cmd: &str, repo: &str, ty: &str, action: &str, object: &Option<String>) -> Vec<String> {
    let mut a = vec!["op".into(), cmd.into(), opt("repo", repo), opt("type", ty)];
    a.push(opt("action", action));
    a.extend(object.iter().map(|o| opt("object", o)));
    a
}

fn with_payload(mut a: Vec<String>, parents: &[String], payload: &Value) -> Vec<String> {
    a.extend(parents.iter().map(|p| opt("parent", p)));
    a.push(opt("payload-json", &payload.to_string()));
    a
}

/// `{"id": <last stdout line>}`: `op post` and `op attach-proof` print the op id last.
fn id_json(out: &str) -> String {
    let id = out.lines().last().unwrap_or_default().trim();
    serde_json::json!({ "id": id }).to_string()
}

#[tool_router]
impl ArkivGit {
    pub fn new(runner: Runner) -> Self {
        Self {
            runner,
            tool_router: Self::tool_router(),
        }
    }

    /// A server using [`Runner::from_env`].
    pub fn from_env() -> Result<Self, String> {
        Ok(Self::new(Runner::from_env()))
    }

    #[tool(description = "Clone a Charter repo into a new local directory with git.")]
    pub async fn repo_clone(&self, Parameters(a): Parameters<CloneArgs>) -> ToolResult {
        let (url, dir) = (url(&a.repo)?, confine(&self.runner.work, &a.dir)?);
        let args = ["clone", "-q", "--", &url, &dir.to_string_lossy()].map(String::from);
        self.runner
            .git(&self.runner.work, &args)
            .await
            .map_err(err)?;
        Ok(serde_json::json!({ "dir": a.dir }).to_string())
    }

    #[tool(
        description = "Optionally commit all changes in a clone, then git push HEAD to \
        refs/heads/<branch> of the repo under this identity; returns {head} (the full commit sha)."
    )]
    pub async fn git_push(&self, Parameters(a): Parameters<PushArgs>) -> ToolResult {
        let (url, dir) = (url(&a.repo)?, confine(&self.runner.work, &a.dir)?);
        let git = |args: &[&str]| {
            let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
            let dir = dir.clone();
            async move { self.runner.git(&dir, &args).await.map_err(err) }
        };
        if let Some(m) = &a.message {
            git(&["add", "-A"]).await?;
            git(&["commit", "-q", "-m", m]).await?;
        }
        let refspec = format!("HEAD:refs/heads/{}", a.branch);
        git(&["push", "-q", "--", &url, &refspec]).await?;
        let head = git(&["rev-parse", "HEAD"]).await?;
        Ok(serde_json::json!({ "head": head.trim() }).to_string())
    }

    #[tool(
        description = "List a repo's verified collaborative-object ops (issues, patches, reviews, \
        agent grants, comments, payments) in Arkiv order as a JSON array of {id, object, author, \
        type, action, parents, ts_ms, block, payload}. Filter by type, action and object."
    )]
    pub async fn op_list(&self, Parameters(a): Parameters<ListArgs>) -> ToolResult {
        let mut args = vec!["op".into(), "list".into(), "--json".into()];
        args.push(opt("repo", &url(&a.repo)?));
        args.extend(a.type_name.iter().map(|t| opt("type", t)));
        args.extend(a.object.iter().map(|o| opt("object", o)));
        let out = self.runner.arkiv(&args).await.map_err(err)?;
        let ops: Vec<Value> = serde_json::from_str(&out).map_err(|e| e.to_string())?;
        let keep = |o: &Value| a.action.as_deref().is_none_or(|x| o["action"] == x);
        Ok(Value::Array(ops.into_iter().filter(keep).collect()).to_string())
    }

    #[tool(description = "Sign and post one op as this agent's identity; returns {id}.")]
    pub async fn op_post(&self, Parameters(a): Parameters<PostArgs>) -> ToolResult {
        let base = op_args("post", &url(&a.repo)?, &a.type_name, &a.action, &a.object);
        let out = self
            .runner
            .arkiv(&with_payload(base, &a.parents, &a.payload))
            .await
            .map_err(err)?;
        Ok(id_json(&out))
    }

    #[tool(
        description = "Post a RISC Zero receipt as xyz.arkiv.blob ops (chunks already on the \
        repo are skipped), then post one op (default: xyz.arkiv.patch create) whose payload \
        gets a `proofs` entry citing them; returns {id} of that op."
    )]
    pub async fn attach_proof(&self, Parameters(a): Parameters<AttachArgs>) -> ToolResult {
        let ty = a.type_name.as_deref().unwrap_or("xyz.arkiv.patch");
        let action = a.action.as_deref().unwrap_or("create");
        let base = op_args("attach-proof", &url(&a.repo)?, ty, action, &a.object);
        let (receipt, public) = (
            confine(&self.runner.work, &a.receipt)?,
            confine(&self.runner.work, &a.public)?,
        );
        let mut args = with_payload(base, &a.parents, &a.payload);
        args.push(opt("receipt", &receipt.to_string_lossy()));
        args.push(opt("public", &public.to_string_lossy()));
        let out = self.runner.arkiv(&args).await.map_err(err)?;
        Ok(id_json(&out))
    }

    #[tool(
        description = "Fold the repo's pinned program: its state (bounties, patches, grants, \
        rejected ops with reasons), accepted heads, rejected pushes and obligations."
    )]
    pub async fn program_status(&self, Parameters(a): Parameters<RepoArg>) -> ToolResult {
        let args = [
            "program".into(),
            "status".into(),
            opt("repo", &url(&a.repo)?),
        ];
        self.runner.arkiv(&args).await.map_err(err)
    }

    #[tool(
        description = "Keyless escrow audit of the repo: deposits, settlements, balance owed \
        and FLAG lines; a flagged ledger is a tool error with code `flagged`."
    )]
    pub async fn escrow_status(&self, Parameters(a): Parameters<RepoArg>) -> ToolResult {
        let args = ["escrow".into(), "audit".into(), opt("repo", &url(&a.repo)?)];
        self.runner.arkiv(&args).await.map_err(err)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ArkivGit {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("charter", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests;
