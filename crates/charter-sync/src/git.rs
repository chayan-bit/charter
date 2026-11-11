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
        // One append instead of four `git config` spawns (git reads the last value).
        let quoted = hooks
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let extra = format!(
            "[uploadpack]\n\tallowAnySHA1InWant = true\n[core]\n\thooksPath = \"{quoted}\"\n\tlogAllRefUpdates = false\n[gc]\n\tauto = 0\n"
        );
        let config = dir.join("config");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&config)
            .and_then(|mut f| f.write_all(extra.as_bytes()))
            .map_err(|e| SyncError::io(config.display(), e))?;
        Ok(g)
    }

    /// `(name, oid)` of every ref under `prefix`.
    pub fn refs(&self, prefix: &str) -> Result<Vec<(String, String)>> {
        self.refs_under(&[prefix])
    }

    /// `(name, oid)` of every ref under any of `prefixes`, in one `for-each-ref`.
    pub fn refs_under(&self, prefixes: &[&str]) -> Result<Vec<(String, String)>> {
        let mut args = vec!["for-each-ref", "--format=%(refname) %(objectname)"];
        args.extend_from_slice(prefixes);
        let out = self.run(&args, b"")?;
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
    /// and without per-object zlib (format v3 pieces, brotli-encoded by the caller,
    /// head-model 13.3); empty when there is nothing to send.
    /// Exclusions this cache lacks (another device's tips it has not fetched) are dropped:
    /// the pack may then repeat objects, but never names one git cannot find.
    /// A pack without zlib larger than `cap` (what readers decode at most) is built again
    /// with git's zlib, as v0.4.0 sent it.
    pub fn thin_pack_bytes(
        &self,
        tips: &[String],
        exclude: &[String],
        cap: u64,
    ) -> Result<Vec<u8>> {
        let exclude = self.present(exclude)?;
        let input = rev_input(tips, &exclude);
        let args = [
            "pack-objects",
            "--revs",
            "-q",
            "--thin",
            "--stdout",
            "--delta-base-offset",
        ];
        let mut bytes = self.run(&[&args[..], NO_ZLIB_ARGS].concat(), input.as_bytes())?;
        if bytes.len() as u64 > cap {
            bytes = self.run(&args, input.as_bytes())?;
        }
        let empty = bytes.len() < PACK_HEADER + 20
            || u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) == 0;
        Ok(if empty { vec![] } else { bytes })
    }

    /// Whether anything reachable from `tips` is missing from `exclude` (as for
    /// [`Self::thin_pack_bytes`]): one object answers, so a refs-only push costs one spawn.
    pub fn has_new_objects(&self, tips: &[String], exclude: &[String]) -> Result<bool> {
        let input = rev_input(tips, &self.present(exclude)?);
        let args = ["rev-list", "--objects", "--max-count=1", "--stdin"];
        Ok(!self.run(&args, input.as_bytes())?.is_empty())
    }

    /// Snapshot packs (head-model 4.4 and 13.3): self-contained packs of everything reachable
    /// from `tips` that no pack in `keep` (git pack trailers of packs in this repo) holds,
    /// with deltas searched afresh and without per-object zlib. Objects of `keep` are never
    /// delta bases, so each layer indexes alone. If any pack would decode past `cap` (what
    /// readers accept), all are built again with git's zlib, as v0.4.0 built them.
    pub fn snapshot_packs(
        &self,
        tips: &[String],
        keep: &[String],
        prefix: &Path,
        cap: u64,
    ) -> Result<Vec<PathBuf>> {
        let keep: Vec<String> = keep
            .iter()
            .map(|t| format!("--keep-pack=pack-{t}.pack"))
            .collect();
        let build = |zlib: &[&str]| {
            let mut extra: Vec<&str> = SNAPSHOT_PACK_ARGS.to_vec();
            extra.extend_from_slice(zlib);
            extra.extend(keep.iter().map(String::as_str));
            self.full_packs_with(tips, &[], prefix, &extra)
        };
        let packs = build(NO_ZLIB_ARGS)?;
        let size = |p: &PathBuf| p.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        if packs.iter().all(|p| size(p) <= cap) {
            return Ok(packs);
        }
        for p in &packs {
            let _ = std::fs::remove_file(p);
        }
        build(&[])
    }

    /// Whether this repo holds the pack with git trailer `trailer`.
    pub fn has_pack(&self, trailer: &str) -> bool {
        let dir = match &self.git_dir {
            Some(d) => d.join("objects/pack"),
            None => match self.run(&["rev-parse", "--absolute-git-dir"], b"") {
                Ok(out) => PathBuf::from(String::from_utf8_lossy(&out).trim()).join("objects/pack"),
                Err(_) => return false,
            },
        };
        trailer.bytes().all(|b| b.is_ascii_hexdigit())
            && dir.join(format!("pack-{trailer}.pack")).is_file()
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
        self.full_packs_with(tips, exclude, prefix, &[])
    }

    fn full_packs_with(
        &self,
        tips: &[String],
        exclude: &[String],
        prefix: &Path,
        extra: &[&str],
    ) -> Result<Vec<PathBuf>> {
        let input = rev_input(tips, exclude);
        let prefix_s = prefix.to_string_lossy().into_owned();
        let mut args = vec![
            "pack-objects",
            "--revs",
            "-q",
            "--max-pack-size=100m",
            "--delta-base-offset",
        ];
        args.extend_from_slice(extra);
        args.push(&prefix_s);
        let out = self.run(&args, input.as_bytes())?;
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

/// `pack-objects` options of snapshot packs: deltas searched afresh with window 50
/// (MEASURED in speed-and-cost 2.8).
const SNAPSHOT_PACK_ARGS: &[&str] = &["--no-reuse-delta", "--window=50"];

/// Packs the writer brotli-encodes whole (head-model 13.3): no per-object zlib, and objects
/// never copied from existing (zlib) packs.
const NO_ZLIB_ARGS: &[&str] = &["--no-reuse-object", "--compression=0"];

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

/// Object count from a pack header (0 for an empty pack).
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn noise(len: usize) -> Vec<u8> {
        let mut x = 0x2545_f491_4f6c_dd1du64;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    /// Two near-identical 200 KB blobs, packed without deltas (as old pushes leave them).
    fn undeltified_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tmp");
        let d = tmp.path();
        sh(d, &["init", "-q"]);
        let mut blob = noise(200_000);
        std::fs::write(d.join("a.bin"), &blob).expect("write");
        sh(d, &["add", "."]);
        sh(d, &["commit", "-qm", "one"]);
        blob[100] ^= 1;
        std::fs::write(d.join("a.bin"), &blob).expect("write");
        sh(d, &["commit", "-qam", "two"]);
        sh(d, &["repack", "-adq", "--window=0", "--depth=0"]);
        tmp
    }

    /// A repo whose one commit holds a 3 MB text blob: about 3 MB without zlib, 13 KB with.
    fn text_blob_repo() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().expect("tmp");
        let d = tmp.path();
        sh(d, &["init", "-q"]);
        let text: Vec<u8> = (0..3_000_000).map(|i| b"fn main() {}\n"[i % 13]).collect();
        std::fs::write(d.join("big.rs"), &text).expect("write");
        sh(d, &["add", "."]);
        sh(d, &["commit", "-qm", "one"]);
        let head = sh(d, &["rev-parse", "HEAD"]);
        (tmp, head)
    }

    #[test]
    fn a_snapshot_pack_that_would_decode_past_the_cap_is_built_with_zlib() {
        let (tmp, head) = text_blob_repo();
        let g = Git::work(Some(tmp.path()));
        let cap = 1 << 20;
        let packs = g
            .snapshot_packs(&[head], &[], &tmp.path().join("snap"), cap)
            .expect("pack");
        assert!(!packs.is_empty());
        for p in &packs {
            let size = p.metadata().expect("meta").len();
            assert!(size <= cap, "{size} B decodes past the {cap} B reader cap");
        }
    }

    #[test]
    fn a_thin_pack_that_would_decode_past_the_cap_is_built_with_zlib() {
        let (tmp, head) = text_blob_repo();
        let g = Git::work(Some(tmp.path()));
        let small = g
            .thin_pack_bytes(std::slice::from_ref(&head), &[], 1 << 20)
            .expect("pack");
        assert!(small.len() <= 1 << 20, "{} B", small.len());
        let raw = g.thin_pack_bytes(&[head], &[], 1 << 30).expect("pack");
        assert!(raw.len() > 2_000_000, "under the cap the pack has no zlib");
    }

    #[test]
    fn a_new_cache_is_configured_without_extra_spawns() {
        let tmp = tempfile::tempdir().expect("tmp");
        let dir = tmp.path().join("odd \"dir\\").join("git");
        let g = Git::init_cache(&dir).expect("init");
        let get = |k: &str| {
            String::from_utf8(g.run(&["config", "--get", k], b"").expect(k)).expect("utf8")
        };
        assert_eq!(get("uploadpack.allowAnySHA1InWant").trim(), "true");
        assert_eq!(get("gc.auto").trim(), "0");
        assert_eq!(get("core.logAllRefUpdates").trim(), "false");
        assert_eq!(
            PathBuf::from(get("core.hooksPath").trim()),
            dir.join("no-hooks")
        );
        assert!(Git::init_cache(&dir).is_ok(), "reopening is a no-op");
    }

    #[test]
    fn snapshot_packs_recompute_deltas_the_cache_lacks() {
        let tmp = undeltified_repo();
        let g = Git::work(Some(tmp.path()));
        let head = sh(tmp.path(), &["rev-parse", "HEAD"]);
        let packs = g
            .snapshot_packs(&[head], &[], &tmp.path().join("snap"), MAX_PACK_BYTES)
            .expect("pack");
        let size: u64 = packs
            .iter()
            .map(|p| p.metadata().expect("meta").len())
            .sum();
        assert!(size < 300_000, "second blob stored as a delta: {size} B");
    }

    #[test]
    fn a_layer_leaves_out_every_object_of_the_kept_packs() {
        let tmp = undeltified_repo();
        let d = tmp.path();
        let g = Git::work(Some(d));
        let pack = sh(d, &["rev-parse", "--git-path", "objects/pack"]);
        let kept: Vec<String> = std::fs::read_dir(d.join(&pack))
            .expect("dir")
            .filter_map(|e| e.ok()?.file_name().into_string().ok())
            .filter_map(|n| Some(n.strip_prefix("pack-")?.strip_suffix(".pack")?.to_owned()))
            .collect();
        assert_eq!(kept.len(), 1);
        assert!(g.has_pack(&kept[0]));
        assert!(!g.has_pack(&"0".repeat(40)));
        std::fs::write(d.join("b.txt"), b"new").expect("write");
        sh(d, &["add", "."]);
        sh(d, &["commit", "-qm", "three"]);
        let head = sh(d, &["rev-parse", "HEAD"]);
        let layer = g
            .snapshot_packs(
                std::slice::from_ref(&head),
                &kept,
                &d.join("layer"),
                MAX_PACK_BYTES,
            )
            .expect("layer");
        assert_eq!(layer.len(), 1);
        // The new commit, its tree and the new blob; nothing of the kept pack.
        assert_eq!(pack_object_count(&layer[0]).expect("count"), 3);
    }
}
