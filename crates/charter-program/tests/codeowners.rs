//! The git host functions of engine rhai/2 and the code-owners template (issue #53).

mod common;

use std::collections::BTreeMap;
use std::rc::Rc;

use charter_program::{ENGINE_RHAI_V1, ENGINE_RHAI_V2, Event, Host, Program, engine_for};
use common::*;
use serde_json::json;

/// Commits 0x10 (base) and 0x11, 0x12, 0x14 (heads; 0x14 adds a non-UTF-8 path); 0x13
/// has no objects.
struct Fake;

impl Host for Fake {
    fn changed_paths(&self, a: &[u8], b: &[u8]) -> Option<Result<Vec<String>, String>> {
        match (a.first()?, b.first()?) {
            (0x10, 0x11) => Some(Ok(vec!["src/lib.rs".into(), "README.md".into()])),
            (0x10, 0x12) => Some(Ok(vec!["notes.txt".into()])),
            (0x10, 0x14) => Some(Err("path \"latin1-\u{fffd}.txt\" is not UTF-8".into())),
            _ => None,
        }
    }
    fn file_at(&self, commit: &[u8], path: &str) -> Option<Option<Vec<u8>>> {
        let rules = format!("*.rs {}\n", hx(0xa1));
        match (*commit.first()?, path) {
            (0x10, ".github/CODEOWNERS") => Some(Some(rules.into_bytes())),
            (0x10..=0x12 | 0x14, _) => Some(None),
            _ => None,
        }
    }
}

fn source() -> String {
    let path = format!(
        "{}/../../programs/codeowners.rhai",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(path).expect("template file")
}

fn program() -> Program {
    let src = source();
    Program::load(engine_for(&src), &src)
        .expect("load")
        .with_host(Rc::new(Fake))
}

/// Main at 0x10, a patch at `head`, an approval by `by`, then main moves to `head`.
fn events(head: u8, by: u8) -> Vec<Event> {
    let patch = json!({"title": "t", "head": oid_hex(head)});
    let review = json!({"patch": hx(0x50), "head": oid_hex(head)});
    vec![
        main(1, None, 0x10),
        op(2, 5, 0x50, "xyz.arkiv.patch", "create", patch),
        op(3, by, 0x51, "xyz.arkiv.review", "approve", review),
        main(4, Some(0x10), head),
    ]
}

fn fold(p: &Program, evs: Vec<Event>) -> charter_program::Outcome {
    p.fold(repo(), evs).expect("fold")
}

#[test]
fn programs_that_read_git_are_pinned_as_rhai_2() {
    assert_eq!(engine_for(&source()), ENGINE_RHAI_V2);
    assert_eq!(engine_for("fn init(r) { 0 } // file_at"), ENGINE_RHAI_V1);
    assert_eq!(engine_for("fn init(r) { my_file_at(1) }"), ENGINE_RHAI_V1);
}

#[test]
fn main_needs_an_owner_approval_for_each_owned_path() {
    let ok = fold(&program(), events(0x11, 0xa1));
    assert_eq!(ok.accepted.len(), 2, "{:?}", ok.rejected);
    let no = fold(&program(), events(0x11, 0xd1));
    let why = &no.rejected.first().expect("rejected").1;
    assert!(why.contains("src/lib.rs needs an approval"), "{why}");
    let unowned = fold(&program(), events(0x12, 0xd2));
    assert!(unowned.rejected.is_empty(), "{:?}", unowned.rejected);
    let mut stray = events(0x11, 0xa1);
    stray[1] = op(
        2,
        5,
        0x50,
        "xyz.arkiv.patch",
        "create",
        json!({"head": oid_hex(0x12)}),
    );
    let o = fold(&program(), stray);
    assert!(
        o.rejected[0].1.contains("only to a patch head"),
        "{:?}",
        o.rejected
    );
}

#[test]
fn missing_objects_reject_and_are_never_checkpointed() {
    let src = source();
    let p = program();
    let evs = events(0x13, 0xa1);
    let (o, cp) = p
        .fold_cached(
            &src,
            repo(),
            BTreeMap::new(),
            &evs,
            &|_, _| Some(true),
            None,
        )
        .expect("fold");
    assert_eq!(o.rejected.len(), 1, "fail closed");
    assert!(
        cp.is_none(),
        "the verdict is taken again once objects arrive"
    );
    let (_, cp) = p
        .fold_cached(
            &src,
            repo(),
            BTreeMap::new(),
            &events(0x11, 0xa1),
            &|_, _| Some(true),
            None,
        )
        .expect("fold");
    assert!(cp.is_some());
}

#[test]
fn a_non_utf8_path_rejects_but_the_fold_stays_decided() {
    let src = source();
    let (o, cp) = program()
        .fold_cached(
            &src,
            repo(),
            BTreeMap::new(),
            &events(0x14, 0xa1),
            &|_, _| Some(true),
            None,
        )
        .expect("fold");
    let why = &o.rejected.first().expect("rejected").1;
    assert!(why.contains("not UTF-8"), "{why}");
    assert!(
        cp.is_some(),
        "every object is here, so the verdict is final"
    );
}

#[test]
fn without_a_host_or_on_rhai_1_the_functions_fail_closed() {
    let src = source();
    let dry = Program::load(ENGINE_RHAI_V2, &src).expect("load");
    let o = fold(&dry, events(0x11, 0xa1));
    assert!(
        o.rejected[0].1.contains("no git objects"),
        "{:?}",
        o.rejected
    );
    let old = Program::load(ENGINE_RHAI_V1, &src)
        .expect("load")
        .with_host(Rc::new(Fake));
    let o = fold(&old, events(0x11, 0xa1));
    assert!(
        o.rejected[0].1.contains("program error"),
        "{:?}",
        o.rejected
    );
}

#[test]
fn host_answers_are_memoized_and_capped() {
    struct Count(std::cell::Cell<u32>);
    impl Host for Count {
        fn changed_paths(&self, _: &[u8], _: &[u8]) -> Option<Result<Vec<String>, String>> {
            self.0.set(self.0.get() + 1);
            Some(Ok(vec![]))
        }
        fn file_at(&self, _: &[u8], _: &str) -> Option<Option<Vec<u8>>> {
            Some(None)
        }
    }
    let src = r#"
        fn init(r) { 0 }
        fn check_push(s, p) {
            if p.old == "" { return; }
            for i in 0..3 { changed_paths(p.old, p.head); }
            let n = 0;
            for i in 0..600 { file_at(p.head, `f${i}`); n += 1; }
        }"#;
    let host = Rc::new(Count(std::cell::Cell::new(0)));
    let p = Program::load(ENGINE_RHAI_V2, src)
        .expect("load")
        .with_host(host.clone());
    let o = fold(&p, vec![main(1, None, 0x10), main(2, Some(0x10), 0x11)]);
    assert_eq!(host.0.get(), 1, "same question, one host call");
    assert!(
        o.rejected[0].1.contains("host call budget"),
        "{:?}",
        o.rejected
    );
}
