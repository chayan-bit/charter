//! What took a pending commit's nonce once it is used: the recorded txs' receipts, or else
//! the tx mined at that nonce itself, found in the blocks after [`Pending::from`]. Read only
//! on that abnormal path ([`crate::group::ready`]).

use charter_store::{Address, Arkiv, Field, Pred, Query, Select, StoreError, TxHash};

use crate::mls::Pending;

/// Blocks one attempt reads at most while looking for the tx at a pending nonce; the next
/// attempt goes on down from [`Pending::cursor`].
pub(crate) const SCAN: u64 = 256;

/// What took a pending commit's recorded nonce.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Taken {
    /// One of the commit's txs landed.
    Commit,
    /// A tx that cannot carry it: one of its own that reverted, a recorded plug or grant,
    /// or a tx of the same address this home never recorded (another machine's).
    Other,
    /// Neither a recorded receipt nor the tx at that nonce is found yet.
    Unknown,
}

/// What took `p`'s nonce, which `c` (the control address) has used. Receipts first; with
/// none found (a node behind, or receipts pruned), the tx mined at that nonce, which moves
/// `p.cursor` on (the caller saves it).
pub(crate) async fn taken_by<A: Arkiv>(
    a: &A,
    c: Address,
    p: &mut Pending,
) -> Result<Taken, StoreError> {
    let mut other = false;
    for h in &p.txs {
        match a.receipt(*h).await? {
            Some(r) if r.success => return Ok(Taken::Commit),
            Some(_) => other = true,
            None => {}
        }
    }
    for h in &p.others {
        if other {
            break;
        }
        other = a.receipt(*h).await?.is_some();
    }
    if other {
        return Ok(Taken::Other);
    }
    Ok(match at_nonce(a, c, p).await? {
        // shortcut: a commit tx found with no receipt (pruned) is taken as landed, though it
        // may have reverted and a crash kept it from being resent; upgrade if receipts prune.
        Some(h) if p.txs.contains(&h) => Taken::Commit,
        Some(_) => Taken::Other,
        None => Taken::Unknown,
    })
}

/// Where block `b` puts the tx of `c` at nonce `n`.
enum Seen {
    Holds(TxHash),
    /// Every tx of `c` in `b` is below `n`: it is mined later.
    Before,
    /// A tx of `c` in `b` is above `n`: it is mined earlier.
    After,
    /// No tx of `c` (or a node that does not have `b` yet).
    Absent,
}

async fn seen<A: Arkiv>(a: &A, c: Address, n: u64, b: u64) -> Result<Seen, StoreError> {
    let mut seen = Seen::Absent;
    for tx in a.block_txs(b).await?.into_iter().filter(|t| t.from == c) {
        match tx.nonce {
            x if x == n => return Ok(Seen::Holds(tx.hash)),
            x if x > n => seen = Seen::After,
            _ if matches!(seen, Seen::Absent) => seen = Seen::Before,
            _ => {}
        }
    }
    Ok(seen)
}

/// The tx `c` signed at `p.nonce`, mined after block `p.from`. The blocks `c` created an
/// entity in (one query: commits, grants, revoke anchors and delegations all create one)
/// bound it by binary search: below the first one holding a later nonce, above the last one
/// holding only earlier ones. The blocks left are then read down from that upper bound (or
/// the head, or `p.cursor` where the last attempt stopped), since the tx that took the
/// nonce sits right under the next tx of `c`: at most [`SCAN`] reads per call. A walk that
/// reaches the lower bound without it (a node behind the one that reported the nonce used)
/// starts over: `None` stays temporary.
async fn at_nonce<A: Arkiv>(
    a: &A,
    c: Address,
    p: &mut Pending,
) -> Result<Option<TxHash>, StoreError> {
    let n = p.nonce;
    let mut lo = p.from; // no block up to `lo` holds it
    let mut hi = match (a.head().await?, p.cursor) {
        (head, 0) => head,
        (head, cursor) => head.min(cursor), // every block above `cursor` is ruled out
    };
    let q = Query::from(Pred::eq(Field::Creator, c));
    let select = Select {
        key: false,
        created_at: true,
        ..Select::default()
    };
    let mut made: Vec<u64> = charter_store::paging::offset_all(a, &q, select)
        .await?
        .iter()
        .filter_map(|e| e.created_at)
        .filter(|b| (lo + 1..=hi).contains(b))
        .collect();
    made.sort_unstable();
    made.dedup();
    let mut reads = 0;
    while !made.is_empty() && reads < SCAN {
        let b = made[made.len() / 2];
        reads += 1;
        match seen(a, c, n, b).await? {
            Seen::Holds(h) => return Ok(Some(h)),
            Seen::Before => lo = b,
            Seen::After => hi = b - 1,
            Seen::Absent => {}
        }
        made.retain(|x| *x != b && (lo + 1..=hi).contains(x));
    }
    for b in (lo + 1..=hi).rev() {
        if reads == SCAN {
            p.cursor = b;
            return Ok(None);
        }
        reads += 1;
        match seen(a, c, n, b).await? {
            Seen::Holds(h) => return Ok(Some(h)),
            Seen::Before => break,
            Seen::After | Seen::Absent => {}
        }
    }
    p.cursor = 0;
    Ok(None)
}
