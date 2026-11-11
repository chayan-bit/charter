//! The performance gate (issue #6, docs/p5/speed-and-cost.md section 7): every operation's
//! exact cost on the fake chain (round trips, calls, queries, txs, calldata bytes, gas)
//! must equal the checked-in baseline `bench/baseline.json`. A regression fails, and so
//! does an improvement, so the baseline always states what the code costs today.
//!
//! Update the baseline only as its own explicit commit:
//! `CHARTER_BENCH_UPDATE=1 cargo test -p charter-ui --test bench_gate`.

#[path = "../../charter-sync/tests/common/mod.rs"]
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use charter_core::{CobOpBody, Rid, ZERO};
use charter_store::Arkiv;
use charter_store::fake::{Meter, Metrics};
use charter_sync::{HeadPush, Want, git::Git};
use common::*;

type Bench = BTreeMap<String, Metrics>;

fn baseline_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/baseline.json")
}

/// Commits with fixed dates, so every run builds byte-identical packs.
fn commit_at(work: &Path, files: &[(&str, &[u8])], msg: &str) {
    for (p, b) in files {
        std::fs::write(work.join(p), b).expect("write");
    }
    for args in [vec!["add", "-A"], vec!["commit", "-q", "-m", msg]] {
        let out = Command::new("git")
            .current_dir(work)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(["-c", "commit.gpgsign=false"])
            .args(&args)
            .output()
            .expect("git");
        assert!(out.status.success(), "git {args:?}");
    }
}

/// `list for-push` (the fetch sync) and a format v3 push of `refs`, all through `a`.
async fn push_on<A: Arkiv>(a: &A, p: &Peer, rid: &Rid, work: &Path, refs: &[&str]) {
    let repo = p.repo(rid);
    let updates: Vec<_> = refs.iter().map(|r| upd(r)).collect();
    let view = Box::pin(charter_sync::sync(a, &repo, &p.want()))
        .await
        .expect("sync");
    let git = Git::work(Some(work));
    let hp = HeadPush::prepare(a, &repo, &p.cfg, &p.keys, view, &git, &updates).expect("prepare");
    Box::pin(hp.run()).await.expect("push");
}

async fn fetch_on<A: Arkiv>(a: &A, p: &Peer, rid: &Rid) {
    Box::pin(charter_sync::sync(a, &p.repo(rid), &Want::default()))
        .await
        .expect("fetch");
}

fn op(rid: &Rid, n: u64) -> CobOpBody {
    CobOpBody {
        rid: *rid,
        type_name: "xyz.arkiv.issue".into(),
        object: ZERO,
        parents: vec![],
        action: "create".into(),
        payload: charter_sync::cob::json::to_cbor(&serde_json::json!({"title": n}))
            .expect("payload"),
        ts_ms: 1_700_000_000_000,
    }
}

/// Every gated operation once, in a fixed order on one fresh chain.
async fn run_all() -> Bench {
    let net = Net::new();
    let m = Meter::new(&net);
    let mut out = Bench::new();
    let mut take = |name: &str, m: &Meter<Net>| {
        out.insert(name.into(), m.metrics());
        m.reset();
    };
    let alice = Peer::new(&net, 1);
    let rid = init(&net, &alice, "bench").await;
    let tmp = tempfile::tempdir().expect("tmp");
    let work = work_repo(tmp.path(), "work");
    commit_at(&work, &[("a", b"one")], "feat: one");
    Box::pin(push_on(&net, &alice, &rid, &work, &["refs/heads/main"])).await;
    let bob = Peer::new(&net, 2);
    Box::pin(fetch_on(&net, &bob, &rid)).await;

    commit_at(&work, &[("a", b"two")], "feat: two");
    m.reset();
    Box::pin(push_on(&m, &alice, &rid, &work, &["refs/heads/main"])).await;
    take("push", &m);
    Box::pin(fetch_on(&m, &bob, &rid)).await;
    take("fetch", &m);
    Box::pin(fetch_on(&m, &bob, &rid)).await;
    take("fetch_noop", &m);
    git(&work, &["branch", "topic"]);
    Box::pin(push_on(&m, &alice, &rid, &work, &["refs/heads/topic"])).await;
    take("push_refs_only", &m);
    commit_at(&work, &[("big", &noise(7, 48 << 10))], "feat: big");
    Box::pin(push_on(&m, &alice, &rid, &work, &["refs/heads/main"])).await;
    take("push_bulk", &m);
    let carol = Peer::new(&net, 3);
    Box::pin(fetch_on(&m, &carol, &rid)).await;
    take("clone", &m);
    charter_sync::cob::post(&m, &alice.cfg, &alice.keys, &op(&rid, 1))
        .await
        .expect("post");
    take("op_post", &m);

    // The UI's sync loop on bob's home: a round that finds the new op, then nothing new.
    Box::pin(charter_ui::sync_loop::round(&m, &bob.cfg, &rid))
        .await
        .expect("round");
    take("ui_round", &m);
    Box::pin(charter_ui::sync_loop::round(&m, &bob.cfg, &rid))
        .await
        .expect("round");
    take("ui_round_noop", &m);
    let seen = charter_ui::pace::probe(&net, &rid, None)
        .await
        .expect("probe");
    m.reset();
    charter_ui::pace::probe(&m, &rid, Some(seen))
        .await
        .expect("probe");
    take("ui_probe_noop", &m);

    // A pinned program: bob's fetch folds the ops and alice's new head.
    let src = include_str!("../../../programs/bounty.rhai");
    charter_sync::program::publish(&net, &alice.cfg, &alice.keys, &rid, "rhai/1", src)
        .await
        .expect("publish");
    let pin = charter_core::ProgramPin {
        engine: "rhai/1".into(),
        hash: charter_program::Program::hash(src),
    };
    let st = charter_sync::state::State::default();
    charter_sync::identity::propose(&net, &alice.cfg, &alice.keys, &rid, &st, |b| {
        charter_core::IdBody {
            program: Some(pin),
            ..b.clone()
        }
    })
    .await
    .expect("pin");
    Box::pin(fetch_on(&net, &bob, &rid)).await;
    commit_at(&work, &[("a", b"three")], "feat: three");
    Box::pin(push_on(&net, &alice, &rid, &work, &["refs/heads/main"])).await;
    m.reset();
    Box::pin(fetch_on(&m, &bob, &rid)).await;
    take("fold", &m);
    out
}

