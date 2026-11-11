//! The spend ledger (docs/p6/ui-v2.md 4.4): `$CHARTER_HOME/ui/spend.json`, one entry per
//! confirmed write from this UI, with the fees its receipts show. Local only.

use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

pub const FILE: &str = "ui/spend.json";
const VERSION: u32 = 1;

/// One confirmed write.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spend {
    pub ts_ms: u64,
    /// Block of its last transaction.
    pub block: u64,
    /// Repo id, hex.
    pub repo: String,
    pub action: String,
    /// Fees paid, wei (decimal).
    pub wei: String,
    /// Moved besides fees (an escrow deposit), wei (decimal).
    pub value: String,
    pub txs: usize,
    /// Sent without the cost dialog (the small-write setting).
    pub skipped: bool,
    /// The fee is the estimate: a receipt was not seen for every transaction.
    pub estimated: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    v: u32,
    entries: Vec<Spend>,
}

/// Serializes appends within this process.
static LOCK: Mutex<()> = Mutex::new(());

fn load(path: &Path) -> Result<Ledger, String> {
    match std::fs::read(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Ledger::default()),
        Err(e) => Err(e.to_string()),
        Ok(b) => {
            let l: Ledger = serde_json::from_slice(&b).map_err(|e| e.to_string())?;
            match l.v {
                VERSION => Ok(l),
                v => Err(format!("unknown ledger version {v}")),
            }
        }
    }
}

/// Every entry, newest first.
pub fn read(home: &Path) -> Result<Vec<Spend>, String> {
    let mut e = load(&home.join(FILE))?.entries;
    e.reverse();
    Ok(e)
}

/// Appends `s`, atomically (mode 0600). An unreadable ledger is not overwritten.
pub fn record(home: &Path, s: Spend) -> Result<(), String> {
    let _held = LOCK
        .lock()
        .map_err(|_| "spend ledger lock poisoned".to_owned())?;
    let path = home.join(FILE);
    let mut l = load(&path)?;
    l.v = VERSION;
    l.entries.push(s);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    charter_sync::state::write_json(&path, &l).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(block: u64) -> Spend {
        Spend {
            ts_ms: 1,
            block,
            repo: "ab".into(),
            action: "Comment".into(),
            wei: "2164000000000000".into(),
            value: "0".into(),
            txs: 2,
            skipped: false,
            estimated: false,
        }
    }

    #[test]
    fn entries_append_and_read_newest_first_and_a_bad_file_is_kept() {
        let home = tempfile::tempdir().expect("test");
        assert!(read(home.path()).expect("test").is_empty());
        record(home.path(), entry(1)).expect("test");
        record(home.path(), entry(2)).expect("test");
        assert_eq!(read(home.path()).expect("test"), vec![entry(2), entry(1)]);
        std::fs::write(home.path().join(FILE), b"{oops").expect("test");
        assert!(read(home.path()).is_err());
        assert!(record(home.path(), entry(3)).is_err());
        assert_eq!(
            std::fs::read(home.path().join(FILE)).expect("test"),
            b"{oops"
        );
    }
}
