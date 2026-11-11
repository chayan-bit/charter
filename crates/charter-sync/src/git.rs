//! The `git` CLI wrapper (design 4.1: stock git for every pack operation, no libgit2/gix).
//!
//! A [`Git`] targets either the bare cache (`--git-dir`, with the repo-location variables
//! git sets for a remote helper removed so they never leak into cache commands) or the
//! working repo (a path in tests; the inherited `GIT_DIR` inside the helper).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use charter_core::ErrorCode;

use crate::{Result, SyncError};

/// Variables that point git at a repository; cleared for cache commands.
const REPO_ENV: [&str; 11] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_COMMON_DIR",
    "GIT_QUARANTINE_PATH",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
];

/// Header of a git pack: `PACK`, version, object count.
const PACK_HEADER: usize = 12;

#[derive(Debug, Clone)]
pub struct Git {
    /// `Some(dir)`: the cache (`--git-dir`); `None` with `cwd`: a working repo.
    git_dir: Option<PathBuf>,
    cwd: Option<PathBuf>,
}

/// One line of an `update-ref --stdin` transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefOp {
    /// `update <ref> <new> <old>`; `old = None` requires the ref not to exist.
    Update {
        name: String,
        new: String,
        old: Option<String>,
    },
    Delete {
        name: String,
        old: String,
    },
}

fn zero_oid() -> String {
    "0".repeat(40)
}

impl Git {
    /// The bare cache at `dir`.
    pub fn cache(dir: &Path) -> Self {
        Self {
            git_dir: Some(dir.to_path_buf()),
            cwd: None,
        }
    }

    /// A working repo: `Some(path)` in tests, `None` inside the helper (inherited env).
    pub fn work(dir: Option<&Path>) -> Self {
        Self {
            git_dir: None,
            cwd: dir.map(Path::to_path_buf),
        }
    }

    pub fn dir(&self) -> Option<&Path> {
        self.git_dir.as_deref()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new("git");
        if let Some(d) = &self.git_dir {
            REPO_ENV.iter().for_each(|v| {
                c.env_remove(v);
            });
            c.arg("--git-dir").arg(d);
        }
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        c.args(args);
        c
    }

    fn fail(args: &[&str], stderr: &[u8]) -> SyncError {
        SyncError::Git {
            cmd: args.first().copied().unwrap_or_default().to_owned(),
            msg: String::from_utf8_lossy(stderr).trim().to_owned(),
        }
    }

    /// Runs git with `stdin`, returns stdout; a non-zero exit is an error with stderr.
    pub fn run(&self, args: &[&str], stdin: &[u8]) -> Result<Vec<u8>> {
        let _t = charter_store::trace::span(|| {
            format!(
                "git {}",
                args.join(" ").chars().take(80).collect::<String>()
            )
        });
        let mut child = self
            .command(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| SyncError::io("git", e))?;
        let mut pipe = child
            .stdin
            .take()
            .ok_or_else(|| SyncError::io("git", "no stdin"))?;
        let input = stdin.to_vec();
        // A writer thread so a large stdout never deadlocks against our stdin.
        let writer = std::thread::spawn(move || pipe.write_all(&input));
        let out = child
            .wait_with_output()
            .map_err(|e| SyncError::io("git", e))?;
        let _ = writer.join();
        if !out.status.success() {
            return Err(Self::fail(args, &out.stderr));
        }
        Ok(out.stdout)
    }

    /// The exit code of `git <args>` (no stdin, output dropped); `None` if killed by a signal.
    pub fn exit_code(&self, args: &[&str]) -> Result<Option<i32>> {
        let _t = charter_store::trace::span(|| {
            format!(
                "git {}",
                args.join(" ").chars().take(80).collect::<String>()
            )
        });
        let status = self
            .command(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|e| SyncError::io("git", e))?;
        Ok(status.code())
    }

