//! End to end through the real binaries: stock `git push arkiv::<rid>` and
//! `git clone arkiv::<rid>` against the file-persisted MemArkiv backend.

mod common;

use common::e2e::*;

#[test]
fn git_push_and_clone_through_the_remote_helper() {
    let tmp = tempfile::tempdir().expect("tmp");
    let log = tmp.path().join("chain.log");
    let (alice, bob) = (
        User::new(tmp.path(), "alice", &log),
        User::new(tmp.path(), "bob", &log),
    );
    let keys = alice.arkiv(&["keys", "init"]);
    fund(&log, field(&keys, "wallet:"));
    let created = alice.arkiv(&[
        "init",
        "--name",
        "demo",
        "--lang",
        "rust",
        "--license",
        "MIT",
    ]);
    let url = field(&created, "url:").to_owned();
    assert!(url.starts_with("arkiv::z"));

    let work = tmp.path().join("work");
    alice.git(tmp.path(), &["init", "-q", &work.to_string_lossy()]);
    write(&work, "README", b"hello arkiv\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: first"]);
    alice.git(&work, &["push", "-q", &url, "main"]);
    std::fs::create_dir_all(work.join("src")).expect("mkdir");
    write(
        &work,
        "src/lib.rs",
        &(0..50_000u32)
            .flat_map(|i| i.to_le_bytes())
            .collect::<Vec<_>>(),
    );
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: second"]);
    alice.git(&work, &["tag", "v1"]);
    alice.git(&work, &["push", "-q", &url, "main", "refs/tags/v1"]);

    let dest = tmp.path().join("clone");
    bob.git(tmp.path(), &["clone", "-q", &url, &dest.to_string_lossy()]);
    assert_eq!(
        bob.git(&dest, &["rev-parse", "HEAD"]),
        alice.git(&work, &["rev-parse", "HEAD"])
    );
    assert_eq!(
        bob.git(&dest, &["ls-tree", "-r", "HEAD"]),
        alice.git(&work, &["ls-tree", "-r", "HEAD"])
    );
    assert_eq!(
        bob.git(&dest, &["rev-parse", "v1"]),
        alice.git(&work, &["rev-parse", "v1"])
    );
    assert_eq!(bob.git(&dest, &["branch", "--show-current"]), "main");
    bob.git(&dest, &["fsck", "--full", "--strict"]);
    let packs = std::fs::read_dir(dest.join(".git/objects/pack")).expect("packs");
    assert!(
        packs
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".keep")),
        "no stale .keep"
    );

    write(&work, "README", b"hello again\n");
    alice.git(&work, &["commit", "-qam", "fix: readme"]);
    alice.git(&work, &["push", "-q", &url, "main"]);
    bob.git(&dest, &["pull", "-q", "--ff-only"]);
    assert_eq!(
        std::fs::read(dest.join("README")).expect("read"),
        b"hello again\n"
    );

    let status = alice.arkiv(&["status", &url]);
    assert!(status.contains("delegate: seq 2"), "{status}");
    assert!(alice.arkiv(&["search", "--prefix", "de"]).contains(&url));
    assert!(
        alice
            .arkiv(&["id", "show", &url])
            .contains("name:           demo")
    );
    assert!(
        alice
            .arkiv(&["keepalive", &url, "--dry-run"])
            .contains("would extend")
    );
    assert!(
        alice
            .arkiv(&["fund", "--bytes", "1000000"])
            .contains("estimated need")
    );
}

