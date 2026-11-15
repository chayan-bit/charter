//! Parity batch 3 pages on their own fixture, so the shared one keeps its exact shape:
//! signed commits (#61), labels, assignees and milestones (#40), code owners (#53).

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use charter_core::{Rid, ZERO};
use charter_sync::cob;
use charter_sync::gitsig::{CLAIM_NAMESPACE, CLAIM_TYPE, claim_message};
use charter_sync::ids::hex32;
use charter_ui::security::Auth;
use charter_ui::{App, Shared, router, sync_loop, writes};
use common::*;
use serde_json::json;

struct Fx {
    net: Arc<Net>,
    alice: Peer,
    rid: Rid,
    hex: String,
    /// Signed with alice's claimed key, with an unclaimed key, and unsigned.
    claimed: String,
    unclaimed: String,
    plain: String,
    /// Labelled `bug` and `docs`, milestone `v1`, assigned to alice.
    issue: String,
    /// A patch from `plain` to `unclaimed`, with an inline comment at that head and one
    /// at an older head.
    patch: String,
    _work: tempfile::TempDir,
}

fn ssh_key(dir: &Path, name: &str) -> String {
    let p = dir.join(name).to_string_lossy().into_owned();
    let out = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f", &p])
        .output()
        .expect("ssh-keygen");
    assert!(out.status.success());
    p
}

fn ssh_sign(key: &str, msg: &str) -> String {
    use std::io::Write as _;
    let mut c = Command::new("ssh-keygen")
        .args(["-q", "-Y", "sign", "-n", CLAIM_NAMESPACE, "-f", key])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    c.stdin
        .take()
        .expect("stdin")
        .write_all(msg.as_bytes())
        .expect("write");
    String::from_utf8(c.wait_with_output().expect("wait").stdout).expect("utf8")
}

fn signed(work: &Path, key: &str, file: &str) -> String {
    std::fs::write(work.join(file), file).expect("write");
    git(work, &["add", "-A"]);
    let sk = format!("user.signingkey={key}");
    git(
        work,
        &[
            "-c",
            "gpg.format=ssh",
            "-c",
            &sk,
            "commit",
            "-q",
            "-S",
            "-m",
            file,
        ],
    );
    git(work, &["rev-parse", "HEAD"])
}

async fn build() -> Fx {
    let net = Net::new();
    let mut alice = Peer::new(&net, 3);
    alice.cfg.alias = "alice".into();
    let rid = init(&net, &alice, "parity").await;
    let dir = tempfile::tempdir().expect("work");
    let work = work_repo(dir.path(), "w");
    let me = hex32(&alice.keys.nid());
    let owners = format!("*.txt {me}\n");
    let files: [(&str, &[u8]); 2] = [
        ("README.md", b"# Parity\n"),
        ("CODEOWNERS", owners.as_bytes()),
    ];
    let plain = commit(&work, &files, "feat: plain");
    let (key, other) = (ssh_key(dir.path(), "k"), ssh_key(dir.path(), "o"));
    let claimed = signed(&work, &key, "a.txt");
    let unclaimed = signed(&work, &other, "b.txt");
    push(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("push");
    let line = std::fs::read_to_string(format!("{key}.pub")).expect("pub");
    let sig = ssh_sign(&key, &claim_message(&alice.keys.nid()));
    let p = json!({"key": line.trim(), "sig": sig});
    post(&net, &alice, &rid, CLAIM_TYPE, "create", ZERO, p).await;
    let iss = json!({"title": "Triage me", "labels": ["bug"], "milestone": "v1"});
    let issue = post(&net, &alice, &rid, "xyz.arkiv.issue", "create", ZERO, iss).await;
    let label = json!({"labels": ["docs"]});
    post(&net, &alice, &rid, "xyz.arkiv.issue", "label", issue, label).await;
    let assign = json!({"assignees": [me]});
    post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.issue",
        "assign",
        issue,
        assign,
    )
    .await;
    let other = json!({"title": "Untriaged"});
    post(&net, &alice, &rid, "xyz.arkiv.issue", "create", ZERO, other).await;
    let pj = json!({"title": "Add files", "head": unclaimed, "base": plain});
    let patch = post(&net, &alice, &rid, "xyz.arkiv.patch", "create", ZERO, pj).await;
    let ph = hex32(&patch);
    let rv = post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.review",
        "create",
        ZERO,
        json!({"patch": ph}),
    )
    .await;
    let inline = |head: &str, body: &str| {
        json!({"patch": ph, "head": head, "body": "Looks fine", "comments": [
            {"path": "b.txt", "line": 1, "body": body, "suggestion": "b, better"}
        ]})
    };
    post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.review",
        "comment",
        rv,
        inline(&claimed, "Old note"),
    )
    .await;
    post(
        &net,
        &alice,
        &rid,
        "xyz.arkiv.review",
        "comment",
        rv,
        inline(&unclaimed, "Rename this"),
    )
    .await;
    let r = sync_loop::round(&net, &alice.cfg, &rid)
        .await
        .expect("round");
    assert!(matches!(r, sync_loop::Outcome::Synced(_)), "{r:?}");
    Fx {
        hex: hex32(&rid),
        issue: hex32(&issue),
        patch: ph,
        net: Arc::new(net),
        alice,
        rid,
        claimed,
        unclaimed,
        plain,
        _work: dir,
    }
}

