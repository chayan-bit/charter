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
