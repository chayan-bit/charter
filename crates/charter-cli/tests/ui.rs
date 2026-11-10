//! `chart ui` through the real binary: a peer with an empty `$CHARTER_HOME` names a repo
//! pushed by another, the sync loop fetches it, and the printed URL serves it verified.

mod common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use common::e2e::{User, field, fund, write};

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One HTTP/1.1 GET: (status, raw headers, body).
fn get(port: u16, path: &str, cookie: Option<&str>) -> (u16, String, String) {
    let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    let cookie = cookie
        .map(|c| format!("Cookie: {c}\r\n"))
        .unwrap_or_default();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{cookie}Connection: close\r\n\r\n"
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

    let mut child = bob
        .cmd(env!("CARGO_BIN_EXE_chart"), &bob.home)
        .args(["ui", &url])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    let mut line = String::new();
    BufReader::new(child.stdout.take().expect("stdout"))
        .read_line(&mut line)
        .expect("url");
    let _guard = Kill(child);
    let link = line.trim();
    let rest = link
        .strip_prefix("http://127.0.0.1:")
        .expect("loopback url");
    let (port, path) = rest.split_once('/').expect("path");
    let port: u16 = port.parse().expect("port");
    assert!(
        path.starts_with("?token=") && path.len() == 7 + 64,
        "{link}"
    );

    assert_eq!(get(port, "/", None).0, 401, "no cookie, no page");
    let (status, head, _) = get(port, &format!("/{path}"), None);
    assert_eq!(status, 303);
    let cookie = head
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: "))
        .and_then(|c| c.split(';').next())
        .expect("cookie")
        .to_owned();

    let rid = url.trim_start_matches("arkiv::");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let (status, _, body) = get(port, &format!("/r/{rid}"), Some(&cookie));
        if status == 200 && body.contains("vp-verified") {
            assert!(
                body.contains("uidemo") && body.contains("<h1>uidemo</h1>"),
                "{body}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "repo never synced: {status} {body}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }
}
