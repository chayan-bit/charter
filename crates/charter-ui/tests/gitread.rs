//! The in-process gix reads match the git CLI they replaced, on a repo with a root commit,
//! a modify, a delete, a rename, a binary, a merge and an annotated tag.

use std::path::Path;
use std::process::Command;

use charter_ui::{diff, gitread};

/// Runs git in `dir` with fixed identity and `date` as both author and committer date.
fn git(dir: &Path, date: u32, args: &[&str]) -> String {
    let when = format!("{} +0000", 1_700_000_000 + date);
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", &when)
        .env("GIT_COMMITTER_DATE", &when)
        .args(["-c", "user.name=T U", "-c", "user.email=t@example.com"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn write(work: &Path, files: &[(&str, &[u8])]) {
    for (p, b) in files {
        let f = work.join(p);
        std::fs::create_dir_all(f.parent().expect("parent")).expect("mkdir");
        std::fs::write(&f, b).expect("write");
    }
}

fn commit(work: &Path, date: u32, msg: &str) -> String {
    git(work, date, &["add", "-A"]);
    git(work, date, &["commit", "-q", "-m", msg]);
    git(work, date, &["rev-parse", "HEAD"])
}

struct Repo {
    _tmp: tempfile::TempDir,
    bare: std::path::PathBuf,
    commits: Vec<String>,
    tag: String,
}

fn build() -> Repo {
    let tmp = tempfile::tempdir().expect("tmp");
    let work = tmp.path().join("w");
    git(tmp.path(), 0, &["init", "-q", &work.to_string_lossy()]);
    let long: String = (0..40).map(|i| format!("line {i}\n")).collect();
    write(&work, &[("a.txt", b"1\n2\n3\n"), ("dir/b.txt", b"b\n")]);
    write(
        &work,
        &[("bin.dat", &[0, 1, 2]), ("old.txt", long.as_bytes())],
    );
    let c1 = commit(&work, 1, "feat: first\n\nbody");
    git(&work, 2, &["tag", "-a", "t1", "-m", "tag", &c1]);
    std::fs::remove_file(work.join("dir/b.txt")).expect("rm");
    std::fs::remove_file(work.join("old.txt")).expect("rm");
    write(&work, &[("a.txt", b"1\ntwo\n3\n4\n"), ("bin.dat", &[0, 9])]);
    write(&work, &[("new.txt", format!("{long}tail\n").as_bytes())]);
    let c2 = commit(&work, 3, "change: modify, delete, rename");
    git(&work, 4, &["checkout", "-q", "-b", "side", &c1]);
    write(&work, &[("s.txt", b"side\n")]);
    let side = commit(&work, 5, "side");
    git(&work, 6, &["checkout", "-q", "main"]);
    git(
        &work,
        7,
        &["merge", "-q", "--no-ff", "-m", "merge side", "side"],
    );
    let merge = git(&work, 7, &["rev-parse", "HEAD"]);
    let bare = tmp.path().join("r");
    let to = bare.join("git");
    git(
        tmp.path(),
        8,
        &[
            "clone",
            "-q",
            "--bare",
            &work.to_string_lossy(),
            &to.to_string_lossy(),
        ],
    );
    let tag = git(&to, 8, &["rev-parse", "t1"]);
    Repo {
        _tmp: tmp,
        bare,
        commits: vec![c1, c2, side, merge],
        tag,
    }
}

/// `(path, from, add, del, binary)` per file.
fn summary(d: &str) -> Vec<(String, Option<String>, usize, usize, bool)> {
    let (files, _) = diff::parse(d, usize::MAX);
    files
        .into_iter()
        .map(|f| (f.path, f.from, f.add, f.del, f.binary))
        .collect()
}

#[test]
fn gix_reads_match_the_git_cli() {
    let r = build();
    let g = gitread::local(&gitread::open(&r.bare).expect("open"));
    let cli = |args: &[&str]| git(&r.bare.join("git"), 9, args);
    let [c1, c2, side, merge] = [0, 1, 2, 3].map(|i| r.commits[i].clone());
    for c in &r.commits {
        let want = cli(&[
            "diff-tree",
            "-p",
            "-M",
            "--root",
            "--no-color",
            "--no-commit-id",
            "--diff-merges=first-parent",
            c,
        ]);
        let got = diff::of_commit(&g, c).expect("diff");
        assert_eq!(
            summary(&got),
            summary(&want),
            "diff of {c}:\n{got}\nvs\n{want}"
        );
    }
    assert!(
        summary(&diff::of_commit(&g, &c2).expect("diff"))
            .iter()
            .any(|f| f.1.as_deref() == Some("old.txt"))
    );
    let log: Vec<String> = gitread::log(&g, &merge, 0, 100)
        .into_iter()
        .map(|c| c.oid)
        .collect();
    assert_eq!(log.join("\n"), cli(&["log", "--format=%H", &merge]));
    let page: Vec<String> = gitread::log(&g, &merge, 1, 2)
        .into_iter()
        .map(|c| c.oid)
        .collect();
    assert_eq!(
        page.join("\n"),
        cli(&["log", "--format=%H", "--skip=1", "-n2", &merge])
    );
    for p in ["", "a.txt", "dir", "new.txt", "s.txt", "bin.dat"] {
        let want = cli(&[
            "log",
            "-1",
            "--format=%H",
            &merge,
            "--",
            if p.is_empty() { "." } else { p },
        ]);
        let got = gitread::last_change(&g, &merge, p)
            .map(|c| c.oid)
            .unwrap_or_default();
        assert_eq!(got, want, "last change of {p:?}");
    }
    let ls: Vec<String> = gitread::tree(&g, &merge, "")
        .expect("tree")
        .iter()
        .map(|e| {
            format!(
                "{} {} {} {}",
                e.kind,
                e.oid,
                e.size.map_or("-".into(), |s| s.to_string()),
                e.name
            )
        })
        .collect();
    let mut want: Vec<String> = cli(&["ls-tree", "--long", &merge])
        .lines()
        .map(|l| {
            let (m, n) = l.split_once('\t').expect("tab");
            let f: Vec<&str> = m.split_whitespace().collect();
            format!("{} {} {} {n}", f[1], f[2], f[3])
        })
        .collect();
    want.sort_by_key(|l| {
        (
            !l.starts_with("tree"),
            l.split(' ').nth(3).map(str::to_owned),
        )
    });
    assert_eq!(ls, want);
    assert_eq!(
        gitread::tree(&g, &merge, "a.txt"),
        None,
        "a blob is not a directory"
    );
    assert_eq!(gitread::blob(&g, &merge, "dir/b.txt"), None);
    assert_eq!(
        gitread::blob(&g, &merge, "a.txt").expect("blob"),
        b"1\ntwo\n3\n4\n"
    );
    let info = gitread::commit(&g, &c1).expect("commit");
    assert_eq!(
        (info.author.as_str(), info.author_time),
        ("T U <t@example.com>", 1_700_000_001)
    );
    assert_eq!(
        (info.message.as_str(), info.git_signed),
        ("feat: first\n\nbody\n", false)
    );
    let m = gitread::commit(&g, &merge).expect("merge");
    assert_eq!(m.parents, vec![c2.clone(), side.clone()]);
    assert_eq!(
        gitread::resolve(&g, None, &r.tag).as_deref(),
        Some(c1.as_str()),
        "tags peel"
    );
    let blob = cli(&["rev-parse", &format!("{merge}:a.txt")]);
    assert_eq!(
        gitread::resolve(&g, None, &blob),
        None,
        "a blob is not a commit"
    );
    assert!(gitread::is_ancestor(&g, &c1, &merge) && gitread::is_ancestor(&g, &side, &merge));
    assert!(!gitread::is_ancestor(&g, &merge, &c1) && !gitread::is_ancestor(&g, &c2, &side));
}
