//! Parity batch 3 through the real binaries on the fake chain: signing-key claims and
//! signed-commit checks (#61), labels (#40), code owners (#53), archive, rename and
//! transfer (#63), sealed in private repos.

mod common;

use std::path::Path;

use charter_cli::backend::MEM_CHAIN_ID;
use charter_core::{AttrValue, Kind};
use charter_store::fake::FileMemArkiv;
use charter_sync::ids::parse_url;
use charter_sync::observe;
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

/// An approval of `patch` at `head` by `u`.
fn approve(u: &User, url: &str, patch: &str, head: &str) {
    let review = post(
        u,
        url,
        "xyz.arkiv.review",
        "create",
        None,
        &format!(r#"{{"patch":"{patch}"}}"#),
    );
    let body = format!(r#"{{"patch":"{patch}","head":"{head}"}}"#);
    post(u, url, "xyz.arkiv.review", "approve", Some(&review), &body);
}

#[test]
fn the_codeowners_program_needs_an_owner_approval_per_changed_path() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let (alice, bob) = (user(root, "alice", &log), user(root, "bob", &log));
    let (url, work, _) = repo_with_main(&alice, root, "owned");
    let bob_nid = field(&bob.arkiv(&["keys", "show"]), "nid:").to_owned();
    write(&work, "CODEOWNERS", format!("*.rs {bob_nid}\n").as_bytes());
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-qm", "chore: owners"]);
    alice.git(&work, &["push", "-q", &url, "main"]);
    let file = program("codeowners");
    let pinned = alice.arkiv(&["program", "pin", "--repo", &url, "--file", &file]);
    assert!(pinned.contains("program:        rhai/2"), "{pinned}");

    write(&work, "lib.rs", b"fn f() {}\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-qm", "feat: lib"]);
    let c1 = alice.git(&work, &["rev-parse", "HEAD"]);
    let patch = post(
        &alice,
        &url,
        "xyz.arkiv.patch",
        "create",
        None,
        &format!(r#"{{"title":"lib","head":"{c1}"}}"#),
    );
    approve(&alice, &url, &patch, &c1);
    let refused = alice.run("git", &work, &["push", &url, "main"]);
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(
        err.contains("E_PROGRAM_REJECTED: lib.rs needs an approval from one of its owners"),
        "a delegate is not an owner: {err}"
    );
    approve(&bob, &url, &patch, &c1);
    alice.git(&work, &["push", "-q", &url, "main"]);
    let carol = User::new(root, "carol", &log);
    let dest = root.join("carol");
    carol.git(root, &["clone", "-q", &url, &dest.to_string_lossy()]);
    assert_eq!(carol.git(&dest, &["rev-parse", "HEAD"]), c1);
}

#[test]
fn archive_rename_and_transfer_are_delegate_revisions() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let (alice, bob) = (user(root, "alice", &log), user(root, "bob", &log));
    let (url, work, _) = repo_with_main(&alice, root, "keep");

    let shown = alice.arkiv(&["id", "propose", &url, "--archive"]);
    assert!(shown.contains("archived:       yes"), "{shown}");
    let c1 = commit(&alice, &work, "v1\n");
    let refused = alice.run("git", &work, &["push", &url, "main"]);
    let err = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success() && err.contains("E_ARCHIVED"),
        "{err}"
    );
    let status = alice.arkiv(&["status", &url]);
    assert!(!status.contains(&c1[..7]), "nothing pushed: {status}");
    let shown = alice.arkiv(&["id", "propose", &url, "--unarchive"]);
    assert!(!shown.contains("archived:"), "{shown}");
    alice.git(&work, &["push", "-q", &url, "main"]);

    let bob_nid = field(&bob.arkiv(&["keys", "show"]), "nid:").to_owned();
    let alice_nid = field(&alice.arkiv(&["keys", "show"]), "nid:").to_owned();
    let hand_over = |extra: &[&str]| {
        let mut args = vec![
            "id",
            "propose",
            &url,
            "--name",
            "kept",
            "--add-delegate",
            &bob_nid,
            "--remove-delegate",
            &alice_nid,
        ];
        args.extend(extra);
        alice.run(env!("CARGO_BIN_EXE_chart"), &alice.home, &args)
    };
    // Bob never wrote, so his id may be a typo: refused before anything is signed.
    let unknown = hand_over(&["--confirm", "keep"]);
    let err = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        !unknown.status.success() && err.contains("never written to Arkiv"),
        "{err}"
    );
    post(
        &bob,
        &url,
        "xyz.arkiv.issue",
        "create",
        None,
        r#"{"title":"hi"}"#,
    );
    // Leaving the delegate seat is typed out.
    let unconfirmed = hand_over(&[]);
    let err = String::from_utf8_lossy(&unconfirmed.stderr);
    assert!(
        !unconfirmed.status.success() && err.contains("--confirm keep"),
        "{err}"
    );
    let moved = hand_over(&["--confirm", "keep"]);
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    let moved = String::from_utf8_lossy(&moved.stdout).into_owned();
    assert!(moved.contains("name:           kept"), "{moved}");
    assert!(
        moved.contains(&format!("delegates:      {bob_nid}")),
        "{moved}"
    );
    // Alice is no delegate now: her revision never takes effect; bob's does.
    let late = alice.arkiv(&["id", "propose", &url, "--archive"]);
    assert!(!late.contains("archived:"), "{late}");
    let bobs = bob.arkiv(&["id", "propose", &url, "--archive"]);
    assert!(bobs.contains("archived:       yes"), "{bobs}");
}

