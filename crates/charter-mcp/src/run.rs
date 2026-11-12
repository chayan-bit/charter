//! Runs the `chart` CLI and `git` as child processes and maps failures to the CLI's
//! stable error codes, so the MCP layer holds no business logic of its own.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Where the binaries live and what extra environment children get.
#[derive(Debug, Clone)]
pub struct Runner {
    /// The `chart` binary; its directory also holds `git-remote-arkiv`.
    pub bin: PathBuf,
    /// Extra variables for every child, on top of this process's environment.
    pub env: Vec<(String, String)>,
    /// The working area: every path a tool takes must stay inside it (`confine`).
    pub work: PathBuf,
}

/// A failed child as the tool error: `{code, message, exit}`, `code` one of the `E_*`
/// codes, `usage`, `flagged` or `null`, `message` the child's stderr, `stdout` its output.
pub fn failure(exit: Option<i32>, stdout: &str, stderr: &str) -> Value {
    json!({
        "code": code_of(stderr),
        "message": stderr.trim(),
        "exit": exit,
        "stdout": stdout.trim(),
    })
}

/// The first `E_*` code in `stderr`, else the CLI's `usage:` or `flagged:` class.
pub fn code_of(stderr: &str) -> Option<String> {
    let at = stderr.match_indices("E_").find_map(|(i, _)| {
        let tail = &stderr[i..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c == '_'))
            .unwrap_or(tail.len());
        (end > 2).then(|| tail[..end].to_owned())
    });
    at.or_else(|| {
        ["usage", "flagged"]
            .into_iter()
            .find(|k| stderr.contains(&format!("chart: {k}:")))
            .map(str::to_owned)
    })
}

impl Runner {
    /// `CHARTER_BIN` (or the pre-rename `ARKIV_GIT_BIN`), else `chart` next to this executable, else `chart` on `PATH`.
    pub fn from_env() -> Self {
        let sibling = std::env::current_exe()
            .ok()
            .and_then(|p| Some(p.parent()?.join("chart")))
            .filter(|p| p.is_file());
        let bin = std::env::var_os("CHARTER_BIN")
            .or_else(|| std::env::var_os("ARKIV_GIT_BIN"))
            .map(PathBuf::from)
            .or(sibling)
            .unwrap_or_else(|| PathBuf::from("chart"));
        // An unknown working directory leaves an empty path, which confines nothing in.
        let work = std::env::current_dir().unwrap_or_default();
        Self {
            bin,
            env: vec![],
            work,
        }
    }

    fn command(&self, program: &Path, dir: Option<&Path>) -> tokio::process::Command {
        let mut c = tokio::process::Command::new(program);
        // git finds `git-remote-arkiv` beside `chart`.
        if let Some(d) = self.bin.parent().filter(|d| !d.as_os_str().is_empty()) {
            let path = std::env::var("PATH").unwrap_or_default();
            c.env("PATH", format!("{}:{path}", d.display()));
        }
        c.envs(self.env.iter().map(|(k, v)| (k, v)));
        if let Some(d) = dir {
            c.current_dir(d);
        }
        c.stdin(std::process::Stdio::null());
        c
    }

    async fn output(&self, mut c: tokio::process::Command) -> Result<String, Value> {
        let out = c
            .output()
            .await
            .map_err(|e| json!({"code": null, "message": format!("spawn: {e}"), "exit": null}))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if out.status.success() {
            return Ok(stdout);
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(failure(out.status.code(), &stdout, &stderr))
    }

    /// `chart <args>`; stdout on success.
    pub async fn arkiv(&self, args: &[String]) -> Result<String, Value> {
        let mut c = self.command(&self.bin, None);
        c.args(args);
        self.output(c).await
    }

    /// `git <args>` in `dir`; stdout on success.
    pub async fn git(&self, dir: &Path, args: &[String]) -> Result<String, Value> {
        let mut c = self.command(Path::new("git"), Some(dir));
        c.args(args);
        self.output(c).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_come_from_the_cli_line() {
        let c = |s: &str| code_of(s);
        assert_eq!(c("chart: E_NOT_LIVE: gone").as_deref(), Some("E_NOT_LIVE"));
        assert_eq!(
            c("error: E_PROGRAM_REJECTED: needs approval\n").as_deref(),
            Some("E_PROGRAM_REJECTED")
        );
        assert_eq!(c("chart: usage: bad").as_deref(), Some("usage"));
        assert_eq!(c("chart: flagged: 1 flag").as_deref(), Some("flagged"));
        assert_eq!(c("E_ alone"), None);
        assert_eq!(c("fatal: nope"), None);
    }
}
