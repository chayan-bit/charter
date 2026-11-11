//! docs/p6/ui-v2.md slices 0-3 and Settings: plain words, peer names, the equivocation
//! banner, branches, Settings that apply at once and survive a restart, the small-write
//! setting, the wallet ledger, and a wallet too low to send.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use charter_sync::Keys;
use charter_sync::ids::hex32;
use charter_ui::cache::{self, RepoView};
use charter_ui::security::Auth;
use charter_ui::{App, Shared, router, spend, writes};
use common::*;

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

fn view(home: &Path) -> RepoView {
    let dir = home.join("repos").join(&fixture().hex);
    cache::read(&dir.join(cache::VIEW))
        .expect("read")
        .expect("view")
}

/// The text a reader sees: tags, attributes and Technical details removed.
fn visible(h: &str) -> String {
    let mut s = h.to_owned();
    while let Some(a) = s.find("<details class=\"tech\">") {
        let b = a + s[a..].find("</details>").expect("closed") + "</details>".len();
        s.replace_range(a..b, " ");
    }
    for tag in ["<title>", "<script", "<style"] {
        if let Some(a) = s.find(tag) {
            let end = s[a..].find("</").map_or(s.len(), |e| a + e);
            s.replace_range(a..end, " ");
        }
    }
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn no_page_shows_protocol_words_or_bare_block_numbers_outside_technical_details() {
    let f = fixture();
    let pages = [
        "/".to_owned(),
        format!("/r/{}", f.hex),
        format!("/r/{}/branches", f.hex),
        format!("/r/{}/tree/main/src", f.hex),
        format!("/r/{}/log/main", f.hex),
        format!("/r/{}/commit/{}", f.hex, f.main),
        format!("/r/{}/issues", f.hex),
        format!("/r/{}/issues/{}", f.hex, f.issue),
        format!("/r/{}/patches", f.hex),
        format!("/r/{}/patches/{}", f.hex, f.patch),
        format!("/r/{}/releases", f.hex),
        format!("/r/{}/rules", f.hex),
        "/wallet".to_owned(),
        "/settings".to_owned(),
    ];
    for p in &pages {
        let h = page(p).await;
        let v = visible(&h).to_lowercase();
        for word in ["delegate", "anchored", "entity", " rev ", "rev 1"] {
            assert!(!v.contains(word), "{p} shows {word:?}");
        }
        let bare = v
            .match_indices("block ")
            .any(|(i, _)| v[i + 6..].starts_with(|c: char| c.is_ascii_digit()));
        assert!(!bare, "{p} shows a bare block number");
        assert!(!v.contains(" 0 s ago"), "{p}");
    }
    // Times read as times; the block stays in the tooltip.
    let h = page(&format!("/r/{}/issues/{}", f.hex, f.issue)).await;
    assert!(h.contains(" ago</time>") && h.contains(", block "), "{h}");
}

#[tokio::test(flavor = "multi_thread")]
async fn peers_are_named_by_their_published_alias() {
    let f = fixture();
    let h = page(&format!("/r/{}/issues/{}", f.hex, f.issue)).await;
    assert!(h.contains("<b>alice</b> opened this"), "{h}");
    let h = page("/settings").await;
    assert!(h.contains("alice") && h.contains("name=\"nick."), "{h}");
}

#[tokio::test(flavor = "multi_thread")]
async fn branches_page_lists_branches_and_tags_with_their_last_commit() {
    let f = fixture();
    let h = page(&format!("/r/{}/branches", f.hex)).await;
    assert!(
        h.contains("Branches and tags") && h.contains(">main<"),
        "{h}"
    );
    assert!(h.contains(">v1<") && h.contains(">tag<"), "{h}");
    assert!(
        h.contains("fix: &#60;smaller&#62; main"),
        "last commit, escaped"
    );
    // The picker on the code pages reaches every ref without typing a URL.
    let h = page(&format!("/r/{}", f.hex)).await;
    assert!(
        h.contains(&format!("/r/{}/tree/v1", f.hex)) && h.contains("/branches\""),
        "{h}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_equivocation_shows_the_red_banner_on_every_repo_page() {
    let f = fixture();
    let h = home();
    let mut v = view(h.path());
    let nid = v.peers[0].nid.clone();
    v.equivocations.push(cache::EquivocationJson {
        kind: "head".into(),
        nid,
        seq: 3,
        keys: ["aa".repeat(32), "bb".repeat(32)],
    });
    let dir = h.path().join("repos").join(&f.hex);
    cache::write(&dir.join(cache::VIEW), &v).expect("write");
    let app = app_for(h.path());
    for p in [
        format!("/r/{}", f.hex),
        format!("/r/{}/issues", f.hex),
        format!("/r/{}/commit/{}", f.hex, f.main),
        format!("/r/{}/releases", f.hex),
    ] {
        let t = send(app.clone(), req(&p)).await.text();
        assert!(
            t.contains("banner bad") && t.contains("has conflicting histories."),
            "{p}: {t}"
        );
    }
    let t = send(app.clone(), req("/")).await.text();
    assert!(t.contains("has conflicting histories"), "{t}");
}

const DEFAULTS: [(&str, &str); 6] = [
    ("names", "both"),
    ("small", "always"),
    ("fonts", "embedded"),
    ("merged", "infer"),
    ("times", "block"),
    ("theme", "system"),
];

/// Each changed preference, as one issue/patch page renders it.
async fn assert_applied(app: &axum::Router) {
    let f = fixture();
    let t = send(
        app.clone(),
        req(&format!("/r/{}/issues/{}", f.hex, f.issue)),
    )
    .await
    .text();
    assert!(t.contains("<b>Ravi</b> opened this"), "nickname only: {t}");
    assert!(t.contains("their clock: "), "author's clock");
    assert!(
        t.contains("data-theme=\"dark\"") && t.contains("class=\"sysfont\""),
        "theme and fonts"
    );
    let t = send(app.clone(), req(&format!("/r/{}/patches", f.hex)))
        .await
        .text();
    assert!(
        t.contains("<span class=\"chip\">open</span>"),
        "merged only on a note: {t}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_apply_at_once_survive_a_restart_and_refuse_bad_input() {
    let h = home();
    let app = app_for(h.path());
    let t = send(app.clone(), req("/settings")).await.text();
    assert!(t.contains("value=\"both\" checked") && t.contains("value=\"always\" checked"));
    let nid = view(h.path()).peers[0].nid.clone();
    let key = format!("nick.{nid}");
    let kv = [
        ("names", "nickname"),
        (key.as_str(), "Ravi"),
        ("small", "skip"),
        ("skip_under", "0.002"),
        ("fonts", "system"),
        ("merged", "ops"),
        ("times", "both"),
        ("theme", "dark"),
    ];
    let r = send(app.clone(), form("/settings", &kv)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    assert_eq!(r.header(header::LOCATION), "/settings?saved=1");
    assert_applied(&app).await;
    // A restart reads the same file.
    assert_applied(&app_for(h.path())).await;
    let file = h.path().join(charter_ui::prefs::FILE);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    // Bad input is refused and changes nothing.
    let saved = std::fs::read(&file).expect("prefs");
    let mut bad = DEFAULTS.to_vec();
    bad.extend([("small", "skip"), ("skip_under", "5")]);
    bad.retain(|(k, v)| *k != "small" || *v == "skip");
    let r = send(app.clone(), form("/settings", &bad)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST, "{}", r.text());
    assert_eq!(std::fs::read(&file).expect("prefs"), saved);
    // Without the CSRF field nothing is saved either.
    let r = send(
        app.clone(),
        Request::post("/settings")
            .header(header::HOST, HOST)
            .header(header::COOKIE, format!("charter_ui={TOKEN}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(
                "names=alias&small=always&fonts=embedded&merged=infer&times=block&theme=light",
            ))
            .expect("request"),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert_eq!(std::fs::read(&file).expect("prefs"), saved);
    // The header toggle writes the same file.
    let r = send(app.clone(), form("/settings/theme", &[("theme", "light")])).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let t = send(app.clone(), req("/settings")).await.text();
    assert!(t.contains("data-theme=\"light\"") && t.contains("value=\"light\" checked"));
    // A corrupt file falls back to the defaults, with a notice on every page.
    std::fs::write(&file, b"{oops").expect("corrupt");
    let t = send(app_for(h.path()), req("/")).await.text();
    assert!(
        t.contains("Settings reset.") && !t.contains("data-theme=\"light\""),
        "{t}"
    );
}

/// A repo with a second peer (bob) whose UI writes with `keys`; bob's home holds the prefs.
async fn bob_app(
    seed: u8,
    keys_seed: u8,
    prefs: Option<&str>,
) -> (axum::Router, Arc<writes::Writer>, String, tempfile::TempDir) {
    let net = Arc::new(Net::new());
    let alice = Peer::new(&net, seed);
    let rid = init(&net, &alice, "bobs").await;
    let bob = Peer::new(&net, seed + 1);
    let r = charter_ui::sync_loop::round(&*net, &bob.cfg, &rid).await;
    assert!(r.is_ok(), "{r:?}");
    if let Some(p) = prefs {
        std::fs::create_dir_all(bob.home.path().join("ui")).expect("mkdir");
        std::fs::write(bob.home.path().join(charter_ui::prefs::FILE), p).expect("prefs");
    }
    let keys = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([keys_seed; 32]));
    let w = writes::spawn(net.clone(), bob.cfg.clone(), keys);
    let app = App::new(
        bob.home.path().to_path_buf(),
        Arc::new(Shared::default()),
        CSRF.into(),
    );
    let app = router(
        Arc::new(app.with_writer(w.clone())),
        Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())),
    );
    (app, w, hex32(&rid), bob.home)
}

async fn confirmed(w: &writes::Writer, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = w.status(id).expect("status");
        assert_eq!(s.error, None);
        if s.block.is_some() && s.done_ms > 0 {
            return;
        }
        assert!(Instant::now() < deadline, "never confirmed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_write_under_the_small_write_limit_skips_the_dialog_and_is_in_the_ledger() {
    let prefs = r#"{"skip_under": "0.1"}"#;
    let (app, w, hex, home) = bob_app(31, 32, Some(prefs)).await;
    let kv = [("kind", "issue"), ("title", "cheap")];
    let r = send(app.clone(), form(&format!("/r/{hex}/write"), &kv)).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "no dialog: {}", r.text());
    let at = r.header(header::LOCATION);
    let id = at.rsplit('/').next().expect("id").to_owned();
    assert_eq!(at, format!("/r/{hex}/write/{id}"));
    confirmed(&w, &id).await;
    let t = send(app.clone(), req(&at)).await.text();
    assert!(
        t.contains("Sent without asking") && t.contains("0.1 tGLM limit"),
        "{t}"
    );
    assert!(t.contains("Cost 0."), "{t}");
    let e = spend::read(home.path()).expect("ledger");
    assert_eq!(e.len(), 1);
    assert!(e[0].skipped && e[0].repo == hex && e[0].txs >= 1, "{e:?}");
    assert!(e[0].wei.parse::<u128>().expect("wei") > 0);
    let t = send(app.clone(), req("/wallet")).await.text();
    assert!(
        t.contains("no dialog") && t.contains("over 1 write since"),
        "{t}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_dialog_shows_wallet_now_and_after_and_a_confirmed_write_is_in_the_ledger() {
    let (app, w, hex, home) = bob_app(41, 42, None).await;
    let kv = [("kind", "issue"), ("title", "asked")];
    let r = send(app.clone(), form(&format!("/r/{hex}/write"), &kv)).await;
    let html = r.text();
    assert_eq!(r.status, StatusCode::OK, "the default always asks: {html}");
    for want in [
        "What you publish",
        "asked",
        "Most you can pay",
        "Wallet now",
        "After, at least",
        "Kept for",
        "Public and permanent",
    ] {
        assert!(html.contains(want), "{want}: {html}");
    }
    assert!(!html.contains("type=\"submit\" disabled"), "{html}");
    let at = html.find("/write/").expect("form") + 7;
    let id = html[at..at + 64].to_owned();
    let r = send(app.clone(), form(&format!("/r/{hex}/write/{id}"), &[])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    confirmed(&w, &id).await;
    let t = send(app.clone(), req(&format!("/r/{hex}/write/{id}")))
        .await
        .text();
    assert!(
        t.contains(">Published<") && t.contains("wallet history"),
        "{t}"
    );
    let shown = &visible(&t);
    assert!(
        !shown.contains("0x"),
        "no tx hash on the success path: {shown}"
    );
    let e = spend::read(home.path()).expect("ledger");
    assert!(e.len() == 1 && !e[0].skipped, "{e:?}");
    let t = send(app.clone(), req("/wallet")).await.text();
    assert!(
        t.contains("Spent from this computer") && t.contains(">bobs<"),
        "{t}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wallet_too_low_to_pay_disables_send_and_warns() {
    // Keys of a seed nobody funded: the wallet and the control address hold nothing.
    let (app, _w, hex, _home) = bob_app(51, 99, None).await;
    let kv = [("kind", "issue"), ("title", "unfunded")];
    let r = send(app.clone(), form(&format!("/r/{hex}/write"), &kv)).await;
    let html = r.text();
    assert_eq!(r.status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Not enough funds") && html.contains("type=\"submit\" disabled"),
        "{html}"
    );
    let t = send(app.clone(), req("/wallet")).await.text();
    assert!(t.contains("Running low"), "{t}");
    assert!(t.contains("wallet low"), "the header chip warns: {t}");
}
