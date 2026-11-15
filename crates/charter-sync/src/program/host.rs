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

    /// Walks `path` one tree at a time, so an object that has not arrived (the commit, a tree
    /// on the way, or the blob) reads as missing, and "no file" means a name a held tree lacks.
    fn file_at(&self, commit: &[u8], path: &str) -> Option<Option<Vec<u8>>> {
        let mut tree = format!("{}^{{commit}}", hex::encode(commit));
        let names: Vec<&str> = path.split('/').collect();
        for (i, name) in names.iter().enumerate() {
            if name.is_empty() || *name == "." || *name == ".." || name.contains('\n') {
                return Some(None);
            }
            let args = [
                "--literal-pathspecs",
                "ls-tree",
                "-z",
                "--full-tree",
                &tree,
                "--",
                name,
            ];
            let out = self.0.run(&args, b"").ok()?;
            let entry = out.split(|b| *b == 0).find_map(|e| {
                let tab = e.iter().position(|b| *b == b'\t')?;
                (&e[tab + 1..] == name.as_bytes())
                    .then(|| String::from_utf8_lossy(&e[..tab]).into_owned())
            });
            let Some(entry) = entry else {
                return Some(None);
            };
            let mut f = entry.split(' ');
            let (ty, oid) = (f.nth(1)?, f.next()?);
            match (ty, i + 1 == names.len()) {
                ("blob", true) => {
                    return self.0.run(&["cat-file", "blob", oid], b"").ok().map(Some);
                }
                ("tree", false) => tree = oid.to_owned(),
                _ => return Some(None),
            }
        }
        Some(None)
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

    /// Drops a loose object from the repo at `d`, as if it never arrived.
    fn lose(d: &std::path::Path, oid: &str) {
        let f = d.join(".git/objects").join(&oid[..2]).join(&oid[2..]);
        std::fs::remove_file(f).expect("loose object");
    }

    /// A base commit owning `*.rs` from `.github/CODEOWNERS`, and a head changing `src/lib.rs`.
    fn owned_repo(owner: &str) -> (tempfile::TempDir, String, String) {
        let d = tempfile::tempdir().expect("dir");
        git(d.path(), &["init", "-q"]);
        std::fs::create_dir_all(d.path().join(".github")).expect("mkdir");
        std::fs::create_dir_all(d.path().join("src")).expect("mkdir");
        let rules = format!("*.rs {owner}\n");
        std::fs::write(d.path().join(".github/CODEOWNERS"), rules).expect("write");
        std::fs::write(d.path().join("src/lib.rs"), "a").expect("write");
        git(d.path(), &["add", "-A"]);
        git(d.path(), &["commit", "-qm", "one"]);
        let one = git(d.path(), &["rev-parse", "HEAD"]);
        std::fs::write(d.path().join("src/lib.rs"), "b").expect("write");
        git(d.path(), &["commit", "-qam", "two"]);
        let two = git(d.path(), &["rev-parse", "HEAD"]);
        (d, one, two)
    }

    #[test]
    fn a_missing_tree_or_blob_is_missing_objects_not_no_file() {
        let (d, one, _) = owned_repo("x");
        let h = GitHost(Git::work(Some(d.path())));
        let c = hex::decode(&one).expect("hex");
        assert_eq!(
            h.file_at(&c, "src/nope.rs"),
            Some(None),
            "absent in a held tree"
        );
        assert_eq!(
            h.file_at(&c, "src/lib.rs/x"),
            Some(None),
            "a blob has no children"
        );
        assert_eq!(h.file_at(&c, "/src/lib.rs"), Some(None));
        lose(
            d.path(),
            &git(d.path(), &["rev-parse", &format!("{one}:src/lib.rs")]),
        );
        assert_eq!(h.file_at(&c, "src/lib.rs"), None, "missing blob");
        lose(
            d.path(),
            &git(d.path(), &["rev-parse", &format!("{one}:.github")]),
        );
        assert_eq!(h.file_at(&c, ".github/CODEOWNERS"), None, "missing subtree");
        assert_eq!(
            h.file_at(&c, "src/nope.rs"),
            Some(None),
            "src is still held"
        );
        lose(
            d.path(),
            &git(d.path(), &["rev-parse", &format!("{one}^{{tree}}")]),
        );
        assert_eq!(h.file_at(&c, "CODEOWNERS"), None, "missing root tree");
    }

    /// CODEOWNERS whose subtree has not arrived must not read as "no owners": the push is
    /// rejected and the fold stays undecided until the objects arrive.
    #[test]
    fn a_missing_codeowners_tree_leaves_the_fold_undecided_and_never_accepts() {
        use charter_program::{Event, Program, ProgramOp, RefUpdate, RepoInfo, cbor_from_json};
        let owner = [0xa1; 32];
        let (d, one, two) = owned_repo(&hex::encode(owner));
        let src = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../programs/codeowners.rhai"
        ))
        .expect("template");
        let id = |b: u8| [b; 32];
        let op = |n: u8, author: u8, ty: &str, action: &str, payload: serde_json::Value| {
            Event::Op(ProgramOp {
                id: id(n),
                author: id(author),
                rid: id(0xee),
                object: id(0x50 + n % 2),
                type_name: ty.into(),
                action: action.into(),
                parents: vec![],
                payload: cbor_from_json(&payload).expect("payload"),
                ts_ms: 1_700_000_000_000 + u64::from(n),
                arkiv_order: u64::from(n),
                proofs: vec![],
            })
        };
        let main = |n: u64, old: &str, new: &str| {
            Event::Push(RefUpdate {
                author: id(0xd1),
                name: "refs/heads/main".into(),
                old: hex::decode(old).expect("hex"),
                new: hex::decode(new).expect("hex"),
                arkiv_order: n,
                signed_base: false,
            })
        };
        let p50 = hex::encode(id(0x50));
        let events = vec![
            main(1, "", &one),
            op(
                2,
                5,
                "xyz.arkiv.patch",
                "create",
                serde_json::json!({"title": "t", "head": two}),
            ),
            op(
                3,
                0xd2,
                "xyz.arkiv.review",
                "approve",
                serde_json::json!({"patch": p50, "head": two}),
            ),
            main(4, &one, &two),
        ];
        let repo = RepoInfo {
            rid: id(0xee),
            delegates: vec![id(0xd1), id(0xd2), id(0xd3)],
            quorum: 2,
            name: "demo".into(),
        };
        let fold = || {
            Program::load(charter_program::engine_for(&src), &src)
                .expect("load")
                .with_host(std::rc::Rc::new(GitHost(Git::work(Some(d.path())))))
                .fold_cached(
                    &src,
                    repo.clone(),
                    Default::default(),
                    &events,
                    &|_, _| Some(true),
                    None,
                )
                .expect("fold")
        };
        let (o, cp) = fold();
        assert_eq!(o.accepted.len(), 1, "a non-owner approval is not enough");
        assert!(
            o.rejected[0].1.contains("src/lib.rs needs an approval"),
            "{:?}",
            o.rejected
        );
        assert!(cp.is_some());
        lose(
            d.path(),
            &git(d.path(), &["rev-parse", &format!("{one}:.github")]),
        );
        let (o, cp) = fold();
        assert_eq!(o.accepted.len(), 1, "never accepted: {:?}", o.rejected);
        assert_eq!(o.rejected.len(), 1);
        assert!(cp.is_none(), "undecided until the objects arrive");
    }
}
