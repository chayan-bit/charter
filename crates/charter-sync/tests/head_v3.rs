//! Format v3 through `MemArkiv` (docs/p5/head-model.md 13): pushes write v3 heads, a v2
//! head (as v0.4.0 writes it) migrates on the next push, after which the v0.4.0 reader
//! stops with `E_UNSUPPORTED_VERSION`, and the program walk reads both formats in one chain.

mod common;
#[path = "../../charter-core/tests/common/v040.rs"]
mod v040;

use charter_core::{
    HeadBody, HeadFormat, HeadPack, HeadRef, PackPieces, PieceRef, PieceRun, Rid, VerifyError, ZERO,
};
use charter_store::Arkiv;
use charter_sync::heads::DeviceView;
use charter_sync::keys::Keys;
use charter_sync::pieces::piece_op;
use charter_sync::write::{entity_patch_op, sign};
use common::*;

fn dev_of(p: &Peer) -> Keys {
    charter_sync::device::device_keys(p.home.path()).expect("dev")
}

fn sha256(b: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).into()
}

async fn view_of(net: &Net, p: &Peer, rid: &Rid) -> (charter_sync::View, DeviceView) {
    let view = Box::pin(charter_sync::sync(net, &p.repo(rid), &p.want()))
        .await
        .expect("sync");
    let d = view.peers[&p.keys.nid()]
        .v2
        .as_ref()
        .expect("v2")
        .device(&dev_of(p).nid())
        .expect("device")
        .clone();
    (view, d)
}

/// The thin pack v0.4.0 sends for `tip` over `base` (zlib objects, git's defaults).
fn thin_pack(work: &std::path::Path, tip: &str, base: &str) -> Vec<u8> {
    use std::io::Write;
    let mut child = std::process::Command::new("git")
        .args(["pack-objects", "--revs", "-q", "--thin", "--stdout"])
        .current_dir(work)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("git");
    let mut stdin = child.stdin.take().expect("stdin");
    stdin
        .write_all(format!("{tip}\n^{base}\n").as_bytes())
        .expect("write");
    drop(stdin);
    let out = child.wait_with_output().expect("pack-objects");
    assert!(out.status.success());
    out.stdout
}

/// What a v0.4.0 device writes for a push of `main` from `base` to `tip`: half the thin
/// pack as a bulk piece from `B_d`, the other half inline, and a v2 head patch with every
/// attribute from `C_d` (head-model 4.2 and 7.1).
async fn v040_push(net: &Net, p: &Peer, rid: &Rid, work: &std::path::Path, base: &str, tip: &str) {
    let (view, d) = view_of(net, p, rid).await;
    let head = d.head.clone().expect("head");
    let landing = charter_sync::heads::landing(net, &d.device, &head, u64::MAX)
        .await
        .expect("rpc")
        .expect("landing");
    let pack = thin_pack(work, tip, base);
    let (bulk, inline) = pack.split_at(pack.len() / 2);
    let dev = dev_of(p);
    net.fund(dev.bulk_address(rid));
    net.fund(dev.control_address(rid));
    let landed = charter_sync::write::send(
        net,
        &dev.bulk(rid),
        &fast(),
        vec![vec![piece_op(rid, bulk).expect("op")]],
    )
    .await
    .expect("bulk piece");
    let old = head
        .body
        .refs
        .iter()
        .find(|e| e.name == "refs/heads/main")
        .expect("main");
    let body = HeadBody {
        seq: head.body.seq + 1,
        id_head: view.identity.head().h,
        at: net.mem.head().await.expect("head"),
        pieces_created: head.body.pieces_created + 1,
        refs: vec![HeadRef {
            name: old.name.clone(),
            oid: hex::decode(tip).expect("hex"),
            counter: old.counter + 1,
            stamp: 0,
            base: hex::decode(base).expect("hex"),
        }],
        packs: vec![HeadPack {
            pack_id: sha256(&pack),
            pieces: vec![
                PieceRef {
                    tx: landed[0].receipt.tx_hash.0,
                    sha256: sha256(bulk),
                },
                PieceRef {
                    tx: ZERO,
                    sha256: sha256(inline),
                },
            ],
        }],
        prev_tx: landing.tx,
        ..head.body.clone()
    };
    let signed = sign(dev.sk(), &body).expect("sign");
    let ops = vec![
        piece_op(rid, inline).expect("op"),
        entity_patch_op::<HeadBody>(head.key, &signed.bytes).expect("op"),
    ];
    send_ops(net, &dev.control(rid), ops).await;
}

