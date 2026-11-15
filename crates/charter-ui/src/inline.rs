//! Inline review comments (issue #38, docs/p3/cob-ops.md "Inline review comments"): a
//! review `comment` or `approve` op may carry `comments`, each anchored to a line of the
//! diff of the patch at the op's `head`, with an optional suggested replacement. Payloads
//! come from the chain, so every value is checked here; a bad comment is dropped, never
//! shown, and the rest of the op still counts.

use serde_json::Value;

/// Comments one op may carry; the rest are ignored.
pub const MAX_COMMENTS: usize = 32;
/// Bytes of a path.
pub const PATH_MAX: usize = 1024;
/// Characters of a comment body or of a suggestion.
pub const TEXT_MAX: usize = 8000;
/// Lines one comment may span (`line` to `end_line`).
pub const SPAN_MAX: u32 = 200;
/// The highest line number a comment may name.
const LINE_MAX: u64 = 10_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inline {
    pub path: String,
    /// `new` (the patch head's side) or `old` (the base's side).
    pub side: &'static str,
    pub line: u32,
    /// Equal to `line` for a one-line comment.
    pub end: u32,
    /// Sanitized HTML of the markdown body.
    pub body: String,
    /// Replacement lines for `line..=end` on the new side, if suggested.
    pub suggestion: Option<Vec<String>>,
}

impl Inline {
    /// `src/lib.rs:12` or `src/lib.rs:12-14`.
    pub fn place(&self) -> String {
        match self.end == self.line {
            true => format!("{}:{}", self.path, self.line),
            false => format!("{}:{}-{}", self.path, self.line, self.end),
        }
    }
}

fn path_ok(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= PATH_MAX
        && !p.starts_with('/')
        && !p.chars().any(crate::plain::hidden)
}

fn text(v: &Value) -> Option<&str> {
    v.as_str().filter(|s| s.chars().count() <= TEXT_MAX)
}

fn line(v: &Value) -> Option<u32> {
    v.as_u64()
        .filter(|n| (1..=LINE_MAX).contains(n))
        .and_then(|n| u32::try_from(n).ok())
}

fn one(c: &Value) -> Option<Inline> {
    let path = c["path"].as_str().filter(|p| path_ok(p))?.to_owned();
    let side = match c["side"].as_str().unwrap_or("new") {
        "new" => "new",
        "old" => "old",
        _ => return None,
    };
    let start = line(&c["line"])?;
    let end = match &c["end_line"] {
        Value::Null => start,
        v => line(v).filter(|e| *e >= start && e - start < SPAN_MAX)?,
    };
    let body = text(&c["body"])?;
    let suggestion = match &c["suggestion"] {
        Value::Null => None,
        // A suggestion replaces lines of the head, so it exists on the new side only.
        v if side == "new" => Some(text(v)?.lines().map(str::to_owned).collect()),
        _ => return None,
    };
    (!body.trim().is_empty() || suggestion.is_some()).then(|| Inline {
        path,
        side,
        line: start,
        end,
        body: crate::markdown::render(body),
        suggestion,
    })
}

/// The valid comments of a review payload, at most [`MAX_COMMENTS`], in payload order.
pub fn parse(p: &Value) -> Vec<Inline> {
    let all = p["comments"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    all.iter().take(MAX_COMMENTS).filter_map(one).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn valid_comments_are_kept_and_bad_ones_dropped() {
        let p = json!({"comments": [
            {"path": "src/a.rs", "line": 3, "body": "why?"},
            {"path": "src/a.rs", "side": "old", "line": 2, "end_line": 4, "body": "**gone**"},
            {"path": "b.txt", "line": 1, "body": "", "suggestion": "one\ntwo"},
            {"path": "/abs", "line": 1, "body": "x"},
            {"path": "c", "line": 0, "body": "x"},
            {"path": "c", "line": 5, "end_line": 4, "body": "x"},
            {"path": "c", "line": 1, "end_line": 300, "body": "x"},
            {"path": "c", "side": "left", "line": 1, "body": "x"},
            {"path": "c", "side": "old", "line": 1, "body": "x", "suggestion": "y"},
            {"path": "c", "line": 1, "body": "x".repeat(TEXT_MAX + 1)},
            {"path": "c", "line": 1, "body": " "},
            {"path": "a\u{7}b", "line": 1, "body": "x"},
            "not a map",
        ]});
        let c = parse(&p);
        assert_eq!(c.len(), 3, "{c:?}");
        assert_eq!((c[0].side, c[0].place()), ("new", "src/a.rs:3".into()));
        assert_eq!((c[1].side, c[1].place()), ("old", "src/a.rs:2-4".into()));
        assert!(c[1].body.contains("<strong>gone</strong>"));
        assert_eq!(
            c[2].suggestion.as_deref(),
            Some(&["one".into(), "two".into()][..])
        );
    }

    #[test]
    fn an_op_carries_at_most_the_cap() {
        let many: Vec<Value> = (1..=40)
            .map(|i| json!({"path": "a", "line": i, "body": "x"}))
            .collect();
        assert_eq!(parse(&json!({ "comments": many })).len(), MAX_COMMENTS);
        assert!(parse(&json!({"comments": "x"})).is_empty());
    }
}
