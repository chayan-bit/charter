//! Page handlers. Each loads what it needs from the repo cache on a blocking thread (git
//! runs as subprocesses), renders one askama template and never touches Arkiv.

pub mod cobs;
pub mod code;
pub mod forms;
pub mod history;
pub mod live;
pub mod program;
pub mod repos;
mod verify;
pub mod write;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use askama::Template;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use charter_core::{Rid, VerifiedOp};
use charter_sync::ids::{hex32, parse_id};

use crate::cache::{self, Cobs, RepoView};
use crate::gitread::{self, Git};
use crate::sync_loop::{Round, Shared};
use crate::{assets, fold};

pub use verify::Verify;

/// Shared by every handler.
pub struct App {
    pub home: PathBuf,
    pub shared: Arc<Shared>,
    pub csrf: String,
    /// Signs and sends writes; `None` when `$CHARTER_HOME` has no keys.
    pub writer: Option<Arc<crate::writes::Writer>>,
    /// One gix handle per repo, keyed by the `synced_ms` of the view it was opened under.
    repos: Mutex<HashMap<Rid, (u64, gix::ThreadSafeRepository)>>,
}

impl App {
    pub fn new(home: PathBuf, shared: Arc<Shared>, csrf: String) -> Self {
        Self {
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

    /// The repo's handle, opened once and reopened after each sync round so new refs and
    /// packs show.
    fn git(&self, rid: Rid, dir: &Path, synced: u64) -> Option<Git> {
        let mut m = self.repos.lock().ok()?;
        if let Some((_, ts)) = m.get(&rid).filter(|(s, _)| *s == synced) {
            return Some(gitread::local(ts));
        }
        let ts = gitread::open(dir)?;
        let g = gitread::local(&ts);
        m.insert(rid, (synced, ts));
        Some(g)
    }
}

/// `theme` cookie (`light` or `dark`), so the server renders the attribute without a flash.
#[derive(Debug, Clone, Default)]
pub struct Theme(pub &'static str);

impl<S: Send + Sync> FromRequestParts<S> for Theme {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(p: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        let cookies = p.headers.get_all(header::COOKIE);
        let v = cookies
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
            .find_map(|kv| kv.trim().strip_prefix("theme="));
        Ok(Theme(match v {
            Some("light") => "light",
            Some("dark") => "dark",
            _ => "",
        }))
    }
}

/// Fields of the page frame (`base.html`).
#[derive(Debug, Clone)]
pub struct Layout {
    pub title: String,
    pub v: &'static str,
    pub csrf: String,
    pub theme: &'static str,
    /// Write forms are shown (keys are loaded).
    pub writes: bool,
    /// `(href, label)`; an empty href is the current page.
    pub crumbs: Vec<(String, String)>,
    /// The repo's event stream (`live.js`); empty off repo pages.
    pub events: String,
}

impl Layout {
    pub fn new(app: &App, theme: &Theme, title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            v: assets::version(),
            csrf: app.csrf.clone(),
            theme: theme.0,
            writes: app.writer.is_some(),
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
        0..60 => format!("{s} s ago"),
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
    pub view: Option<RepoView>,
    pub round: Round,
}

impl Repo {
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
pub fn open(app: &App, theme: &Theme, rid: &str) -> Result<Repo, Response> {
    let gone = || not_found_page(app, theme, "No such repo in this $CHARTER_HOME.");
    let rid = parse_id(rid).map_err(|_| gone())?;
    let hex = hex32(&rid);
    let dir = app.home.join("repos").join(&hex);
    if !dir.join("git").is_dir() {
        return Err(gone());
    }
    app.shared.focus(rid);
    let view: Option<RepoView> = cache::read(&dir.join(cache::VIEW)).ok().flatten();
    let synced = view.as_ref().map_or(0, |v| v.synced_ms);
    let Some(git) = app.git(rid, &dir, synced) else {
        let msg = "The git cache of this repo cannot be opened.";
        return Err(error_page(
            app,
            theme,
            StatusCode::INTERNAL_SERVER_ERROR,
            msg,
        ));
    };
    Ok(Repo {
        rid,
        view,
        git,
        round: app.shared.round(&rid),
        hex,
        dir,
    })
}

/// Repo header, tabs and verification panel (`repo.html`).
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
}

impl Head {
    pub fn new(r: &Repo, ops: &[VerifiedOp], tab: &'static str) -> Self {
        let count = |t: &str| {
            ops.iter()
                .filter(|o| o.body.type_name == t && o.body.is_create())
                .count()
        };
        Self {
            hex: r.hex.clone(),
            name: r.name(),
            description: r
                .view
                .as_ref()
                .map(|v| v.description.clone())
                .unwrap_or_default(),
            default_ref: seg(&r.default_ref()),
            tab,
            issues: count(fold::ISSUE),
            patches: count(fold::PATCH),
            verify: Verify::new(r.view.as_ref(), &r.round),
        }
    }
}

/// Repo page frame: layout with `charter / <repo> / ...` crumbs, plus the header.
pub fn frame(app: &App, theme: &Theme, r: &Repo, tab: &'static str, title: &str) -> (Layout, Head) {
    let ops = r.ops();
    let mut l = Layout::new(app, theme, format!("{title} - {}", r.name()));
    l.crumbs.push((format!("/r/{}", r.hex), r.name()));
    l.events = format!("/r/{}/events", r.hex);
    (l, Head::new(r, &ops, tab))
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

pub fn error_page(app: &App, theme: &Theme, status: StatusCode, msg: &str) -> Response {
    let l = Layout::new(app, theme, status.canonical_reason().unwrap_or("Error"));
    let page = ErrorPage {
        l,
        status: status.as_u16(),
        msg: msg.to_owned(),
    };
    (status, html(&page)).into_response()
}

pub fn not_found_page(app: &App, theme: &Theme, msg: &str) -> Response {
    error_page(app, theme, StatusCode::NOT_FOUND, msg)
}

pub async fn not_found(
    axum::extract::State(app): axum::extract::State<Arc<App>>,
    theme: Theme,
) -> Response {
    not_found_page(&app, &theme, "Nothing here.")
}
