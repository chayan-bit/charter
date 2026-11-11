//! `chart device list` and `chart device revoke` through the real binaries (issue #27,
//! docs/p5/head-model.md section 15).

mod common;

use common::e2e::*;

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).expect("mkdir");
    for e in std::fs::read_dir(from).expect("read").flatten() {
        let dest = to.join(e.file_name());
        match e.file_type().expect("type").is_dir() {
            true => copy_dir(&e.path(), &dest),
            false => {
                std::fs::copy(e.path(), &dest).expect("copy");
            }
        }
    }
}

#[test]
fn the_identity_lists_and_revokes_a_device_and_the_device_can_no_longer_push() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let [alice, laptop] = ["alice", "laptop"].map(|n| User::new(root, n, &log));
    fund(&log, field(&alice.arkiv(&["keys", "init"]), "wallet:"));
    let url = field(&alice.arkiv(&["init", "--name", "devices"]), "url:").to_owned();
    let work = root.join("work");
    alice.git(root, &["init", "-q", &work.to_string_lossy()]);
    write(&work, "README", b"v0\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: v0"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    // The same identity on a second machine: its keys copied, a device key of its own.
    copy_dir(
        &alice.home.join(".charter/keys"),
        &laptop.home.join(".charter/keys"),
    );
    let lap = root.join("lap");
    laptop.git(root, &["clone", "-q", &url, &lap.to_string_lossy()]);
    laptop.git(&lap, &["checkout", "-q", "-b", "topic"]);
    write(&lap, "T", b"t\n");
    laptop.git(&lap, &["add", "-A"]);
    laptop.git(&lap, &["commit", "-q", "-m", "feat: topic"]);
    laptop.git(&lap, &["push", "-q", "origin", "topic"]);

    let list = alice.arkiv(&["device", "list", &url]);
    assert_eq!(list.lines().count(), 2, "{list}");
    assert!(
        list.contains("this machine") && list.contains("active"),
        "{list}"
    );
    let lap_nid = list
        .lines()
        .find(|l| l.contains("active"))
        .and_then(|l| l.split_whitespace().next())
        .expect("laptop nid")
        .to_owned();
    let out = alice.arkiv(&["device", "revoke", &url, &lap_nid]);
    assert!(out.contains("up to seq 0 stay valid"), "{out}");
    assert!(out.contains("it still holds your identity"), "{out}");
    let list = alice.arkiv(&["device", "list", &url]);
    assert!(list.contains("revoked at seq 0"), "{list}");

    write(&lap, "T", b"t2\n");
    laptop.git(&lap, &["commit", "-qam", "feat: t2"]);
    let push = laptop.run("git", &lap, &["push", "-q", "origin", "topic"]);
    assert!(!push.status.success());
    let err = String::from_utf8_lossy(&push.stderr);
    assert!(err.contains("E_DEVICE_REVOKED"), "{err}");

    // Revoking this machine's own device is refused.
    let own = list
        .lines()
        .find(|l| l.contains("this machine"))
        .and_then(|l| l.split_whitespace().next())
        .expect("own nid");
    let refused = alice.run(
        env!("CARGO_BIN_EXE_chart"),
        &alice.home,
        &["device", "revoke", &url, own],
    );
    assert!(!refused.status.success());
}

/// Head-model 15.7: in a private repo an admin revoking her own laptop removes its MLS
/// leaf at once (her remaining device commits it, and the CLI prints the cost) and says
/// what that leaves; `chart member sync` then finds nothing left to remove.
#[test]
fn revoking_a_private_repo_device_removes_its_mls_leaf_and_says_so() {
    let tmp = tempfile::tempdir().expect("tmp");
    let (root, log) = (tmp.path(), tmp.path().join("chain.log"));
    let [alice, laptop] = ["alice", "laptop"].map(|n| User::new(root, n, &log));
    fund(&log, field(&alice.arkiv(&["keys", "init"]), "wallet:"));
    let url = field(
        &alice.arkiv(&["init", "--private", "--name", "vault"]),
        "url:",
    )
    .to_owned();
    let work = root.join("work");
    alice.git(root, &["init", "-q", &work.to_string_lossy()]);
    write(&work, "README", b"v0\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: v0"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    copy_dir(
        &alice.home.join(".charter/keys"),
        &laptop.home.join(".charter/keys"),
    );
    let kp = field(&laptop.arkiv(&["member", "keypackage"]), "key package:").to_owned();
    alice.arkiv(&[
        "member",
        "add",
        "--repo",
        &url,
        "--key-package",
        &kp,
        "--device",
    ]);
    let lap = root.join("lap");
    laptop.git(root, &["clone", "-q", &url, &lap.to_string_lossy()]);
    laptop.git(&lap, &["checkout", "-q", "-b", "topic"]);
    write(&lap, "T", b"t\n");
    laptop.git(&lap, &["add", "-A"]);
    laptop.git(&lap, &["commit", "-q", "-m", "feat: topic"]);
    laptop.git(&lap, &["push", "-q", "origin", "topic"]);
    let members = alice.arkiv(&["member", "list", "--repo", &url]);
    assert!(members.contains("Admin 2 devices"), "{members}");

    let list = alice.arkiv(&["device", "list", &url]);
    let lap_nid = list
        .lines()
        .find(|l| l.contains("active"))
        .and_then(|l| l.split_whitespace().next())
        .expect("laptop nid")
        .to_owned();
    let out = alice.arkiv(&["device", "revoke", &url, &lap_nid]);
    assert!(
        out.contains("no MLS leaf names this device any more"),
        "{out}"
    );
    assert!(
        out.contains("removed 1 revoked device leaf(s) in one commit"),
        "{out}"
    );
    assert!(out.contains("; cost "), "{out}");
    assert!(out.contains("it still holds your identity"), "{out}");
    let members = alice.arkiv(&["member", "list", "--repo", &url]);
    assert!(members.contains("Admin 1 device"), "{members}");
    let sync = alice.arkiv(&["member", "sync", "--repo", &url]);
    assert!(sync.contains("nothing to remove"), "{sync}");
}
