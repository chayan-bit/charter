//! `chart ui` through the real binary on the file-backed fake: a peer with an empty
//! `$CHARTER_HOME` names a repo pushed by another and the printed URL serves it verified;
//! and an issue, a comment, a patch review and a release written through the UI's HTTP
//! endpoints verify with the real `chart op list` and `chart release verify`; and a push
//! from another process reaches an open event stream within one sync round.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::ci::*;
use common::e2e::{User, field, fund, write};
use common::programs::*;

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One HTTP/1.1 request: (status, raw headers, body).
fn http(
    port: u16,
    req: &str,
    path: &str,
    cookie: Option<&str>,
    form: &str,
) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let cookie = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    let body = match form {
        "" => String::new(),
        f => format!(
            "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\n",
            f.len()
        ),
    };
    write!(
        s,
        "{req} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}{body}Connection: close\r\n\r\n{form}"
    )
    .expect("send");
    let mut out = String::new();
    s.read_to_string(&mut out).expect("read");
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head.to_owned(), body.to_owned())
}

fn get(port: u16, path: &str, cookie: Option<&str>) -> (u16, String, String) {
    http(port, "GET", path, cookie, "")
}

/// A running `chart ui [<url>]` of `u`: the guard, its port and the session path.
fn start(u: &User, url: &str) -> (Kill, u16, String) {
    let mut child = u
        .cmd(env!("CARGO_BIN_EXE_chart"), &u.home)
        .args(["ui", url])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("url");
    let link = line.trim();
    let rest = link
        .strip_prefix("http://127.0.0.1:")
        .expect("loopback url");
    let (port, path) = rest.split_once('/').expect("path");
    (Kill(child), port.parse().expect("port"), path.to_owned())
}

/// Exchanges the URL token for the session cookie.
fn login(port: u16, path: &str) -> String {
    let (status, head, _) = get(port, &format!("/{path}"), None);
    assert_eq!(status, 303);
    head.lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .and_then(|c| c.split(';').next())
        .expect("cookie")
        .to_owned()
}

/// Polls `path` until its body contains `want`.
fn wait_for(port: u16, cookie: &str, path: &str, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, _, body) = get(port, path, Some(cookie));
        if status == 200 && body.contains(want) {
            return body;
        }
        assert!(
            Instant::now() < deadline,
            "{path} never showed {want}: {status} {body}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn chart_ui_fetches_a_named_repo_and_serves_it_behind_the_session_cookie() {
    let tmp = tempfile::tempdir().expect("tmp");
    let log = tmp.path().join("chain.log");
    let (alice, bob) = (
        User::new(tmp.path(), "alice", &log),
        User::new(tmp.path(), "bob", &log),
    );
    fund(&log, field(&alice.arkiv(&["keys", "init"]), "wallet:"));
    let created = alice.arkiv(&["init", "--name", "uidemo", "--description", "ui e2e"]);
    let url = field(&created, "url:").to_owned();
    let work = tmp.path().join("work");
    alice.git(tmp.path(), &["init", "-q", &work.to_string_lossy()]);
    write(&work, "README.md", b"# uidemo\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "first"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    let (_guard, port, path) = start(&bob, &url);
    assert!(
        path.starts_with("?token=") && path.len() == 7 + 64,
        "{path}"
    );
    assert_eq!(get(port, "/", None).0, 401, "no cookie, no page");
    let cookie = login(port, &path);
    let rid = url.trim_start_matches("arkiv::");
    let body = wait_for(port, &cookie, &format!("/r/{rid}"), "seal verified");
    assert!(
        body.contains("uidemo") && body.contains(">uidemo</a></h1>"),
        "{body}"
    );
    let issue = wait_for(port, &cookie, &format!("/r/{rid}/issues/new"), "New issue");
    assert!(issue.contains("Writes are off"), "bob has no keys: {issue}");
}

/// The CSRF token every page carries.
fn csrf(page: &str) -> String {
    let at = page
        .find("name=\"csrf-token\" content=\"")
        .expect("csrf meta")
        + 27;
    page[at..at + 64].to_owned()
}

/// One write through the UI: price, confirm, wait for the receipt.
fn ui_write(port: u16, cookie: &str, rid: &str, kv: &[(&str, &str)]) {
    let token = csrf(&get(port, &format!("/r/{rid}"), Some(cookie)).2);
    let enc = |s: &str| {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect::<String>()
    };
    let mut fields: Vec<String> = kv.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect();
    fields.push(format!("csrf={token}"));
    let (status, _, page) = http(
        port,
        "POST",
        &format!("/r/{rid}/write"),
        Some(cookie),
        &fields.join("&"),
    );
    assert_eq!(status, 200, "{page}");
    assert!(page.contains("Confirm write"), "{page}");
    let at = page.find("/write/").expect("confirm form") + 7;
    let id = &page[at..at + 64];
    let status_path = format!("/r/{rid}/write/{id}");
    let (code, head, _) = http(
        port,
        "POST",
        &status_path,
        Some(cookie),
        &format!("csrf={token}"),
    );
    assert_eq!(code, 303, "{head}");
    wait_for(
        port,
        cookie,
        &format!("{status_path}?part=1"),
        "<dt>Block</dt><dd><code>",
    );
}

#[test]
fn writes_made_in_the_ui_verify_with_chart_op_list_and_chart_release_verify() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let alice = user(root, "alice", &log);
    let (url, work, _) = repo_with_main(&alice, root, "uiw");
    let ci = user(root, "ci", &log);
    let gated = gated_for(&hex_id(&nid(&ci)), root);
    alice.arkiv(&[
        "program",
        "pin",
        "--repo",
        &url,
        "--file",
        &gated.to_string_lossy(),
    ]);
    alice.git(&work, &["checkout", "-q", "-b", "feat/ui"]);
    let c1 = commit(&alice, &work, "made for the ui\n");
    let p1 = patch(&alice, &work, &url, "feat/ui");

    let (_guard, port, path) = start(&alice, &url);
    let cookie = login(port, &path);
    let rid =
        hex::encode(charter_sync::ids::parse_id(url.trim_start_matches("arkiv::")).expect("rid"));
    wait_for(port, &cookie, &format!("/r/{rid}"), "seal verified");

    ui_write(
        port,
        &cookie,
        &rid,
        &[
            ("kind", "issue"),
            ("title", "Typed in the UI"),
            ("body", "found it"),
        ],
    );
    let ops = alice.arkiv(&[
        "op",
        "list",
        "--repo",
        &url,
        "--type",
        "xyz.arkiv.issue",
        "--json",
    ]);
    let list: serde_json::Value = serde_json::from_str(&ops).expect("json");
    let issue = list[0]["id"].as_str().expect("issue id").to_owned();
    assert_eq!(list[0]["payload"]["title"], "Typed in the UI");
    ui_write(
        port,
        &cookie,
        &rid,
        &[
            ("kind", "comment"),
            ("issue", &issue),
            ("body", "a comment from the UI"),
        ],
    );
    ui_write(
        port,
        &cookie,
        &rid,
        &[
            ("kind", "review"),
            ("patch", &p1),
            ("verdict", "approve"),
            ("head", &c1),
        ],
    );

    let ops = alice.arkiv(&["op", "list", "--repo", &url]);
    for want in [
        "xyz.arkiv.issue create",
        "xyz.arkiv.issue comment",
        "xyz.arkiv.review create",
        "xyz.arkiv.review approve",
    ] {
        assert!(ops.contains(want), "{want} in:\n{ops}");
    }
    assert!(
        ops.contains("a comment from the UI") && ops.contains(&c1),
        "{ops}"
    );

    // CI passes at the approved head and the merge script pushes it: the UI approval counts.
    run_ci(&ci, &url, "true");
    ok(merge(&alice, &work, &url, &p1));

    let (dist, dl) = (root.join("dist"), root.join("dl"));
    std::fs::create_dir_all(&dist).expect("dist");
    write(&dist, "ui.tar.gz", b"from the ui");
    let art = format!("ui.tar.gz={}", dist.join("ui.tar.gz").display());
    let loc = format!("ui.tar.gz=file://{}", dist.join("ui.tar.gz").display());
    ui_write(
        port,
        &cookie,
        &rid,
        &[
            ("kind", "release"),
            ("tag", "v1"),
            ("commit", &c1),
            ("artifacts", &art),
            ("locations", &loc),
        ],
    );
    ok(script(
        &ci,
        &ci.home,
        "chart-release-attest.sh",
        &["--repo", &url, "--tag", "v1"],
        "",
    ));
    let dl_s = dl.to_string_lossy();
    let args = [
        "release",
        "verify",
        "--repo",
        &url,
        "--tag",
        "v1",
        "--download-dir",
        &dl_s,
    ];
    let out = ok(alice.run(env!("CARGO_BIN_EXE_chart"), &alice.home, &args));
    assert!(
        out.contains("valid under the pinned program") && out.contains("OK ui.tar.gz"),
        "{out}"
    );
    wait_for(
        port,
        &cookie,
        &format!("/r/{rid}/releases"),
        "Allowed by the repo's rules",
    );
}

