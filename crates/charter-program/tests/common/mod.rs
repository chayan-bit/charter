#![allow(dead_code)]

use charter_program::*;
use serde_json::{Value, json};

pub fn id(b: u8) -> [u8; 32] {
    [b; 32]
}

pub fn hx(b: u8) -> String {
    hex::encode(id(b))
}

pub fn oid(b: u8) -> Vec<u8> {
    vec![b; 20]
}

pub fn oid_hex(b: u8) -> String {
    hex::encode(oid(b))
}

/// Delegates 0xd1, 0xd2, 0xd3 with quorum 2.
pub fn repo() -> RepoInfo {
    RepoInfo {
        rid: id(0xee),
        delegates: vec![id(0xd1), id(0xd2), id(0xd3)],
        quorum: 2,
        name: "demo".into(),
    }
}

pub fn op(order: u64, author: u8, object: u8, ty: &str, action: &str, payload: Value) -> Event {
    Event::Op(ProgramOp {
        id: id(order as u8),
        author: id(author),
        rid: id(0xee),
        object: id(object),
        type_name: ty.into(),
        action: action.into(),
        parents: vec![],
        payload: cbor_from_json(&payload).expect("payload"),
        ts_ms: 1_700_000_000_000 + order,
        arkiv_order: order,
        proofs: vec![],
    })
}

pub fn push(order: u64, author: u8, name: &str, old: Option<u8>, new: u8) -> Event {
    Event::Push(RefUpdate {
        author: id(author),
        name: name.into(),
        old: old.map(oid).unwrap_or_default(),
        new: oid(new),
        arkiv_order: order,
        signed_base: false,
    })
}

pub fn main(order: u64, old: Option<u8>, new: u8) -> Event {
    push(order, 0xd1, "refs/heads/main", old, new)
}

pub fn template(name: &str) -> Program {
    let path = format!("{}/../../programs/{name}.rhai", env!("CARGO_MANIFEST_DIR"));
    let src = std::fs::read_to_string(&path).expect("template file");
    Program::load(ENGINE_RHAI_V1, &src).expect("template loads")
}

pub fn load(src: &str) -> Program {
    Program::load(ENGINE_RHAI_V1, src).expect("loads")
}

pub fn state(o: &Outcome) -> Value {
    serde_json::from_str(&o.state_json).expect("state json")
}

pub fn empty() -> Value {
    json!({})
}
