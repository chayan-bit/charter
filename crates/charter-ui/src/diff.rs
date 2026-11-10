//! Unified diff (`git diff-tree -p`) to per-file line lists for the commit page.

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
