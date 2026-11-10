//! Syntax highlighting: syntect scopes mapped to the stylesheet's eight syntax roles and
//! written as flat role runs, one self-contained HTML fragment per line so every line can
//! carry its own anchor. The syntax set loads once, on the first highlighted file.

use std::collections::HashMap;
use std::sync::OnceLock;

use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

/// Lines highlighted per file; the rest is plain text (docs/p6/ui.md section 6).
pub const HIGHLIGHT_LINES: usize = 2_000;
/// Role elements are `<x-k>`, `<x-s>`, ...: autonomous custom elements, which render like
/// `span` and need no `class` attribute, so a highlighted line stays light.
const TAG: &str = "x-";

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

/// The stylesheet's syntax roles. A scope takes the role of its most specific matching
/// entry (most atoms; a later entry wins a tie), which is how the CSS cascade coloured
/// syntect's nested classed spans before this flattening.
const ROLES: [(&[&str], char); 17] = [
    (&["keyword"], 'k'),
    (&["storage"], 'k'),
    (&["string"], 's'),
    (&["constant", "numeric"], 'n'),
    (&["constant", "language"], 'n'),
    (&["comment"], 'c'),
    (&["entity", "name", "function"], 'f'),
    (&["support", "function"], 'f'),
    (&["entity", "name", "type"], 't'),
    (&["storage", "type"], 't'),
    (&["support", "type"], 't'),
    (&["keyword", "operator"], 'o'),
    (&["punctuation"], 'o'),
    // Quotes and comment markers take their string or comment colour, not punctuation's.
    (&["punctuation", "definition", "string"], 's'),
    (&["punctuation", "definition", "comment"], 'c'),
    (&["entity", "other", "attribute-name"], 'a'),
    (&["meta", "attribute"], 'a'),
];

fn role_of(scope: Scope) -> Option<char> {
    let name = scope.build_string();
    let atoms: Vec<&str> = name.split('.').collect();
    ROLES
        .iter()
        .enumerate()
        .filter(|(_, (need, _))| need.iter().all(|a| atoms.contains(a)))
        .max_by_key(|(i, (need, _))| (need.len(), *i))
        .map(|(_, (_, r))| *r)
}

/// The colour at the top of `stack`: its innermost scope that has a role, as inherited
/// colour works in CSS.
fn role(stack: &ScopeStack, memo: &mut HashMap<Scope, Option<char>>) -> Option<char> {
    stack
        .as_slice()
        .iter()
        .rev()
        .find_map(|s| *memo.entry(*s).or_insert_with(|| role_of(*s)))
}

/// One line as flat role runs: adjacent text of one role shares an element, and
/// whitespace joins whatever run it sits in (it has no colour to keep).
#[derive(Default)]
struct Runs {
    out: String,
    role: Option<char>,
    text: String,
}

impl Runs {
    fn push(&mut self, role: Option<char>, text: &str) {
        let blank = text.bytes().all(|b| b == b' ' || b == b'\t');
        if !blank && role != self.role {
            self.flush();
            self.role = role;
        }
        self.text.push_str(&escape_text(text));
    }

    fn flush(&mut self) {
        let text = std::mem::take(&mut self.text);
        match self.role {
            Some(r) if !text.is_empty() => {
                self.out.push_str(&format!("<{TAG}{r}>{text}</{TAG}{r}>"));
            }
            _ => self.out.push_str(&text),
        }
    }

    fn finish(mut self) -> String {
        self.flush();
        self.out
    }
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
    let mut memo = HashMap::new();
    let mut out = Vec::with_capacity(all.len());
    for (i, line) in all.iter().enumerate() {
        let html = (i < HIGHLIGHT_LINES)
            .then(|| classed(set, &mut state, &mut stack, &mut memo, line))
            .flatten();
        out.push(html.unwrap_or_else(|| plain(line)));
    }
    out
}

fn plain(line: &str) -> String {
    escape_text(line.trim_end_matches(['\n', '\r']))
}

/// Escaped for element content (never an attribute): only `&`, `<` and `>`.
pub fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// One line as role runs; `None` if syntect fails on it (the caller falls back to plain).
fn classed(
    set: &SyntaxSet,
    state: &mut ParseState,
    stack: &mut ScopeStack,
    memo: &mut HashMap<Scope, Option<char>>,
    line: &str,
) -> Option<String> {
    let ops = state.parse_line(line, set).ok()?;
    let body = line.trim_end_matches(['\n', '\r']);
    let mut runs = Runs::default();
    let mut at = 0;
    for (i, op) in ops {
        let i = i.min(body.len());
        if i > at {
            runs.push(role(stack, memo), body.get(at..i)?);
            at = i;
        }
        stack.apply(&op).ok()?;
    }
    runs.push(role(stack, memo), body.get(at..)?);
    Some(runs.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_is_flat_role_runs_per_line() {
        let src = "/* a\n b */\npub fn main() { let s = \"<x>\"; }\n";
        let v = lines("main.rs", src, usize::MAX);
        assert_eq!(v.len(), 3);
        assert_eq!(v[1], "<x-c> b */</x-c>", "the block comment carries over");
        // `fn` is `storage.type`, so it takes the type colour, as the old cascade did.
        let head = "<x-k>pub </x-k><x-t>fn </x-t><x-f>main</x-f><x-o>() { </x-o>";
        assert!(v[2].starts_with(head), "{}", v[2]);
        assert!(v[2].contains("<x-s>\"&lt;x&gt;\"</x-s>"), "{}", v[2]);
        for l in &v {
            assert_eq!(l.matches("<x-").count(), l.matches("</x-").count(), "{l}");
        }
    }

    #[test]
    fn roles_follow_the_old_css_cascade() {
        let r = |s: &str| role_of(Scope::new(s).expect("scope"));
        assert_eq!(r("keyword.operator.rust"), Some('o'), "two atoms beat one");
        assert_eq!(r("storage.type.rust"), Some('t'));
        assert_eq!(r("storage.modifier.rust"), Some('k'));
        assert_eq!(r("constant.numeric.integer"), Some('n'));
        assert_eq!(r("entity.name.function.rust"), Some('f'));
        assert_eq!(r("variable.other.rust"), None);
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
        assert!(v[HIGHLIGHT_LINES - 1].contains("<x-"));
        assert!(v[HIGHLIGHT_LINES..].iter().all(|l| !l.contains('<')));
        assert_eq!(lines("gen.rs", &src, 10).len(), 10);
    }
}
