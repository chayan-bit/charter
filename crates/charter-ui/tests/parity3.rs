//! Parity batch 3 pages on their own fixture, so the shared one keeps its exact shape:
//! signed commits (#61).

mod common;

use std::path::Path;
use std::process::Command;
use std::sync::OnceLock;

use axum::http::StatusCode;
use charter_core::{Rid, ZERO};
use charter_sync::gitsig::{CLAIM_NAMESPACE, CLAIM_TYPE, claim_message};
use charter_sync::ids::hex32;
use charter_ui::sync_loop;
use common::*;
use serde_json::json;

struct Fx {
    net: Net,
    alice: Peer,
    rid: Rid,
    hex: String,
    /// Signed with alice's claimed key, with an unclaimed key, and unsigned.
    claimed: String,
    unclaimed: String,
    plain: String,
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
    let plain = commit(&work, &[("README.md", b"# Parity\n")], "feat: plain");
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
    let r = sync_loop::round(&net, &alice.cfg, &rid)
        .await
        .expect("round");
    assert!(matches!(r, sync_loop::Outcome::Synced(_)), "{r:?}");
    Fx {
        hex: hex32(&rid),
        net,
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
