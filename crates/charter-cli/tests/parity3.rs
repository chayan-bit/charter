//! Parity batch 3 through the real binaries on the fake chain: signing-key claims and
//! signed-commit checks (#61).

mod common;

use std::path::Path;

use common::e2e::*;
use common::programs::*;

/// A fresh Ed25519 key in `dir`; returns the private key path.
fn ssh_key(dir: &Path, name: &str) -> String {
    let p = dir.join(name).to_string_lossy().into_owned();
    let out = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", name, "-f", &p])
        .output()
        .expect("ssh-keygen");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    p
}

/// A commit SSH-signed with `key` on top of `work`'s HEAD; its id.
fn signed_commit(u: &User, work: &Path, key: &str, msg: &str) -> String {
    write(work, "SIGNED", msg.as_bytes());
    u.git(work, &["add", "-A"]);
    let sk = format!("user.signingkey={key}");
    let args = [
        "-c",
        "gpg.format=ssh",
        "-c",
        &sk,
        "commit",
        "-q",
        "-S",
        "-m",
        msg,
    ];
    u.git(work, &args);
    u.git(work, &["rev-parse", "HEAD"])
}

#[test]
fn a_claimed_ssh_key_shows_who_signed_a_commit() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let (alice, bob) = (user(root, "alice", &log), user(root, "bob", &log));
    let (url, work, _) = repo_with_main(&alice, root, "signed");
    let key = ssh_key(root, "alice_ed25519");
    let c = signed_commit(&alice, &work, &key, "feat: signed");
    alice.git(&work, &["push", "-q", &url, "main"]);

    let before = alice.arkiv(&["signing-key", "verify", "--repo", &url, &c]);
    assert!(
        before.contains("a key no one in this repo claimed"),
        "{before}"
    );
    let id = alice.arkiv(&["signing-key", "claim", "--repo", &url, "--key", &key]);
    assert_eq!(id.trim().len(), 64, "{id}");
    let nid = field(&alice.arkiv(&["keys", "show"]), "nid:").to_owned();

    let dest = root.join("bob-clone");
    bob.git(root, &["clone", "-q", &url, &dest.to_string_lossy()]);
    let seen = bob.arkiv(&["signing-key", "verify", "--repo", &url, &c]);
    assert!(
        seen.starts_with(&format!("verified: signed by {nid}")),
        "{seen}"
    );

    // A later claim of the same key, even with the key in hand, never displaces alice's.
    bob.arkiv(&["signing-key", "claim", "--repo", &url, "--key", &key]);
    let still = bob.arkiv(&["signing-key", "verify", "--repo", &url, &c]);
    assert!(
        still.starts_with(&format!("verified: signed by {nid}")),
        "{still}"
    );
    let bad = bob.run(
        "chart",
        root,
        &["signing-key", "verify", "--repo", &url, "HEAD"],
    );
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("usage:"),
        "refs are refused"
    );
}
