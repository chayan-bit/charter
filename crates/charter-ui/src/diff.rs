//! The commit page's diff: gix writes it as unified text (the shape of `git diff-tree -p
//! -M --root` against the first parent), and [`parse`] turns that into per-file line lists.

use gix::bstr::{BStr, ByteSlice};
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, Diff, InternedInput, UnifiedDiff};
use gix::object::tree::diff::ChangeDetached as Change;

use crate::gitread::Git;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// `+`, `-`, ` ` or `@` (hunk header).
    pub kind: char,
    pub old: Option<u32>,
    pub new: Option<u32>,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileDiff {
    pub path: String,
    /// The old path of a rename.
    pub from: Option<String>,
    pub add: usize,
    pub del: usize,
    pub binary: bool,
    pub lines: Vec<Line>,
}

/// `@@ -a[,b] +c[,d] @@` to `(a, c)`.
fn hunk_start(h: &str) -> Option<(u32, u32)> {
    let mut it = h.split_whitespace().skip(1);
    let num = |s: Option<&str>, sign: char| -> Option<u32> {
        s?.strip_prefix(sign)?.split(',').next()?.parse().ok()
    };
    Some((num(it.next(), '-')?, num(it.next(), '+')?))
}

struct Parser {
    files: Vec<FileDiff>,
    shown: usize,
    limit: usize,
    hidden: usize,
    old: u32,
    new: u32,
}

impl Parser {
    fn push(&mut self, kind: char, text: &str) {
        let Some(f) = self.files.last_mut() else {
            return;
        };
        let (old, new) = match kind {
            '+' => (None, Some(self.new)),
            '-' => (Some(self.old), None),
            ' ' => (Some(self.old), Some(self.new)),
            _ => (None, None),
        };
        match kind {
            '+' => (f.add, self.new) = (f.add + 1, self.new + 1),
            '-' => (f.del, self.old) = (f.del + 1, self.old + 1),
            ' ' => (self.old, self.new) = (self.old + 1, self.new + 1),
            _ => {}
        }
        if self.shown >= self.limit {
            self.hidden += 1;
            return;
        }
        self.shown += 1;
        f.lines.push(Line {
            kind,
            old,
            new,
            text: text.to_owned(),
        });
    }

    fn header(&mut self, l: &str) {
        let Some(f) = self.files.last_mut() else {
            return;
        };
        if let Some(p) = l.strip_prefix("+++ b/") {
            f.path = p.to_owned();
        } else if let Some(p) = l.strip_prefix("--- a/").filter(|_| f.path.is_empty()) {
            f.path = p.to_owned();
        } else if let Some(p) = l.strip_prefix("rename from ") {
            f.from = Some(p.to_owned());
        } else if let Some(p) = l.strip_prefix("rename to ") {
            f.path = p.to_owned();
        } else if l.starts_with("Binary files ") {
            f.binary = true;
        }
    }
}

/// Files of `diff`, with at most `limit` content lines kept; the second value counts the
/// lines left out.
pub fn parse(diff: &str, limit: usize) -> (Vec<FileDiff>, usize) {
    let mut p = Parser {
        files: vec![],
        shown: 0,
        limit,
        hidden: 0,
        old: 0,
        new: 0,
    };
    let mut in_hunk = false;
    for l in diff.lines() {
        if let Some(rest) = l.strip_prefix("diff --git ") {
            let path = rest.split(" b/").last().unwrap_or(rest).to_owned();
            p.files.push(FileDiff {
                path,
                ..FileDiff::default()
            });
            in_hunk = false;
        } else if l.starts_with("@@") {
            if let Some((o, n)) = hunk_start(l) {
                (p.old, p.new, in_hunk) = (o, n, true);
            }
            p.push('@', l);
        } else if in_hunk && let Some(k) = l.chars().next().filter(|c| "+- ".contains(*c)) {
            p.push(k, &l[1..]);
        } else if !in_hunk {
            p.header(l);
        }
    }
    (p.files, p.hidden)
}

/// One side of a changed file: path and blob id.
type Side<'a> = Option<(&'a BStr, gix::ObjectId)>;

/// `(old, new, renamed)`, or `None` for a tree entry (its files come as their own changes).
fn sides(c: &Change) -> Option<(Side<'_>, Side<'_>, bool)> {
    let s = match c {
        Change::Addition {
            location,
            entry_mode,
            id,
            ..
        } if !entry_mode.is_tree() => (None, Some((location.as_bstr(), *id)), false),
        Change::Deletion {
            location,
            entry_mode,
            id,
            ..
        } if !entry_mode.is_tree() => (Some((location.as_bstr(), *id)), None, false),
        Change::Modification {
            location,
            previous_id,
            entry_mode,
            id,
            ..
        } if !entry_mode.is_tree() => (
            Some((location.as_bstr(), *previous_id)),
            Some((location.as_bstr(), *id)),
            false,
        ),
        Change::Rewrite {
            source_location,
            source_id,
            location,
            entry_mode,
            id,
            copy,
            ..
        } if !entry_mode.is_tree() => {
            let (old, new) = (source_location.as_bstr(), location.as_bstr());
            (Some((old, *source_id)), Some((new, *id)), !copy)
        }
        _ => return None,
    };
    Some(s)
}

