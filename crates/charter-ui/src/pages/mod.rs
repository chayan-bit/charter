//! Page handlers. Each loads what it needs from the repo cache on a blocking thread (git
//! runs as subprocesses), renders one askama template and never touches Arkiv.

pub mod cobs;
pub mod code;
pub mod forms;
pub mod history;
pub mod live;
pub mod money;
pub mod people;
pub mod program;
pub mod repos;
pub mod review;
pub mod search;
pub mod settings;
mod verify;
pub mod wallet;
pub mod write;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use askama::Template;
use axum::extract::FromRequestParts;
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{Html, IntoResponse, Response};
use charter_core::{Rid, VerifiedOp};
use charter_sync::ids::{hex32, parse_id, to_text};

use crate::cache::{self, Cobs, RepoView};
use crate::gitread::{self, Git};
use crate::last::{self, Last};
use crate::prefs::{Fonts, Prefs, Store};
use crate::sync_loop::{Round, Shared, mins_until};
use crate::{assets, fold};

use people::{Clock, People};
pub use verify::Verify;

/// Shared by every handler.
type Opened = (u64, gix::ThreadSafeRepository, Arc<Last>);

pub struct App {
    pub home: PathBuf,
    pub shared: Arc<Shared>,
    pub csrf: String,
    /// Signs and sends writes; `None` when `$CHARTER_HOME` has no keys.
    pub writer: Option<Arc<crate::writes::Writer>>,
    /// Local preferences (`ui/prefs.json`).
    pub prefs: Store,
    /// The Arkiv source of a submitted search ([`crate::search::arkiv`]); `None` in tests
    /// that read no chain.
    pub arkiv: Option<crate::search::ArkivSearch>,
    /// Arkiv results by query for a few minutes, so a reload or Back costs no queries.
    arkiv_seen: Mutex<HashMap<String, (std::time::Instant, Arc<crate::search::ArkivFound>)>>,
    /// One gix handle and last-commit index per repo, keyed by the `synced_ms` of the view
    /// they were read under.
    repos: Mutex<HashMap<Rid, Opened>>,
}

impl App {
    pub fn new(home: PathBuf, shared: Arc<Shared>, csrf: String) -> Self {
        Self {
            prefs: Store::load(&home),
            home,
            shared,
            csrf,
            writer: None,
            arkiv: None,
            arkiv_seen: Mutex::default(),
            repos: Mutex::default(),
        }
    }

    /// Submitted searches may also ask Arkiv, through `f`.
    pub fn with_arkiv(self, f: crate::search::ArkivSearch) -> Self {
        Self {
            arkiv: Some(f),
            ..self
        }
    }

    /// Writes enabled, through `w`.
    pub fn with_writer(self, w: Arc<crate::writes::Writer>) -> Self {
        Self {
            writer: Some(w),
            ..self
        }
    }

    /// The repo's handle and `last.json`, read once and again after each sync round so new
    /// refs, packs and tips show.
    fn git(&self, rid: Rid, dir: &Path, synced: u64) -> Option<(Git, Arc<Last>)> {
        let mut m = self.repos.lock().ok()?;
        if let Some((_, ts, last)) = m.get(&rid).filter(|(s, _, _)| *s == synced) {
            return Some((gitread::local(ts), last.clone()));
        }
        let ts = gitread::open(dir)?;
        let g = gitread::local(&ts);
        let last: Arc<Last> = Arc::new(
            cache::read(&dir.join(last::FILE))
                .ok()
                .flatten()
                .unwrap_or_default(),
        );
        m.insert(rid, (synced, ts, last.clone()));
        Some((g, last))
    }
}

/// The preferences one request renders with (read once, so a page is consistent while
/// Settings saves).
#[derive(Debug, Clone, Default)]
pub struct Pref(pub Arc<Prefs>);

impl FromRequestParts<Arc<App>> for Pref {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(_: &mut Parts, app: &Arc<App>) -> Result<Self, Self::Rejection> {
        Ok(Pref(app.prefs.get()))
    }
}

/// The header's sync indicator: `ok`, `busy`, `warn`, `bad` or `idle`, what it says, its
/// tooltip, and where it leads (the failing repo, or the list when several fail).
#[derive(Debug, Clone)]
pub struct SyncChip {
    pub state: &'static str,
    pub text: String,
    pub title: String,
    pub href: String,
}

