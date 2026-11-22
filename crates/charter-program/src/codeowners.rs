//! CODEOWNERS matching (issue #53, docs/p3/programs.md "Code owners"): which peers own a
//! repo path, by the rules of a `CODEOWNERS` file. Patterns are gitignore globs (gix-glob,
//! as git itself matches them) with GitHub's CODEOWNERS rules on top: the last matching
//! line wins, a line with no owners leaves its paths unowned, `!` lines are ignored, and
//! a pattern ending in `/*` owns only the files directly in that directory. Owners are
//! peer ids, 64 hex digits or `z` + base58btc; other tokens (`@user`, emails) are skipped.

use gix_glob::Pattern;
use gix_glob::pattern::{Case, Mode};
use gix_glob::wildmatch;

/// Where a CODEOWNERS file may sit, in the order they are looked up (GitHub's places).
pub const FILES: [&str; 3] = ["CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"];

/// Lines past this are ignored, so a huge file costs bounded work per path.
const MAX_RULES: usize = 1000;
/// Owners one line may name.
const MAX_OWNERS: usize = 32;

/// A peer id in hex, from 64 hex digits (any case) or `z` + base58btc of 32 bytes.
fn owner(token: &str) -> Option<String> {
    if token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Some(token.to_ascii_lowercase());
    }
    let bytes = bs58::decode(token.strip_prefix('z')?).into_vec().ok()?;
    let id: [u8; 32] = bytes.try_into().ok()?;
    Some(hex::encode(id))
}

/// Whether `pattern` matches `path` or, unless it ends in `/*`, a directory above it.
fn matches(pattern: &Pattern, only_files: bool, path: &str) -> bool {
    let hit = |p: &str, is_dir: bool| {
        let base = p.rfind('/').map(|i| i + 1);
        pattern.matches_repo_relative_path(
            p.into(),
            base,
            Some(is_dir),
            Case::Sensitive,
            wildmatch::Mode::NO_MATCH_SLASH_LITERAL,
        )
    };
    let dirs = path.match_indices('/').map(|(i, _)| &path[..i]);
    hit(path, false) || (!only_files && dirs.into_iter().any(|d| hit(d, true)))
}

struct Rule {
    pattern: Pattern,
    only_files: bool,
    owners: Vec<String>,
}

/// A parsed `CODEOWNERS` file, reusable across many paths.
#[derive(Default)]
pub struct CodeOwners {
    rules: Vec<Rule>,
}

impl CodeOwners {
    /// Parses `text` once into up to [`MAX_RULES`] rules.
    pub fn parse(text: &str) -> Self {
        let mut rules = Vec::new();
        let lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
        for line in lines.filter(|l| !l.starts_with('#')).take(MAX_RULES) {
            let mut it = line.split_whitespace();
            let Some(raw) = it.next() else { continue };
            let Some(pattern) = Pattern::from_bytes(raw.as_bytes()) else {
                continue;
            };
            if pattern.mode.contains(Mode::NEGATIVE) {
                continue;
            }
            let mut owners: Vec<String> = it.filter_map(owner).take(MAX_OWNERS).collect();
            owners.sort();
            owners.dedup();
            rules.push(Rule {
                pattern,
                only_files: raw.ends_with("/*"),
                owners,
            });
        }
        Self { rules }
    }

    /// The owners (peer ids, hex) of the repo path `path` (no leading `/`).
    pub fn owners(&self, path: &str) -> Vec<String> {
        self.rules
            .iter()
            .rev()
            .find(|r| matches(&r.pattern, r.only_files, path))
            .map(|r| r.owners.clone())
            .unwrap_or_default()
    }
}

/// The owners (peer ids, hex) of the repo path `path` (no leading `/`) under `text`.
pub fn owners(text: &str, path: &str) -> Vec<String> {
    CodeOwners::parse(text).owners(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn z(hex: &str) -> String {
        format!(
            "z{}",
            bs58::encode(hex::decode(hex).expect("hex")).into_string()
        )
    }

    #[test]
    fn the_last_matching_line_wins() {
        let text = format!("# owners\n*  {A}\n*.rs {B} @someone\n/docs/ {}\n", z(A));
        assert_eq!(owners(&text, "README.md"), [A]);
        assert_eq!(
            owners(&text, "src/deep/lib.rs"),
            [B],
            "basename glob anywhere"
        );
        assert_eq!(owners(&text, "docs/guide/a.rs"), [A], "z form, later line");
        assert_eq!(
            owners(&text, "src/docs/a.md"),
            [A],
            "anchored /docs/ is root only"
        );
    }

    #[test]
    fn directories_globs_and_unowned_lines() {
        let text = format!("apps/ {A}\ndocs/* {B}\n**/logs {A} {B}\napps/vendor\n");
        assert_eq!(owners(&text, "x/apps/a.c"), [A], "dir pattern anywhere");
        assert_eq!(owners(&text, "docs/a.md"), [B]);
        assert!(owners(&text, "docs/sub/a.md").is_empty(), "/* is one level");
        assert_eq!(owners(&text, "deep/logs/x/y.log"), [A, B]);
        assert!(
            owners(&text, "apps/vendor/lib.c").is_empty(),
            "no owners unowns"
        );
        assert!(owners(&text, "other.txt").is_empty());
    }

    #[test]
    fn bad_lines_and_tokens_are_skipped() {
        let text = format!("!*.md {A}\n*.md nobody {B} {} zzz\n", A.to_uppercase());
        assert_eq!(owners(&text, "a.md"), [A, B]);
        assert!(owners("", "a.md").is_empty());
    }

    #[test]
    fn parsed_codeowners_matches_one_shot_across_paths() {
        let text = format!("* {A}\n*.rs {B}\n/docs/ {}\napps/vendor\n", z(A));
        let parsed = CodeOwners::parse(&text);
        for p in [
            "README.md",
            "src/lib.rs",
            "docs/guide/x.rs",
            "apps/vendor/a.rs",
        ] {
            assert_eq!(parsed.owners(p), owners(&text, p), "{p}");
        }
    }
}
