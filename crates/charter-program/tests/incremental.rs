//! Incremental folds: resuming a checkpoint over the later events gives exactly the
//! outcome (and the next checkpoint) of a fold from the start.

mod common;

use std::collections::BTreeMap;

use charter_program::{Checkpoint, ENGINE_RHAI_V1, Event, Outcome, Program};
use common::*;
use serde_json::json;

fn source(name: &str) -> String {
    let path = format!("{}/../../programs/{name}.rhai", env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(path).expect("template file")
}

/// A mixed history: patches, reviews, CI attestations, bounties and pushes.
fn history() -> Vec<Event> {
    let mut v = vec![];
    let mut o = 0;
    let mut next = || {
        o += 1;
        o
    };
    for round in 0..4u8 {
        let head = 0x10 + round;
        let patch = json!({"title": "t", "head": oid_hex(head)});
        v.push(op(next(), 5, 0x50, "xyz.arkiv.patch", "create", patch));
        for d in [0xd1, 0xd2] {
            let a = json!({"patch": hx(0x50), "head": oid_hex(head)});
            v.push(op(next(), d, 0x51, "xyz.arkiv.review", "approve", a));
        }
        let ci =
            json!({"head": oid_hex(head), "status": "pass", "suite": "cargo", "log_sha256": "00"});
        v.push(op(next(), 0xd3, 0x40, "xyz.arkiv.attest", "ci", ci));
        let b = json!({"amount": 5, "title": "b"});
        v.push(op(next(), 0xd1, 0x60, "xyz.arkiv.bounty", "create", b));
        let old = (round > 0).then(|| head - 1);
        v.push(main(next(), old, head));
        v.push(push(next(), 0xd2, "refs/heads/dev", None, head));
    }
    v
}

fn full(src: &str, events: &[Event], ancestry: charter_program::Ancestry<'_>) -> Outcome {
    let p = Program::load(ENGINE_RHAI_V1, src).expect("load");
    p.fold_with(repo(), BTreeMap::new(), events.to_vec(), ancestry)
        .expect("fold")
}

fn cached(
    src: &str,
    events: &[Event],
    ancestry: charter_program::Ancestry<'_>,
    cp: Option<&Checkpoint>,
) -> (Outcome, Option<Checkpoint>) {
    let p = Program::load(ENGINE_RHAI_V1, src).expect("load");
    p.fold_cached(src, repo(), BTreeMap::new(), events, ancestry, cp)
        .expect("fold")
}

fn known(a: &[u8], b: &[u8]) -> Option<bool> {
    Some(a[0] <= b[0])
}

#[test]
fn resuming_at_every_split_equals_a_full_fold_for_every_template() {
    let events = history();
    for name in [
        "ci-gated",
        "ci-attested",
        "branch-protection",
        "bounty",
        "agent-bounty",
        "sealed-patch",
    ] {
        let src = source(name);
        let want = full(&src, &events, &known);
        let (_, want_cp) = cached(&src, &events, &known, None);
        let want_cp = want_cp.expect("checkpointable");
        for k in 0..=events.len() {
            let (_, cp) = cached(&src, &events[..k], &known, None);
            let cp = cp.expect("checkpointable");
            let json = serde_json::to_string(&cp).expect("serialize");
            let cp: Checkpoint = serde_json::from_str(&json).expect("parse");
            let (got, next) = cached(&src, &events, &known, Some(&cp));
            assert_eq!(got, want, "{name}: resumed after {k} events");
            assert_eq!(
                next.as_ref(),
                Some(&want_cp),
                "{name}: checkpoint after {k}"
            );
        }
    }
}

#[test]
fn a_stale_or_foreign_checkpoint_is_ignored() {
    let events = history();
    let src = source("ci-gated");
    let (_, cp) = cached(&src, &events[..10], &known, None);
    let cp = cp.expect("cp");
    let want = full(&src, &events, &known);
    // A different event at position 3: the prefix hash no longer matches.
    let mut changed = events.clone();
    changed[3] = op(4, 0xd3, 0x51, "xyz.arkiv.review", "approve", json!({}));
    let want_changed = full(&src, &changed, &known);
    assert_eq!(cached(&src, &changed, &known, Some(&cp)).0, want_changed);
    // Another program, or fewer events than the checkpoint holds.
    let other = source("branch-protection");
    assert_eq!(
        cached(&other, &events, &known, Some(&cp)).0,
        full(&other, &events, &known)
    );
    let short = &events[..5];
    assert_eq!(
        cached(&src, short, &known, Some(&cp)).0,
        full(&src, short, &known)
    );
    assert_eq!(cached(&src, &events, &known, Some(&cp)).0, want);
}

#[test]
fn a_checkpoint_from_another_build_is_ignored() {
    let events = history();
    let src = source("ci-gated");
    let (_, cp) = cached(&src, &events[..10], &known, None);
    let mut j = serde_json::to_value(cp.expect("cp")).expect("json");
    // Same keys otherwise, but written by another build (whose state would show).
    j["engine"] = json!("rhai/1 charter-program/0.0.0-other");
    j["logs"] = json!(["from another build"]);
    let cp: Checkpoint = serde_json::from_value(j).expect("parse");
    let (got, next) = cached(&src, &events, &known, Some(&cp));
    assert_eq!(got, full(&src, &events, &known));
    assert_eq!(
        next.map(|c| c.engine),
        Some(charter_program::CHECKPOINT_ENGINE.to_owned())
    );
}

#[test]
fn an_unanswered_ancestry_question_is_never_checkpointed() {
    let events = history();
    let src = source("ci-gated");
    let unknown = |_: &[u8], _: &[u8]| None;
    let (o, cp) = cached(&src, &events, &unknown, None);
    assert_eq!(o, full(&src, &events, &unknown));
    assert!(
        cp.is_none(),
        "a later fold with the objects could decide differently"
    );
}

#[test]
fn a_resumed_fold_spends_from_the_same_fuel_budget() {
    // Each op burns about 150k operations, so the 5M fold budget runs out near op 33.
    let src = r#"
        fn init(repo) { #{ n: 0 } }
        fn on_op(s, op) { let i = 0; while i < 37000 { i += 1; } s.n += 1; s }
    "#;
    let events: Vec<Event> = (1..=45)
        .map(|o| op(o, 1, 1, "t", "a", json!(null)))
        .collect();
    let want = full(src, &events, &known);
    assert!(
        want.logs.iter().any(|l| l.contains("budget")),
        "{:?}",
        want.logs
    );
    let (_, cp) = cached(src, &events[..20], &known, None);
    let (got, _) = cached(src, &events, &known, cp.as_ref());
    assert_eq!(got, want);
}