impl SyncChip {
    pub fn of(shared: &Shared) -> Self {
        let h = shared.health();
        let chip = |state, text: String, title: String, href: String| Self {
            state,
            text,
            title,
            href,
        };
        if let Some(t) = shared.paused().filter(|t| *t > std::time::Instant::now()) {
            let m = mins_until(t);
            return chip(
                "warn",
                "Sync paused".into(),
                format!("Arkiv's RPC quota is nearly used up. Sync resumes in {m} min."),
                String::new(),
            );
        }
        match h.failing.as_slice() {
            [(rid, e)] => chip(
                "bad",
                "1 repo failed to sync".into(),
                format!("Sync failed: {e}"),
                format!("/r/{}", hex32(rid)),
            ),
            [_, ..] => chip(
                "bad",
                format!("{} repos failed to sync", h.failing.len()),
                "Open the repo list to see why".into(),
                "/".into(),
            ),
            [] if h.running => chip(
                "busy",
                "Syncing".into(),
                "Checking Arkiv for changes".into(),
                String::new(),
            ),
            [] => match h.ok_ms {
                Some(t) => chip(
                    "ok",
                    format!("Synced {}", ago(t)),
                    "Every repo here was checked against Arkiv".into(),
                    String::new(),
                ),
                None => chip(
                    "idle",
                    "Starting sync".into(),
                    "The first check against Arkiv has not finished".into(),
                    String::new(),
                ),
            },
        }
    }
}

/// The header's wallet chip, from the balance the writer last read.
#[derive(Debug, Clone)]
pub struct WalletChip {
    /// `None` before the first read.
    pub amount: Option<money::Amount>,
    pub low: bool,
}

/// A page-wide banner: `warn`, `bad` or `off`, an icon, bold lead, text and a link.
#[derive(Debug, Clone)]
pub struct Banner {
    pub kind: &'static str,
    pub icon: &'static str,
    pub lead: String,
    pub text: String,
    pub link: Option<(String, String)>,
}

/// Fields of the page frame (`base.html`).
#[derive(Debug, Clone)]
pub struct Layout {
    pub title: String,
    pub v: &'static str,
    pub csrf: String,
    pub theme: &'static str,
    /// System fonts instead of the embedded ones (Settings).
    pub sysfont: bool,
    /// Write forms are shown (keys are loaded and the repo is not archived).
    pub writes: bool,
    /// The repo is archived, which is why write forms are hidden.
    pub archived: bool,
    /// Which main destination this page is under: `repos`, `wallet`, `search` or `settings`.
    pub nav: &'static str,
    pub sync: SyncChip,
    pub wallet: Option<WalletChip>,
    pub banners: Vec<Banner>,
    /// `(href, label)`; an empty href is the current page.
    pub crumbs: Vec<(String, String)>,
    /// The page's event stream (`live.js`): the repo's on repo pages, every repo's on the list.
    pub events: String,
    /// What the header's live parts refetch on a repo page (`/r/<rid>/verify`); empty
    /// elsewhere, where they refetch the page itself.
    pub live_src: String,
}

impl Layout {
    pub fn new(app: &App, pref: &Pref, title: impl Into<String>) -> Self {
        let notice = app.prefs.notice().map(|n| Banner {
            kind: "warn",
            icon: "warning-circle",
            lead: "Settings reset.".into(),
            text: n,
            link: Some(("/settings".into(), "Open Settings".into())),
        });
        Self {
            title: title.into(),
            v: assets::version(),
            csrf: app.csrf.clone(),
            theme: pref.0.theme.attr(),
            sysfont: pref.0.fonts == Fonts::System,
            writes: app.writer.is_some(),
            archived: false,
            nav: "repos",
            sync: SyncChip::of(&app.shared),
            wallet: app.writer.as_deref().map(wallet::chip),
            banners: notice.into_iter().collect(),
            crumbs: vec![],
            events: String::new(),
            live_src: String::new(),
        }
    }

    /// Why write forms are missing.
    pub fn off(&self) -> &'static str {
        match self.archived {
            true => ARCHIVED_OFF,
            false => write::WRITES_OFF,
        }
    }
}

pub const ARCHIVED_OFF: &str =
    "This repository is archived, so Charter writes nothing to it (advisory: other clients can).";

/// Seven hex digits, as every hash is shown.
pub fn short(h: &str) -> String {
    h.chars().take(7).collect()
}

