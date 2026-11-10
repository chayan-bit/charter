//! A fixture repo pushed for real into a `MemArkiv` (charter-sync's own test fixtures),
//! synced once by the UI's sync round, and a router to request pages from.
#![allow(dead_code)]

#[path = "../../../charter-sync/tests/common/mod.rs"]
mod sync;

use std::sync::{Arc, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use charter_core::{CobOpBody, Hash32, IdBody, ProgramPin, Rid, ZERO};
use charter_sync::cob::{self, json};
use charter_sync::ids::hex32;
use charter_sync::{identity, state::State};
use charter_ui::security::Auth;
use charter_ui::{App, Shared, router, sync_loop};
use serde_json::{Value, json};
use tower::ServiceExt;

pub use sync::{Net, Peer, commit, git, init, push, work_repo};

pub const PORT: u16 = 4321;
pub const TOKEN: &str = "t0ken";
pub const CSRF: &str = "csrf";
pub const HOST: &str = "127.0.0.1:4321";

pub struct Fixture {
    pub net: Net,
    pub alice: Peer,
    pub rid: Rid,
    pub hex: String,
    pub main: String,
    pub first: String,
    pub issue: String,
    pub patch: String,
    pub work: tempfile::TempDir,
}

pub const BOUNTY: &str = include_str!("../../../../programs/bounty.rhai");

pub async fn post(
    net: &Net,
    p: &Peer,
    rid: &Rid,
    t: &str,
    action: &str,
    obj: Hash32,
    v: Value,
) -> Hash32 {
    let body = CobOpBody {
        rid: *rid,
        type_name: t.into(),
        object: obj,
        parents: if obj == ZERO { vec![] } else { vec![obj] },
        action: action.into(),
        payload: json::to_cbor(&v).expect("payload"),
        ts_ms: 1_700_000_000_000,
    };
    cob::post(net, &p.cfg, &p.keys, &body).await.expect("post")
}

fn files(work: &std::path::Path) -> (String, String) {
    let long: String = (0..2_500).map(|i| format!("let x{i} = {i};\n")).collect();
    let first = commit(
        work,
        &[
            (
                "README.md",
                b"# Demo\n\nHello <script>alert(1)</script> **world**.\n",
            ),
            ("src/main.rs", b"fn main() {\n    println!(\"<hi>\");\n}\n"),
            ("src/deep/x.txt", b"x\n"),
            ("long.rs", long.as_bytes()),
            ("bin.dat", &[0u8, 1, 2, 255]),
        ],
        "feat: first\n\nThe body of the first commit.",
    );
    let main = commit(
        work,
        &[("src/main.rs", b"fn main() {}\n")],
        "fix: <smaller> main",
    );
    (first, main)
}

async fn pin(net: &Net, alice: &Peer, rid: &Rid) {
    let engine = charter_program::ENGINE_RHAI_V1;
    charter_sync::program::publish(net, &alice.cfg, &alice.keys, rid, engine, BOUNTY)
        .await
        .expect("publish");
    let hash = charter_program::Program::hash(BOUNTY);
    identity::propose(
        net,
        &alice.cfg,
        &alice.keys,
        rid,
        &State::default(),
        |b: &IdBody| IdBody {
            program: Some(ProgramPin {
                engine: engine.into(),
                hash,
            }),
            ..b.clone()
        },
    )
    .await
    .expect("pin");
}

async fn build() -> Fixture {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "demo").await;
    let dir = tempfile::tempdir().expect("work");
    let work = work_repo(dir.path(), "w");
    let (first, main) = files(&work);
    git(&work, &["tag", "v1", &first]);
    push(
        &net,
        &alice,
        &rid,
        &work,
        &["refs/heads/main", "refs/tags/v1"],
    )
    .await
    .expect("push");
    pin(&net, &alice, &rid).await;
    let issue = post(&net, &alice, &rid, "xyz.arkiv.issue", "create", ZERO, json!({"title": "Crash on <b>start</b>", "body": "It **crashes**. <script>x()</script>", "bounty": 5})).await;
    post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.issue",
        "comment",
        issue,
        json!({"body": "seen it too"}),
    )
    .await;
    let patch = post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.patch",
        "create",
        ZERO,
        json!({"title": "Fix the crash", "head": main, "base": first, "closes": hex32(&issue)}),
    )
    .await;
    let review = post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.review",
        "create",
        ZERO,
        json!({"patch": hex32(&patch)}),
    )
    .await;
    post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.review",
        "approve",
        review,
        json!({"patch": hex32(&patch), "head": main}),
    )
    .await;
    post(&net, &alice, &rid, "xyz.arkiv.release", "create", ZERO, json!({"tag": "v1", "commit": first, "artifacts": [{"name": "demo.tgz", "target": "", "sha256": "ab".repeat(32), "size": 10, "locations": ["https://example.com/demo.tgz"]}]})).await;
    post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.issue",
        "close",
        issue,
        json!({"reason": "fixed"}),
    )
    .await;
    let r = sync_loop::round(&net, &alice.cfg, &rid)
        .await
        .expect("round");
    assert_eq!(r, sync_loop::Outcome::Synced);
    Fixture {
        hex: hex32(&rid),
        net,
        alice,
        rid,
        main,
        first,
        issue: hex32(&issue),
        patch: hex32(&patch),
        work: dir,
    }
}

/// Built once per test binary on its own runtime.
pub fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        std::thread::spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("rt")
                .block_on(build())
        })
        .join()
        .expect("fixture")
    })
}

pub fn app_for(home: &std::path::Path) -> axum::Router {
    let shared = Arc::new(Shared::default());
    let app = Arc::new(App::new(home.to_path_buf(), shared, CSRF.into()));
    router(app, Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())))
}

pub fn app() -> axum::Router {
    app_for(fixture().alice.home.path())
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: axum::http::HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn header(&self, h: header::HeaderName) -> String {
        self.headers
            .get(h)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned()
    }
}

pub async fn send(app: axum::Router, req: Request<Body>) -> Resp {
    let r = app.oneshot(req).await.expect("response");
    let (parts, body) = r.into_parts();
    let body = axum::body::to_bytes(body, 64 << 20)
        .await
        .expect("body")
        .to_vec();
    Resp {
        status: parts.status,
        headers: parts.headers,
        body,
    }
}

/// An authenticated GET.
pub fn req(path: &str) -> Request<Body> {
    Request::get(path)
        .header(header::HOST, HOST)
        .header(header::COOKIE, format!("charter_ui={TOKEN}"))
        .body(Body::empty())
        .expect("request")
}

pub async fn get(path: &str) -> Resp {
    send(app(), req(path)).await
}

/// GET that must be 200 with HTML; returns the body.
pub async fn page(path: &str) -> String {
    let r = get(path).await;
    assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.text());
    assert!(
        r.header(header::CONTENT_TYPE).starts_with("text/html"),
        "{path}"
    );
    r.text()
}