fn fx() -> &'static Fx {
    static F: OnceLock<Fx> = OnceLock::new();
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

async fn html(path: &str) -> String {
    let r = send(app_for(fx().alice.home.path()), req(path)).await;
    assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.text());
    r.text()
}

#[tokio::test]
async fn commit_pages_show_who_signed_with_a_claimed_ssh_key() {
    let f = fx();
    let page = html(&format!("/r/{}/commit/{}", f.hex, f.claimed)).await;
    assert!(page.contains("Verified: signed by alice"), "{page}");
    assert!(page.contains("SHA256:"), "fingerprint shown");
    let page = html(&format!("/r/{}/commit/{}", f.hex, f.unclaimed)).await;
    assert!(
        page.contains("a key no one in this repo has claimed"),
        "{page}"
    );
    assert!(!page.contains("Verified:"));
    let page = html(&format!("/r/{}/commit/{}", f.hex, f.plain)).await;
    assert!(
        page.contains(r#"<span class="muted">unsigned</span>"#),
        "{page}"
    );
    let _ = (&f.net, &f.rid);
}

#[tokio::test]
async fn labels_milestones_and_assignees_show_and_filter() {
    let f = fx();
    let list = html(&format!("/r/{}/issues", f.hex)).await;
    assert!(
        list.contains(r#"<a class="lbl" href="?label=bug">bug</a>"#),
        "{list}"
    );
    assert!(
        list.contains("Milestone v1") && list.contains("Assigned to"),
        "{list}"
    );
    let only = html(&format!("/r/{}/issues?label=docs", f.hex)).await;
    assert!(
        only.contains("Triage me") && !only.contains("Untriaged"),
        "{only}"
    );
    assert!(only.contains("Showing issues labelled"), "{only}");
    let none = html(&format!("/r/{}/issues?milestone=v9", f.hex)).await;
    assert!(none.contains("No issues match"), "{none}");
    let page = html(&format!("/r/{}/issues/{}", f.hex, f.issue)).await;
    assert!(page.contains("<dt>Labels</dt>") && page.contains("<dt>Milestone</dt>"));
    assert!(
        page.contains(r#"added <span class="lbl">docs</span>"#),
        "{page}"
    );
    assert!(page.contains("assigned <b>"), "{page}");
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

fn writing_app() -> axum::Router {
    let f = fx();
    let keys = charter_sync::Keys::from_seed(charter_crypto::MasterSeed::from_bytes([3; 32]));
    let w = writes::spawn(f.net.clone(), f.alice.cfg.clone(), keys);
    let app = App::new(
        f.alice.home.path().to_path_buf(),
        Arc::new(Shared::default()),
        CSRF.into(),
    );
    router(
        Arc::new(app.with_writer(w)),
        Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())),
    )
}

/// One write at a time: every writing test signs with alice's keys, from one address.
static WRITES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Prices `kv`, confirms it and waits for its block; returns the confirm page.
async fn write(kv: &[(&str, &str)]) -> String {
    let _one = WRITES.lock().await;
    let (f, app) = (fx(), writing_app());
    let r = send(app.clone(), form(&format!("/r/{}/write", f.hex), kv)).await;
    let page = r.text();
    assert_eq!(r.status, StatusCode::OK, "{page}");
    let at = page.find("/write/").expect("confirm form") + 7;
    let id = &page[at..at + 64];
    let r = send(app.clone(), form(&format!("/r/{}/write/{id}", f.hex), &[])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let part = send(app.clone(), req(&format!("/r/{}/write/{id}?part=1", f.hex)))
            .await
            .text();
        if part.contains("confirmed") {
            return page;
        }
        assert!(
            !part.contains("failed") && Instant::now() < deadline,
            "{part}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn the_triage_form_signs_one_op_per_change() {
    let f = fx();
    let before = cob::list(&*f.net, &f.rid, None, Some("xyz.arkiv.issue"))
        .await
        .expect("list")
        .len();
    let back = format!("/r/{}/issues/{}", f.hex, f.issue);
    let page = write(&[
        ("kind", "triage"),
        ("on", "issues"),
        ("object", &f.issue),
        ("back", &back),
        ("labels", "bug, docs, ui"),
        ("cur_labels", "bug, docs"),
        ("milestone", "v1"),
        ("cur_milestone", "v1"),
    ])
    .await;
    assert!(
        page.contains("Update issue?") && page.contains("Labels added: ui"),
        "{page}"
    );
    // Issue ops only: other tests write review ops on the same chain at the same time.
    let ops = cob::list(&*f.net, &f.rid, None, Some("xyz.arkiv.issue"))
        .await
        .expect("list");
    assert_eq!(ops.len(), before + 1, "one op for one kind of change");
    let last = ops.last().expect("op");
    assert_eq!(
        (last.body.action.as_str(), last.object()),
        ("label", charter_sync::ids::unhex32(&f.issue).expect("id"))
    );
}

#[tokio::test]
async fn blob_pages_name_the_code_owners_of_the_file() {
    let f = fx();
    let page = html(&format!("/r/{}/blob/main/a.txt", f.hex)).await;
    assert!(
        page.contains(r#"<p class="owners">Code owners <b>alice</b></p>"#),
        "{page}"
    );
    let page = html(&format!("/r/{}/blob/main/README.md", f.hex)).await;
    assert!(
        !page.contains(r#"class="owners""#),
        "an unowned file has no line"
    );
}

/// Prices `kv` on `app` for `rid`, confirms it and waits for its block; the confirm page.
async fn write_on(app: &axum::Router, hex: &str, kv: &[(&str, &str)]) -> String {
    let r = send(app.clone(), form(&format!("/r/{hex}/write"), kv)).await;
    let page = r.text();
    assert_eq!(r.status, StatusCode::OK, "{page}");
    let at = page.find("/write/").expect("confirm form") + 7;
    let id = &page[at..at + 64];
    let r = send(app.clone(), form(&format!("/r/{hex}/write/{id}"), &[])).await;
    assert_eq!(r.status, StatusCode::SEE_OTHER, "{}", r.text());
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let part = send(app.clone(), req(&format!("/r/{hex}/write/{id}?part=1")))
            .await
            .text();
        if part.contains("confirmed") {
            return page;
        }
        assert!(
            !part.contains("failed") && Instant::now() < deadline,
            "{part}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_maintainer_archives_and_renames_from_the_manage_page() {
    let net = Arc::new(Net::new());
    let alice = Peer::new(&net, 4);
    let rid = init(&net, &alice, "shelf").await;
    let hex = hex32(&rid);
    let keys = charter_sync::Keys::from_seed(charter_crypto::MasterSeed::from_bytes([4; 32]));
    let w = writes::spawn(net.clone(), alice.cfg.clone(), keys);
    let app = App::new(
        alice.home.path().to_path_buf(),
        Arc::new(Shared::default()),
        CSRF.into(),
    );
    let app = router(
        Arc::new(app.with_writer(w)),
        Arc::new(Auth::with(PORT, TOKEN.into(), CSRF.into())),
    );
    let round = || async {
        let r = sync_loop::round(&*net, &alice.cfg, &rid)
            .await
            .expect("round");
        assert!(matches!(r, sync_loop::Outcome::Synced(_)), "{r:?}");
    };
    round().await;
    let get = |p: String| {
        let app = app.clone();
        async move {
            let r = send(app, req(&p)).await;
            assert_eq!(r.status, StatusCode::OK, "{p}");
            r.text()
        }
    };
    let home = get(format!("/r/{hex}")).await;
    assert!(
        home.contains(&format!(r#"href="/r/{hex}/manage""#)),
        "{home}"
    );
    let page = get(format!("/r/{hex}/manage")).await;
    assert!(page.contains(r#"name="name" value="shelf""#), "{page}");

    let base = [
        ("kind", "identity"),
        ("cur_name", "shelf"),
        ("back", &format!("/r/{hex}")),
    ];
    let kv = [&base[..], &[("name", "shelved"), ("archived", "1")]].concat();
    let confirm = write_on(&app, &hex, &kv).await;
    assert!(
        confirm.contains("Update repository?")
            && confirm
                .contains("Renamed to shelved; archived (advisory: honest clients stop writing)"),
        "{confirm}"
    );
    round().await;
    let home = get(format!("/r/{hex}")).await;
    assert!(
        home.contains("shelved") && home.contains("Archived."),
        "{home}"
    );
    assert!(home.contains(r#"class="arch""#), "header chip: {home}");
    let list = get("/".to_owned()).await;
    assert!(
        list.contains("</svg>Archived</span>"),
        "repo list chip: {list}"
    );
    let new = get(format!("/r/{hex}/issues/new")).await;
    assert!(
        new.contains("so Charter writes nothing to it (advisory: other clients can)."),
        "{new}"
    );
    assert!(!new.contains(r#"value="issue""#), "no write form");

    let kv = [
        ("kind", "identity"),
        ("cur_name", "shelved"),
        ("name", "shelved"),
        ("cur_archived", "1"),
        ("archived", "0"),
    ];
    write_on(&app, &hex, &kv).await;
    round().await;
    let home = get(format!("/r/{hex}")).await;
    assert!(!home.contains("Archived."), "{home}");

    // A hand-over to an id that never wrote (a typo) is refused before anything is signed.
    let typo = "ab".repeat(32);
    let hand = |to: &str| {
        [
            ("kind", "identity"),
            ("cur_name", "shelved"),
            ("name", "shelved"),
            ("hand_to", to),
            ("confirm", "shelved"),
        ]
        .map(|(k, v)| (k, v.to_owned()))
    };
    let kv = hand(&typo);
    let kv: Vec<(&str, &str)> = kv.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let refused = send(app.clone(), form(&format!("/r/{hex}/write"), &kv))
        .await
        .text();
    assert!(refused.contains("never written to Arkiv"), "{refused}");
    // A peer with a peer document (it made a repo) can take it over.
    let bob = Peer::new(&net, 5);
    init(&net, &bob, "elsewhere").await;
    let kv = hand(&hex32(&bob.keys.nid()));
    let kv: Vec<(&str, &str)> = kv.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let confirm = write_on(&app, &hex, &kv).await;
    assert!(confirm.contains("Handed over to"), "{confirm}");
}

#[tokio::test]
async fn inline_comments_sit_under_their_line_and_old_ones_are_outdated() {
    let f = fx();
    let files = html(&format!("/r/{}/patches/{}/files", f.hex, f.patch)).await;
    let under = files.find("Rename this").expect("current note");
    let row = files.find(r#"<tr class=ic>"#).expect("note row");
    assert!(
        row < under,
        "the note is in the row after its line: {files}"
    );
    assert!(files.contains("<code>b.txt:1</code>") && files.contains("Suggested change"));
    assert!(
        files.contains("<tr class=a><td class=sg><td class=lc>b, better</tr>"),
        "{files}"
    );
    let old = files.find("1 outdated comment").expect("outdated list");
    assert!(files[old..].contains("Old note"), "{files}");
    assert!(
        files.contains(r#"<h2><a href="/r/"#) && files.contains("+1"),
        "diff shown"
    );
    let conv = html(&format!("/r/{}/patches/{}", f.hex, f.patch)).await;
    assert!(
        conv.contains(&format!("/patches/{}/files#ic-", f.patch)),
        "{conv}"
    );
    assert!(
        conv.contains("suggested change") && !conv.contains("comments:"),
        "{conv}"
    );
}

#[tokio::test]
async fn the_files_page_review_batches_inline_comments_in_one_op() {
    let f = fx();
    let back = format!("/r/{}/patches/{}/files", f.hex, f.patch);
    write(&[
        ("kind", "review"),
        ("patch", &f.patch),
        ("head", &f.unclaimed),
        ("back", &back),
        ("verdict", "comment"),
        ("p.0", "a.txt"),
        ("p.1", "b.txt"),
        ("c.0.n.1", "First file"),
        ("c.1.n.1", ""),
        ("s.1.n.1", "replacement"),
        ("c.1.o.9", "  "),
    ])
    .await;
    let ops = cob::list(&*f.net, &f.rid, None, Some("xyz.arkiv.review"))
        .await
        .expect("list");
    let last = ops.last().expect("op");
    let p = charter_sync::cob::json::to_json(&last.body.payload).expect("json");
    assert_eq!(last.body.action, "comment");
    assert_eq!(p["head"], json!(f.unclaimed));
    assert_eq!(
        p["comments"],
        json!([
            {"path": "a.txt", "side": "new", "line": 1, "body": "First file"},
            {"path": "b.txt", "side": "new", "line": 1, "body": "", "suggestion": "replacement"}
        ])
    );
}
