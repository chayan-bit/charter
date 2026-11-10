//! Read-only git plumbing over a repo's bare cache, through charter-sync's `Git` wrapper
//! (no lock: git readers are safe next to a writer). Every ref and path from a URL is
//! validated here and reaches git as one argument after a resolved oid, never via a shell.

use std::path::Path;

use charter_sync::git::Git;

use crate::cache::RepoView;

pub fn cache(repo_dir: &Path) -> Git {
    Git::cache(&repo_dir.join("git"))
}

/// 40 or 64 lowercase hex digits.
pub fn is_oid(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A repo-relative path with no empty, `.` or `..` segment and no control bytes; `""` is
/// the root. `None` means the path is refused.
pub fn clean_path(p: &str) -> Option<String> {
    let segs: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    let ok = segs
        .iter()
        .all(|s| *s != "." && *s != ".." && !s.bytes().any(|b| b < 0x20 || b == 0x7f));
    ok.then(|| segs.join("/"))
}

/// The commit a URL ref names: an oid, or a branch or tag of the canonical refs.
pub fn resolve(git: &Git, view: Option<&RepoView>, r: &str) -> Option<String> {
    let candidate = if is_oid(r) {
        r.to_owned()
    } else {
        let refs = view.map(|v| v.canonical.as_slice()).unwrap_or_default();
        let names = [
            format!("refs/heads/{r}"),
            format!("refs/tags/{r}"),
            r.to_owned(),
        ];
        names
            .iter()
            .find_map(|n| refs.iter().find(|(name, _)| name == n))
            .map(|(_, o)| o.clone())?
    };
    let spec = format!("{candidate}^{{commit}}");
    let out = git
        .run(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &spec,
            ],
            b"",
        )
        .ok()?;
    let oid = String::from_utf8_lossy(&out).trim().to_owned();
    is_oid(&oid).then_some(oid)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub mode: String,
    /// `blob`, `tree` or `commit` (a submodule).
    pub kind: String,
    pub oid: String,
    pub size: Option<u64>,
    pub name: String,
}

/// Object type of `<commit>:<path>` (`tree`, `blob`), `None` if it does not exist.
pub fn kind_at(git: &Git, commit: &str, path: &str) -> Option<String> {
    let out = git
        .run(&["cat-file", "-t", &format!("{commit}:{path}")], b"")
        .ok()?;
    Some(String::from_utf8_lossy(&out).trim().to_owned())
}

/// Entries of the tree at `<commit>:<path>`, directories first, then by name.
pub fn tree(git: &Git, commit: &str, path: &str) -> charter_sync::Result<Vec<Entry>> {
    let out = git.run(
        &["ls-tree", "-z", "--long", &format!("{commit}:{path}")],
        b"",
    )?;
    let mut v: Vec<Entry> = out
        .split(|b| *b == 0)
        .filter_map(|rec| {
            let rec = std::str::from_utf8(rec).ok()?;
            let (meta, name) = rec.split_once('\t')?;
            let mut f = meta.split_whitespace();
            let (mode, kind, oid) = (f.next()?, f.next()?, f.next()?);
            Some(Entry {
                mode: mode.into(),
                kind: kind.into(),
                oid: oid.into(),
                size: f.next().and_then(|s| s.parse().ok()),
                name: name.into(),
            })
        })
        .collect();
    v.sort_by(|a, b| (a.kind != "tree", &a.name).cmp(&(b.kind != "tree", &b.name)));
    Ok(v)
}