fn v040_accepts(
    view: &charter_sync::View,
    d: &DeviceView,
    rid: &Rid,
    nid: &[u8; 32],
) -> Result<Option<u64>, VerifyError> {
    v040::accept_device_head(rid, nid, &d.device, &view.identity, &d.rows, &d.nonce)
        .map(|h| h.map(|e| e.body.seq))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_v2_head_migrates_on_the_next_push_and_v040_then_stops_with_unsupported_version() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "migrate").await;
    let nid = alice.keys.nid();
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    let one = commit(&work, &[("a.txt", b"one")], "feat: one");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("first push");
    let two = commit(&work, &[("big.txt", &noise(3, 40_000))], "feat: two");
    v040_push(&net, &alice, &rid, &work, &one, &two).await;

    let (view, d) = view_of(&net, &alice, &rid).await;
    let head = d.head.clone().expect("head");
    assert_eq!((head.format, head.body.seq), (HeadFormat::V2, 1));
    assert_eq!(
        v040_accepts(&view, &d, &rid, &nid),
        Ok(Some(1)),
        "v0.4.0 reads the v2 head"
    );

    let three = commit(&work, &[("c.txt", b"three")], "feat: three");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("migrating push");
    let (view, d) = view_of(&net, &alice, &rid).await;
    let head = d.head.clone().expect("head");
    assert_eq!((head.format, head.body.seq), (HeadFormat::V3, 2));
    assert_eq!(head.packs.len(), 2, "the carried v2 pack and the new one");
    let PackPieces::Runs(carried) = &head.packs[0].pieces else {
        panic!("the carried pack is re-encoded as runs")
    };
    assert_eq!(carried.len(), 2, "the bulk piece tx and the v2 head tx");
    assert!(!carried.contains(&PieceRun::HERE));
    let row = d.rows.iter().find(|o| o.key == head.key).expect("row");
    assert_eq!(
        row.attr("ag.v"),
        Some(&charter_core::AttrValue::U64(3)),
        "the migrating patch set every attribute"
    );
    assert!(
        matches!(
            v040_accepts(&view, &d, &rid, &nid),
            Err(VerifyError::UnsupportedVersion { .. })
        ),
        "v0.4.0 fails closed on the migrated head"
    );

    // A fresh reader clones the mixed history after every piece entity expired.
    net.mine(30);
    let bob = Peer::new(&net, 2);
    let dest = tmp.path().join("dest");
    let (_, listing) = Box::pin(clone_into(&net, &bob, &rid, &dest))
        .await
        .expect("clone");
    assert_eq!(listing.refs[0].1, three);
    git(&dest, &["fsck", "--full", "--strict"]);

    // The program walk follows `prev_tx` through v3, v2 and v3 bodies alike.
    let v2 = view.peers[&nid].v2.clone().expect("v2");
    let mut cache = charter_sync::program::heads::BodyCache::default();
    let bodies = charter_sync::program::heads::identity_bodies(&net, &rid, &v2, 0, &mut cache)
        .await
        .expect("walk");
    let seqs: Vec<u64> = bodies.iter().map(|b| b.body.seq).collect();
    assert_eq!(seqs, [0, 1, 2]);
    let oids: Vec<String> = bodies
        .iter()
        .map(|b| hex::encode(&b.body.refs[0].oid))
        .collect();
    assert_eq!(oids, [one, two, three.clone()]);
    let timeline = charter_sync::program::heads::timeline(&bodies);
    let (_, last) = timeline.last().expect("timeline");
    assert_eq!(hex::encode(&last["refs/heads/main"].entry.oid), three);
}

#[tokio::test(flavor = "multi_thread")]
async fn later_pushes_patch_only_the_body_and_its_hash_and_send_no_marker_beside_a_piece() {
    let net = Net::new();
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "minimal").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit(&work, &[("a.txt", b"one")], "feat: one");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("first push");
    let dev = dev_of(&alice);
    let before = created_by(&net, dev.control_address(&rid)).await.len();
    commit(&work, &[("b.txt", b"two")], "feat: two");
    push2(&net, &alice, &rid, &work, &["refs/heads/main"])
        .await
        .expect("inline push");
    let made = created_by(&net, dev.control_address(&rid)).await;
    assert_eq!(made.len(), before + 1, "the inline piece and no marker");
    git(&work, &["branch", "topic"]);
    push2(&net, &alice, &rid, &work, &["refs/heads/topic"])
        .await
        .expect("refs-only push");
    let made = created_by(&net, dev.control_address(&rid)).await;
    let marker = made.iter().filter(|o| o.attributes.is_empty()).count();
    assert_eq!(
        (made.len(), marker),
        (before + 2, 1),
        "one attribute-less marker"
    );

    let (_, d) = view_of(&net, &alice, &rid).await;
    let head = d.head.expect("head");
    let landing = charter_sync::heads::landing(&net, &d.device, &head, u64::MAX)
        .await
        .expect("rpc")
        .expect("landing");
    let mined = net
        .mem
        .mined_tx(landing.tx.into())
        .await
        .expect("rpc")
        .expect("tx");
    let rows = charter_sync::heads::calldata_rows(&net, &mined);
    let patch = rows
        .iter()
        .find(|o| o.attr("ag.h") == Some(&charter_core::AttrValue::Bytes32(head.h)))
        .expect("head row");
    let names: Vec<&str> = patch.attributes.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["ag.body", "ag.h"], "a minimal patch");
}
