//! The own cost of each parity batch 3 feature on the fake chain (rounds, calls, queries,
//! txs, calldata bytes, gas), printed for the report and held to one tx per op (plus any wallet top-up).
//! The performance gate (`bench_gate.rs`) is untouched: none of these operations existed
//! before, and the existing ones it measures must not move.
//! See the table with `cargo test -p charter-ui --test parity3_costs -- --nocapture`.

#[path = "../../charter-sync/tests/common/mod.rs"]
mod common;

use std::path::Path;
use std::process::{Command, Stdio};

use charter_core::{CobOpBody, Hash32, IdBody, ProgramPin, Rid, ZERO};
use charter_store::Arkiv;
use charter_store::fake::{Meter, Metrics};
use charter_sync::gitsig::{self, CLAIM_NAMESPACE, CLAIM_TYPE};
use charter_sync::state::State;
use charter_sync::{HeadPush, Want, git::Git};
use common::*;
use serde_json::{Value, json};

fn body(rid: &Rid, ty: &str, action: &str, object: Option<Hash32>, p: &Value) -> CobOpBody {
    CobOpBody {
        rid: *rid,
        type_name: ty.into(),
        object: object.unwrap_or(ZERO),
        parents: object.into_iter().collect(),
        action: action.into(),
        payload: charter_sync::cob::json::to_cbor(p).expect("payload"),
        ts_ms: 1_700_000_000_000,
    }
}

async fn post<A: Arkiv>(a: &A, p: &Peer, b: &CobOpBody) -> Hash32 {
    charter_sync::cob::post(a, &p.cfg, &p.keys, b)
        .await
        .expect("post")
}

/// A claim payload for `p`: a fresh Ed25519 key and its `ssh-keygen -Y sign` proof.
fn claim_payload(dir: &Path, p: &Peer) -> Value {
    let key = dir.join("id_ed25519");
    let made = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", "k", "-f"])
        .arg(&key)
        .status()
        .expect("ssh-keygen");
    assert!(made.success());
    let mut c = Command::new("ssh-keygen")
        .args(["-q", "-Y", "sign", "-n", CLAIM_NAMESPACE, "-f"])
        .arg(&key)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("ssh-keygen");
    let msg = gitsig::claim_message(&p.keys.nid());
    let mut stdin = c.stdin.take().expect("stdin");
    std::io::Write::write_all(&mut stdin, msg.as_bytes()).expect("sign");
    drop(stdin);
    let sig = String::from_utf8(c.wait_with_output().expect("sign").stdout).expect("utf8");
    let line = std::fs::read_to_string(key.with_extension("pub")).expect("pub");
    json!({"key": line.trim(), "sig": sig})
}

async fn push_main(net: &Net, p: &Peer, rid: &Rid, work: &Path) {
    let repo = p.repo(rid);
    let view = Box::pin(charter_sync::sync(net, &repo, &p.want()))
        .await
        .expect("sync");
    let git = Git::work(Some(work));
    let ups = [upd("refs/heads/main")];
    let hp = HeadPush::prepare(net, &repo, &p.cfg, &p.keys, view, &git, &ups).expect("prepare");
    Box::pin(hp.run()).await.expect("push");
}

async fn fetch<A: Arkiv>(a: &A, p: &Peer, rid: &Rid) -> charter_sync::View {
    Box::pin(charter_sync::sync(a, &p.repo(rid), &Want::default()))
        .await
        .expect("fetch")
}

fn print(rows: &[(&str, Metrics)]) {
    eprintln!("{:<24} rounds calls queries txs bytes gas", "feature");
    for (n, m) in rows {
        let f = m.fields().map(|(_, v)| v.to_string()).join(" ");
        eprintln!("{n:<24} {f}");
    }
}