/// A non-delegate's push lands in its own namespace and is fetched through its peer url
/// `arkiv::<rid>/<nid>`; a delegate merges it and a fresh clone gets the merge.
#[test]
fn a_contributor_push_is_fetched_through_its_peer_url() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let [alice, bob, carol] = ["alice", "bob", "carol"].map(|n| User::new(root, n, &log));
    fund(&log, field(&alice.arkiv(&["keys", "init"]), "wallet:"));
    let url = field(&alice.arkiv(&["init", "--name", "contrib"]), "url:").to_owned();
    let work = root.join("work");
    alice.git(root, &["init", "-q", &work.to_string_lossy()]);
    write(&work, "README", b"v0\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: v0"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    let bob_keys = bob.arkiv(&["keys", "init"]);
    fund(&log, field(&bob_keys, "wallet:"));
    let bob_url = format!("{url}/{}", field(&bob_keys, "nid:"));
    let bw = root.join("bob-clone");
    bob.git(root, &["clone", "-q", &url, &bw.to_string_lossy()]);
    bob.git(&bw, &["checkout", "-q", "-b", "fix"]);
    write(&bw, "FIX", b"fix\n");
    bob.git(&bw, &["add", "-A"]);
    bob.git(&bw, &["commit", "-q", "-m", "fix: contributor"]);
    bob.git(&bw, &["push", "-q", "origin", "fix"]);
    let fix = bob.git(&bw, &["rev-parse", "HEAD"]);

    alice.git(&work, &["fetch", "-q", &bob_url, "refs/heads/fix"]);
    assert_eq!(alice.git(&work, &["rev-parse", "FETCH_HEAD"]), fix);
    alice.git(
        &work,
        &["merge", "-q", "--no-ff", "-m", "merge: fix", "FETCH_HEAD"],
    );
    alice.git(&work, &["push", "-q", &url, "main"]);
    let merge = alice.git(&work, &["rev-parse", "HEAD"]);

    let cw = root.join("carol-clone");
    carol.git(root, &["clone", "-q", &url, &cw.to_string_lossy()]);
    assert_eq!(carol.git(&cw, &["rev-parse", "HEAD"]), merge);
    carol.git(&cw, &["merge-base", "--is-ancestor", &fix, "HEAD"]);
    carol.git(&cw, &["fsck", "--full", "--strict"]);
}

#[test]
fn errors_map_to_git_and_exit_codes() {
    let tmp = tempfile::tempdir().expect("tmp");
    let log = tmp.path().join("chain.log");
    let alice = User::new(tmp.path(), "alice", &log);
    let arkiv = env!("CARGO_BIN_EXE_chart");
    let unknown = format!("arkiv::z{}", "1".repeat(32));
    let clone = alice.run("git", tmp.path(), &["clone", "-q", &unknown, "x"]);
    let err = String::from_utf8_lossy(&clone.stderr);
    assert!(!clone.status.success());
    assert!(err.contains("fatal: chart E_ID_NOT_LIVE"), "{err}");

    assert_eq!(
        alice.run(arkiv, tmp.path(), &["search"]).status.code(),
        Some(2)
    );
    assert_eq!(
        alice.run(arkiv, tmp.path(), &["bogus"]).status.code(),
        Some(2)
    );
    alice.arkiv(&["keys", "init"]);
    let poor = alice.run(arkiv, tmp.path(), &["init", "--name", "poor"]);
    assert_eq!(
        poor.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&poor.stderr)
    );
    assert!(String::from_utf8_lossy(&poor.stderr).contains("E_INSUFFICIENT_FUNDS"));

    fund(&log, field(&alice.arkiv(&["keys", "show"]), "wallet:"));
    let url = field(&alice.arkiv(&["init", "--name", "rich"]), "url:").to_owned();
    let work = tmp.path().join("w");
    alice.git(tmp.path(), &["init", "-q", &work.to_string_lossy()]);
    alice.git(&work, &["commit", "-q", "--allow-empty", "-m", "feat: x"]);
    let bad = alice.run("git", &work, &["push", &url, "main:refs/weird/x"]);
    assert!(!bad.status.success());
    assert!(
        String::from_utf8_lossy(&bad.stderr).contains("E_BAD_REFS"),
        "{}",
        String::from_utf8_lossy(&bad.stderr)
    );
    let empty = tmp.path().join("empty");
    let cloned = alice.run(
        "git",
        tmp.path(),
        &["clone", "-q", &url, &empty.to_string_lossy()],
    );
    assert!(
        cloned.status.success(),
        "an empty repo clones: {}",
        String::from_utf8_lossy(&cloned.stderr)
    );
}

