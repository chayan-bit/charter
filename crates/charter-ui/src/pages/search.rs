//! `/search` (docs/p6/ui-v2.md 7.1): repos, issue and patch titles, commit subjects and file
//! names from the cache on this computer, rendered at once and never touching the chain.
//! With Settings on "Local + Arkiv" a submitted search also asks Arkiv for repos by name,
//! in a second section that loads after the page (an htmx fragment), once per submitted
//! query: typing asks nothing, and neither the sync loop nor `pace.rs` ever searches.

use std::cmp::Reverse;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use charter_sync::ids::{hex32, unhex32};
use serde::Deserialize;

use super::people::Clock;
use super::{App, Layout, Pref, html, seg, short};
use crate::cache::{self, Cobs, RepoView};
use crate::prefs::SearchScope;
use crate::search::{ArkivFound, ArkivHit, Section, Terms, enc_path};
use crate::sync_loop::cached_repos;
use crate::{assets, fold, gitread};

/// Commits read per repo, newest first from the default branch.
const SCAN_COMMITS: usize = 2_000;
/// File names read per repo.
const SCAN_FILES: usize = 20_000;
/// A finished Arkiv search is kept this long, so a reload or Back asks nothing again.
const SEEN_TTL: Duration = Duration::from_secs(300);
const SEEN_MAX: usize = 16;

pub struct RepoRow {
    pub hex: String,
    pub name: String,
    pub description: String,
    pub private: bool,
}

pub struct ThreadRow {
    pub hex: String,
    pub repo: String,
    pub id: String,
    pub title: String,
    pub when: String,
    pub full: String,
    pub block: u64,
}

pub struct CommitRow {
    pub hex: String,
    pub repo: String,
    pub oid: String,
    pub subject: String,
    pub age: String,
    pub time: i64,
}

pub struct FileRow {
    pub repo: String,
    pub href: String,
    pub dir: String,
    pub file: String,
}

/// What the cache on this computer holds for a query.
pub struct Local {
    /// Repos on this computer.
    pub here: usize,
    pub repos: Section<RepoRow>,
    pub issues: Section<ThreadRow>,
    pub patches: Section<ThreadRow>,
    pub commits: Section<CommitRow>,
    pub files: Section<FileRow>,
}