#[tokio::test]
async fn each_new_feature_costs_one_tx_per_op() {
    let net = Net::new();
    let m = Meter::new(&net);
    let mut rows = vec![];
    let mut take = |name: &'static str, m: &Meter<Net>| {
        rows.push((name, m.metrics()));
        m.reset();
    };
    let (alice, bob) = (Peer::new(&net, 1), Peer::new(&net, 2));
    let rid = init(&net, &alice, "costs").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    let owners = format!("* {}\n", h32_hex(&bob.keys.nid()));
    let base = commit(&work, &[("CODEOWNERS", owners.as_bytes())], "chore: owners");
    Box::pin(push_main(&net, &alice, &rid, &work)).await;
    fetch(&net, &bob, &rid).await;

    // #61: a signing-key claim, and the claim listing `chart signing-key verify` reads.
    let claim = body(
        &rid,
        CLAIM_TYPE,
        "create",
        None,
        &claim_payload(tmp.path(), &alice),
    );
    m.reset();
    post(&m, &alice, &claim).await;
    take("signing_key_claim", &m);
    let ops = charter_sync::cob::list(&m, &rid, None, Some(CLAIM_TYPE))
        .await
        .expect("list");
    take("signing_key_verify", &m);
    assert_eq!(gitsig::claims(&ops).of(&alice.keys.nid()).len(), 1);

    // #40: one label op on an issue.
    let issue = body(
        &rid,
        "xyz.arkiv.issue",
        "create",
        None,
        &json!({"title": "t"}),
    );
    let issue = post(&net, &alice, &issue).await;
    let label = json!({"labels": ["bug", "ui"]});
    m.reset();
    post(
        &m,
        &alice,
        &body(&rid, "xyz.arkiv.issue", "label", Some(issue), &label),
    )
    .await;
    take("label", &m);

    // #38: a review with two inline comments, one a suggestion (create, then approve).
    let c1 = commit(&work, &[("lib.rs", b"fn f() {}\n")], "feat: lib");
    let patch = json!({"title": "lib", "head": c1, "base": base});
    let patch = post(
        &net,
        &alice,
        &body(&rid, "xyz.arkiv.patch", "create", None, &patch),
    )
    .await;
    let ph = h32_hex(&patch);
    let review = json!({"patch": ph});
    m.reset();
    let rv = post(
        &m,
        &bob,
        &body(&rid, "xyz.arkiv.review", "create", None, &review),
    )
    .await;
    let approve = json!({"patch": ph, "head": c1, "comments": [
        {"path": "lib.rs", "line": 1, "body": "name it g"},
        {"path": "lib.rs", "line": 1, "body": "like this", "suggestion": "fn g() {}"},
    ]});
    post(
        &m,
        &bob,
        &body(&rid, "xyz.arkiv.review", "approve", Some(rv), &approve),
    )
    .await;
    take("review_inline_2_ops", &m);

    // #53: bob's fetch that folds a codeowners program over alice's owner-approved push.
    let src = include_str!("../../../programs/codeowners.rhai");
    let engine = charter_program::engine_for(src);
    assert_eq!(engine, "rhai/2");
    charter_sync::program::publish(&net, &alice.cfg, &alice.keys, &rid, engine, src)
        .await
        .expect("publish");
    let pin = ProgramPin {
        engine: engine.into(),
        hash: charter_program::Program::hash(src),
    };
    let st = State::default();
    charter_sync::identity::propose(&net, &alice.cfg, &alice.keys, &rid, &st, |b| IdBody {
        program: Some(pin),
        ..b.clone()
    })
    .await
    .expect("pin");
    fetch(&net, &bob, &rid).await;
    Box::pin(push_main(&net, &alice, &rid, &work)).await;
    m.reset();
    let view = fetch(&m, &bob, &rid).await;
    take("fold_codeowners", &m);
    let out = &view.program.expect("the fold ran").outcome;
    assert!(out.rejected.is_empty(), "{:?}", out.rejected);
    let main = out.heads.get("refs/heads/main").map(hex::encode);
    assert_eq!(
        main.as_deref(),
        Some(c1.as_str()),
        "the owner approval lets it in"
    );

    // #63: archiving is one identity revision (rename and hand-over cost the same).
    m.reset();
    charter_sync::identity::propose(&m, &alice.cfg, &alice.keys, &rid, &st, |b| IdBody {
        archived: Some(true),
        ..b.clone()
    })
    .await
    .expect("archive");
    take("archive_revision", &m);

    print(&rows);
    // One tx per op; at most one more is the signer wallet's top-up transfer (no calldata),
    // which the existing send path makes whenever the wallet runs low, whatever the op.
    for (n, r) in &rows {
        let ops = match *n {
            "review_inline_2_ops" => 2,
            "signing_key_verify" | "fold_codeowners" => 0,
            _ => 1,
        };
        assert!(
            r.txs >= ops && r.txs <= ops + u64::from(ops > 0),
            "{n}: {r:?}"
        );
    }
}