#[test]
fn ops_post_and_list_through_the_cli() {
    let tmp = tempfile::tempdir().expect("tmp");
    let log = tmp.path().join("chain.log");
    let (alice, bob) = (
        User::new(tmp.path(), "alice", &log),
        User::new(tmp.path(), "bob", &log),
    );
    fund(&log, field(&alice.arkiv(&["keys", "init"]), "wallet:"));
    fund(&log, field(&bob.arkiv(&["keys", "init"]), "wallet:"));
    let url = field(&alice.arkiv(&["init", "--name", "ops"]), "url:").to_owned();
    let post = |u: &User, args: &[&str]| {
        let base = ["op", "post", "--repo", &url, "--type", "xyz.arkiv.issue"];
        u.arkiv(&[&base[..], args].concat()).trim().to_owned()
    };
    let obj = post(
        &alice,
        &["--action", "create", "--payload-json", r#"{"title":"bug"}"#],
    );
    assert_eq!(obj.len(), 64, "{obj}");
    let c1 = post(
        &bob,
        &[
            "--action",
            "comment",
            "--object",
            &obj,
            "--parent",
            &obj,
            "--payload-json",
            r#"{"body":"same","n":[1,-2]}"#,
        ],
    );

    let text = alice.arkiv(&["op", "list", "--repo", &url, "--object", &obj]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with(&obj) && lines[0].contains(r#"{"title":"bug"}"#));
    assert!(lines[1].starts_with(&c1) && lines[1].contains(" comment "));
    let listed = bob.arkiv(&[
        "op",
        "list",
        "--repo",
        &url,
        "--type",
        "xyz.arkiv.issue",
        "--json",
    ]);
    let v: serde_json::Value = serde_json::from_str(&listed).expect("json");
    assert_eq!(v[1]["id"], c1.as_str());
    assert_eq!(v[1]["object"], obj.as_str());
    assert_eq!(v[1]["parents"][0], obj.as_str());
    assert_eq!(
        v[1]["payload"],
        serde_json::json!({"body": "same", "n": [1, -2]})
    );

    let arkiv = env!("CARGO_BIN_EXE_chart");
    let float = alice.run(
        arkiv,
        tmp.path(),
        &[
            "op",
            "post",
            "--repo",
            &url,
            "--type",
            "t",
            "--action",
            "create",
            "--payload-json",
            "[1.5]",
        ],
    );
    assert_eq!(float.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&float.stderr).contains("E_BAD_FORMAT"));
}

#[test]
fn dev_faucet_funds_mem_wallets_only() {
    let tmp = tempfile::tempdir().expect("tmp");
    let log = tmp.path().join("chain.log");
    let alice = User::new(tmp.path(), "alice", &log);
    let wallet = field(&alice.arkiv(&["keys", "init"]), "wallet:").to_owned();
    let funded = alice.arkiv(&["fund", "--dev", "100"]);
    assert!(funded.contains(&wallet), "{funded}");
    assert!(funded.contains("balance: 100 tGLM"), "{funded}");
    assert!(alice.arkiv(&["init", "--name", "faucet"]).contains("url:"));

    let rabdi = alice
        .cmd(env!("CARGO_BIN_EXE_chart"), tmp.path())
        .env("CHARTER_BACKEND", "rabdi")
        .args(["fund", "--dev", "1"])
        .output()
        .expect("spawn");
    assert_eq!(rabdi.status.code(), Some(2));
    let err = String::from_utf8_lossy(&rabdi.stderr);
    assert!(
        err.contains("only works with CHARTER_BACKEND=mem:"),
        "{err}"
    );
}

#[test]
fn private_repo_members_push_and_clone_outsiders_get_not_member() {
    let tmp = tempfile::tempdir().expect("tmp");
    let log = tmp.path().join("chain.log");
    let [alice, bob, carol] = ["alice", "bob", "carol"].map(|n| User::new(tmp.path(), n, &log));
    for u in [&alice, &bob, &carol] {
        fund(&log, field(&u.arkiv(&["keys", "init"]), "wallet:"));
    }
    let url = field(
        &alice.arkiv(&["init", "--private", "--name", "secret"]),
        "url:",
    )
    .to_owned();
    let kp = field(&bob.arkiv(&["member", "keypackage"]), "key package:").to_owned();
    let added = alice.arkiv(&[
        "member",
        "add",
        "--repo",
        &url,
        "--key-package",
        &kp,
        "--role",
        "read",
    ]);
    assert!(added.contains("as Read"), "{added}");
    assert!(
        alice
            .arkiv(&["member", "list", "--repo", &url])
            .contains("Admin")
    );

    let work = tmp.path().join("work");
    alice.git(tmp.path(), &["init", "-q", &work.to_string_lossy()]);
    write(&work, "README", b"private\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: first"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    let dest = tmp.path().join("bob-clone");
    bob.git(tmp.path(), &["clone", "-q", &url, &dest.to_string_lossy()]);
    assert_eq!(
        std::fs::read(dest.join("README")).expect("read"),
        b"private\n"
    );
    write(&dest, "README", b"bob\n");
    bob.git(&dest, &["commit", "-qam", "feat: bob"]);
    let denied = bob.run("git", &dest, &["push", "-q", &url, "main"]);
    assert!(String::from_utf8_lossy(&denied.stderr).contains("E_ROLE_DENIED"));

    let out = carol.run("git", tmp.path(), &["clone", "-q", &url, "carol-clone"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("E_NOT_MEMBER"));
}

#[test]
fn peer_url_fetches_a_non_delegate_peers_refs() {
    // design 4.1: `arkiv::<rid>/<nid>` lists that peer's verified refs, delegate or not.
    let tmp = tempfile::tempdir().expect("tmp");
    let log = tmp.path().join("chain.log");
    let [alice, bob] = ["alice", "bob"].map(|n| User::new(tmp.path(), n, &log));
    let bob_nid = {
        let keys = bob.arkiv(&["keys", "init"]);
        fund(&log, field(&keys, "wallet:"));
        field(&keys, "nid:").to_owned()
    };
    fund(&log, field(&alice.arkiv(&["keys", "init"]), "wallet:"));
    let url = field(&alice.arkiv(&["init", "--name", "peers"]), "url:").to_owned();
    let work = tmp.path().join("work");
    alice.git(tmp.path(), &["init", "-q", &work.to_string_lossy()]);
    write(&work, "README", b"v0\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: v0"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    let dest = tmp.path().join("bob-clone");
    bob.git(tmp.path(), &["clone", "-q", &url, &dest.to_string_lossy()]);
    write(&dest, "README", b"bob\n");
    bob.git(&dest, &["commit", "-qam", "feat: bob"]);
    bob.git(&dest, &["push", "-q", &url, "main"]);
    let bobs = bob.git(&dest, &["rev-parse", "HEAD"]);

    alice.git(&work, &["fetch", "-q", &format!("{url}/{bob_nid}"), "main"]);
    assert_eq!(alice.git(&work, &["rev-parse", "FETCH_HEAD"]), bobs);
}

/// A dry run (and a push the program refuses) stages refs in the cache; the helper drops
/// them again, and says so when it discards a v0.3.0 push left unfinished.
#[test]
fn a_dry_run_push_leaves_no_staging_refs_and_an_old_pending_push_is_reported() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let alice = User::new(root, "alice", &log);
    fund(&log, field(&alice.arkiv(&["keys", "init"]), "wallet:"));
    let url = field(&alice.arkiv(&["init", "--name", "dry"]), "url:").to_owned();
    let work = root.join("work");
    alice.git(root, &["init", "-q", &work.to_string_lossy()]);
    write(&work, "a", b"1\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: a"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    let rid = charter_sync::ids::parse_url(url.trim_start_matches("arkiv::"))
        .expect("url")
        .rid;
    let home = alice.home.join(".charter");
    let (cache, pending) = {
        let repo = charter_sync::Repo::open(&home, &rid).expect("repo");
        (repo.git_dir(), repo.pending_path())
    };
    std::fs::write(&pending, b"{}").expect("old pending push");
    write(&work, "a", b"2\n");
    alice.git(&work, &["commit", "-qam", "feat: b"]);
    let out = alice.run("git", &work, &["push", "--dry-run", &url, "main"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("v0.3.0"), "{err}");
    assert!(!pending.exists(), "the old pending push is discarded");
    let staged = alice.git(
        &cache,
        &["for-each-ref", "--format=%(refname)", "refs/arkiv/staging/"],
    );
    assert_eq!(staged, "", "no staging refs left behind");
}