/// `12 s ago`, `3 min ago`, `5 h ago`, `4 d ago`.
pub fn ago(then_ms: u64) -> String {
    let s = crate::sync_loop::now_ms().saturating_sub(then_ms) / 1000;
    match s {
        0..10 => "just now".into(),
        10..60 => format!("{s} s ago"),
        60..3_600 => format!("{} min ago", s / 60),
        3_600..86_400 => format!("{} h ago", s / 3_600),
        _ => format!("{} d ago", s / 86_400),
    }
}

/// [`ago`] of a git timestamp in seconds.
pub fn age(secs: i64) -> String {
    ago(u64::try_from(secs).unwrap_or(0).saturating_mul(1000))
}

/// `%2F`-escapes `/` and `%` so a ref name stays one path segment.
pub fn seg(r: &str) -> String {
    r.replace('%', "%25").replace('/', "%2F")
}

/// One cached repo, loaded for a request.
pub struct Repo {
    pub rid: Rid,
    pub hex: String,
    pub dir: PathBuf,
    pub git: Git,
    /// The last-commit index of the ref tips; empty until a sync round writes it.
    pub last: Arc<Last>,
    pub view: Option<RepoView>,
    pub round: Round,
}

impl Repo {
    /// A commit's row: a lookup in the last-commit index when it is a ref tip, else one
    /// object read.
    pub fn tip(&self, commit: &str) -> Option<gitread::CommitRow> {
        self.last
            .get(commit, "")
            .or_else(|| gitread::tip(&self.git, commit))
    }

    pub fn name(&self) -> String {
        self.view
            .as_ref()
            .map_or_else(|| short(&self.hex), |v| v.name.clone())
    }

    /// Default branch name for links (`main` before the first sync).
    pub fn default_ref(&self) -> String {
        self.view
            .as_ref()
            .map_or("main".into(), |v| v.default_branch.clone())
    }

    /// Cached verified ops; an unreadable cache reads as empty (the panel shows the error).
    pub fn ops(&self) -> Vec<VerifiedOp> {
        cache::read::<Cobs>(&self.dir.join(cache::COBS))
            .ok()
            .flatten()
            .and_then(|c| c.ops().ok())
            .unwrap_or_default()
    }
}

/// Loads the repo `rid` names (hex or `z...`), or the 404 page.
pub fn open(app: &App, pref: &Pref, rid: &str) -> Result<Repo, Response> {
    let gone = || not_found_page(app, pref, "No such repo in this $CHARTER_HOME.");
    let rid = parse_id(rid).map_err(|_| gone())?;
    let hex = hex32(&rid);
    let dir = app.home.join("repos").join(&hex);
    if !dir.join("git").is_dir() {
        return Err(gone());
    }
    app.shared.focus(rid);
    let view: Option<RepoView> = cache::read(&dir.join(cache::VIEW)).ok().flatten();
    let synced = view.as_ref().map_or(0, |v| v.synced_ms);
    let Some((git, last)) = app.git(rid, &dir, synced) else {
        let msg = "The git cache of this repo cannot be opened.";
        return Err(error_page(
            app,
            pref,
            StatusCode::INTERNAL_SERVER_ERROR,
            msg,
        ));
    };
    Ok(Repo {
        rid,
        view,
        git,
        last,
        round: app.shared.round(&rid),
        hex,
        dir,
    })
}