/// An open `/events` stream, past the response head.
fn stream(port: u16, cookie: &str, path: &str) -> BufReader<TcpStream> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(60)))
        .expect("timeout");
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nCookie: {cookie}\r\nAccept: text/event-stream\r\n\r\n"
    )
    .expect("send");
    let mut r = BufReader::new(s);
    let mut line = String::new();
    r.read_line(&mut line).expect("status");
    assert!(line.contains(" 200 "), "{line}");
    while line != "\r\n" {
        line.clear();
        r.read_line(&mut line).expect("header");
    }
    r
}

/// The next event's name (chunk-size lines and data lines are skipped).
fn next_event(r: &mut BufReader<TcpStream>) -> String {
    let mut line = String::new();
    loop {
        line.clear();
        assert!(r.read_line(&mut line).expect("event") > 0, "stream closed");
        if let Some(name) = line.trim_end().strip_prefix("event: ") {
            return name.to_owned();
        }
    }
}

#[test]
fn a_push_from_another_process_reaches_an_open_event_stream_within_one_round() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let alice = user(root, "alice", &log);
    let bob = User::new(root, "bob", &log);
    let (url, work, _) = repo_with_main(&alice, root, "live");
    let (_guard, port, path) = start(&bob, &url);
    let cookie = login(port, &path);
    let rid =
        hex::encode(charter_sync::ids::parse_id(url.trim_start_matches("arkiv::")).expect("rid"));
    wait_for(port, &cookie, &format!("/r/{rid}"), "seal verified");
    let mut events = stream(port, &cookie, &format!("/r/{rid}/events"));
    while next_event(&mut events) != "sync" {}

    let c = commit(&alice, &work, "pushed while the page is open\n");
    alice.git(&work, &["push", "-q", &url, "main"]);
    let pushed = Instant::now();
    // A round already running when the push landed may end without it; the next one
    // must carry it.
    let mut rounds = 0;
    loop {
        match next_event(&mut events).as_str() {
            "push" => break,
            "sync" => rounds += 1,
            _ => {}
        }
        assert!(rounds <= 1, "a full round passed without the push");
    }
    eprintln!("push event {:?} after git push returned", pushed.elapsed());
    let log = get(port, &format!("/r/{rid}/log/main"), Some(&cookie)).2;
    assert!(log.contains(&c), "the log shows the pushed commit");
}
