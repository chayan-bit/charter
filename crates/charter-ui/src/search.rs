//! Search (docs/p6/ui-v2.md 7.1): the words a query is made of, the local result sections
//! and the Arkiv source. The Arkiv source is one function ([`ArkivSearch`]) the page calls
//! only when a search is submitted and Settings asks for it; it reuses charter-sync's
//! `chart search` code and every hit it returns has had its identity chain re-read.
//! Everything that comes from the chain is untrusted: [`arkiv_hit`] drops what does not
//! look like a repo and the templates escape the rest.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use charter_store::{Arkiv, StoreError};
use charter_sync::ids::hex32;
use charter_sync::search::{Hit, SearchQuery, search_until};

/// A query is cut to this many characters.
pub const MAX_QUERY: usize = 100;
const MAX_TERMS: usize = 8;
/// Rows shown per local section.
pub const SHOWN: usize = 20;
/// Candidates the Arkiv search verifies: 1 query for the candidates plus 3 for each
/// one-maintainer candidate, so at most 31 queries (310 units) per submitted search.
pub const CANDIDATES: u32 = 10;
/// A submitted Arkiv search stops verifying at this percent of the RPC quota used, well
/// under `pace::PAUSE_AT`, so it can never push the quota into the sync loop's pause.
pub const STOP_AT: f64 = 60.0;
/// Longest description shown for an Arkiv hit.
const MAX_DESC: usize = 200;

/// The words of a query, lowercase; a text matches when it holds every one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terms(Vec<String>);

impl Terms {
    /// `None` for a blank query.
    pub fn parse(q: &str) -> Option<Self> {
        let q: String = q
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_QUERY)
            .collect();
        let t: Vec<String> = q
            .split_whitespace()
            .take(MAX_TERMS)
            .map(str::to_lowercase)
            .collect();
        (!t.is_empty()).then_some(Self(t))
    }

    pub fn matches(&self, text: &str) -> bool {
        let t = text.to_lowercase();
        self.0.iter().all(|w| t.contains(w.as_str()))
    }

    /// The query as shown back, words joined by single spaces.
    pub fn text(&self) -> String {
        self.0.join(" ")
    }

    /// What goes to Arkiv: the first word, as a repo-name prefix, if it can be one
    /// (`[a-z0-9._-]`, 1 to 64 bytes). Arkiv matches prefixes only, not words inside.
    pub fn name_prefix(&self) -> Option<&str> {
        let w = self.0.first()?;
        let ok = w.len() <= 64
            && w.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b));
        ok.then_some(w.as_str())
    }
}

/// Up to [`SHOWN`] rows of a section and how many matched in all.
#[derive(Debug, Clone)]
pub struct Section<T> {
    pub total: usize,
    pub rows: Vec<T>,
}

impl<T> Default for Section<T> {
    fn default() -> Self {
        Self {
            total: 0,
            rows: vec![],
        }
    }
}

impl<T> Section<T> {
    /// Sorts `all` by `key`, keeps the first [`SHOWN`].
    pub fn of<K: Ord>(mut all: Vec<T>, key: impl FnMut(&T) -> K) -> Self {
        all.sort_by_key(key);
        let total = all.len();
        all.truncate(SHOWN);
        Self { total, rows: all }
    }

    pub fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub fn more(&self) -> usize {
        self.total - self.rows.len()
    }
}

/// `a/b c.rs` as one URL path: every byte but `A-Za-z0-9-._~` and `/` is `%XX`.
pub fn enc_path(p: &str) -> String {
    let mut o = String::with_capacity(p.len());
    for b in p.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~/".contains(&b) {
            o.push(char::from(b));
        } else {
            o.push_str(&format!("%{b:02X}"));
        }
    }
    o
}

/// A repo found on Arkiv, as the page shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArkivHit {
    /// Repo id, hex.
    pub hex: String,
    pub name: String,
    pub description: String,
    pub lang: Option<String>,
    pub license: Option<String>,
}

/// What one Arkiv search found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ArkivFound {
    pub hits: Vec<ArkivHit>,
    /// It stopped verifying at [`STOP_AT`] percent of the quota used.
    pub stopped: bool,
}

fn tag(s: &Option<String>) -> Option<String> {
    let ok = |s: &&String| {
        (1..=32).contains(&s.len())
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
    };
    s.as_ref().filter(ok).cloned()
}

/// A verified hit as a row, or `None` when its head does not look like a repo: a name
/// outside `[a-z0-9._-]` or over 64 bytes. The description is stripped of control
/// characters and cut; language and licence must look like short tags.
pub fn arkiv_hit(h: &Hit) -> Option<ArkivHit> {
    let b = &h.head;
    let name_ok = (1..=64).contains(&b.name.len())
        && b.name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c));
    name_ok.then(|| ArkivHit {
        hex: hex32(&h.rid),
        name: b.name.clone(),
        description: b
            .description
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_DESC)
            .collect(),
        lang: tag(&b.lang),
        license: tag(&b.license),
    })
}