/// Repo header, tabs and the Verified seal (`repo.html`), plus who and when for the page.
#[derive(Debug, Clone)]
pub struct Head {
    pub hex: String,
    pub name: String,
    pub description: String,
    pub default_ref: String,
    pub tab: &'static str,
    pub issues: usize,
    pub patches: usize,
    pub verify: Verify,
    pub private: bool,
    /// Revoked devices' leaves this admin can remove (a private repo's priced write).
    pub revoked_leaves: usize,
    /// `z...` address, as `git clone arkiv::` takes it.
    pub addr: String,
    pub clock: Clock,
    pub people: People,
    /// The newest signed head: `(block, peer)`, the last push.
    pub pushed: Option<(u64, String)>,
    /// Archived (issue #63): advisory, so honest clients stop writing to it.
    pub archived: bool,
    /// When the earliest entity the repo needs runs out on Arkiv: `(date, class)`.
    pub renew: Option<(String, &'static str)>,
}

/// The newest signed head of a view: `(block, peer)`.
pub fn last_push(v: Option<&RepoView>) -> Option<(u64, String)> {
    v.into_iter()
        .flat_map(|v| &v.peers)
        .filter_map(|p| p.head.as_ref().map(|h| (h.block, p.nid.clone())))
        .max()
}

/// The date of the earliest expiry among what the repo needs, read off the cached view, and
/// how loud to be about it: `bad` once passed, `warn` from where `git fetch` warns (7 days;
/// a push writes data for 30, so a 30-day line would be amber from the first push).
fn renew_by(v: &RepoView) -> Option<(String, &'static str)> {
    renew_at(
        v.min_expiry,
        v.head_block,
        v.synced_ms,
        crate::sync_loop::now_ms(),
    )
}

/// [`renew_by`] at the wall-clock time `now` (ms).
fn renew_at(
    min_expiry: Option<u64>,
    head_block: u64,
    synced_ms: u64,
    now: u64,
) -> Option<(String, &'static str)> {
    const WARN_MS: u64 = charter_sync::repo::WARN_EXPIRY_BLOCKS * people::BLOCK_MS;
    let ahead = min_expiry?.saturating_sub(head_block);
    let at = synced_ms.saturating_add(ahead.saturating_mul(people::BLOCK_MS));
    let left = at.saturating_sub(now);
    let class = match left {
        0 => "bad",
        l if l < WARN_MS => "warn",
        _ => "",
    };
    Some((people::date(at), class))
}

impl Head {
    pub fn new(r: &Repo, ops: &[VerifiedOp], tab: &'static str, people: People) -> Self {
        let count = |t: &str| {
            ops.iter()
                .filter(|o| o.body.type_name == t && o.body.is_create())
                .count()
        };
        let v = r.view.as_ref();
        Self {
            hex: r.hex.clone(),
            name: r.name(),
            description: v.map(|v| v.description.clone()).unwrap_or_default(),
            default_ref: seg(&r.default_ref()),
            tab,
            issues: count(fold::ISSUE),
            patches: count(fold::PATCH),
            verify: Verify::new(v, &r.round),
            private: v.is_some_and(|v| v.private),
            revoked_leaves: v.map_or(0, |v| v.revoked_leaves),
            addr: to_text(&r.rid),
            clock: Clock::of(v),
            pushed: last_push(v),
            archived: v.is_some_and(|v| v.archived),
            renew: v.and_then(renew_by),
            people,
        }
    }

    /// This computer's identity is one of the repo's maintainers.
    pub fn maintains(&self) -> bool {
        let you = self.people.you.as_ref();
        you.is_some_and(|y| self.verify.delegates.contains(y))
    }

    /// `nid` is this computer's own identity.
    pub fn is_you(&self, nid: &str) -> bool {
        self.people.you.as_deref() == Some(nid)
    }

    /// A peer's name (alias, nickname or short id, per Settings).
    pub fn who(&self, nid: &str) -> String {
        self.people.name(nid)
    }
    /// `12 min ago` of a block.
    pub fn when(&self, block: &u64) -> String {
        self.clock.when(*block)
    }
    /// Tooltip of a block time, with the block number.
    pub fn full(&self, block: &u64) -> String {
        self.clock.full(*block)
    }
    /// The author's own clock, when Settings shows it.
    pub fn their(&self, ts_ms: &u64) -> Option<String> {
        self.people.author_clock(*ts_ms)
    }
    pub fn short(&self, h: &str) -> String {
        short(h)
    }
    pub fn glm(&self, wei: &str) -> money::Amount {
        money::glm(wei)
    }
    /// Maintainer names, joined: `you, ravi`.
    pub fn maintainers(&self) -> String {
        let names: Vec<String> = self.verify.delegates.iter().map(|d| self.who(d)).collect();
        names.join(", ")
    }
}

/// The people of a page: aliases from the view, nicknames from Settings, "you" from keys.
pub fn people(app: &App, pref: &Pref, v: Option<&RepoView>) -> People {
    let you = app.writer.as_ref().map(|w| w.nid.clone());
    People::new(v, pref.0.clone(), you)
}

/// Repo page frame: layout with `charter / <repo> / ...` crumbs, plus the header.
pub fn frame(app: &App, pref: &Pref, r: &Repo, tab: &'static str, title: &str) -> (Layout, Head) {
    let ops = r.ops();
    let mut l = Layout::new(app, pref, format!("{title} - {}", r.name()));
    l.crumbs.push((format!("/r/{}", r.hex), r.name()));
    l.events = format!("/r/{}/events", r.hex);
    l.live_src = format!("/r/{}/verify", r.hex);
    let head = Head::new(r, &ops, tab, people(app, pref, r.view.as_ref()));
    if head.verify.state == "equivocation" {
        l.banners.push(Banner {
            kind: "bad",
            icon: "seal-warning",
            lead: format!("{} has conflicting histories.", head.name),
            text: "A key of this repo signed two different versions at the same step. \
                   Don't merge or release until a maintainer explains."
                .into(),
            link: Some((format!("/r/{}#verify", head.hex), "See details".into())),
        });
    }
    if head.archived {
        // Archive is a convention honest clients keep: the helper refuses pushes, the UI writes.
        (l.writes, l.archived) = (false, true);
        l.banners.push(Banner {
            kind: "off",
            icon: "archive",
            lead: "Archived.".into(),
            text: "Its maintainers stopped taking pushes, issues and patches, so Charter \
                   writes nothing to it. This is advisory: other clients can still write."
                .into(),
            link: head
                .maintains()
                .then(|| (format!("/r/{}/manage", head.hex), "Manage".into())),
        });
    }
    (l, head)
}

pub fn html<T: Template>(t: &T) -> Response {
    match t.render() {
        Ok(s) => Html(s).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("template: {e}\n"),
        )
            .into_response(),
    }
}