/// Blob bytes; a submodule commit or a missing object reads as empty.
fn data(git: &Git, side: Side<'_>) -> Vec<u8> {
    side.and_then(|(_, id)| git.find_blob(id).ok())
        .map(|b| b.detach().data)
        .unwrap_or_default()
}

/// Git's binary test: a NUL in the first 8000 bytes.
fn binary(b: &[u8]) -> bool {
    b.iter().take(8_000).any(|c| *c == 0)
}

fn hunks(old: &[u8], new: &[u8]) -> String {
    let (a, b) = (old.to_str_lossy(), new.to_str_lossy());
    let input = InternedInput::new(a.as_ref(), b.as_ref());
    let mut d = Diff::compute(Algorithm::Histogram, &input);
    d.postprocess_lines(&input);
    let sink = ConsumeBinaryHunk::new(String::new(), "\n");
    UnifiedDiff::new(&d, &input, sink, ContextSize::symmetrical(3))
        .consume()
        .unwrap_or_default()
}

/// Appends one file in `git diff` form.
fn file(git: &Git, c: &Change, out: &mut String) {
    let Some((old, new, renamed)) = sides(c) else {
        return;
    };
    let name = |s: Side<'_>| s.map(|(p, _)| p.to_str_lossy().into_owned());
    let (a, b) = (name(old), name(new));
    let (pa, pb) = (a.clone().or(b.clone()), b.clone().or(a.clone()));
    out.push_str(&format!(
        "diff --git a/{} b/{}\n",
        pa.unwrap_or_default(),
        pb.unwrap_or_default()
    ));
    if let (true, Some(a), Some(b)) = (renamed, &a, &b) {
        out.push_str(&format!("rename from {a}\nrename to {b}\n"));
    }
    let (x, y) = (data(git, old), data(git, new));
    if x == y {
        return;
    }
    let path = |p: &Option<String>, side: &str| {
        p.as_ref()
            .map_or("/dev/null".into(), |p| format!("{side}/{p}"))
    };
    if binary(&x) || binary(&y) {
        out.push_str(&format!(
            "Binary files {} and {} differ\n",
            path(&a, "a"),
            path(&b, "b")
        ));
        return;
    }
    out.push_str(&format!("--- {}\n+++ {}\n", path(&a, "a"), path(&b, "b")));
    out.push_str(&hunks(&x, &y));
}

/// Unified diff of `oid` against its first parent (the empty tree for a root commit),
/// with renames detected at git's default 50% similarity.
pub fn of_commit(git: &Git, oid: &str) -> Option<String> {
    let c = git
        .find_commit(gix::ObjectId::from_hex(oid.as_bytes()).ok()?)
        .ok()?;
    let new = c.tree().ok()?;
    let old = match c.parent_ids().next() {
        Some(p) => Some(git.find_commit(p).ok()?.tree().ok()?),
        None => None,
    };
    let opts = gix::diff::Options::default().with_rewrites(Some(Default::default()));
    let mut changes = git.diff_tree_to_tree(old.as_ref(), Some(&new), opts).ok()?;
    changes.sort_by(|x, y| x.location().cmp(y.location()));
    let mut out = String::new();
    for ch in &changes {
        file(git, ch, &mut out);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: &str = "diff --git a/a.txt b/a.txt\nindex 1..2 100644\n--- a/a.txt\n+++ b/a.txt\n@@ -1,2 +1,2 @@\n x\n-old\n+new\ndiff --git a/b.bin b/b.bin\nBinary files a/b.bin and b/b.bin differ\ndiff --git a/c b/d\nsimilarity index 100%\nrename from c\nrename to d\n";

    #[test]
    fn files_hunks_renames_and_binaries_parse() {
        let (f, hidden) = parse(D, usize::MAX);
        assert_eq!((f.len(), hidden), (3, 0));
        assert_eq!((f[0].path.as_str(), f[0].add, f[0].del), ("a.txt", 1, 1));
        let kinds: String = f[0].lines.iter().map(|l| l.kind).collect();
        assert_eq!(kinds, "@ -+");
        assert_eq!((f[0].lines[2].old, f[0].lines[3].new), (Some(2), Some(2)));
        assert!(f[1].binary);
        assert_eq!((f[2].path.as_str(), f[2].from.as_deref()), ("d", Some("c")));
        let (f, hidden) = parse(D, 2);
        assert_eq!((f[0].lines.len(), hidden, f[0].add), (2, 2, 1));
    }
}