/// Why an Arkiv search could not run, in words for the page.
pub type ArkivFuture = Pin<Box<dyn Future<Output = Result<ArkivFound, String>> + Send>>;

/// The Arkiv source: a repo-name prefix in, verified hits out.
pub type ArkivSearch = Arc<dyn Fn(String) -> ArkivFuture + Send + Sync>;

fn quota_high<A: Arkiv>(a: &A) -> bool {
    a.quota()
        .filter(|q| q.until() > Instant::now())
        .and_then(|q| q.used())
        .is_some_and(|p| p >= STOP_AT)
}

/// The Arkiv source over `a` (the sync loop's reads, which bail on a 429): `chart search
/// --prefix`, limited to [`CANDIDATES`] candidates. One candidate query, then the identity
/// chain of each candidate is re-read (the library's verification); hits whose head no
/// longer matches are dropped there, rows that do not look like repos here.
pub fn arkiv<A: Arkiv + 'static>(a: Arc<A>) -> ArkivSearch {
    Arc::new(move |prefix: String| {
        let a = a.clone();
        Box::pin(async move {
            if quota_high(&*a) {
                return Ok(ArkivFound {
                    hits: vec![],
                    stopped: true,
                });
            }
            let q = SearchQuery {
                prefix: Some(prefix),
                limit: CANDIDATES,
                ..SearchQuery::default()
            };
            match search_until(&*a, &q, || quota_high(&*a)).await {
                Ok((hits, stopped)) => Ok(ArkivFound {
                    hits: hits.iter().filter_map(arkiv_hit).collect(),
                    stopped,
                }),
                Err(charter_sync::SyncError::Store(StoreError::RateLimited { retry_after })) => {
                    let wait = retry_after.map_or(60, |d| d.as_secs().max(1));
                    Err(format!(
                        "Arkiv's rate limit was reached. Try again in {wait} s."
                    ))
                }
                Err(e) => {
                    // The detail goes to the log; the page says only that Arkiv failed.
                    charter_sync::warn(&format!("ui search: {e}"));
                    Err("Arkiv did not answer.".into())
                }
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use charter_core::IdBody;

    #[test]
    fn terms_are_lowercase_words_and_every_word_must_match() {
        let t = Terms::parse("  Fix   LEDGER ").expect("terms");
        assert_eq!(t.text(), "fix ledger");
        assert!(t.matches("fix-empty-Ledger"));
        assert!(!t.matches("fix only"));
        assert_eq!(Terms::parse(" \t\n"), None);
        assert_eq!(
            Terms::parse(&"a".repeat(500)).expect("cut").text().len(),
            MAX_QUERY
        );
    }

    #[test]
    fn only_a_possible_repo_name_prefix_goes_to_arkiv() {
        let p = |q| Terms::parse(q).and_then(|t| t.name_prefix().map(str::to_owned));
        assert_eq!(p("Demo repo").as_deref(), Some("demo"));
        assert_eq!(p("my_repo.v2-x").as_deref(), Some("my_repo.v2-x"));
        assert_eq!(p("a/b"), None);
        assert_eq!(p("naïve"), None);
        assert_eq!(p(&"a".repeat(65)), None);
    }

    #[test]
    fn paths_are_percent_encoded_by_segment() {
        assert_eq!(enc_path("src/a b#c?.rs"), "src/a%20b%23c%3F.rs");
        assert_eq!(enc_path("ü"), "%C3%BC");
    }

    #[test]
    fn sections_keep_the_first_rows_and_count_the_rest() {
        let s = Section::of((0..30).rev().collect(), |n| *n);
        assert_eq!(
            (s.total, s.rows.len(), s.more(), s.rows[0]),
            (30, SHOWN, 10, 0)
        );
    }

    fn hit(name: &str, desc: &str, lang: Option<&str>) -> Hit {
        let body = IdBody {
            rid: None,
            rev: 0,
            parent: [0; 32],
            name: name.into(),
            description: desc.into(),
            default_branch: "main".into(),
            delegates: vec![[1; 32]],
            threshold: 1,
            visibility: 0,
            lang: lang.map(Into::into),
            license: None,
            nonce: None,
            program: None,
            archived: None,
        };
        Hit {
            rid: [7; 32],
            head: body,
            at: None,
        }
    }

    #[test]
    fn hostile_rows_are_dropped_or_cleaned() {
        assert_eq!(arkiv_hit(&hit("<b>x</b>", "", None)), None);
        assert_eq!(arkiv_hit(&hit("Upper", "", None)), None);
        assert_eq!(arkiv_hit(&hit("", "", None)), None);
        assert_eq!(arkiv_hit(&hit(&"a".repeat(65), "", None)), None);
        let h = arkiv_hit(&hit("ok", "a\u{7}b\u{1b}[31m<i>", Some("ru<st"))).expect("hit");
        assert_eq!(h.description, "ab[31m<i>");
        assert_eq!(h.lang, None);
        let long = arkiv_hit(&hit("ok", &"d".repeat(900), Some("rust"))).expect("hit");
        assert_eq!(
            (long.description.len(), long.lang.as_deref()),
            (MAX_DESC, Some("rust"))
        );
    }
}