/// Runs a page on the blocking pool (git subprocesses, file reads).
pub async fn blocking(f: impl FnOnce() -> Response + Send + 'static) -> Response {
    tokio::task::spawn_blocking(f)
        .await
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorPage {
    pub l: Layout,
    pub status: u16,
    pub msg: String,
}

pub fn error_page(app: &App, pref: &Pref, status: StatusCode, msg: &str) -> Response {
    let l = Layout::new(app, pref, status.canonical_reason().unwrap_or("Error"));
    let page = ErrorPage {
        l,
        status: status.as_u16(),
        msg: msg.to_owned(),
    };
    (status, html(&page)).into_response()
}

pub fn not_found_page(app: &App, pref: &Pref, msg: &str) -> Response {
    error_page(app, pref, StatusCode::NOT_FOUND, msg)
}

pub async fn not_found(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    pref: Pref,
) -> Response {
    not_found_page(&app, &pref, "Nothing here.")
}

#[cfg(test)]
mod tests {
    use super::renew_at;

    const DAY: u64 = 43_200; // blocks of 2 s
    const SYNCED: u64 = 1_800_000_000_000; // 15 Jan 2027 08:00 UTC
    const HEAD: u64 = 1_000_000;

    fn at(days_left: u64, now_days: u64) -> Option<(String, &'static str)> {
        renew_at(
            Some(HEAD + days_left * DAY),
            HEAD,
            SYNCED,
            SYNCED + now_days * 86_400_000,
        )
    }

    /// A push writes data for 30 days, so a fresh repo must not be amber: amber starts
    /// where `git fetch` starts warning (7 days), red once the date passed.
    #[test]
    fn renew_by_is_quiet_for_a_fresh_push_amber_from_7_days_and_red_once_passed() {
        assert_eq!(at(60, 0), Some(("16 Mar 2027".into(), "")));
        assert_eq!(at(30, 0).map(|r| r.1), Some(""));
        assert_eq!(at(29, 0), Some(("13 Feb 2027".into(), "")));
        assert_eq!(at(7, 0).map(|r| r.1), Some(""));
        assert_eq!(at(10, 4).map(|r| r.1), Some("warn"));
        assert_eq!(at(10, 9).map(|r| r.1), Some("warn"));
        assert_eq!(at(10, 10), Some(("25 Jan 2027".into(), "bad")));
        assert_eq!(at(10, 11).map(|r| r.1), Some("bad"));
    }

    #[test]
    fn renew_by_is_red_for_an_expiry_already_behind_the_synced_head() {
        let r = renew_at(Some(HEAD - DAY), HEAD, SYNCED, SYNCED);
        assert_eq!(r, Some(("15 Jan 2027".into(), "bad")));
    }

    #[test]
    fn renew_by_is_absent_without_an_expiry() {
        assert_eq!(renew_at(None, HEAD, SYNCED, SYNCED), None);
    }
}
