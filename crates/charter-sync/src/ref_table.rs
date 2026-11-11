//! Ref tables (docs/p5/head-model.md section 14): reading a v4 head's table from calldata
//! into its effective refs, and the writer's choice between an inline v3 body, reusing the
//! previous table, and a new one.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use charter_core::{
    Device, DeviceHead, ErrorCode, Hash32, HeadBody, HeadRef, MAX_TABLE_SIZE, RefTable, Rid, codec,
    decode_table, effective_refs, encode_table,
};
use charter_store::Arkiv;

use crate::state::LandingJson;
use crate::{Result, SyncError};

/// Most bytes of differing entries a body lists inline while it reuses a table (14.3).
pub const REUSE_INLINE_MAX: usize = 16 * 1024;
/// Head body bytes besides refs and packs (fixed fields, snapshot hashes), generously.
const BODY_FIXED: usize = 1_024;

/// Decoded tables by `table_id`; the hash names the bytes, so sharing across devices and
/// repos is safe. shortcut: per process only, persist it under the repo cache if CLI
/// fetches of large devices become common (head-model 14.4).
static TABLES: LazyLock<Mutex<HashMap<Hash32, Arc<Vec<HeadRef>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn bad(t: &RefTable, what: &str) -> SyncError {
    SyncError::coded(
        ErrorCode::BadRefs,
        format!("ref table {}: {what}", hex::encode(t.table_id)),
    )
}

fn sha256(b: &[u8]) -> Hash32 {
    use sha2::{Digest, Sha256};
    Sha256::digest(b).into()
}

/// A table already read or written by this process.
pub fn cached(table_id: &Hash32) -> Option<Arc<Vec<HeadRef>>> {
    TABLES.lock().ok()?.get(table_id).cloned()
}

/// Remembers a verified table (read from calldata, written by this push, or from a cache
/// file that stored only verified tables).
pub fn remember(table_id: Hash32, refs: Arc<Vec<HeadRef>>) {
    if let Ok(mut t) = TABLES.lock() {
        t.insert(table_id, refs);
    }
}

/// The table `t` of `dev`: from memory, else its pieces from `B_d`'s calldata, checked
/// against `table_id` and decoded under [`MAX_TABLE_SIZE`].
pub async fn load<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    t: &RefTable,
) -> Result<Arc<Vec<HeadRef>>> {
    if let Some(refs) = cached(&t.table_id) {
        return Ok(refs);
    }
    let senders = [dev.bulk].into();
    // Table runs never name the head tx (validated), so the landing is unused.
    let here = LandingJson {
        tx: [0; 32],
        block: 0,
        index: 0,
    };
    let stored = crate::pieces::run_bytes(a, rid, &senders, &here, &t.runs).await?;
    if stored.is_empty() {
        return Err(SyncError::coded(
            ErrorCode::NotLive,
            format!(
                "ref table {}: its pieces were not found in calldata",
                hex::encode(t.table_id)
            ),
        ));
    }
    if sha256(&stored) != t.table_id {
        return Err(bad(t, "pieces do not hash to table_id"));
    }
    let codec = t.codec().map_err(|e| bad(t, &e.to_string()))?;
    let bytes =
        codec::decode(codec, &stored, MAX_TABLE_SIZE).map_err(|e| bad(t, &e.to_string()))?;
    let refs = Arc::new(decode_table(&bytes).map_err(|e| bad(t, &e.to_string()))?);
    remember(t.table_id, refs.clone());
    Ok(refs)
}

/// `body` (a v4 body's shared fields) with its effective refs.
pub async fn effective<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    body: &HeadBody,
    t: &RefTable,
) -> Result<HeadBody> {
    let table = load(a, rid, dev, t).await?;
    effective_refs(body, t, &table).map_err(|e| bad(t, &e.to_string()))
}

/// Overlays a v4 head's table, so every later step (merge, fold, push) reads complete refs.
pub async fn resolve<A: Arkiv>(
    a: &A,
    rid: &Rid,
    dev: &Device,
    head: &mut DeviceHead,
) -> Result<()> {
    if let Some(t) = head.refs_table.clone() {
        head.body = effective(a, rid, dev, &head.body, &t).await?;
    }
    Ok(())
}

/// How the next body carries its refs (head-model 14.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Choice {
    /// A v3 body listing every entry.
    Inline,
    /// The previous table, with only the differing entries inline.
    Reuse {
        table: RefTable,
        inline: Vec<HeadRef>,
    },
    /// A new table to upload before the head tx, and the entries listed inline.
    New {
        table: Vec<HeadRef>,
        inline: Vec<HeadRef>,
    },
}

