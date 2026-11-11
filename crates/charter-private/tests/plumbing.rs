//! Writes that would leave a private repo unsealed are refused (`E_UNSEALED_PRIVATE`).

mod common;

use charter_core::{AttrValue, CobOpBody, Kind, Rid, ZERO};
use charter_private::{Keyring, Sealed};
use charter_store::Arkiv;
use charter_sync::observe;
use common::*;

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

/// `kind attr` of every row carrying `rid` in plaintext.
async fn plaintext_rid_rows<A: Arkiv>(a: &A, rid: &Rid) -> Vec<String> {
    let mut out = vec![];
    for k in KINDS {
        for o in observe::all(a, observe::kind(k)).await.expect("query") {
            for at in &o.attributes {
                let hit = match &at.value {
                    AttrValue::Bytes32(b) => b == rid,
                    AttrValue::Bytes(b) => b.windows(32).any(|w| w == rid),
                    AttrValue::Str(s) => s.contains(&hex::encode(rid)),
                    AttrValue::U64(_) => false,
                };
                if hit {
                    out.push(format!("{} {}", k.name(), at.name));
                }
            }
        }
    }
    out
}

async fn ring(net: &Net, p: &Peer, rid: &Rid) -> Keyring {
    charter_private::ring_for(net, &p.cfg, Some(&p.keys), rid)
        .await
        .expect("ring")
        .expect("private")
}

fn issue(rid: &Rid) -> CobOpBody {
    CobOpBody {
        rid: *rid,
        type_name: "xyz.arkiv.issue".into(),
        object: ZERO,
        parents: vec![],
        action: "create".into(),
        payload: charter_sync::cob::json::to_cbor(&serde_json::json!({"title": "secret"}))
            .expect("cbor"),
        ts_ms: 1,
    }
}

#[test]
fn unsealed_writes_to_a_private_repo_are_refused() {
    run(async {
        let net = Net::new();
        let alice = Peer::new(&net, 1);
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init");
        // What `chart id accept` and an op post did on the plain backend.
        let acc = charter_sync::identity::accept(&net, &alice.cfg, &alice.keys, &rid, &[7; 32]);
        assert_eq!(code(acc.await), "E_UNSEALED_PRIVATE");
        let op = issue(&rid);
        let post = charter_sync::cob::post(&net, &alice.cfg, &alice.keys, &op);
        assert_eq!(code(post.await), "E_UNSEALED_PRIVATE");
        let leaks = plaintext_rid_rows(&net, &rid).await;
        assert!(
            leaks.is_empty(),
            "plaintext rows name the private rid: {leaks:?}"
        );
        // The same op through the sealed view goes out.
        let sealed = Sealed::new(&net, ring(&net, &alice, &rid).await);
        charter_sync::cob::post(&sealed, &alice.cfg, &alice.keys, &issue(&rid))
            .await
            .expect("sealed post");
        assert!(plaintext_rid_rows(&net, &rid).await.is_empty());
    });
}

#[test]
fn the_guard_reads_the_keyring_where_charter_private_writes_it() {
    let (home, rid) = (std::path::Path::new("/h"), [9u8; 32]);
    assert_eq!(
        Keyring::path(home, &rid),
        charter_sync::guard::keyring_path(home, &rid)
    );
}

/// Queries of `keepalive --dry-run`, `status` and `id show` for one peer.
async fn reads<A: Arkiv>(net: &Net, a: &A, p: &Peer, rid: &Rid) -> [usize; 3] {
    let repo = p.repo(rid);
    let opts = charter_sync::keepalive::KeepOpts {
        horizon: 1_000_000,
        renew_below: 1_000_000,
        max_spend: None,
        dry_run: true,
    };
    net.take_queries();
    charter_sync::keepalive::keepalive(a, &repo, &p.cfg, &p.keys.wallet(), &opts)
        .await
        .expect("keepalive");
    let k = net.take_queries().len();
    charter_sync::keepalive::status(a, &repo, &p.cfg)
        .await
        .expect("status");
    let s = net.take_queries().len();
    let st = charter_sync::state::State::default();
    charter_sync::identity::show(a, rid, &st).await.expect("id");
    [k, s, net.take_queries().len()]
}

/// The perf rule: the sealing selector the CLI now runs first costs a public repo nothing
/// once its `.public` marker exists, and its first identity query is reused by the view.
#[test]
fn the_selector_adds_no_round_trips_on_a_public_repo() {
    run(async {
        let net = Net::new();
        let (alice, bob, carol) = (Peer::new(&net, 1), Peer::new(&net, 2), Peer::new(&net, 3));
        let rid = charter_sync::identity::init(&net, &alice.cfg, &alice.keys, &meta("open"))
            .await
            .expect("init");
        let tmp = tempfile::tempdir().expect("tmp");
        let work = work_repo(tmp.path(), "work");
        commit(&work, &[("a", b"x")], "feat: x");
        push(&net, &alice, &rid, &work, &["refs/heads/main"])
            .await
            .expect("push");
        let mut plain = vec![];
        let mut selected = vec![];
        for _ in 0..2 {
            plain.push(reads(&net, &net, &carol, &rid).await);
            net.take_queries();
            let keys = Some(&bob.keys);
            let b = charter_private::for_repo(&net, &bob.cfg, keys, &rid)
                .await
                .expect("sel");
            let sel = net.take_queries().len();
            let [k, s, i] = reads(&net, &b, &bob, &rid).await;
            selected.push([k + sel, s, i]);
        }
        eprintln!(
            "COUNTS public [keepalive, status, id show] plain {plain:?} selector {selected:?}"
        );
        assert_eq!(plain, selected);
        // The private repo through the selector (the plain backend fails with E_ID_NOT_LIVE).
        let rid = charter_private::init(&net, &alice.cfg, &alice.keys, &meta("secret"))
            .await
            .expect("init --private");
        let ring = ring(&net, &alice, &rid).await;
        push(
            &Sealed::new(&net, ring),
            &alice,
            &rid,
            &work,
            &["refs/heads/main"],
        )
        .await
        .expect("push");
        net.take_queries();
        let b = charter_private::for_repo(&net, &alice.cfg, Some(&alice.keys), &rid)
            .await
            .expect("sel");
        let sel = net.take_queries().len();
        let [k, s, i] = reads(&net, &b, &alice, &rid).await;
        eprintln!(
            "COUNTS private [keepalive, status, id show] selector [{}, {s}, {i}]",
            k + sel
        );
    });
}
