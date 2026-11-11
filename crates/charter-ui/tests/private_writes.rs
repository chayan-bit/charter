//! UI writes on a private repo are sealed like the CLI's: the issue lands sealed under the
//! blinded rid, a member reads it back, and the confirm dialog prices the sealed size.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use charter_core::{AttrValue, Kind, Rid};
use charter_store::Arkiv;
use charter_sync::identity::RepoMeta;
use charter_sync::ids::hex32;
use charter_sync::observe;
use charter_ui::security::Auth;
use charter_ui::writes::{self, Draft, Obj, Step};
use charter_ui::{App, Shared, router};
use common::*;

const KINDS: [Kind; 13] = [
    Kind::Peer,
    Kind::Bind,
    Kind::Id,
    Kind::IdAcc,
    Kind::Refs,
    Kind::Anchor,
    Kind::Manifest,
    Kind::Chunk,
    Kind::Cob,
    Kind::Mls,
    Kind::Device,
    Kind::Head,
    Kind::HeadV3,
];

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(64 << 20)
        .enable_all()
        .build()
        .expect("rt")
}

fn meta(name: &str) -> RepoMeta {
    RepoMeta {
        name: name.into(),
        description: "d".into(),
        default_branch: "main".into(),
        lang: None,
        license: None,
    }
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

/// `kind attr` of every row carrying `rid` in plaintext, or `needle` anywhere in a bytes value.
async fn plaintext<A: Arkiv>(a: &A, rid: &Rid, needle: &[u8]) -> Vec<String> {
    let mut out = vec![];
    for k in KINDS {
        for o in observe::all(a, observe::kind(k)).await.expect("query") {
            for at in &o.attributes {
                let hit = match &at.value {
                    AttrValue::Bytes32(b) => b == rid,
                    AttrValue::Bytes(b) => {
                        b.windows(32).any(|w| w == rid)
                            || b.windows(needle.len()).any(|w| w == needle)
                    }
                    AttrValue::Str(s) => s.contains(&hex32(rid)),
                    AttrValue::U64(_) => false,
                };
                if hit {
                    out.push(format!("{} {}", k.name(), at.name));
                }
            }
        }
    }
    out
}

/// Opens an issue through the router's form and confirm dialog; waits for its block.
async fn open_issue(app: axum::Router, rid: &Rid, title: &str, body: &str) {
    let hex = hex32(rid);
    let kv = [("kind", "issue"), ("title", title), ("body", body)];
    let r = send(app.clone(), form(&format!("/r/{hex}/write"), &kv)).await;
    let html = r.text();
    assert_eq!(r.status, StatusCode::OK, "{html}");
    let at = html.find("/write/").expect("confirm form") + 7;
    let id = html[at..at + 64].to_owned();
    let r = send(app.clone(), form(&format!("/r/{hex}/write/{id}"), &[])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let part = send(app.clone(), req(&format!("/r/{hex}/write/{id}?part=1")))
            .await
            .text();
        assert!(!part.contains("Write failed"), "{part}");
        if part.contains("toast t-confirmed") {
            return;
        }
        assert!(Instant::now() < deadline, "never confirmed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// The keys of `Peer::new(net, seed)`, for the write worker (keys are not `Clone`).
fn keys(seed: u8) -> charter_sync::Keys {
    charter_sync::Keys::from_seed(charter_crypto::MasterSeed::from_bytes([seed; 32]))
}

fn app_with_writer(net: &Arc<Net>, alice: &Peer) -> axum::Router {
    app_for_peer(net, alice, 1)
}

/// A router whose write worker signs as `Peer::new(net, seed)`.
fn app_for_peer(net: &Arc<Net>, alice: &Peer, seed: u8) -> axum::Router {
    let w = writes::spawn(net.clone(), alice.cfg.clone(), keys(seed));
    let app = App::new(
        alice.home.path().to_path_buf(),
        Arc::new(Shared::default()),
        CSRF.into(),
    );
    router(
        Arc::new(app.with_writer(w)),
        Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())),
    )
}

#[test]
fn a_ui_issue_on_a_private_repo_is_sealed() {
    let rt = rt();
    rt.block_on(rt.spawn(async {
        let net = Arc::new(Net::new());
        let alice = Peer::new(&net, 1);
        let rid = charter_private::init(&*net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init --private");
        // What the UI does when the member opens the repo: a sealed sync round (it reports
        // the empty repo, which has no pushed head yet).
        let _ = charter_ui::sync_loop::round(&*net, &alice.cfg, &rid).await;
        drop(charter_sync::Repo::open(alice.home.path(), &rid).expect("repo"));
        let app = app_with_writer(&net, &alice);
        net.take_queries();
        open_issue(
            app,
            &rid,
            "CVE-embargoed-auth-bypass",
            "the admin password is hunter2",
        )
        .await;
        eprintln!(
            "COUNTS ui private write: queries {}",
            net.take_queries().len()
        );

        let leaks = plaintext(&*net, &rid, b"hunter2").await;
        assert!(
            leaks.is_empty(),
            "plaintext rows of the private repo: {leaks:?}"
        );
        // A member reads the issue back through the sealed view.
        let ring = charter_private::ring_for(&*net, &alice.cfg, Some(&alice.keys), &rid)
            .await
            .expect("ring")
            .expect("private");
        let sealed = charter_private::Sealed::new(&*net, ring);
        let f =
            charter_store::Pred::And(vec![observe::kind(Kind::Cob), observe::b32("ag.rid", &rid)]);
        let ops = observe::all(&sealed, f).await.expect("ops");
        let body = |o: &charter_core::Observed| match o.attr("ag.body") {
            Some(AttrValue::Bytes(b)) => b.windows(7).any(|w| w == b"hunter2"),
            _ => false,
        };
        assert!(ops.iter().any(body), "the member reads the sealed issue");
    }))
    .expect("task");
}

fn issue_draft(rid: Rid) -> Draft {
    Draft {
        rid,
        label: "Open issue".into(),
        back: String::new(),
        steps: vec![Step::Op {
            type_name: "xyz.arkiv.issue",
            action: "create",
            object: Obj::New,
            payload: serde_json::json!({"title": "t", "body": "b"}),
        }],
    }
}

#[test]
fn the_confirm_dialog_prices_a_private_write_at_its_sealed_size() {
    let rt = rt();
    rt.block_on(rt.spawn(async {
        let net = Arc::new(Net::new());
        let alice = Peer::new(&net, 1);
        let public = charter_sync::identity::init(&*net, &alice.cfg, &alice.keys, &meta("open"))
            .await
            .expect("init");
        let private = charter_private::init(&*net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init --private");
        let w = writes::spawn(net.clone(), alice.cfg.clone(), keys(1));
        let (_, _, open) = w.prepare(issue_draft(public)).await.expect("public");
        let (_, _, sealed) = w.prepare(issue_draft(private)).await.expect("private");
        eprintln!("public cost {}, private cost {}", open.cost, sealed.cost);
        assert_eq!(open.entities, sealed.entities);
        assert!(
            sealed.cost > open.cost,
            "sealing pads every entity, so the private write costs more"
        );
    }))
    .expect("task");
}

/// Query and call counts of one UI issue write on a public repo (the perf rule: no slower).
#[test]
fn ui_write_round_trips_on_a_public_repo() {
    let rt = rt();
    rt.block_on(rt.spawn(async {
        let net = Arc::new(Net::new());
        let alice = Peer::new(&net, 1);
        let rid = charter_sync::identity::init(&*net, &alice.cfg, &alice.keys, &meta("open"))
            .await
            .expect("init");
        charter_ui::sync_loop::round(&*net, &alice.cfg, &rid)
            .await
            .expect("sync round");
        let app = app_with_writer(&net, &alice);
        net.take_queries();
        open_issue(app, &rid, "t", "b").await;
        eprintln!(
            "COUNTS ui public write: queries {}",
            net.take_queries().len()
        );
    }))
    .expect("task");
}

/// Queries of one issue write by alice: through the UI (price, confirm, send), and as the CLI
/// posts it (`chart op post`: the sealing selector, then `cob::post`).
async fn write_counts(net: &Arc<Net>, alice: &Peer, rid: &Rid) -> (usize, usize) {
    let app = app_with_writer(net, alice);
    net.take_queries();
    open_issue(app, rid, "t", "b").await;
    let ui = net.take_queries().len();
    let k = keys(1);
    let b = charter_private::for_repo(&**net, &alice.cfg, Some(&k), rid)
        .await
        .expect("selector");
    let body = charter_core::CobOpBody {
        rid: *rid,
        type_name: "xyz.arkiv.issue".into(),
        object: charter_core::ZERO,
        parents: vec![],
        action: "create".into(),
        payload: charter_sync::cob::json::to_cbor(&serde_json::json!({"title": "t"}))
            .expect("cbor"),
        ts_ms: 1,
    };
    charter_sync::cob::post(&b, &alice.cfg, &k, &body)
        .await
        .expect("cli post");
    (ui, net.take_queries().len())
}

/// The perf rule: a private repo's UI write pays for one group refresh, exactly as its CLI
/// write does, on top of what the same write costs on a public repo.
#[test]
fn a_private_ui_write_refreshes_the_group_once_like_the_cli() {
    let rt = rt();
    rt.block_on(rt.spawn(async {
        let net = Arc::new(Net::new());
        let alice = Peer::new(&net, 1);
        let public = charter_sync::identity::init(&*net, &alice.cfg, &alice.keys, &meta("open"))
            .await
            .expect("init");
        let private = charter_private::init(&*net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init --private");
        for rid in [public, private] {
            let _ = charter_ui::sync_loop::round(&*net, &alice.cfg, &rid).await;
            drop(charter_sync::Repo::open(alice.home.path(), &rid).expect("cache"));
        }
        let (ui_pub, cli_pub) = write_counts(&net, &alice, &public).await;
        let (ui_priv, cli_priv) = write_counts(&net, &alice, &private).await;
        eprintln!(
            "COUNTS issue write queries: ui public {ui_pub}, ui private {ui_priv}, \
             cli public {cli_pub}, cli private {cli_priv}"
        );
        assert_eq!(cli_priv, cli_pub + 1, "the CLI's one refresh");
        assert_eq!(ui_priv, ui_pub + 1, "the UI's one refresh");
    }))
    .expect("task");
}

/// Pricing reads the last saved keyring, but the send refreshes the group: a removal (key
/// rotation) between the two still seals the write under the new epoch.
#[test]
fn a_rotation_between_price_and_send_seals_under_the_new_epoch() {
    let rt = rt();
    rt.block_on(rt.spawn(async {
        let net = Arc::new(Net::new());
        let (alice, bob, carol) = (Peer::new(&net, 1), Peer::new(&net, 2), Peer::new(&net, 3));
        let rid = charter_private::init(&*net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init --private");
        for (p, role) in [
            (&bob, charter_private::Role::Write),
            (&carol, charter_private::Role::Read),
        ] {
            let kp = charter_private::publish_key_package(&*net, &p.cfg, &p.keys)
                .await
                .expect("key package");
            charter_private::add_member(&*net, &alice.cfg, &alice.keys, &rid, &kp, role)
                .await
                .expect("add");
        }
        // Bob's keyring is saved at the current epoch, as his last sync left it.
        let old = charter_private::ring_for(&*net, &bob.cfg, Some(&bob.keys), &rid)
            .await
            .expect("ring")
            .expect("member");
        let (old_ep, _) = old.current().expect("epoch");
        drop(charter_sync::Repo::open(bob.home.path(), &rid).expect("cache"));
        let hex = hex32(&rid);
        let app = app_for_peer(&net, &bob, 2);
        let kv = [
            ("kind", "issue"),
            ("title", "t"),
            ("body", "after carol left"),
        ];
        let r = send(app.clone(), form(&format!("/r/{hex}/write"), &kv)).await;
        let html = r.text();
        assert_eq!(r.status, StatusCode::OK, "{html}");
        let at = html.find("/write/").expect("confirm form") + 7;
        let id = html[at..at + 64].to_owned();
        // Carol is removed after bob priced the draft and before he confirms it.
        charter_private::remove_member(&*net, &alice.cfg, &alice.keys, &rid, &carol.keys.nid())
            .await
            .expect("remove");
        let r = send(app.clone(), form(&format!("/r/{hex}/write/{id}"), &[])).await;
        assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let part = send(app.clone(), req(&format!("/r/{hex}/write/{id}?part=1")))
                .await
                .text();
            assert!(!part.contains("Write failed"), "{part}");
            if part.contains("toast t-confirmed") {
                break;
            }
            assert!(Instant::now() < deadline, "never confirmed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let eps: Vec<_> = observe::all(&*net, observe::kind(Kind::Cob))
            .await
            .expect("cob rows")
            .iter()
            .filter_map(|o| match o.attr("ag.ep") {
                Some(AttrValue::U64(e)) => Some(*e),
                _ => None,
            })
            .collect();
        assert_eq!(eps.len(), 1, "bob's issue is the only op: {eps:?}");
        assert!(
            eps[0] > old_ep,
            "sealed under epoch {} after rotating past {old_ep}",
            eps[0]
        );
    }))
    .expect("task");
}