    /// The `oids` this repo has, in order (`cat-file --batch-check`).
    pub fn present(&self, oids: &[String]) -> Result<Vec<String>> {
        if oids.is_empty() {
            return Ok(vec![]);
        }
        let input: String = oids.iter().map(|o| format!("{o}\n")).collect();
        let out = self.run(&["cat-file", "--batch-check"], input.as_bytes())?;
        let missing: std::collections::BTreeSet<&str> = std::str::from_utf8(&out)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.strip_suffix(" missing"))
            .collect();
        Ok(oids
            .iter()
            .filter(|o| !missing.contains(o.as_str()))
            .cloned()
            .collect())
    }

    /// `git <args>` with a file as stdin.
    pub fn run_with_file(&self, args: &[&str], file: &Path) -> Result<Vec<u8>> {
        let _t = charter_store::trace::span(|| {
            format!(
                "git {}",
                args.join(" ").chars().take(80).collect::<String>()
            )
        });
        let f = std::fs::File::open(file).map_err(|e| SyncError::io(file.display(), e))?;
        let out = self
            .command(args)
            .stdin(f)
            .output()
            .map_err(|e| SyncError::io("git", e))?;
        if !out.status.success() {
            return Err(Self::fail(args, &out.stderr));
        }
        Ok(out.stdout)
    }

    /// Creates a bare cache at `dir` (if absent), configured for oid wants and no hooks.
    pub fn init_cache(dir: &Path) -> Result<Self> {
        let g = Self::cache(dir);
        if dir.join("HEAD").exists() {
            return Ok(g);
        }
        let mut c = Command::new("git");
        REPO_ENV.iter().for_each(|v| {
            c.env_remove(v);
        });
        let out = c
            .args(["init", "--bare", "-q"])
            .arg(dir)
            .output()
            .map_err(|e| SyncError::io("git init", e))?;
        if !out.status.success() {
            return Err(Self::fail(&["init"], &out.stderr));
        }
        let hooks = dir.join("no-hooks");
        std::fs::create_dir_all(&hooks).map_err(|e| SyncError::io(hooks.display(), e))?;
        let hooks = hooks.to_string_lossy().into_owned();
        for (k, v) in [
            ("uploadpack.allowAnySHA1InWant", "true"),
            ("core.hooksPath", hooks.as_str()),
            ("gc.auto", "0"),
            ("core.logAllRefUpdates", "false"),
        ] {
            g.run(&["config", k, v], b"")?;
        }
        Ok(g)
    }

    /// `(name, oid)` of every ref under `prefix`.
    pub fn refs(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        let out = self.run(
            &["for-each-ref", "--format=%(refname) %(objectname)", prefix],
            b"",
        )?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(n, o)| (n.to_owned(), o.to_owned()))
            .collect())
    }

    /// One atomic `update-ref --stdin` transaction with old-value checks.
    pub fn update_refs(&self, ops: &[RefOp]) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let mut s = String::new();
        for op in ops {
            match op {
                RefOp::Update { name, new, old } => {
                    let old = old.clone().unwrap_or_else(zero_oid);
                    s.push_str(&format!("update {name} {new} {old}\n"));
                }
                RefOp::Delete { name, old } => s.push_str(&format!("delete {name} {old}\n")),
            }
        }
        self.run(&["update-ref", "--stdin"], s.as_bytes())
            .map(|_| ())
    }

    /// Packs of everything reachable from `tips` and not from `exclude`, written as
    /// `<prefix>-<hash>.pack`. With `CHARTER_THIN_PACKS` set, one thin pack (deltas may use
    /// objects of `exclude` as bases) when it stays under 100 MiB; otherwise self-contained
    /// packs split at 100 MiB. Empty packs are removed.
    pub fn pack_objects(
        &self,
        tips: &[String],
        exclude: &[String],
        prefix: &Path,
    ) -> Result<Vec<PathBuf>> {
        if thin_packs_enabled()
            && let Some(packs) = self.thin_pack(tips, exclude, prefix)?
        {
            return Ok(packs);
        }
        self.full_packs(tips, exclude, prefix)
    }

    /// One thin pack of everything reachable from `tips` and not from `exclude`, in memory
    /// (format v2 pieces); empty when there is nothing to send.
    /// Exclusions this cache lacks (another device's tips it has not fetched) are dropped:
    /// the pack may then repeat objects, but never names one git cannot find.
    pub fn thin_pack_bytes(&self, tips: &[String], exclude: &[String]) -> Result<Vec<u8>> {
        let exclude = self.present(exclude)?;
        let input = rev_input(tips, &exclude);
        let bytes = self.run(
            &[
                "pack-objects",
                "--revs",
                "-q",
                "--thin",
                "--stdout",
                "--delta-base-offset",
            ],
            input.as_bytes(),
        )?;
        let empty = bytes.len() < PACK_HEADER + 20
            || u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) == 0;
        Ok(if empty { vec![] } else { bytes })
    }

    /// Self-contained packs of everything reachable from `tips` (format v2 snapshots).
    pub fn full_packs_of(&self, tips: &[String], prefix: &Path) -> Result<Vec<PathBuf>> {
        self.full_packs(tips, &[], prefix)
    }

    /// At most one thin pack; `None` when it would exceed the 100 MiB manifest cap.
    fn thin_pack(
        &self,
        tips: &[String],
        exclude: &[String],
        prefix: &Path,
    ) -> Result<Option<Vec<PathBuf>>> {
        let input = rev_input(tips, exclude);
        let bytes = self.run(
            &[
                "pack-objects",
                "--revs",
                "-q",
                "--thin",
                "--stdout",
                "--delta-base-offset",
            ],
            input.as_bytes(),
        )?;
        if bytes.len() as u64 > MAX_PACK_BYTES {
            return Ok(None);
        }
        if bytes.len() < PACK_HEADER + 20
            || u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) == 0
        {
            return Ok(Some(vec![]));
        }
        let trailer = hex::encode(&bytes[bytes.len() - 20..]);
        let pack = PathBuf::from(format!("{}-{trailer}.pack", prefix.to_string_lossy()));
        std::fs::write(&pack, &bytes).map_err(|e| SyncError::io(pack.display(), e))?;
        Ok(Some(vec![pack]))
    }

    fn full_packs(
        &self,
        tips: &[String],
        exclude: &[String],
        prefix: &Path,
    ) -> Result<Vec<PathBuf>> {
        let input = rev_input(tips, exclude);
        let prefix_s = prefix.to_string_lossy().into_owned();
        let out = self.run(
            &[
                "pack-objects",
                "--revs",
                "-q",
                "--max-pack-size=100m",
                "--delta-base-offset",
                &prefix_s,
            ],
            input.as_bytes(),
        )?;
        let mut packs = vec![];
        for hash in String::from_utf8_lossy(&out).lines() {
            let pack = PathBuf::from(format!("{prefix_s}-{hash}.pack"));
            for ext in ["idx", "rev"] {
                let _ = std::fs::remove_file(pack.with_extension(ext));
            }
            if pack_object_count(&pack)? == 0 {
                let _ = std::fs::remove_file(&pack);
                continue;
            }
            packs.push(pack);
        }
        Ok(packs)
    }

    /// Indexes a verified pack into the cache (`--fsck-objects` checks every object; links
    /// are checked afterwards by [`Git::connected`] once all packs are in).
    pub fn index_pack(&self, pack: &Path) -> Result<()> {
        self.run_with_file(
            // `--fix-thin` completes thin packs from objects already in the cache; it is a
            // no-op for self-contained packs.
            &[
                "index-pack",
                "--stdin",
                "--fix-thin",
                "--fsck-objects",
                "--keep=arkiv",
            ],
            pack,
        )
        .map(|_| ())
        .map_err(|e| SyncError::coded(ErrorCode::BadPack, format!("index-pack: {e}")))
    }

    /// Whether every object reachable from `tips` but not from `old` is present.
    pub fn connected(&self, tips: &[String], old: &[String]) -> bool {
        if tips.is_empty() {
            return true;
        }
        let mut input = tips.join("\n");
        input.push_str("\n--not\n");
        input.push_str(&old.join("\n"));
        input.push('\n');
        self.run(
            &[
                "rev-list",
                "--objects",
                "--missing=error",
                "--quiet",
                "--stdin",
            ],
            input.as_bytes(),
        )
        .is_ok()
    }

    /// In the working repo: `git push --no-verify <cache> <refspecs>`.
    pub fn push_to(&self, cache: &Path, refspecs: &[String]) -> Result<()> {
        if refspecs.is_empty() {
            return Ok(());
        }
        let cache = cache.to_string_lossy().into_owned();
        let mut args = vec!["push", "--no-verify", "-q", cache.as_str()];
        args.extend(refspecs.iter().map(String::as_str));
        self.run(&args, b"").map(|_| ())
    }

    /// In the working repo: fetch `oids` from the cache (`uploadpack.allowAnySHA1InWant`).
    /// `git fetch-pack --keep` of `oids` from `cache`; returns the hashes of the packs it
    /// kept, whose `.keep` files the caller hands back to git (`lock` in the helper).
    pub fn fetch_pack_from(&self, cache: &Path, oids: &[String]) -> Result<Vec<String>> {
        if oids.is_empty() {
            return Ok(vec![]);
        }
        let cache = cache.to_string_lossy().into_owned();
        let mut input = oids.join("\n");
        input.push('\n');
        let out = self.run(
            &[
                "fetch-pack",
                "--stdin",
                "-q",
                "--no-progress",
                "--keep",
                &cache,
            ],
            input.as_bytes(),
        )?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .filter_map(|l| {
                l.strip_prefix("keep\t")
                    .or_else(|| l.strip_prefix("pack\t"))
            })
            .map(str::to_owned)
            .collect())
    }
}