/// Bytes of the blob at `<commit>:<path>`.
pub fn blob(git: &Git, commit: &str, path: &str) -> charter_sync::Result<Vec<u8>> {
    git.run(&["cat-file", "blob", &format!("{commit}:{path}")], b"")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRow {
    pub oid: String,
    pub author: String,
    pub time: i64,
    pub subject: String,
}

const LOG_FORMAT: &str = "--format=%H%x00%an%x00%at%x00%s%x1e";

fn rows(out: &[u8]) -> Vec<CommitRow> {
    String::from_utf8_lossy(out)
        .split('\x1e')
        .filter_map(|r| {
            let mut f = r.trim_start_matches('\n').split('\0');
            Some(CommitRow {
                oid: f.next().filter(|o| is_oid(o))?.into(),
                author: f.next()?.into(),
                time: f.next()?.parse().ok()?,
                subject: f.next()?.into(),
            })
        })
        .collect()
}

/// Up to `n` commits from `commit` after skipping `skip`, newest first; with `path`, only
/// commits touching it.
pub fn log(
    git: &Git,
    commit: &str,
    skip: usize,
    n: usize,
    path: Option<&str>,
) -> charter_sync::Result<Vec<CommitRow>> {
    let (skip, n) = (format!("--skip={skip}"), format!("--max-count={n}"));
    let mut args = vec![
        "log",
        LOG_FORMAT,
        &skip,
        &n,
        "--end-of-options",
        commit,
        "--",
    ];
    args.extend(path.filter(|p| !p.is_empty()));
    Ok(rows(&git.run(&args, b"")?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub oid: String,
    pub parents: Vec<String>,
    pub author: String,
    pub author_time: i64,
    pub committer: String,
    pub message: String,
    /// The commit object carries a `gpgsig` header (not checked: no gpg on this path).
    pub git_signed: bool,
}

pub fn commit(git: &Git, oid: &str) -> charter_sync::Result<CommitInfo> {
    let raw = git.run(&["cat-file", "commit", oid], b"")?;
    let text = String::from_utf8_lossy(&raw);
    let (head, message) = text.split_once("\n\n").unwrap_or((&text, ""));
    let field = |k: &str| -> Vec<String> {
        head.lines()
            .filter_map(|l| l.strip_prefix(k)?.strip_prefix(' '))
            .map(str::to_owned)
            .collect()
    };
    let person = |k: &str| field(k).into_iter().next().unwrap_or_default();
    let (author, time) = split_person(&person("author"));
    Ok(CommitInfo {
        oid: oid.to_owned(),
        parents: field("parent"),
        author,
        author_time: time,
        committer: split_person(&person("committer")).0,
        message: message.to_owned(),
        git_signed: !field("gpgsig").is_empty(),
    })
}

/// `Name <mail> 1700000000 +0000` to (`Name <mail>`, time).
fn split_person(p: &str) -> (String, i64) {
    let mut it = p.rsplitn(3, ' ');
    let (_tz, time, who) = (it.next(), it.next(), it.next());
    match (who, time.and_then(|t| t.parse().ok())) {
        (Some(w), Some(t)) => (w.to_owned(), t),
        _ => (p.to_owned(), 0),
    }
}

/// Unified diff of `oid` against its first parent (or the empty tree for a root commit).
pub fn diff(git: &Git, oid: &str) -> charter_sync::Result<String> {
    let out = git.run(
        &[
            "diff-tree",
            "-p",
            "-M",
            "--root",
            "--no-color",
            "--no-commit-id",
            "--diff-merges=first-parent",
            oid,
        ],
        b"",
    )?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Whether `a` is an ancestor of (or equal to) `b`.
pub fn is_ancestor(git: &Git, a: &str, b: &str) -> bool {
    git.run(&["merge-base", "--is-ancestor", a, b], b"").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_and_oids_are_validated() {
        assert_eq!(clean_path("a//b/").as_deref(), Some("a/b"));
        assert_eq!(clean_path("").as_deref(), Some(""));
        for bad in ["../etc", "a/../../b", "./a", "a\nb", "a\0b"] {
            assert_eq!(clean_path(bad), None, "{bad:?}");
        }
        assert!(is_oid(&"a".repeat(40)) && !is_oid(&"A".repeat(40)) && !is_oid("--all"));
        assert_eq!(
            split_person("A B <a@b> 1700000000 +0200"),
            ("A B <a@b>".into(), 1_700_000_000)
        );
    }
}