impl Local {
    fn empty(here: usize) -> Self {
        Self {
            here,
            repos: Section::default(),
            issues: Section::default(),
            patches: Section::default(),
            commits: Section::default(),
            files: Section::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
            && self.issues.is_empty()
            && self.patches.is_empty()
            && self.commits.is_empty()
            && self.files.is_empty()
    }
}

fn threads(
    ops: &[charter_core::VerifiedOp],
    type_name: &str,
    terms: &Terms,
    (hex, repo, clock): (&str, &str, &Clock),
) -> Vec<ThreadRow> {
    fold::titles(ops, type_name)
        .into_iter()
        .filter(|(_, title, _)| terms.matches(title))
        .map(|(id, title, block)| ThreadRow {
            hex: hex.to_owned(),
            repo: repo.to_owned(),
            id,
            title,
            when: clock.when(block),
            full: clock.full(block),
            block,
        })
        .collect()
}

/// Reads every cached repo once: the view, the verified ops and, for commit subjects and
/// file names, the default branch of the git cache. No lock, no chain.
fn scan(app: &App, terms: Option<&Terms>) -> Local {
    let rids = cached_repos(&app.home);
    let Some(terms) = terms else {
        return Local::empty(rids.len());
    };
    let (mut repos, mut issues, mut patches, mut commits, mut files) =
        (vec![], vec![], vec![], vec![], vec![]);
    for rid in &rids {
        let hex = hex32(rid);
        let dir = app.home.join("repos").join(&hex);
        let view: Option<RepoView> = cache::read(&dir.join(cache::VIEW)).ok().flatten();
        let name = view
            .as_ref()
            .map_or_else(|| short(&hex), |v| v.name.clone());
        let desc = view.as_ref().map(|v| v.description.clone());
        let desc = desc.unwrap_or_default();
        if terms.matches(&format!("{name} {desc}")) {
            repos.push(RepoRow {
                hex: hex.clone(),
                name: name.clone(),
                description: desc,
                private: view.as_ref().is_some_and(|v| v.private),
            });
        }
        let ops = cache::read::<Cobs>(&dir.join(cache::COBS))
            .ok()
            .flatten()
            .and_then(|c| c.ops().ok())
            .unwrap_or_default();
        let clock = Clock::of(view.as_ref());
        let ctx = (hex.as_str(), name.as_str(), &clock);
        issues.extend(threads(&ops, fold::ISSUE, terms, ctx));
        patches.extend(threads(&ops, fold::PATCH, terms, ctx));
        let Some(v) = &view else { continue };
        let Some(ts) = gitread::open(&dir) else {
            continue;
        };
        let git = gitread::local(&ts);
        let Some(tip) = gitread::resolve(&git, Some(v), &v.default_branch) else {
            continue;
        };
        for c in gitread::log(&git, &tip, 0, SCAN_COMMITS) {
            if terms.matches(&c.subject) {
                commits.push(CommitRow {
                    hex: hex.clone(),
                    repo: name.clone(),
                    age: super::age(c.time),
                    time: c.time,
                    subject: c.subject,
                    oid: c.oid,
                });
            }
        }
        for p in gitread::paths(&git, &tip, SCAN_FILES) {
            if !terms.matches(&p) {
                continue;
            }
            let (dir_part, file) = p.rsplit_once('/').map_or(("", p.as_str()), |(d, f)| (d, f));
            files.push(FileRow {
                href: format!("/r/{hex}/blob/{}/{}", seg(&v.default_branch), enc_path(&p)),
                dir: if dir_part.is_empty() {
                    String::new()
                } else {
                    format!("{dir_part}/")
                },
                file: file.to_owned(),
                repo: name.clone(),
            });
        }
    }
    let first = terms.text();
    Local {
        here: rids.len(),
        repos: Section::of(repos, |r| {
            (!r.name.to_lowercase().contains(&first), r.name.clone())
        }),
        issues: Section::of(issues, |t| Reverse(t.block)),
        patches: Section::of(patches, |t| Reverse(t.block)),
        commits: Section::of(commits, |c| Reverse(c.time)),
        files: Section::of(files, |f| (f.repo.clone(), f.dir.clone(), f.file.clone())),
    }
}

/// An Arkiv hit not on this computer.
pub struct ArkivRow {
    pub hit: ArkivHit,
    pub short: String,
    /// A Fetch was pressed and the first sync has not finished.
    pub fetching: bool,
}

pub enum ArkivState {
    /// The page rendered; the fragment at `href` asks Arkiv.
    Loading {
        href: String,
    },
    Found {
        rows: Vec<ArkivRow>,
        /// Hits already on this computer (shown above, not repeated).
        hidden: usize,
        /// Stopped verifying at the quota mark.
        stopped: bool,
    },
    /// Nothing was asked, and why.
    Skipped(String),
    Failed {
        msg: String,
        retry: String,
    },
}

/// The Arkiv section (`search_arkiv.html`), in the page and as the fragment.
#[derive(Template)]
#[template(path = "search_arkiv.html")]
pub struct ArkivPart {
    pub v: &'static str,
    pub csrf: String,
    /// The query as shown.
    pub q: String,
    /// The name prefix asked for.
    pub prefix: String,
    pub state: ArkivState,
}

impl ArkivPart {
    /// The section as HTML for the page; the template escapes every field.
    pub fn part(&self) -> String {
        self.render().unwrap_or_default()
    }
}

#[derive(Template)]
#[template(path = "search.html")]
pub struct SearchPage {
    pub l: Layout,
    pub q: String,
    pub local: Local,
    pub arkiv_on: bool,
    pub a: Option<ArkivPart>,
    /// A Fetch just started for this repo id.
    pub fetching: Option<String>,
}

impl SearchPage {
    pub fn short(&self, h: &str) -> String {
        short(h)
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct Params {
    pub q: Option<String>,
    /// Ask Arkiv now (the fragment's request, or the link without JavaScript).
    pub arkiv: Option<u8>,
    pub fetching: Option<String>,
}

fn href(q: &str, extra: &str) -> String {
    let enc = serde_urlencoded::to_string([("q", q)]).unwrap_or_default();
    format!("/search?{enc}{extra}")
}

/// The finished search for `prefix`, if one ran in the last [`SEEN_TTL`].
fn seen(app: &App, prefix: &str) -> Option<Arc<ArkivFound>> {
    let m = app.arkiv_seen.lock().ok()?;
    let (at, f) = m.get(prefix)?;
    (at.elapsed() < SEEN_TTL).then(|| f.clone())
}

fn keep(app: &App, prefix: &str, f: &Arc<ArkivFound>) {
    if let Ok(mut m) = app.arkiv_seen.lock() {
        m.retain(|_, (at, _)| at.elapsed() < SEEN_TTL);
        if m.len() >= SEEN_MAX {
            m.clear();
        }
        m.insert(prefix.to_owned(), (Instant::now(), f.clone()));
    }
}

/// Asks Arkiv (or the recent answer) for repos named like the first word of `terms`: at
/// most one candidate query plus the verification of 10 candidates.
async fn ask(app: &Arc<App>, terms: &Terms) -> ArkivState {
    let q = terms.text();
    let Some(prefix) = terms.name_prefix() else {
        return ArkivState::Skipped(
            "Arkiv matches repository names, which use a to z, 0 to 9, dots, dashes and \
             underscores. Nothing was asked."
                .into(),
        );
    };
    let retry = href(&q, "&arkiv=1");
    let found = match seen(app, prefix) {
        Some(f) => f,
        None => {
            if let Some(why) = app.shared.quota_wait() {
                return ArkivState::Skipped(format!("{why}. Nothing was asked."));
            }
            let Some(f) = &app.arkiv else {
                return ArkivState::Skipped("This session has no connection to Arkiv.".into());
            };
            match f(prefix.to_owned()).await {
                Ok(found) => {
                    let found = Arc::new(found);
                    if !found.stopped {
                        keep(app, prefix, &found);
                    }
                    found
                }
                Err(msg) => return ArkivState::Failed { msg, retry },
            }
        }
    };
    let here: BTreeSet<String> = cached_repos(&app.home).iter().map(hex32).collect();
    let (local, rows): (Vec<_>, Vec<_>) = found.hits.iter().partition(|h| here.contains(&h.hex));
    ArkivState::Found {
        hidden: local.len(),
        stopped: found.stopped,
        rows: rows
            .into_iter()
            .map(|h| ArkivRow {
                short: short(&h.hex),
                fetching: unhex32(&h.hex).is_ok_and(|r| app.shared.wanted(&r)),
                hit: h.clone(),
            })
            .collect(),
    }
}

fn part(app: &App, terms: &Terms, state: ArkivState) -> ArkivPart {
    ArkivPart {
        v: assets::version(),
        csrf: app.csrf.clone(),
        q: terms.text(),
        prefix: terms.name_prefix().unwrap_or_default().to_owned(),
        state,
    }
}

pub async fn page(
    State(app): State<Arc<App>>,
    pref: Pref,
    headers: HeaderMap,
    Query(p): Query<Params>,
) -> Response {
    app.shared.touch();
    let terms = p.q.as_deref().and_then(Terms::parse);
    let arkiv_on = pref.0.search == SearchScope::Arkiv;
    // `arkiv_on` is read from this request's prefs: under "Local only" nothing below asks.
    let asked = terms.as_ref().filter(|_| arkiv_on);
    if let Some(t) = asked
        && p.arkiv.is_some()
        && headers.contains_key("hx-request")
    {
        let state = ask(&app, t).await;
        return html(&part(&app, t, state));
    }
    let scanned = {
        let (app, terms) = (app.clone(), terms.clone());
        tokio::task::spawn_blocking(move || scan(&app, terms.as_ref())).await
    };
    let Ok(local) = scanned else {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    };
    let a = match asked {
        Some(t) if p.arkiv.is_some() => {
            let state = ask(&app, t).await;
            Some(part(&app, t, state))
        }
        Some(t) => {
            let state = ArkivState::Loading {
                href: href(&t.text(), "&arkiv=1"),
            };
            Some(part(&app, t, state))
        }
        None => None,
    };
    let q = terms.as_ref().map(Terms::text).unwrap_or_default();
    let mut l = Layout::new(
        &app,
        &pref,
        if q.is_empty() {
            "Search".into()
        } else {
            format!("{q} - Search")
        },
    );
    l.nav = "search";
    let fetching = p
        .fetching
        .filter(|f| f.len() == 64 && f.bytes().all(|b| b.is_ascii_hexdigit()));
    html(&SearchPage {
        l,
        q,
        local,
        arkiv_on,
        a,
        fetching,
    })
}

#[derive(Debug, Deserialize)]
pub struct FetchForm {
    pub rid: String,
    pub q: Option<String>,
}

/// `POST /search/fetch` (CSRF-guarded): the first sync of a repo found on Arkiv, the same
/// `Shared::want` that `chart ui <rid>` uses. The sync loop runs it at once; the repo
/// shows under Repositories when it is verified here.
pub async fn fetch(State(app): State<Arc<App>>, Form(f): Form<FetchForm>) -> Response {
    let Ok(rid) = unhex32(&f.rid) else {
        return (StatusCode::BAD_REQUEST, "not a repo id\n").into_response();
    };
    app.shared.want(rid);
    let q = f.q.unwrap_or_default();
    let to = href(&q, &format!("&fetching={}", hex32(&rid)));
    (StatusCode::SEE_OTHER, [(header::LOCATION, to)]).into_response()
}
