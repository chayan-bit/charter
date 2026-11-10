//! The fold: `init`, then each event in Arkiv order, then `obligations`.
//!
//! Failure policy: an error in `init` fails the fold; in `on_op` or `on_push` the event is
//! logged and the state is unchanged; in `check_push` the push is rejected (fail closed).

use std::collections::BTreeMap;

use rhai::{Array, Dynamic};

use crate::checkpoint::{Checkpoint, Val};
use crate::engine::Runtime;
use crate::value::{op_map, push_map, repo_map};
use crate::{Ancestry, Event, Outcome, Program, ProgramError, ProgramOp, RefUpdate, RepoInfo};

pub(crate) const NOT_FF: &str = "not a fast-forward of the accepted head";

pub(crate) struct Folder<'a> {
    prog: &'a Program,
    ancestry: Ancestry<'a>,
    rt: Runtime,
    state: Dynamic,
    heads: BTreeMap<String, Vec<u8>>,
    accepted: Vec<RefUpdate>,
    rejected: Vec<(RefUpdate, String)>,
    last: u64,
    /// Some ancestry question had no answer (objects missing): not checkpointable.
    undecided: bool,
}

pub(crate) fn run(
    prog: &Program,
    repo: &RepoInfo,
    seed: BTreeMap<String, Vec<u8>>,
    events: impl IntoIterator<Item = Event>,
    ancestry: Ancestry<'_>,
) -> Result<Outcome, ProgramError> {
    let mut f = Folder::start(prog, repo, seed, ancestry)?;
    f.events(events)?;
    f.finish()
}

impl<'a> Folder<'a> {
    /// Runs `init` with an empty log and full fuel.
    pub(crate) fn start(
        prog: &'a Program,
        repo: &RepoInfo,
        seed: BTreeMap<String, Vec<u8>>,
        ancestry: Ancestry<'a>,
    ) -> Result<Self, ProgramError> {
        let rt = Runtime::new();
        let state = repo_map(repo)
            .and_then(|r| rt.call(&prog.ast, "init", (r,)))
            .map_err(ProgramError::Init)?;
        Ok(Self {
            prog,
            ancestry,
            rt,
            state,
            heads: seed,
            accepted: vec![],
            rejected: vec![],
            last: 0,
            undecided: false,
        })
    }

    /// Continues the fold `cp` paused (its state, heads, verdicts, log and spent fuel).
    pub(crate) fn resume(prog: &'a Program, cp: &Checkpoint, ancestry: Ancestry<'a>) -> Self {
        Self {
            prog,
            ancestry,
            rt: Runtime::resumed(cp.logs.clone(), cp.spent),
            state: cp.state.to_dynamic(),
            heads: cp.heads.clone(),
            accepted: cp.accepted.clone(),
            rejected: cp.rejected.clone(),
            last: cp.last_order,
            undecided: false,
        }
    }

    pub(crate) fn events(
        &mut self,
        events: impl IntoIterator<Item = Event>,
    ) -> Result<(), ProgramError> {
        for ev in events {
            if ev.order() < self.last {
                return Err(ProgramError::OutOfOrder(ev.order()));
            }
            self.last = ev.order();
            match ev {
                Event::Op(op) => self.op(&op),
                Event::Push(p) => self.push(p),
            }
        }
        Ok(())
    }

    /// The fold so far as a checkpoint, before `obligations` runs; `None` when it could
    /// not be restored exactly.
    pub(crate) fn checkpoint(&self, base: Checkpoint) -> Option<Checkpoint> {
        if self.undecided {
            return None;
        }
        Some(Checkpoint {
            last_order: self.last,
            state: Val::of(&self.state)?,
            heads: self.heads.clone(),
            accepted: self.accepted.clone(),
            rejected: self.rejected.clone(),
            logs: self.rt.logs.borrow().clone(),
            spent: self.rt.spent.get(),
            ..base
        })
    }
}

impl Folder<'_> {
    fn call(&self, name: &str, arg: Result<Dynamic, String>) -> Result<Dynamic, String> {
        self.rt
            .call(&self.prog.ast, name, (self.state.clone(), arg?))
    }

    fn op(&mut self, op: &ProgramOp) {
        if !self.prog.abi.on_op {
            return;
        }
        match self.call("on_op", op_map(op)) {
            Ok(s) => self.state = s,
            Err(e) => self
                .rt
                .log(format!("op {} skipped: {e}", hex::encode(op.id))),
        }
    }

    fn push(&mut self, mut p: RefUpdate) {
        // `old` becomes the accepted head; `ff` is real git ancestry from it to `new` when
        // the client has the objects, else whether the pusher's claimed `old` matches it.
        // A signed base (format v2) is decided by the claim alone, never by local objects.
        let head = self.heads.get(&p.name).cloned().unwrap_or_default();
        let ff = head.is_empty()
            || p.new.is_empty()
            || head == p.new
            || (p.signed_base && head == p.old)
            || !p.signed_base && match (self.ancestry)(&head, &p.new) {
                Some(known) => known,
                None => {
                    self.undecided = true;
                    head == p.old
                }
            };
        p.old = head;
        let verdict = match self.prog.abi.check_push {
            true => self
                .call("check_push", push_map(&p, ff))
                .map_err(|e| format!("program error: {e}"))
                .and_then(|a| decide(a, ff)),
            false => decide(Dynamic::UNIT, ff),
        };
        if let Err(reason) = verdict {
            self.rejected.push((p, reason));
            return;
        }
        match p.new.is_empty() {
            true => self.heads.remove(&p.name),
            false => self.heads.insert(p.name.clone(), p.new.clone()),
        };
        if self.prog.abi.on_push {
            match self.call("on_push", push_map(&p, ff)) {
                Ok(s) => self.state = s,
                Err(e) => self.rt.log(format!("on_push for {} failed: {e}", p.name)),
            }
        }
        self.accepted.push(p);
    }

    fn obligations(&self) -> Vec<serde_json::Value> {
        if !self.prog.abi.obligations {
            return vec![];
        }
        let r = self
            .rt
            .call(&self.prog.ast, "obligations", (self.state.clone(),))
            .and_then(|v| v.try_cast::<Array>().ok_or("not an array".into()))
            .and_then(|a| {
                a.iter()
                    .map(|o| serde_json::to_value(o).map_err(|e| e.to_string()))
                    .collect()
            });
        r.unwrap_or_else(|e| {
            self.rt.log(format!("obligations failed: {e}"));
            vec![]
        })
    }

    pub(crate) fn finish(self) -> Result<Outcome, ProgramError> {
        let obligations = self.obligations();
        let state_json =
            serde_json::to_string(&self.state).map_err(|e| ProgramError::State(e.to_string()))?;
        Ok(Outcome {
            state_json,
            accepted: self.accepted,
            rejected: self.rejected,
            obligations,
            logs: self.rt.logs.take(),
            heads: self.heads,
        })
    }
}

/// `()` allows a fast-forward; `true` allows anything; a string is the reject reason.
fn decide(answer: Dynamic, ff: bool) -> Result<(), String> {
    if answer.is_unit() {
        return if ff { Ok(()) } else { Err(NOT_FF.into()) };
    }
    if let Ok(b) = answer.as_bool() {
        return if b {
            Ok(())
        } else {
            Err("rejected by program".into())
        };
    }
    match answer.into_immutable_string() {
        Ok(s) => Err(s.to_string()),
        Err(t) => Err(format!("check_push returned unsupported {t}")),
    }
}
