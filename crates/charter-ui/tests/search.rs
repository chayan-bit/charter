//! docs/p6/ui-v2.md 7.1: search. Local results come from the cache with no chain query; the
//! "Search scope" setting persists; with "Local + Arkiv" one submitted search makes exactly
//! the planned queries (1 for candidates, 3 per one-maintainer candidate), typing and a
//! repeat make none, and hostile text from the chain is escaped.

mod common;

use std::path::Path;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use charter_store::fake::Meter;
use charter_sync::identity::{self, RepoMeta};
use charter_ui::prefs::SearchScope;
use charter_ui::security::Auth;
use charter_ui::{App, Shared, router, search};
use common::*;

type Counted = Meter<'static, Net>;

const EVIL: &str = "<img src=x onerror=alert(1)>";

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read_dir") {
        let e = e.expect("entry");
        let dst = to.join(e.file_name());
        if e.file_type().expect("type").is_dir() {
            copy_dir(&e.path(), &dst);
        } else {
            std::fs::copy(e.path(), dst).expect("copy");
        }
    }
}

/// A private copy of the fixture's home, so Settings written here stay here.
fn home() -> tempfile::TempDir {
    let d = tempfile::tempdir().expect("tmp");
    copy_dir(fixture().alice.home.path(), d.path());
    d
}

/// Two repos on the fake chain that are not in the fixture home: `demo-remote` and
/// `demo-evil`, whose description is hostile. Made once per test binary.
async fn remotes() {
    static DONE: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    DONE.get_or_init(|| async {
        let net = &*fixture().net;
        for (seed, name, description) in [
            (2, "demo-remote", "A repo elsewhere"),
            (3, "demo-evil", EVIL),
        ] {
            let p = Peer::new(net, seed);
            let meta = RepoMeta {
                name: name.into(),
                description: description.into(),
                default_branch: "main".into(),
                lang: Some("rust".into()),
                license: Some("MIT".into()),
            };
            identity::init(net, &p.cfg, &p.keys, &meta)
                .await
                .expect("init");
        }
    })
    .await;
}

struct Rig {
    app: axum::Router,
    meter: Arc<Counted>,
    shared: Arc<Shared>,
}

fn rig(home: &Path) -> Rig {
    let net: &'static Net = &fixture().net;
    let meter = Arc::new(Meter::new(net));
    let shared = Arc::new(Shared::default());
    let app = App::new(home.to_path_buf(), shared.clone(), CSRF.into())
        .with_arkiv(search::arkiv(meter.clone()));
    let app = router(
        Arc::new(app),
        Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())),
    );
    Rig { app, meter, shared }
}

fn form(path: &str, kv: &[(&str, &str)]) -> Request<Body> {
    let mut pairs: Vec<(&str, &str)> = kv.to_vec();
    pairs.push(("csrf", CSRF));
    Request::post(path)
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("charter_ui={TOKEN}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(
            serde_urlencoded::to_string(&pairs).expect("form"),
        ))
        .expect("request")
}

/// The htmx request the loading section makes.
fn fragment(path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("charter_ui={TOKEN}"))
        .header("hx-request", "true")
        .body(Body::empty())
        .expect("request")
}

const SETTINGS: [(&str, &str); 6] = [
    ("names", "both"),
    ("small", "always"),
    ("fonts", "embedded"),
    ("merged", "infer"),
    ("times", "block"),
    ("theme", "system"),
];

async fn choose(r: &Rig, scope: &str) {
    let mut kv = SETTINGS.to_vec();
    kv.push(("search", scope));
    let res = send(r.app.clone(), form("/settings", &kv)).await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
}

async fn text(r: &Rig, path: &str) -> String {
    let res = send(r.app.clone(), req(path)).await;
    assert_eq!(res.status, StatusCode::OK, "{path}: {}", res.text());
    res.text()
}

