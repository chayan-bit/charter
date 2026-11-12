//! The hardened Rhai engine: no floats, time, eval, modules or file access; side effects of
//! `print`/`debug` go to a log; every call and every fold has an operation budget.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use rhai::{AST, Blob, CallFnOptions, Dynamic, Engine, EvalAltResult, FuncArgs, Scope};

use crate::ProgramError;
use crate::host::{self, Calls};

/// Operation budget for one whole fold.
pub(crate) const FOLD_FUEL: u64 = 5_000_000;
/// Operation budget for one ABI call.
pub(crate) const CALL_FUEL: u64 = 200_000;

pub(crate) type Logs = Rc<RefCell<Vec<String>>>;

fn sha256_hex(x: &[u8]) -> String {
    hex::encode(charter_crypto::sha256(x))
}

/// A fresh engine with every determinism and resource limit set.
fn hardened(logs: &Logs, spent: &Rc<Cell<u64>>, calls: Option<&Rc<Calls>>) -> Engine {
    let mut e = Engine::new();
    e.disable_symbol("eval");
    e.set_max_operations(CALL_FUEL)
        .set_max_string_size(64 * 1024)
        .set_max_array_size(10_000)
        .set_max_map_size(10_000)
        .set_max_call_levels(32)
        .set_max_expr_depths(64, 32);
    let (p, d) = (logs.clone(), logs.clone());
    e.on_print(move |s| p.borrow_mut().push(s.into()));
    e.on_debug(move |s, _, pos| d.borrow_mut().push(format!("debug {pos}: {s}")));
    let spent = spent.clone();
    e.on_progress(move |_| {
        spent.set(spent.get() + 1);
        (spent.get() > FOLD_FUEL).then(|| "fold operation budget exhausted".into())
    });
    e.register_fn("sha256_hex", |s: &str| sha256_hex(s.as_bytes()));
    e.register_fn("sha256_hex", |b: Blob| sha256_hex(&b));
    if let Some(c) = calls {
        host::register(&mut e, c);
    }
    e
}

pub(crate) fn compile(source: &str) -> Result<AST, ProgramError> {
    let (logs, spent) = (Logs::default(), Rc::default());
    hardened(&logs, &spent, None)
        .compile(source)
        .map_err(|e| ProgramError::Compile(e.to_string()))
}

/// One engine per fold, with its log and fuel meter.
pub(crate) struct Runtime {
    engine: Engine,
    pub(crate) logs: Logs,
    /// Operations spent so far in this fold (a resumed fold starts where it paused).
    pub(crate) spent: Rc<Cell<u64>>,
    /// The git host of an rhai/2 program; `None` on rhai/1, which has no host functions.
    pub(crate) calls: Option<Rc<Calls>>,
}

impl Runtime {
    pub(crate) fn new(calls: Option<Rc<Calls>>) -> Self {
        Self::resumed(vec![], 0, calls)
    }

    /// A runtime that continues a paused fold: its log so far and the fuel it spent.
    pub(crate) fn resumed(logs: Vec<String>, spent: u64, calls: Option<Rc<Calls>>) -> Self {
        let (logs, spent) = (Rc::new(RefCell::new(logs)), Rc::new(Cell::new(spent)));
        Self {
            engine: hardened(&logs, &spent, calls.as_ref()),
            logs,
            spent,
            calls,
        }
    }

    /// A host question had no answer (objects missing).
    pub(crate) fn undecided(&self) -> bool {
        self.calls.as_ref().is_some_and(|c| c.undecided.get())
    }

    /// Calls script function `name`; the top level of the script is never re-run.
    pub(crate) fn call(
        &self,
        ast: &AST,
        name: &str,
        args: impl FuncArgs,
    ) -> Result<Dynamic, String> {
        let opts = CallFnOptions::new().eval_ast(false).rewind_scope(true);
        self.engine
            .call_fn_with_options::<Dynamic>(opts, &mut Scope::new(), ast, name, args)
            .map_err(|e| match *e {
                EvalAltResult::ErrorTerminated(t, _) => t.to_string(),
                e => e.to_string(),
            })
    }

    pub(crate) fn log(&self, line: String) {
        self.logs.borrow_mut().push(line);
    }
}