impl Choice {
    /// The entries field 7 lists.
    pub fn inline<'a>(&'a self, all: &'a [HeadRef]) -> &'a [HeadRef] {
        match self {
            Choice::Inline => all,
            Choice::Reuse { inline, .. } | Choice::New { inline, .. } => inline,
        }
    }
}

/// The previous head's table, its decoded entries and the block its body landed in.
pub type PrevTable<'a> = (&'a RefTable, &'a [HeadRef], u64);

/// The choice for entries `all` when a body with `packs` bytes of pack list has room for
/// `max` bytes (`max_body_len` of the device key).
pub fn choose(all: &[HeadRef], prev: Option<PrevTable<'_>>, packs: usize, max: usize) -> Choice {
    let fits = |refs: &[HeadRef]| BODY_FIXED + encode_table(refs).len() + packs <= max;
    if fits(all) {
        return Choice::Inline;
    }
    if let Some((t, rows, landing)) = prev {
        let table = t.resolved(landing);
        let names: BTreeSet<&str> = all.iter().map(|e| e.name.as_str()).collect();
        let kept = rows.iter().all(|e| names.contains(e.name.as_str()));
        let stamped: BTreeMap<String, HeadRef> = table
            .stamped(rows)
            .into_iter()
            .map(|e| (e.name.clone(), e))
            .collect();
        let inline: Vec<HeadRef> = all
            .iter()
            .filter(|e| stamped.get(&e.name) != Some(*e))
            .cloned()
            .collect();
        if kept && encode_table(&inline).len() <= REUSE_INLINE_MAX && fits(&inline) {
            return Choice::Reuse { table, inline };
        }
    }
    let (inline, table): (Vec<HeadRef>, Vec<HeadRef>) =
        all.iter().cloned().partition(|e| !e.base.is_empty());
    match fits(&inline) {
        true => Choice::New { table, inline },
        false => Choice::New {
            table: all.to_vec(),
            inline: vec![],
        },
    }
}

/// The stored bytes of table `refs` (brotli when smaller) and their codec and SHA-256.
pub fn stored(refs: &[HeadRef]) -> (codec::Codec, Vec<u8>, Hash32) {
    let (c, bytes) = codec::encode(&encode_table(refs));
    let id = sha256(&bytes);
    (c, bytes, id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(i: usize, oid: u8, stamp: u64, base: bool) -> HeadRef {
        HeadRef {
            name: format!("refs/tags/v{i:05}"),
            oid: vec![oid; 20],
            counter: 1,
            stamp,
            base: if base { vec![1; 20] } else { vec![] },
        }
    }

    fn many(n: usize) -> Vec<HeadRef> {
        (0..n).map(|i| e(i, 2, 0, false)).collect()
    }

    const MAX: usize = 62_119;

    #[test]
    fn a_body_that_fits_stays_inline() {
        assert_eq!(choose(&many(10), None, 0, MAX), Choice::Inline);
    }

    #[test]
    fn an_overflowing_first_push_puts_entries_without_base_into_a_new_table() {
        let mut all = many(5_000);
        all[7] = e(7, 3, 0, true);
        let Choice::New { table, inline } = choose(&all, None, 0, MAX) else {
            panic!("a new table");
        };
        assert_eq!(
            inline,
            vec![all[7].clone()],
            "the fast-forward stays inline"
        );
        assert_eq!(table.len(), 4_999);
    }

    #[test]
    fn a_push_that_moves_one_ref_reuses_the_table_with_concrete_stamps() {
        let rows = many(5_000);
        let t = RefTable {
            table_id: [5; 32],
            codec: 1,
            runs: vec![charter_core::PieceRun {
                block: 9,
                index: 0,
                count: 3,
            }],
            stamp: 0,
        };
        // The next plan: every entry carried with its stamp resolved at landing 12, one moved.
        let mut all: Vec<HeadRef> = rows
            .iter()
            .map(|r| HeadRef {
                stamp: 12,
                ..r.clone()
            })
            .collect();
        all[42] = e(42, 9, 0, true);
        let Choice::Reuse { table, inline } = choose(&all, Some((&t, &rows, 12)), 0, MAX) else {
            panic!("reuse");
        };
        assert_eq!(table.stamp, 12);
        assert_eq!(table.table_id, t.table_id);
        assert_eq!(inline, vec![all[42].clone()]);
        // A name the table lists but the plan dropped forces a new table.
        all.remove(3);
        assert!(matches!(
            choose(&all, Some((&t, &rows, 12)), 0, MAX),
            Choice::New { .. }
        ));
    }
}
