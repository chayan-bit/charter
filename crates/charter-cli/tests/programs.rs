//! Repo programs end to end (docs/p3/programs.md, "Enforcement"): a pinned program is
//! enforced by the remote helper on push and by every client on clone, over MemArkiv.

mod common;

use std::path::Path;

use charter_cli::backend::MEM_CHAIN_ID;
use charter_store::fake::FileMemArkiv;
use charter_sync::git::Git;
use charter_sync::ids::{parse_id, parse_url};
use charter_sync::{Config, HeadPush, Keys, Repo, Update, Want, sync};
use common::e2e::*;
use common::programs::*;

fn clone_head(u: &User, root: &Path, url: &str, dir: &str) -> String {
    let dest = root.join(dir);
    u.git(root, &["clone", "-q", url, &dest.to_string_lossy()]);
    u.git(&dest, &["rev-parse", "HEAD"])
}

/// A modified client: pushes `main` through the sync library directly, without the
/// helper's program check, so the signed head lands on Arkiv anyway.
fn forced_push(u: &User, url: &str, work: &Path) {
    let home = u.home.join(".charter");
    let rid = parse_url(url.trim_start_matches("arkiv::"))
        .expect("url")
        .rid;
    charter_cli::block_on(async {
        let mem = FileMemArkiv::open(&u.log, MEM_CHAIN_ID).expect("open");
        let cfg = Config::for_fake(home.clone());
        let keys = Keys::require(&home).expect("keys");
        let repo = Repo::open(&home, &rid).expect("repo");
        let want = Want {
            must: [keys.nid()].into(),
            ..Want::default()
        };
        let view = sync(&mem, &repo, &want).await.expect("sync");
        let main = "refs/heads/main".to_owned();
        let up = [Update {
            src: Some(main.clone()),
            dst: main,
        }];
        let p = HeadPush::prepare(&mem, &repo, &cfg, &keys, view, &Git::work(Some(work)), &up);
        Box::pin(p.expect("prepare").run())
            .await
            .expect("forced push");
    })
    .expect("runtime");
}