/// Every row on the chain as `(kind, attributes)`.
fn rows(log: &Path) -> Vec<(Kind, Vec<charter_core::Attribute>)> {
    let kinds = [
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
    charter_cli::block_on(async {
        let mem = FileMemArkiv::open(log, MEM_CHAIN_ID).expect("open");
        let mut out = vec![];
        for k in kinds {
            for o in observe::all(&mem, observe::kind(k)).await.expect("query") {
                out.push((k, o.attributes));
            }
        }
        out
    })
    .expect("runtime")
}

#[test]
fn new_ops_and_the_archive_flag_stay_sealed_in_a_private_repo() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let (alice, bob) = (user(root, "alice", &log), user(root, "bob", &log));
    let url = field(
        &alice.arkiv(&["init", "--private", "--name", "vault"]),
        "url:",
    )
    .to_owned();
    let rid = parse_url(url.trim_start_matches("arkiv::"))
        .expect("url")
        .rid;
    let kp = field(&bob.arkiv(&["member", "keypackage"]), "key package:").to_owned();
    let add = [
        "member",
        "add",
        "--repo",
        &url,
        "--key-package",
        &kp,
        "--role",
        "write",
    ];
    alice.arkiv(&add);
    let work = root.join("vault-work");
    alice.git(root, &["init", "-q", &work.to_string_lossy()]);
    let key = ssh_key(root, "alice_ed25519");
    let c = signed_commit(&alice, &work, &key, "feat: signed");
    alice.git(&work, &["push", "-q", &url, "main"]);
    alice.arkiv(&["signing-key", "claim", "--repo", &url, "--key", &key]);
    let issue = post(
        &alice,
        &url,
        "xyz.arkiv.issue",
        "create",
        None,
        r#"{"title":"t"}"#,
    );
    post(
        &alice,
        &url,
        "xyz.arkiv.issue",
        "label",
        Some(&issue),
        r#"{"labels":["secret-label"]}"#,
    );
    let shown = alice.arkiv(&["id", "propose", &url, "--archive"]);
    assert!(shown.contains("archived:       yes"), "{shown}");

    let nid = field(&alice.arkiv(&["keys", "show"]), "nid:").to_owned();
    let dest = root.join("bob-clone");
    bob.git(root, &["clone", "-q", &url, &dest.to_string_lossy()]);
    let seen = bob.arkiv(&["signing-key", "verify", "--repo", &url, &c]);
    assert!(
        seen.starts_with(&format!("verified: signed by {nid}")),
        "{seen}"
    );
    assert!(
        bob.arkiv(&["id", "show", &url])
            .contains("archived:       yes")
    );
    write(&dest, "B", b"b\n");
    bob.git(&dest, &["add", "-A"]);
    bob.git(&dest, &["commit", "-qm", "feat: b"]);
    let refused = bob.run("git", &dest, &["push", &url, "main"]);
    assert!(String::from_utf8_lossy(&refused.stderr).contains("E_ARCHIVED"));

    let (rid_hex, all) = (hex::encode(rid), rows(&log));
    for (k, attrs) in &all {
        for a in attrs {
            let leak = match &a.value {
                AttrValue::Bytes32(b) => *b == rid,
                AttrValue::Bytes(b) => {
                    b.windows(32).any(|w| w == rid) || b.windows(12).any(|w| w == b"secret-label")
                }
                AttrValue::Str(s) => s.contains(&rid_hex) || s.contains("secret-label"),
                AttrValue::U64(v) => a.name == "ag.v" && *k == Kind::Id && *v == 2,
            };
            assert!(!leak, "{} {} is in plaintext", k.name(), a.name);
        }
    }
}
