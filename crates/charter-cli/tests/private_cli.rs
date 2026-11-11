//! `chart id`, `keepalive` and `status` on a private repo go through the sealing selector:
//! they work for a member, and nothing they write names the rid in plaintext.

mod common;

use charter_cli::backend::MEM_CHAIN_ID;
use charter_core::{AttrValue, Kind};
use charter_store::fake::FileMemArkiv;
use charter_sync::ids::parse_url;
use charter_sync::observe;
use common::e2e::*;
use common::programs::*;

const KINDS: [Kind; 13] = [
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

/// `kind attr` of every row on the chain that carries `rid` in plaintext.
fn plaintext_rid_rows(log: &std::path::Path, rid: &[u8; 32]) -> Vec<String> {
    let rid_hex = hex::encode(rid);
    charter_cli::block_on(async {
        let mem = FileMemArkiv::open(log, MEM_CHAIN_ID).expect("open");
        let mut out = vec![];
        for k in KINDS {
            for o in observe::all(&mem, observe::kind(k)).await.expect("query") {
                for a in &o.attributes {
                    let leaks = match &a.value {
                        AttrValue::Bytes32(b) => b == rid,
                        AttrValue::Bytes(b) => b.windows(32).any(|w| w == rid),
                        AttrValue::Str(s) => s.contains(&rid_hex),
                        AttrValue::U64(_) => false,
                    };
                    if leaks {
                        out.push(format!("{} {}", k.name(), a.name));
                    }
                }
            }
        }
        out
    })
    .expect("runtime")
}

#[test]
fn id_keepalive_and_status_work_sealed_on_a_private_repo() {
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
    alice.arkiv(&[
        "member",
        "add",
        "--repo",
        &url,
        "--key-package",
        &kp,
        "--role",
        "write",
    ]);
    let work = root.join("vault-work");
    alice.git(root, &["init", "-q", &work.to_string_lossy()]);
    write(&work, "README", b"v0\n");
    alice.git(&work, &["add", "-A"]);
    alice.git(&work, &["commit", "-q", "-m", "feat: v0"]);
    alice.git(&work, &["push", "-q", &url, "main"]);

    let status = alice.arkiv(&["status", &url]);
    assert!(status.contains("repo vault"), "{status}");
    let kept = alice.arkiv(&["keepalive", &url, "--renew-below", "60d"]);
    assert!(kept.contains("keep set:"), "{kept}");
    assert!(
        !kept.contains("extended 0 "),
        "keepalive extended nothing: {kept}"
    );

    // A two-delegate proposal from alice, accepted by bob: both write control rows.
    let bob_nid = field(&bob.arkiv(&["keys", "show"]), "nid:").to_owned();
    let proposed = alice.arkiv(&[
        "id",
        "propose",
        &url,
        "--add-delegate",
        &bob_nid,
        "--threshold",
        "2",
    ]);
    let rev = field(&proposed, "proposed revision").to_owned();
    let accepted = bob.arkiv(&["id", "accept", &url, &rev]);
    assert!(accepted.contains("threshold:      2"), "{accepted}");
    let shown = bob.arkiv(&["id", "show", &url]);
    assert!(shown.contains("name:           vault"), "{shown}");

    let leaks = plaintext_rid_rows(&log, &rid);
    assert!(
        leaks.is_empty(),
        "plaintext rows name the private rid: {leaks:?}"
    );
}