#[test]
fn branch_protection_is_enforced_on_push_and_clone() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let (alice, bob) = (user(root, "alice", &log), user(root, "bob", &log));
    let (url, work, c0) = repo_with_main(&alice, root, "guarded");
    let pinned = alice.arkiv(&[
        "program",
        "pin",
        "--repo",
        &url,
        "--file",
        &program("branch-protection"),
    ]);
    assert!(pinned.contains("published program source"), "{pinned}");
    assert!(pinned.contains("program:        rhai/1"), "{pinned}");

    // A direct push to main is refused locally and writes nothing.
    let c1 = commit(&alice, &work, "v1\n");
    let direct = alice.run("git", &work, &["push", &url, "main"]);
    let err = String::from_utf8_lossy(&direct.stderr);
    assert!(!direct.status.success());
    assert!(
        err.contains("E_PROGRAM_REJECTED: main needs a patch"),
        "{err}"
    );
    let status = alice.arkiv(&["status", &url]);
    assert!(
        status.contains("delegate: seq 0"),
        "nothing anchored: {status}"
    );

    // A patch at c1 with a delegate approval lets the same push through.
    let patch = post(
        &bob,
        &url,
        "xyz.arkiv.patch",
        "create",
        None,
        &format!(r#"{{"title":"v1","head":"{c1}"}}"#),
    );
    let review = post(
        &alice,
        &url,
        "xyz.arkiv.review",
        "create",
        None,
        &format!(r#"{{"patch":"{patch}"}}"#),
    );
    let approve = format!(r#"{{"patch":"{patch}","head":"{c1}"}}"#);
    post(
        &alice,
        &url,
        "xyz.arkiv.review",
        "approve",
        Some(&review),
        &approve,
    );
    alice.git(&work, &["push", "-q", &url, "main"]);
    let carol = User::new(root, "carol", &log);
    assert_eq!(clone_head(&carol, root, &url, "carol"), c1);
    assert_ne!(c0, c1);

    // A forced push from a modified client lands on Arkiv but every client ignores it.
    let c2 = commit(&alice, &work, "v2\n");
    forced_push(&alice, &url, &work);
    let st = alice.arkiv(&["status", &url]);
    assert!(st.contains("delegate: seq 2"), "{st}");
    let dave = User::new(root, "dave", &log);
    assert_eq!(clone_head(&dave, root, &url, "dave"), c1, "not {c2}");
    let report = dave.arkiv(&["program", "status", "--repo", &url]);
    assert!(
        report.contains(&format!("rejected: refs/heads/main {c1}..{c2}")),
        "{report}"
    );
    assert!(report.contains("main needs a patch"), "{report}");
    assert!(
        report.contains(&format!("head: refs/heads/main {c1}")),
        "{report}"
    );

    // The rejected delegate is not stuck: the signed base of c3 is the accepted head c1
    // (an ancestor), so an approved descendant of c1 (built on the rejected c2) is accepted.
    let c3 = commit(&alice, &work, "v3\n");
    let patch3 = post(
        &bob,
        &url,
        "xyz.arkiv.patch",
        "create",
        None,
        &format!(r#"{{"title":"v3","head":"{c3}"}}"#),
    );
    let approve3 = format!(r#"{{"patch":"{patch3}","head":"{c3}"}}"#);
    post(
        &alice,
        &url,
        "xyz.arkiv.review",
        "approve",
        Some(&review),
        &approve3,
    );
    alice.git(&work, &["push", "-q", &url, "main"]);
    let erin = User::new(root, "erin", &log);
    assert_eq!(clone_head(&erin, root, &url, "erin"), c3);
}

#[test]
fn bounty_obligation_appears_after_the_merge() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let (alice, bob) = (user(root, "alice", &log), user(root, "bob", &log));
    let (url, work, _) = repo_with_main(&alice, root, "bounties");
    alice.arkiv(&[
        "program",
        "pin",
        "--repo",
        &url,
        "--file",
        &program("bounty"),
    ]);
    let before = alice.arkiv(&["program", "status", "--repo", &url]);
    assert!(!before.contains("obligation:"), "{before}");

    let issue = post(
        &alice,
        &url,
        "xyz.arkiv.issue",
        "create",
        None,
        r#"{"title":"crash","bounty":100}"#,
    );
    let c1 = commit(&bob, &work, "fix\n");
    let head = format!(r#"{{"head":"{c1}"}}"#);
    let patch = post(&bob, &url, "xyz.arkiv.patch", "create", None, &head);
    let merged = format!(r#"{{"patch":"{patch}","closes":"{issue}"}}"#);
    post(
        &alice,
        &url,
        "xyz.arkiv.patch",
        "merged",
        Some(&patch),
        &merged,
    );
    let pending = alice.arkiv(&["program", "status", "--repo", &url]);
    assert!(
        !pending.contains("obligation:"),
        "owed only once main moves: {pending}"
    );

    alice.git(&work, &["push", "-q", &url, "main"]);
    let after = alice.arkiv(&["program", "status", "--repo", &url]);
    let bob_nid = field(&bob.arkiv(&["keys", "show"]), "nid:").to_owned();
    let line = after
        .lines()
        .find(|l| l.starts_with("obligation:"))
        .unwrap_or_else(|| panic!("no obligation in {after}"));
    let ob: serde_json::Value =
        serde_json::from_str(line.trim_start_matches("obligation:").trim()).expect("json");
    assert_eq!(ob["kind"], "pay");
    assert_eq!(ob["amount"], 100);
    let bob_hex = hex::encode(parse_id(&bob_nid).expect("nid"));
    assert_eq!(ob["to"], bob_hex.as_str(), "paid to the patch author");

    // The escrow hook sees the same obligation.
    let rid = parse_url(url.trim_start_matches("arkiv::"))
        .expect("url")
        .rid;
    let obligations = charter_cli::block_on(async {
        let mem = FileMemArkiv::open(&log, MEM_CHAIN_ID).expect("open");
        let repo = Repo::open(&root.join("escrow"), &rid).expect("repo");
        charter_sync::program::obligations_in(&mem, &repo).await
    })
    .expect("runtime")
    .expect("obligations");
    assert_eq!(obligations, vec![ob]);
}

#[test]
fn a_pin_without_published_source_fails_closed() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let alice = user(root, "alice", &log);
    let (url, _, _) = repo_with_main(&alice, root, "unpublished");
    // Pin by identity revision only, skipping the publish op `program pin` posts first.
    let home = alice.home.join(".charter");
    let rid = parse_url(url.trim_start_matches("arkiv::"))
        .expect("url")
        .rid;
    let (pin, _) = charter_cli::program::pin_for(Path::new(&program("bounty"))).expect("pin");
    charter_cli::block_on(async {
        let mem = FileMemArkiv::open(&log, MEM_CHAIN_ID).expect("open");
        let (cfg, keys) = (
            Config::for_fake(home.clone()),
            Keys::require(&home).expect("keys"),
        );
        let st = charter_sync::state::State::default();
        let edit = |b: &charter_core::IdBody| charter_core::IdBody {
            program: Some(pin),
            ..b.clone()
        };
        charter_sync::identity::propose(&mem, &cfg, &keys, &rid, &st, edit).await
    })
    .expect("runtime")
    .expect("propose");
    let dest = root.join("clone");
    let out = alice.run("git", root, &["clone", "-q", &url, &dest.to_string_lossy()]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success());
    assert!(err.contains("E_PROGRAM_MISSING"), "{err}");
}
