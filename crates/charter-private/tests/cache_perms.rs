//! The decrypted local cache of a private repo is readable by its owner only.

mod common;

use std::os::unix::fs::PermissionsExt;

use charter_core::Rid;
use charter_private::{Keyring, Sealed};
use common::*;

async fn ring(net: &Net, p: &Peer, rid: &Rid) -> Keyring {
    charter_private::ring_for(net, &p.cfg, Some(&p.keys), rid)
        .await
        .expect("ring")
        .expect("private")
}

/// Every entry under `root` that another local user could read, list or enter.
fn open_to_others(root: &std::path::Path) -> Vec<String> {
    let mut out = vec![];
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).expect("dir") {
            let e = e.expect("entry");
            let m = e.metadata().expect("meta");
            if m.is_dir() {
                stack.push(e.path());
            }
            let mode = m.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                let rel = e
                    .path()
                    .strip_prefix(root)
                    .expect("rel")
                    .display()
                    .to_string();
                out.push(format!("{mode:o} {rel}"));
            }
        }
    }
    out.sort();
    out
}

#[test]
fn decrypted_cache_is_private_to_its_owner() {
    run(async {
        let net = Net::new();
        let alice = Peer::new(&net, 1);
        // A home created under a permissive umask is tightened on first use.
        let home = alice.home.path();
        std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"TOPSECRET")], "feat: one");
        let sealed = Sealed::new(&net, ring(&net, &alice, &rid).await);
        push(&sealed, &alice, &rid, &work, &["refs/heads/main"])
            .await
            .expect("push");
        let sealed = Sealed::new(&net, ring(&net, &alice, &rid).await);
        clone_into(&sealed, &alice, &rid, &tmp.path().join("c"))
            .await
            .expect("clone");
        let mode = |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode();
        assert_eq!(mode(home) & 0o777, 0o700, "$CHARTER_HOME");
        let repo = home.join("repos").join(hex::encode(rid));
        assert_eq!(mode(&home.join("repos")) & 0o777, 0o700);
        assert_eq!(mode(&repo) & 0o777, 0o700);
        // Decrypted git objects and packs carry owner-only modes even inside the 0700 dir.
        let data = |r: &String| {
            ["git/objects", " outgoing", " tmp"]
                .iter()
                .any(|d| r.contains(d))
        };
        let open: Vec<_> = open_to_others(&repo).into_iter().filter(data).collect();
        assert!(
            open.is_empty(),
            "group/other bits on decrypted data: {open:?}"
        );
    });
}

fn mode(p: &std::path::Path) -> u32 {
    std::fs::symlink_metadata(p)
        .expect("meta")
        .permissions()
        .mode()
        & 0o777
}

/// A cache from before caches were owner-only: 0755 dirs and 0644 objects, packs and temp
/// files. The first open tightens it once and marks it; later opens never walk it again.
#[test]
fn a_cache_made_before_the_upgrade_is_tightened_once() {
    let home = tempfile::tempdir().expect("home");
    let rid: Rid = [5; 32];
    let dir = home.path().join("repos").join(hex::encode(rid));
    let git = dir.join("git");
    std::fs::create_dir_all(&dir).expect("dir");
    let out = std::process::Command::new("git")
        .args(["init", "--bare", "-q"])
        .arg(&git)
        .output()
        .expect("git init");
    assert!(out.status.success());
    let files = [
        git.join("objects/ab/cdef0123"),
        git.join("objects/pack/pack-1.pack"),
        dir.join("outgoing/v3-1.pack"),
        dir.join("tmp/fetch.pack"),
        dir.join("state.json"),
    ];
    for f in &files {
        std::fs::create_dir_all(f.parent().expect("parent")).expect("mkdir");
        std::fs::write(f, b"decrypted").expect("write");
    }
    let loosen = |p: &std::path::Path, m: u32| {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).expect("chmod")
    };
    for f in &files {
        loosen(f, 0o644);
        loosen(f.parent().expect("parent"), 0o755);
    }

    drop(charter_sync::Repo::open(home.path(), &rid).expect("open"));
    let open = open_to_others(&dir);
    assert!(open.is_empty(), "left readable by others: {open:?}");
    for f in &files {
        assert_eq!(mode(f), 0o600, "{}", f.display());
    }
    let config = std::fs::read_to_string(git.join("config")).expect("config");
    assert!(config.contains("sharedRepository = 0600"), "{config}");
    assert!(dir.join("owner-only").exists(), "marked");

    // Marked: a file loosened later is not walked again on open.
    loosen(&files[0], 0o644);
    drop(charter_sync::Repo::open(home.path(), &rid).expect("reopen"));
    assert_eq!(mode(&files[0]), 0o644, "the walk ran again");
    assert_eq!(
        config,
        std::fs::read_to_string(git.join("config")).expect("config")
    );
}
