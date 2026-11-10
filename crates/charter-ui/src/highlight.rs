//! Syntax highlighting with syntect's classed HTML (`sy-*` scope classes coloured by the
//! stylesheet's eight syntax roles), one self-contained HTML fragment per line so every
//! line can carry its own anchor. The syntax set loads once, on the first highlighted file.

use std::sync::OnceLock;

use syntect::html::{ClassStyle, line_tokens_to_classed_spans};
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Lines highlighted per file; the rest is plain text (docs/p6/ui.md section 6).
pub const HIGHLIGHT_LINES: usize = 2_000;
const STYLE: ClassStyle = ClassStyle::SpacedPrefixed { prefix: "sy-" };

fn syntaxes() -> &'static SyntaxSet {
    static SET: OnceLock<SyntaxSet> = OnceLock::new();
    SET.get_or_init(SyntaxSet::load_defaults_newlines)
}

/// HTML-escaped `s`.
pub fn escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&#39;"),
            c => o.push(c),
        }
    }
    o
}

fn open_span(scope: &Scope) -> String {
    let classes: Vec<String> = scope
        .build_string()
        .split('.')
        .map(|a| format!("sy-{a}"))
        .collect();
    format!("<span class=\"{}\">", classes.join(" "))
}

fn syntax_for<'a>(set: &'a SyntaxSet, name: &str, first: &str) -> Option<&'a SyntaxReference> {
    let ext = name.rsplit_once('.').map_or(name, |(_, e)| e);
    set.find_syntax_by_extension(ext)
        .or_else(|| set.find_syntax_by_extension(name))
        .or_else(|| set.find_syntax_by_first_line(first))
}

/// Per-line HTML of `text` (file `name`): the first [`HIGHLIGHT_LINES`] lines highlighted
/// when a syntax is known, the rest escaped plain text. At most `limit` lines are returned.
pub fn lines(name: &str, text: &str, limit: usize) -> Vec<String> {
    let all: Vec<&str> = LinesWithEndings::from(text).take(limit).collect();
    let set = syntaxes();
    let first = all.first().copied().unwrap_or("");
    let Some(syntax) = syntax_for(set, name, first) else {
        return all.iter().map(|l| plain(l)).collect();
    };
    let mut state = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut out = Vec::with_capacity(all.len());
    for (i, line) in all.iter().enumerate() {
        let html = (i < HIGHLIGHT_LINES)
            .then(|| classed(set, &mut state, &mut stack, line))
            .flatten();
        out.push(html.unwrap_or_else(|| plain(line)));
    }
    out
}

fn plain(line: &str) -> String {
    escape(line.trim_end_matches(['\n', '\r']))
}

/// One line with the spans still open from earlier lines reopened first and every span
/// closed at the end; `None` if syntect fails on it (the caller falls back to plain).
fn classed(
    set: &SyntaxSet,
    state: &mut ParseState,
    stack: &mut ScopeStack,
    line: &str,
) -> Option<String> {
    let ops = state.parse_line(line, set).ok()?;
    let reopen: String = stack.as_slice().iter().map(open_span).collect();
    let depth = stack.len() as isize;
    let body = line.trim_end_matches(['\n', '\r']);
    let ops: Vec<_> = ops
        .into_iter()
        .map(|(i, op)| (i.min(body.len()), op))
        .collect();
    let (html, delta) = line_tokens_to_classed_spans(body, &ops, STYLE, stack).ok()?;
    let close = "</span>".repeat(usize::try_from(depth + delta).unwrap_or(0));
    Some(format!("{reopen}{html}{close}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_is_classed_per_line_and_balanced() {
        let src = "/* a\n b */\nfn main() { let s = \"<x>\"; }\n";
        let v = lines("main.rs", src, usize::MAX);
        assert_eq!(v.len(), 3);
        for l in &v {
            assert_eq!(
                l.matches("<span").count(),
                l.matches("</span>").count(),
                "{l}"
            );
        }
        assert!(
            v[1].contains("sy-comment"),
            "the block comment reopens: {}",
            v[1]
        );
        assert!(v[2].contains("sy-keyword") || v[2].contains("sy-storage"));
        assert!(v[2].contains("&lt;x&gt;") && !v[2].contains("<x>"));
    }

    #[test]
    fn unknown_types_are_escaped_plain_text() {
        let v = lines("notes.zzz", "<b>&\n", usize::MAX);
        assert_eq!(v, vec!["&lt;b&gt;&amp;".to_owned()]);
    }

    #[test]
    fn only_the_first_2000_lines_are_highlighted() {
        let src: String = (0..2_500).map(|i| format!("let x{i} = {i};\n")).collect();
        let v = lines("gen.rs", &src, usize::MAX);
        assert_eq!(v.len(), 2_500);
        assert!(v[HIGHLIGHT_LINES - 1].contains("<span"));
        assert!(v[HIGHLIGHT_LINES..].iter().all(|l| !l.contains('<')));
        assert_eq!(lines("gen.rs", &src, 10).len(), 10);
    }
}
