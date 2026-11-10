//! docs/p6/ui.md 4.4: writes through the UI endpoints on the in-memory fake. Each write is
//! priced with nothing signed, sent once on confirm, and lands as the op the CLI writes.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use charter_core::VerifiedOp;
use charter_sync::Keys;
use charter_sync::cob::{self, json as cbor};
use charter_sync::ids::{hex32, unhex32};
use charter_ui::security::Auth;
use charter_ui::{App, Shared, router, writes};
use common::*;
use serde_json::Value;

fn writing_app() -> axum::Router {
    writing_app_with(Arc::new(Shared::default()))
}

fn writing_app_with(shared: Arc<Shared>) -> axum::Router {
    let f = fixture();
    // Alice, as the fixture made her (`Peer::new(&net, 1)`).
    let keys = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([1; 32]));
    let w = writes::spawn(f.net.clone(), f.alice.cfg.clone(), keys);
    let app = App::new(f.alice.home.path().to_path_buf(), shared, CSRF.into());
    router(
        Arc::new(app.with_writer(w)),
        Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())),
    )
}

fn form(path: &str, kv: &[(&str, &str)]) -> Request<Body> {
    let mut pairs: Vec<(&str, &str)> = kv.to_vec();
    pairs.push(("csrf", CSRF));
    let body = serde_urlencoded::to_string(&pairs).expect("form");
    Request::post(path)
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("charter_ui={TOKEN}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from(body))
        .expect("request")
}

async fn ops() -> Vec<VerifiedOp> {
    let f = fixture();
    cob::list(&*f.net, &f.rid, None, None).await.expect("list")
}

/// The draft id in a confirm page.
fn draft_id(html: &str) -> String {
    let at = html.find("/write/").expect("confirm form") + 7;
    html[at..at + 64].to_owned()
}

/// Prices `kv`, checks nothing was signed, confirms, waits for the receipt; returns the
/// ops the write added.
async fn write(app: &axum::Router, kv: &[(&str, &str)]) -> Vec<VerifiedOp> {
    let hex = &fixture().hex;
    let before = ops().await.len();
    let r = send(app.clone(), form(&format!("/r/{hex}/write"), kv)).await;
    let html = r.text();
    assert_eq!(r.status, StatusCode::OK, "{html}");
    assert!(
        html.contains("Confirm write") && html.contains("tGLM</button>"),
        "{html}"
    );
    assert_eq!(ops().await.len(), before, "pricing signs nothing");
    let id = draft_id(&html);
    let r = send(app.clone(), form(&format!("/r/{hex}/write/{id}"), &[])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    let status = format!("/r/{hex}/write/{id}");
    assert_eq!(r.header(header::LOCATION), status);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let part = send(app.clone(), req(&format!("{status}?part=1")))
            .await
            .text();
        assert!(!part.contains("Write failed"), "{part}");
        if part.contains("Confirmed in block") {
            assert!(!part.contains("hx-trigger"), "polling stops once confirmed");
            break;
        }
        assert!(part.contains("hx-trigger=\"every 1s\""), "{part}");
        assert!(Instant::now() < deadline, "never confirmed: {part}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let page = send(app.clone(), req(&status)).await.text();
    assert!(
        page.contains("Confirmed in block") && page.contains("<code>0x"),
        "{page}"
    );
    let again = send(app.clone(), form(&status, &[])).await;
    assert_eq!(
        again.status,
        StatusCode::NOT_FOUND,
        "a confirmation sends once"
    );
    ops().await.split_off(before)
}

fn shape(o: &VerifiedOp) -> (String, String, Value) {
    let p = cbor::to_json(&o.body.payload).expect("payload");
    (o.body.type_name.clone(), o.body.action.clone(), p)
}

#[tokio::test(flavor = "multi_thread")]
async fn every_write_is_priced_unsigned_then_sent_once_as_the_cli_would() {
    let f = fixture();
    let app = writing_app();
    let added = write(
        &app,
        &[
            ("kind", "issue"),
            ("title", "From the UI"),
            ("body", "it *works*"),
        ],
    )
    .await;
    let (t, a, p) = shape(&added[0]);
    assert_eq!(
        (t.as_str(), a.as_str(), p["title"].as_str()),
        ("xyz.arkiv.issue", "create", Some("From the UI"))
    );
    let issue = hex32(&added[0].id);

    let added = write(
        &app,
        &[("kind", "comment"), ("issue", &issue), ("body", "me too")],
    )
    .await;
    assert_eq!(hex32(&added[0].object()), issue);
    assert_eq!(added[0].body.parents, vec![added[0].object()]);

    let head = f.main.as_str();
    let kv = [
        ("kind", "review"),
        ("patch", &f.patch),
        ("verdict", "approve"),
        ("head", head),
    ];
    let added = write(&app, &kv).await;
    let (create, approve) = (&added[0], &added[1]);
    assert_eq!(shape(create).0, "xyz.arkiv.review");
    assert_eq!(
        (approve.body.action.as_str(), approve.object()),
        ("approve", create.id)
    );
    assert_eq!(shape(approve).2["head"], head);

    let added = write(
        &app,
        &[("kind", "merge"), ("patch", &f.patch), ("closes", &f.issue)],
    )
    .await;
    let (_, a, p) = shape(&added[0]);
    assert_eq!(
        (a.as_str(), p["closes"].as_str()),
        ("merged", Some(f.issue.as_str()))
    );
    assert_eq!(added[0].object(), unhex32(&f.patch).expect("id"));

    let kv = [
        ("kind", "claim"),
        ("issue", &issue),
        ("title", "Fix it"),
        ("head", head),
    ];
    let (t, a, p) = shape(&write(&app, &kv).await[0]);
    assert_eq!(
        (t.as_str(), a.as_str(), p["closes"].as_str()),
        ("xyz.arkiv.patch", "create", Some(issue.as_str()))
    );

    let dir = tempfile::tempdir().expect("dir");
    let art = dir.path().join("demo.tgz");
    std::fs::write(&art, b"bytes").expect("write");
    let spec = format!("demo.tgz={}", art.display());
    let loc = format!("demo.tgz=file://{}", art.display());
    let kv = [
        ("kind", "release"),
        ("tag", "v2"),
        ("commit", head),
        ("artifacts", &spec),
        ("locations", &loc),
    ];
    let (_, _, p) = shape(&write(&app, &kv).await[0]);
    assert_eq!(p["artifacts"][0]["size"], 5);
    assert_eq!(
        p["artifacts"][0]["locations"][0],
        loc.trim_start_matches("demo.tgz=")
    );

    // Funding needs an appointed escrow; before one, the write is refused unpriced.
    let fund = [("kind", "fund"), ("issue", &issue), ("amount", "0.25")];
    let r = send(app.clone(), form(&format!("/r/{}/write", f.hex), &fund)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert!(r.text().contains("no escrow"), "{}", r.text());
    let esc = tempfile::tempdir().expect("escrow home");
    let ek = charter_escrow::EscrowKeys::init(esc.path()).expect("escrow keys");
    let keys = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([1; 32]));
    let nid = ek.id.nid();
    charter_escrow::ops::appoint(&*f.net, &f.alice.cfg, &keys, &f.rid, &nid, ek.address())
        .await
        .expect("appoint");
    let added = write(&app, &fund).await;
    let wei = 250_000_000_000_000_000_u64;
    let (t, a, p) = shape(&added[1]);
    assert_eq!(added[1].object(), added[0].id);
    assert_eq!(
        (t.as_str(), a.as_str(), &p["amount"]),
        ("xyz.arkiv.bounty", "pledge", &serde_json::json!(wei))
    );
    let ledger = charter_escrow::ledger::load(&*f.net, &f.rid)
        .await
        .expect("ledger");
    assert_eq!(
        ledger.credited(),
        charter_store::U256::from(wei),
        "the deposit is credited"
    );
    assert_eq!(ledger.deposits[0].purpose, format!("bounty:{issue}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_are_off_without_keys_and_a_get_writes_nothing() {
    let f = fixture();
    let hex = &f.hex;
    let r = send(
        app(),
        form(
            &format!("/r/{hex}/write"),
            &[("kind", "issue"), ("title", "x")],
        ),
    )
    .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.text().contains("Writes are off"));
    let page = page(&format!("/r/{hex}/issues/{}", f.issue)).await;
    assert!(
        page.contains("Writes are off") && !page.contains("method=\"post\""),
        "{page}"
    );
    let r = get(&format!("/r/{hex}/write")).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER);
    let r = send(
        writing_app(),
        req(&format!("/r/{hex}/write/{}", "a".repeat(64))),
    )
    .await;
    assert_eq!(
        r.status,
        StatusCode::NOT_FOUND,
        "an unconfirmed id is no write"
    );
}

/// Prices and sends `kv` as `w`'s keys; returns the confirm page and the txs sent.
async fn priced_and_sent(
    app: &axum::Router,
    w: &writes::Writer,
    hex: &str,
    kv: &[(&str, &str)],
) -> (String, usize) {
    let r = send(app.clone(), form(&format!("/r/{hex}/write"), kv)).await;
    let html = r.text();
    assert_eq!(r.status, StatusCode::OK, "{html}");
    let id = draft_id(&html);
    let r = send(app.clone(), form(&format!("/r/{hex}/write/{id}"), &[])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let s = w.status(&id).expect("status");
        assert_eq!(s.error, None);
        if s.block.is_some() {
            return (html, s.txs.len());
        }
        assert!(Instant::now() < deadline, "never confirmed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_confirm_view_counts_the_control_top_up_the_send_makes() {
    let net = Arc::new(Net::new());
    let alice = Peer::new(&net, 21);
    let rid = init(&net, &alice, "topup").await;
    // Bob has never written to the repo: his control address holds nothing.
    let bob = Peer::new(&net, 22);
    let r = charter_ui::sync_loop::round(&*net, &bob.cfg, &rid).await;
    assert!(r.is_ok(), "{r:?}");
    let keys = Keys::from_seed(charter_crypto::MasterSeed::from_bytes([22; 32]));
    let control = keys.control_address(&rid);
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
    let hex = hex32(&rid);
    let kv = [("kind", "issue"), ("title", "needs a top-up")];
    let (html, sent) = priced_and_sent(&app, &w, &hex, &kv).await;
    assert!(html.contains("<dt>Transactions</dt><dd>2</dd>"), "{html}");
    assert!(
        html.contains("<dt>Wallet top-up</dt><dd class=\"mono\">"),
        "{html}"
    );
    assert!(
        html.contains("to the control address, fee at most"),
        "{html}"
    );
    assert_eq!(sent, 2, "the top-up, then the op");
    net.fund(control);
    let kv = [("kind", "issue"), ("title", "covered")];
    let (html, sent) = priced_and_sent(&app, &w, &hex, &kv).await;
    assert!(html.contains("<dt>Transactions</dt><dd>1</dd>"), "{html}");
    assert!(
        html.contains("<dt>Wallet top-up</dt><dd>none needed"),
        "{html}"
    );
    assert_eq!(sent, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_exhausted_rpc_quota_disables_confirm_and_sends_nothing() {
    let hex = &fixture().hex;
    let shared = Arc::new(Shared::default());
    shared.pause(Some(Instant::now() + Duration::from_secs(170)));
    let app = writing_app_with(shared.clone());
    let before = ops().await.len();
    let kv = [("kind", "issue"), ("title", "while limited")];
    let r = send(app.clone(), form(&format!("/r/{hex}/write"), &kv)).await;
    let html = r.text();
    assert_eq!(r.status, StatusCode::OK, "{html}");
    assert!(html.contains("RPC quota: retry after 3 min"), "{html}");
    assert!(html.contains("type=\"submit\" disabled"), "{html}");
    let id = draft_id(&html);
    let r = send(app.clone(), form(&format!("/r/{hex}/write/{id}"), &[])).await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE, "{}", r.text());
    assert!(r.text().contains("RPC quota: retry after 3 min"));
    assert_eq!(ops().await.len(), before, "nothing was sent");
}

#[tokio::test(flavor = "multi_thread")]
async fn write_forms_post_back_with_the_csrf_field() {
    let f = fixture();
    let app = writing_app();
    for p in [
        format!("/r/{}/issues/{}", f.hex, f.issue),
        format!("/r/{}/patches/{}", f.hex, f.patch),
        format!("/r/{}/issues/new", f.hex),
        format!("/r/{}/releases/new", f.hex),
    ] {
        let html = send(app.clone(), req(&p)).await.text();
        let forms = html.matches("method=\"post\"").count();
        assert!(forms > 0, "{p}");
        assert_eq!(
            html.matches(&format!("name=\"csrf\" value=\"{CSRF}\""))
                .count(),
            forms,
            "{p}"
        );
    }
}