/// Every difference between `base` and `now`, one line each; empty when they are equal.
fn compare(base: &Bench, now: &Bench) -> Vec<String> {
    let mut out = vec![];
    for name in base.keys().filter(|n| !now.contains_key(*n)) {
        out.push(format!("{name}: in the baseline but no longer measured"));
    }
    for (name, m) in now {
        let Some(b) = base.get(name) else {
            out.push(format!("{name}: not in the baseline"));
            continue;
        };
        for ((field, was), (_, is)) in b.fields().into_iter().zip(m.fields()) {
            let what = match is.cmp(&was) {
                std::cmp::Ordering::Equal => continue,
                std::cmp::Ordering::Greater => "REGRESSION",
                std::cmp::Ordering::Less => "improvement, record it in the baseline",
            };
            out.push(format!("{name}.{field}: {was} -> {is} ({what})"));
        }
    }
    out
}

fn report(b: &Bench) -> String {
    b.iter()
        .map(|(n, m)| {
            let f: Vec<String> = m.fields().iter().map(|(k, v)| format!("{k}={v}")).collect();
            format!("{n:<16} {}", f.join(" "))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn min_of(a: &Bench, b: &Bench) -> Bench {
    a.iter()
        .map(|(n, m)| (n.clone(), b.get(n).map_or(*m, |o| m.min(o))))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn bench_gate() {
    let path = baseline_path();
    let update = std::env::var("CHARTER_BENCH_UPDATE").as_deref() == Ok("1");
    let base: Option<Bench> = std::fs::read(&path)
        .ok()
        .map(|b| serde_json::from_slice(&b).expect("baseline parses"));
    let mut now = Box::pin(run_all()).await;
    // A cache that went stale on a slow machine can add a read, never remove one: rerun
    // before failing, or before recording, and keep the smallest count of each.
    if update || base.as_ref().is_none_or(|b| !compare(b, &now).is_empty()) {
        if let Some(b) = &base {
            eprintln!("rerunning after: {:?}", compare(b, &now));
        }
        for _ in 0..2 {
            now = min_of(&now, &Box::pin(run_all()).await);
        }
    }
    println!("{}", report(&now));
    if update {
        let json = serde_json::to_string_pretty(&now).expect("json") + "\n";
        std::fs::write(&path, json).expect("write baseline");
        return;
    }
    let base = base.expect("bench/baseline.json is missing; record it with CHARTER_BENCH_UPDATE=1");
    let diff = compare(&base, &now);
    assert!(
        diff.is_empty(),
        "fake-chain costs differ from bench/baseline.json:\n{}\n\
         A regression needs a recorded maintainer decision (issue #6). An improvement is \
         recorded with CHARTER_BENCH_UPDATE=1 in its own commit.",
        diff.join("\n")
    );
}

#[test]
fn gate_fails_a_deliberately_slowed_operation() {
    let m = Metrics {
        rounds: 9,
        calls: 20,
        queries: 6,
        txs: 1,
        bytes: 900,
        gas: 270_000,
    };
    let base = Bench::from([("push".into(), m)]);
    assert!(compare(&base, &base).is_empty());
    let slower = Bench::from([(
        "push".into(),
        Metrics {
            rounds: 10,
            gas: 270_001,
            ..m
        },
    )]);
    let diff = compare(&base, &slower);
    assert_eq!(
        diff,
        [
            "push.rounds: 9 -> 10 (REGRESSION)",
            "push.gas: 270000 -> 270001 (REGRESSION)"
        ]
    );
    let faster = Bench::from([("push".into(), Metrics { queries: 5, ..m })]);
    assert_eq!(
        compare(&base, &faster).len(),
        1,
        "an improvement also fails"
    );
    assert_eq!(
        compare(&base, &Bench::new()).len(),
        1,
        "a dropped operation fails"
    );
}