/// Object count from a pack header (0 for an empty pack).
/// Largest pack one manifest can name (1,600 chunks of 64 KiB).
const MAX_PACK_BYTES: u64 = 100 * 1024 * 1024;

/// `CHARTER_THIN_PACKS` set and not empty. Off by default: v0.2.0 readers cannot index
/// thin packs, so writers opt in until every reader has `--fix-thin`.
pub fn thin_packs_enabled() -> bool {
    std::env::var_os("CHARTER_THIN_PACKS").is_some_and(|v| !v.is_empty())
}

/// `git rev-list --stdin` style input: tips, then `--not`, then exclusions.
fn rev_input(tips: &[String], exclude: &[String]) -> String {
    let mut input = tips.join("\n");
    input.push_str("\n--not\n");
    input.push_str(&exclude.join("\n"));
    input.push('\n');
    input
}

pub fn pack_object_count(pack: &Path) -> Result<u32> {
    use std::io::Read;
    let mut f = std::fs::File::open(pack).map_err(|e| SyncError::io(pack.display(), e))?;
    let mut h = [0u8; PACK_HEADER];
    f.read_exact(&mut h)
        .map_err(|e| SyncError::io(pack.display(), e))?;
    if &h[..4] != b"PACK" {
        return Err(SyncError::coded(ErrorCode::BadPack, "not a git pack"));
    }
    Ok(u32::from_be_bytes([h[8], h[9], h[10], h[11]]))
}
