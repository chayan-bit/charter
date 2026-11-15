use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use charter_core::{CobOpBody, Position, ZERO};
use serde_json::json;

use super::*;
use crate::cob::json::to_cbor;

fn run(dir: &Path, prog: &str, args: &[&str], stdin: &[u8]) -> Vec<u8> {
    let mut c = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    c.stdin
        .take()
        .expect("stdin")
        .write_all(stdin)
        .expect("write");
    let out = c.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "{prog} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// A fresh key `name` in `dir` (`ed25519` or `rsa`); returns its private key path.
fn keygen(dir: &Path, name: &str, kind: &str) -> String {
    let p = dir.join(name).to_string_lossy().into_owned();
    run(
        dir,
        "ssh-keygen",
        &["-q", "-t", kind, "-N", "", "-C", "t", "-f", &p],
        b"",
    );
    p
}

fn pubkey(key: &str) -> String {
    std::fs::read_to_string(format!("{key}.pub"))
        .expect("pub")
        .trim()
        .to_owned()
}

fn sign(dir: &Path, key: &str, ns: &str, msg: &[u8]) -> String {
    let out = run(
        dir,
        "ssh-keygen",
        &["-q", "-Y", "sign", "-f", key, "-n", ns],
        msg,
    );
    String::from_utf8(out).expect("utf8")
}

/// A commit in a new repo under `dir`, SSH-signed with `key` when given; its raw object.
fn commit(dir: &Path, key: Option<&str>) -> Vec<u8> {
    let w = dir.join(format!("w{}", key.map_or(0, str::len)));
    std::fs::create_dir_all(&w).expect("mkdir");
    let g = |args: &[&str]| run(&w, "git", args, b"");
    g(&["init", "-q"]);
    let mut c = vec![
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.invalid",
        "-c",
        "gpg.format=ssh",
    ];
    let sk = key.map(|k| format!("user.signingkey={k}"));
    if let Some(s) = &sk {
        c.extend(["-c", s, "commit", "-S"]);
    } else {
        c.extend(["-c", "commit.gpgsign=false", "commit"]);
    }
    c.extend(["-q", "--allow-empty", "-m", "feat: signed"]);
    g(&c);
    g(&["cat-file", "commit", "HEAD"])
}

fn op(
    n: u8,
    author: Nid,
    action: &str,
    object: [u8; 32],
    payload: serde_json::Value,
) -> VerifiedOp {
    VerifiedOp {
        id: [n; 32],
        author,
        body: CobOpBody {
            rid: [1; 32],
            type_name: CLAIM_TYPE.into(),
            object,
            parents: vec![],
            action: action.into(),
            payload: to_cbor(&payload).expect("cbor"),
            ts_ms: 0,
        },
        arkiv_order: Position {
            block: u64::from(n),
            tx: 0,
            key: [n; 32],
        },
    }
}

fn claim(dir: &Path, n: u8, author: Nid, key: &str, signed_for: Nid) -> VerifiedOp {
    let sig = sign(
        dir,
        key,
        CLAIM_NAMESPACE,
        claim_message(&signed_for).as_bytes(),
    );
    op(
        n,
        author,
        "create",
        ZERO,
        json!({"key": pubkey(key), "sig": sig}),
    )
}

#[test]
fn an_ssh_signed_commit_verifies_and_names_the_claimer() {
    let d = tempfile::tempdir().expect("tmp");
    let key = keygen(d.path(), "k", "ed25519");
    let raw = commit(d.path(), Some(&key));
    let k = SshKey::parse(&pubkey(&key)).expect("key");
    let none = commit_sig(&raw, &Claims::default());
    assert_eq!(
        none,
        CommitSig::Good {
            fingerprint: k.fingerprint(),
            signer: None
        }
    );
    let alice = [7; 32];
    let claims = claims(&[claim(d.path(), 1, alice, &key, alice)]);
    assert_eq!(claims.of(&alice), vec![k.fingerprint()]);
    assert!(
        matches!(commit_sig(&raw, &claims), CommitSig::Good { signer: Some(n), .. } if n == alice)
    );
    // The fingerprint is ssh-keygen's.
    let fp = String::from_utf8(run(
        d.path(),
        "ssh-keygen",
        &["-l", "-f", &format!("{key}.pub")],
        b"",
    ))
    .expect("utf8");
    assert!(fp.contains(&k.fingerprint()), "{fp}");
}

#[test]
fn a_tampered_commit_is_bad_and_other_signatures_are_unchecked() {
    let d = tempfile::tempdir().expect("tmp");
    let key = keygen(d.path(), "k", "ed25519");
    let raw = commit(d.path(), Some(&key));
    let text = String::from_utf8(raw)
        .expect("utf8")
        .replace("feat: signed", "feat: forged");
    assert_eq!(
        commit_sig(text.as_bytes(), &Claims::default()),
        CommitSig::Bad
    );
    assert_eq!(
        commit_sig(&commit(d.path(), None), &Claims::default()),
        CommitSig::Unsigned
    );
    let rsa = keygen(d.path(), "r", "rsa");
    let raw = commit(d.path(), Some(&rsa));
    assert_eq!(
        commit_sig(&raw, &Claims::default()),
        CommitSig::Unchecked("SSH (not Ed25519)")
    );
    let pgp = b"tree 1\ngpgsig -----BEGIN PGP SIGNATURE-----\n \n abc\n -----END PGP SIGNATURE-----\n\nmsg\n";
    assert_eq!(
        commit_sig(pgp, &Claims::default()),
        CommitSig::Unchecked("OpenPGP")
    );
    let (payload, sig) = split_commit(pgp).expect("signed");
    assert_eq!(payload, b"tree 1\n\nmsg\n");
    assert_eq!(
        sig,
        "-----BEGIN PGP SIGNATURE-----\n\nabc\n-----END PGP SIGNATURE-----\n"
    );
}

#[test]
fn claims_need_proof_first_wins_and_only_the_claimer_revokes() {
    let d = tempfile::tempdir().expect("tmp");
    let (key, other) = (
        keygen(d.path(), "k", "ed25519"),
        keygen(d.path(), "o", "ed25519"),
    );
    let (alice, mallory) = ([7; 32], [9; 32]);
    let k = SshKey::parse(&pubkey(&key)).expect("key");
    // Mallory replays Alice's proof, and claims a key whose proof names Alice.
    let replay = claim(d.path(), 1, mallory, &key, alice);
    assert_eq!(claims(&[replay]).signer(&k), None);
    // A proof by another key than the one claimed.
    let sig = sign(
        d.path(),
        &other,
        CLAIM_NAMESPACE,
        claim_message(&mallory).as_bytes(),
    );
    let wrong = op(
        2,
        mallory,
        "create",
        ZERO,
        json!({"key": pubkey(&key), "sig": sig}),
    );
    assert_eq!(claims(&[wrong]).signer(&k), None);
    // Another namespace.
    let sig = sign(d.path(), &key, "git", claim_message(&alice).as_bytes());
    let ns = op(
        3,
        alice,
        "create",
        ZERO,
        json!({"key": pubkey(&key), "sig": sig}),
    );
    assert_eq!(claims(&[ns]).signer(&k), None);
    let first = claim(d.path(), 4, alice, &key, alice);
    let id = first.id;
    let later = claim(d.path(), 5, mallory, &key, mallory);
    assert_eq!(
        claims(&[first.clone(), later.clone()]).signer(&k),
        Some(alice)
    );
    let foreign = op(6, mallory, "revoke", id, json!({}));
    assert_eq!(claims(&[first.clone(), foreign]).signer(&k), Some(alice));
    let revoke = op(7, alice, "revoke", id, json!({}));
    assert_eq!(
        claims(&[first.clone(), revoke.clone(), later.clone()]).signer(&k),
        Some(mallory),
        "free again"
    );
    let stale = op(8, alice, "revoke", id, json!({}));
    assert_eq!(
        claims(&[first, revoke, later, stale]).signer(&k),
        Some(mallory),
        "a second revoke of the dropped claim leaves the later claim alone"
    );
}

#[test]
fn malformed_input_is_refused() {
    assert_eq!(SshKey::parse("ssh-rsa AAAA"), None);
    assert_eq!(SshKey::parse("ssh-ed25519 !!!"), None);
    assert_eq!(
        SshKey::parse(&format!("ssh-ed25519 {}", "A".repeat(2000))),
        None
    );
    let armor =
        |b: &str| format!("-----BEGIN SSH SIGNATURE-----\n{b}\n-----END SSH SIGNATURE-----");
    assert_eq!(
        verify_sshsig(&armor("U1NIU0lH"), b"git", b"m"),
        Err(SigError::Bad)
    );
    assert_eq!(
        verify_sshsig(&armor("@@"), b"git", b"m"),
        Err(SigError::Bad)
    );
    assert_eq!(
        verify_sshsig("nope", b"git", b"m"),
        Err(SigError::Unsupported)
    );
    assert_eq!(
        verify_sshsig(&armor(&"A".repeat(5000)), b"git", b"m"),
        Err(SigError::Bad),
        "an oversized SSH signature"
    );
    assert_eq!(
        verify_sshsig(&"x".repeat(5000), b"git", b"m"),
        Err(SigError::Unsupported)
    );
}

#[test]
fn a_large_foreign_signature_is_unchecked_not_bad() {
    for (begin, end, kind) in [
        ("BEGIN PGP SIGNATURE", "END PGP SIGNATURE", "OpenPGP"),
        ("BEGIN SIGNED MESSAGE", "END SIGNED MESSAGE", "X.509"),
    ] {
        let mut raw = format!("tree 1\ngpgsig -----{begin}-----\n ").into_bytes();
        raw.extend_from_slice("A".repeat(5000).as_bytes());
        raw.extend_from_slice(format!("\n -----{end}-----\n\nmsg\n").as_bytes());
        assert_eq!(
            commit_sig(&raw, &Claims::default()),
            CommitSig::Unchecked(kind)
        );
    }
}

#[test]
fn both_signature_headers_are_left_out_of_the_payload() {
    let raw = b"tree 1\ngpgsig -----BEGIN PGP SIGNATURE-----\n abc\n -----END PGP SIGNATURE-----\ngpgsig-sha256 -----BEGIN PGP SIGNATURE-----\n def\n -----END PGP SIGNATURE-----\nauthor t\n\nmsg\n";
    let (payload, sig) = split_commit(raw).expect("signed");
    assert_eq!(payload, b"tree 1\nauthor t\n\nmsg\n");
    assert_eq!(
        sig,
        "-----BEGIN PGP SIGNATURE-----\nabc\n-----END PGP SIGNATURE-----\n"
    );
}
