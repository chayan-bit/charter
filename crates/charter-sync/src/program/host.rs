//! The git a program on engine rhai/2 reads (`charter_program::Host`): the local cache,
//! which holds every object a fold decides on (pushes are staged there before the check).
//! Any git failure is "objects missing", so the fold fails closed and is not checkpointed;
//! a path that is not UTF-8 is a decided error instead, the same on every client.

use charter_program::Host;

use crate::git::Git;

pub struct GitHost(pub Git);

impl Host for GitHost {
    fn changed_paths(&self, a: &[u8], b: &[u8]) -> Option<Result<Vec<String>, String>> {
        let (a, b) = (hex::encode(a), hex::encode(b));
        let out = match a.is_empty() {
            true => self.0.run(&["ls-tree", "-r", "-z", "--name-only", &b], b""),
            false => {
                let args = [
                    "diff-tree",
                    "-r",
                    "-z",
                    "--name-only",
                    "--no-renames",
                    &a,
                    &b,
                ];
                self.0.run(&args, b"")
            }
        };
        let out = out.ok()?;
        let paths = out.split(|c| *c == 0).filter(|p| !p.is_empty());
        Some(
            paths
                .map(|p| {
                    String::from_utf8(p.to_vec())
                        .map_err(|_| format!("path {:?} is not UTF-8", String::from_utf8_lossy(p)))
                })
                .collect(),
        )
    }

    fn file_at(&self, commit: &[u8], path: &str) -> Option<Option<Vec<u8>>> {
        let c = hex::encode(commit);
        let exists = self
            .0
            .exit_code(&["cat-file", "-e", &format!("{c}^{{commit}}")]);
        if exists.ok()? != Some(0) {
            return None;
        }
        if path.contains('\n') {
            return Some(None);
        }
        let out = self
            .0
            .run(&["cat-file", "--batch"], format!("{c}:{path}\n").as_bytes())
            .ok()?;
        let nl = out.iter().position(|b| *b == b'\n')?;
        let head = String::from_utf8_lossy(&out[..nl]);
        let mut it = head.split(' ');
        let (ty, size) = (it.nth(1), it.next().and_then(|s| s.parse::<usize>().ok()));
        match (ty, size) {
            (Some("blob"), Some(n)) => Some(Some(out.get(nl + 1..nl + 1 + n)?.to_vec())),
            _ => Some(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    #[test]
    fn reads_changed_paths_and_files_from_the_cache() {
        let d = tempfile::tempdir().expect("dir");
        git(d.path(), &["init", "-q"]);
        std::fs::create_dir(d.path().join("src")).expect("mkdir");
        std::fs::write(d.path().join("CODEOWNERS"), "* x\n").expect("write");
        std::fs::write(d.path().join("src/a.rs"), "a").expect("write");
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-qm", "one"]);
        let one = hex::decode(git(d.path(), &["rev-parse", "HEAD"])).expect("hex");
        std::fs::write(d.path().join("src/a.rs"), "b").expect("write");
        std::fs::write(d.path().join("new file"), "c").expect("write");
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-qm", "two"]);
        let two = hex::decode(git(d.path(), &["rev-parse", "HEAD"])).expect("hex");
        let h = GitHost(Git::work(Some(d.path())));
        let changed = h.changed_paths(&one, &two).expect("paths").expect("utf8");
        assert_eq!(changed, ["new file", "src/a.rs"]);
        assert_eq!(
            h.changed_paths(&[], &one).expect("all").expect("utf8"),
            ["CODEOWNERS", "src/a.rs"]
        );
        assert_eq!(h.file_at(&one, "CODEOWNERS"), Some(Some(b"* x\n".to_vec())));
        assert_eq!(h.file_at(&one, "nope"), Some(None));
        assert_eq!(h.file_at(&one, "src"), Some(None), "a tree is not a file");
        assert_eq!(h.file_at(&[7; 20], "CODEOWNERS"), None, "missing commit");
        assert_eq!(h.changed_paths(&one, &[7; 20]), None);
    }

    /// A file name that is not UTF-8 (as a patch made on Linux would arrive) is a decided
    /// error, not "objects missing", which would leave the fold undecided for good.
    #[test]
    fn a_non_utf8_path_is_a_decided_error_not_missing_objects() {
        use std::os::unix::ffi::OsStrExt;
        let d = tempfile::tempdir().expect("dir");
        git(d.path(), &["init", "-q"]);
        std::fs::write(d.path().join("a"), "a").expect("write");
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-qm", "one"]);
        let one = hex::decode(git(d.path(), &["rev-parse", "HEAD"])).expect("hex");
        // APFS refuses non-UTF-8 file names, so the tree is built with plumbing.
        let blob = git(d.path(), &["hash-object", "-w", "a"]);
        let mut info = format!("100644,{blob},latin1-").into_bytes();
        info.extend_from_slice(b"\xe9.txt");
        let st = std::process::Command::new("git")
            .args(["update-index", "--add", "--cacheinfo"])
            .arg(std::ffi::OsStr::from_bytes(&info))
            .current_dir(d.path())
            .status()
            .expect("git");
        assert!(st.success());
        git(d.path(), &["commit", "-qm", "two"]);
        let two = hex::decode(git(d.path(), &["rev-parse", "HEAD"])).expect("hex");
        let h = GitHost(Git::work(Some(d.path())));
        let got = h.changed_paths(&one, &two).expect("objects are all here");
        assert!(got.expect_err("decided").contains("not UTF-8"));
        let all = h.changed_paths(&[], &two).expect("objects are all here");
        assert!(all.is_err());
    }
}
