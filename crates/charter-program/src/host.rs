//! Git reads for programs on engine rhai/2 (issue #53, docs/p3/programs.md "Git host
//! functions"): `changed_paths(a, b)`, `file_at(commit, path)` and the pure
//! `codeowners(text, path)`. Git objects are content addressed, so an answer is the same
//! on every client that has the objects. A client without them gets an error (a push check
//! then rejects, fail closed) and the fold is marked undecided, so it is never checkpointed
//! and the verdict is taken again once the objects arrive.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use rhai::{Array, Dynamic, Engine};

/// Distinct host questions one fold may ask; repeated questions are answered from memory.
pub const MAX_HOST_CALLS: u32 = 256;
/// Bytes `file_at` returns (a script string's limit).
const MAX_FILE: usize = 64 * 1024;
/// Paths `changed_paths` returns (a script array's limit).
const MAX_PATHS: usize = 10_000;

/// The git a program may read, supplied by the client that folds it.
pub trait Host {
    /// Repo paths that differ between commits `a` and `b` (every path of `b` when `a` is
    /// empty); `None` only when the objects are missing, and `Some(Err)` for a decided
    /// answer that is not a path list (a path that is not UTF-8).
    fn changed_paths(&self, a: &[u8], b: &[u8]) -> Option<Result<Vec<String>, String>>;
    /// The blob at `path` in commit `commit`: `Some(None)` when there is no such file,
    /// `None` when the objects are missing.
    fn file_at(&self, commit: &[u8], path: &str) -> Option<Option<Vec<u8>>>;
}

type Answer = Result<Dynamic, String>;
type Fallible = Result<Dynamic, Box<rhai::EvalAltResult>>;

/// A host shared by one fold's engine: memo, call budget and the undecided flag.
pub(crate) struct Calls {
    host: Option<Rc<dyn Host>>,
    memo: RefCell<HashMap<String, Answer>>,
    asked: Cell<u32>,
    pub(crate) undecided: Cell<bool>,
}

impl Calls {
    pub(crate) fn new(host: Option<Rc<dyn Host>>) -> Rc<Self> {
        Rc::new(Self {
            host,
            memo: RefCell::default(),
            asked: Cell::new(0),
            undecided: Cell::new(false),
        })
    }

    fn ask(&self, key: String, f: impl FnOnce(&dyn Host) -> Option<Answer>) -> Answer {
        if let Some(a) = self.memo.borrow().get(&key) {
            return a.clone();
        }
        let host = self
            .host
            .as_deref()
            .ok_or("no git objects here (a dry run): git host functions need a client")?;
        if self.asked.get() >= MAX_HOST_CALLS {
            return Err(format!("host call budget of {MAX_HOST_CALLS} exhausted"));
        }
        self.asked.set(self.asked.get() + 1);
        let a = f(host).unwrap_or_else(|| {
            self.undecided.set(true);
            Err(format!("git objects missing for {key}"))
        });
        self.memo.borrow_mut().insert(key, a.clone());
        a
    }
}

/// A commit id: empty (only where allowed) or 40 or 64 lowercase hex digits.
fn oid(s: &str, empty_ok: bool) -> Result<Vec<u8>, String> {
    let ok = matches!(s.len(), 40 | 64) || (empty_ok && s.is_empty());
    let hexdigits = s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match ok && hexdigits {
        true => hex::decode(s).map_err(|e| e.to_string()),
        false => Err(format!("{s:?} is not a commit id")),
    }
}

fn changed(c: &Calls, a: &str, b: &str) -> Answer {
    let (ka, kb) = (oid(a, true)?, oid(b, false)?);
    c.ask(format!("diff {a} {b}"), |h| {
        Some(match h.changed_paths(&ka, &kb)? {
            Err(e) => Err(e),
            Ok(paths) if paths.len() > MAX_PATHS => {
                Err(format!("more than {MAX_PATHS} changed paths"))
            }
            Ok(paths) => Ok(paths
                .into_iter()
                .map(Dynamic::from)
                .collect::<Array>()
                .into()),
        })
    })
}

fn file(c: &Calls, commit: &str, path: &str) -> Answer {
    let k = oid(commit, false)?;
    c.ask(format!("file {commit}:{path}"), |h| {
        Some(match h.file_at(&k, path)? {
            None => Ok(Dynamic::UNIT),
            Some(b) if b.len() > MAX_FILE => Err(format!("{path} is over {MAX_FILE} bytes")),
            Some(b) => String::from_utf8(b)
                .map(Dynamic::from)
                .map_err(|_| format!("{path} is not UTF-8")),
        })
    })
}

/// Registers the rhai/2 functions on `e`.
pub(crate) fn register(e: &mut Engine, calls: &Rc<Calls>) {
    let (c1, c2) = (calls.clone(), calls.clone());
    e.register_fn("changed_paths", move |a: &str, b: &str| -> Fallible {
        changed(&c1, a, b).map_err(Into::into)
    });
    e.register_fn("file_at", move |commit: &str, path: &str| -> Fallible {
        file(&c2, commit, path).map_err(Into::into)
    });
    e.register_fn("codeowners", |text: &str, path: &str| {
        crate::codeowners::owners(text, path)
            .into_iter()
            .map(Dynamic::from)
            .collect::<Array>()
    });
}

/// Names of the rhai/2 functions, for [`crate::engine_for`].
pub(crate) const NAMES: [&str; 3] = ["changed_paths", "file_at", "codeowners"];
