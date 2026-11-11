//! Page handlers. Each loads what it needs from the repo cache on a blocking thread (git
//! runs as subprocesses), renders one askama template and never touches Arkiv.

pub mod cobs;
pub mod code;
pub mod forms;
pub mod history;
pub mod live;
pub mod people;
pub mod program;
pub mod repos;
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
            repos: Mutex::default(),
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

/// The header's sync indicator: `ok`, `warn`, `bad` or `idle`, and what it says.
#[derive(Debug, Clone)]
pub struct SyncChip {
    pub state: &'static str,
    pub text: String,
}

impl SyncChip {
    fn of(shared: &Shared) -> Self {
        let (ok, failing) = shared.health();
        let (state, text) = match (shared.paused(), failing, ok) {
            (Some(t), _, _) if t > std::time::Instant::now() => (
                "warn",
                format!("Paused: RPC quota, resumes in {} min", mins_until(t)),
            ),
            (_, n, _) if n > 0 => ("bad", "Last sync failed".into()),
            (_, _, Some(t)) => ("ok", format!("Synced {}", ago(t))),
            _ => ("idle", "Starting sync".into()),
        };
        Self { state, text }
    }
}

/// The header's wallet chip, from the balance the writer last read.
#[derive(Debug, Clone)]
pub struct WalletChip {
    /// `12.4780`, or empty before the first read.
    pub amount: String,
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
    /// Write forms are shown (keys are loaded).
    pub writes: bool,
    /// Which main destination this page is under: `repos`, `wallet` or `settings`.
    pub nav: &'static str,
    pub sync: SyncChip,
    pub wallet: Option<WalletChip>,
    pub banners: Vec<Banner>,
    /// `(href, label)`; an empty href is the current page.
    pub crumbs: Vec<(String, String)>,
    /// The repo's event stream (`live.js`); empty off repo pages.
    pub events: String,
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
            nav: "repos",
            sync: SyncChip::of(&app.shared),
            wallet: app.writer.as_deref().map(wallet::chip),
            banners: notice.into_iter().collect(),
            crumbs: vec![],
            events: String::new(),
        }
    }

    /// Why write forms are missing.
    pub fn off(&self) -> &'static str {
        write::WRITES_OFF
    }
}

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
    /// `z...` address, as `git clone arkiv::` takes it.
    pub addr: String,
    pub clock: Clock,
    pub people: People,
    /// The newest signed head: `(block, peer)`, the last push.
    pub pushed: Option<(u64, String)>,
}

/// The newest signed head of a view: `(block, peer)`.
pub fn last_push(v: Option<&RepoView>) -> Option<(u64, String)> {
    v.into_iter()
        .flat_map(|v| &v.peers)
        .filter_map(|p| p.head.as_ref().map(|h| (h.block, p.nid.clone())))
        .max()
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
            addr: to_text(&r.rid),
            clock: Clock::of(v),
            pushed: last_push(v),
            people,
        }
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
    pub fn glm(&self, wei: &str) -> String {
        glm(wei)
    }
    /// Maintainer names, joined: `you, ravi`.
    pub fn maintainers(&self) -> String {
        let names: Vec<String> = self.verify.delegates.iter().map(|d| self.who(d)).collect();
        names.join(", ")
    }
}

/// A decimal wei amount from an op or program state as tGLM; anything else as it is.
pub fn glm(wei: &str) -> String {
    charter_store::U256::from_str_radix(wei.trim_matches('"'), 10)
        .map_or_else(|_| wei.to_owned(), write::tglm)
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