#[tokio::test(flavor = "multi_thread")]
async fn local_search_finds_every_kind_from_the_cache_with_zero_queries() {
    let h = home();
    let r = rig(h.path());
    let hex = &fixture().hex;
    let f = fixture();
    for (q, needle) in [
        ("demo", format!("href=\"/r/{hex}\"")),
        ("crash", format!("/r/{hex}/issues/{}", f.issue)),
        ("fix the crash", format!("/r/{hex}/patches/{}", f.patch)),
        ("smaller", format!("/r/{hex}/commit/{}", f.main)),
        ("long.rs", format!("/r/{hex}/blob/main/long.rs")),
        ("DEEP/X", format!("/r/{hex}/blob/main/src/deep/x.txt")),
    ] {
        let t = text(&r, &format!("/search?q={}", q.replace(' ', "+"))).await;
        assert!(t.contains(&needle), "{q}: {t}");
    }
    // Hostile titles are escaped, never markup.
    let t = text(&r, "/search?q=crash").await;
    assert!(
        t.contains("Crash on &#60;b&#62;start&#60;/b&#62;") && !t.contains("<b>start"),
        "{t}"
    );
    let t = text(&r, "/search?q=smaller").await;
    assert!(
        t.contains("fix: &#60;smaller&#62; main") && !t.contains("<smaller>"),
        "{t}"
    );
    // A query with no match, an empty one and Arkiv-only parameters under "Local only".
    assert!(
        text(&r, "/search?q=zzzznothing")
            .await
            .contains("Nothing on this computer matches")
    );
    assert!(text(&r, "/search").await.contains("Search 1 repository"));
    let res = send(r.app.clone(), fragment("/search?q=demo&arkiv=1")).await;
    assert!(!res.text().contains("id=\"arkiv\""), "{}", res.text());
    let t = text(&r, "/search?q=demo&arkiv=1").await;
    assert!(
        !t.contains("On Arkiv</h2>") && t.contains("Local only"),
        "{t}"
    );
    assert_eq!(
        r.meter.metrics().calls,
        0,
        "Local only makes no chain call at all"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_search_scope_defaults_to_local_and_survives_a_restart() {
    let h = home();
    let r = rig(h.path());
    let t = text(&r, "/settings").await;
    assert!(t.contains("name=\"search\" value=\"local\" checked"), "{t}");
    choose(&r, "arkiv").await;
    let t = text(&r, "/settings").await;
    assert!(t.contains("name=\"search\" value=\"arkiv\" checked"), "{t}");
    let again = rig(h.path());
    let t = text(&again, "/settings").await;
    assert!(t.contains("name=\"search\" value=\"arkiv\" checked"), "{t}");
    let file = std::fs::read_to_string(h.path().join(charter_ui::prefs::FILE)).expect("prefs");
    assert!(file.contains("arkiv"), "{file}");
    assert_eq!(SearchScope::default(), SearchScope::Local);
    // An unknown scope is refused and changes nothing.
    let mut kv = SETTINGS.to_vec();
    kv.push(("search", "everywhere"));
    let res = send(r.app.clone(), form("/settings", &kv)).await;
    assert_eq!(res.status, StatusCode::BAD_REQUEST);
    assert!(
        text(&r, "/settings")
            .await
            .contains("name=\"search\" value=\"arkiv\" checked")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn arkiv_runs_once_per_submitted_search_and_never_while_typing() {
    remotes().await;
    let h = home();
    let r = rig(h.path());
    choose(&r, "arkiv").await;
    // Typing: pages for a growing query render local results and a loading section, and
    // ask the chain nothing.
    for q in ["d", "de", "dem", "demo"] {
        let t = text(&r, &format!("/search?q={q}")).await;
        assert!(
            t.contains("aria-busy=\"true\"") && t.contains("hx-get=\"/search?q="),
            "{t}"
        );
    }
    assert!(text(&r, "/search").await.contains("Local + Arkiv"));
    assert_eq!(
        r.meter.metrics().queries,
        0,
        "no query before a search is submitted"
    );
    // Submitted: 1 candidate query, then 3 per candidate (demo, demo-remote, demo-evil).
    let res = send(r.app.clone(), fragment("/search?q=demo&arkiv=1")).await;
    assert_eq!(res.status, StatusCode::OK);
    let t = res.text();
    assert!(
        t.trim_start().starts_with("<section id=\"arkiv\""),
        "a fragment, not a page: {t}"
    );
    assert!(t.contains("demo-remote") && t.contains("demo-evil"), "{t}");
    assert!(t.contains("On Arkiv, not on this computer"), "{t}");
    assert!(
        t.contains("1 already on this computer"),
        "the cached demo is not repeated: {t}"
    );
    assert_eq!(
        r.meter.metrics().queries,
        1 + 3 * 3,
        "planned queries of one submit"
    );
    // The same search again (reload, Back) is answered from memory.
    let before = r.meter.metrics().queries;
    let again = send(r.app.clone(), fragment("/search?q=demo&arkiv=1")).await;
    assert!(again.text().contains("demo-remote"));
    assert_eq!(r.meter.metrics().queries, before);
    // Switching back to Local only stops asking, even for the same URL.
    choose(&r, "local").await;
    let res = send(r.app.clone(), fragment("/search?q=remote&arkiv=1")).await;
    assert!(!res.text().contains("id=\"arkiv\""));
    assert_eq!(r.meter.metrics().queries, before);
}

#[tokio::test(flavor = "multi_thread")]
async fn chain_text_is_escaped_and_a_word_arkiv_cannot_match_asks_nothing() {
    remotes().await;
    let h = home();
    let r = rig(h.path());
    choose(&r, "arkiv").await;
    let t = send(r.app.clone(), fragment("/search?q=demo-evil&arkiv=1"))
        .await
        .text();
    assert!(t.contains("demo-evil"), "{t}");
    assert!(t.contains("&#60;img src=x onerror=alert(1)&#62;"), "{t}");
    assert!(!t.contains("<img"), "{t}");
    let n = r.meter.metrics().queries;
    let t = send(r.app.clone(), fragment("/search?q=%3Cscript%3E&arkiv=1"))
        .await
        .text();
    assert!(
        t.contains("Nothing was asked") && !t.contains("<script>"),
        "{t}"
    );
    assert_eq!(r.meter.metrics().queries, n);
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_starts_the_first_sync_of_the_repo_and_needs_the_csrf_token() {
    let h = home();
    let r = rig(h.path());
    let rid = "ab".repeat(32);
    let res = send(
        r.app.clone(),
        form("/search/fetch", &[("rid", &rid), ("q", "demo")]),
    )
    .await;
    assert_eq!(res.status, StatusCode::SEE_OTHER, "{}", res.text());
    assert_eq!(
        res.header(header::LOCATION),
        format!("/search?q=demo&fetching={rid}")
    );
    assert!(
        r.shared
            .wanted(&charter_sync::ids::unhex32(&rid).expect("rid"))
    );
    let bad = send(r.app.clone(), form("/search/fetch", &[("rid", "nope")])).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let t = text(&r, &format!("/search?q=demo&fetching={rid}")).await;
    assert!(t.contains("Fetching abababa"), "{t}");
    let no_csrf = Request::post("/search/fetch")
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("charter_ui={TOKEN}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(format!("rid={rid}")))
        .expect("request");
    assert_eq!(
        send(r.app.clone(), no_csrf).await.status,
        StatusCode::FORBIDDEN
    );
}
