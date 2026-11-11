//! `FileMemArkiv`: a [`MemArkiv`] persisted as an append-only log, so separate processes
//! (git running `git-remote-arkiv`, then `chart`, then another clone) share one fake chain.
//!
//! The log holds only the inputs that change the fake: funding, accepted raw txs and mined
//! blocks. Every call first replays what other processes appended, under an exclusive
//! `flock`, so the fake stays deterministic and identical in every process. Blocks are mined
//! on demand: polling the receipt of a tx while txs wait in the pool mines one block.
//! Fault injection is not persisted; use [`MemArkiv`] directly for fault tests.

use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::pin::pin;
use std::sync::Mutex;
use std::task::{Context, Poll, Waker};

use alloy_consensus::transaction::SignerRecoverable;
use alloy_consensus::{Transaction, TxEnvelope};
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{Address, TxHash, U256};
use arkiv_bindings::Operation;

use super::{MemArkiv, Pending};
use crate::{Arkiv, Query, QueryOpts, QueryPage, Receipt, SignedTx, StoreError, TxInfo};

const TAG_CHAIN: u8 = b'C';
const TAG_FUND: u8 = b'F';
const TAG_SEND: u8 = b'S';
const TAG_MINE: u8 = b'M';
const HEADER: usize = 5;

/// See the module docs.
#[derive(Debug)]
pub struct FileMemArkiv {
    mem: MemArkiv,
    log: Mutex<Log>,
}

#[derive(Debug)]
struct Log {
    file: File,
    consumed: u64,
}

fn io(e: std::io::Error) -> StoreError {
    StoreError::Rpc(format!("fake log: {e}"))
}

/// `MemArkiv`'s futures never suspend; drive one to completion in place.
fn now<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(v) => v,
        Poll::Pending => unreachable!("MemArkiv futures complete on first poll"),
    }
}

fn record(tag: u8, payload: &[u8]) -> Vec<u8> {
    let len = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    let mut r = Vec::with_capacity(HEADER + payload.len());
    r.push(tag);
    r.extend_from_slice(&len.to_le_bytes());
    r.extend_from_slice(payload);
    r
}

fn pending_from_raw(raw: &[u8], chain_id: u64) -> Result<Pending, StoreError> {
    let env =
        TxEnvelope::decode_2718(&mut &raw[..]).map_err(|e| StoreError::Rpc(format!("rlp: {e}")))?;
    if env.chain_id() != Some(chain_id) {
        return Err(StoreError::WrongChain {
            expected: chain_id,
            got: env.chain_id().unwrap_or_default(),
        });
    }
    let from = env
        .recover_signer()
        .map_err(|e| StoreError::Rpc(format!("invalid signature: {e}")))?;
    Ok(Pending {
        hash: *env.tx_hash(),
        from,
        nonce: env.nonce(),
        to: env.to().unwrap_or_default(),
        value: env.value(),
        input: env.input().clone(),
        gas_limit: env.gas_limit(),
        max_fee: env.max_fee_per_gas(),
        tip: env.max_priority_fee_per_gas().unwrap_or(0),
    })
}

impl Log {
    /// Replays records appended since the last call (by this or another process).
    fn catch_up(&mut self, mem: &MemArkiv, chain_id: u64) -> Result<(), StoreError> {
        self.file.seek(SeekFrom::Start(self.consumed)).map_err(io)?;
        let mut buf = Vec::new();
        self.file.read_to_end(&mut buf).map_err(io)?;
        let mut at = 0usize;
        // A torn trailing record (a writer crashed mid-append) is never applied.
        while let Some(head) = buf.get(at..at + HEADER) {
            let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
            let Some(payload) = buf.get(at + HEADER..at + HEADER + len) else {
                break;
            };
            apply(mem, chain_id, head[0], payload)?;
            at += HEADER + len;
        }
        self.consumed += at as u64;
        if at < buf.len() {
            self.file.set_len(self.consumed).map_err(io)?;
        }
        Ok(())
    }

    fn append(&mut self, records: &[u8]) -> Result<(), StoreError> {
        if records.is_empty() {
            return Ok(());
        }
        self.file.write_all(records).map_err(io)?;
        self.file.flush().map_err(io)?;
        self.consumed += records.len() as u64;
        Ok(())
    }
}

fn apply(mem: &MemArkiv, chain_id: u64, tag: u8, payload: &[u8]) -> Result<(), StoreError> {
    let corrupt = || StoreError::InconsistentView(format!("fake log record {tag} is corrupt"));
    match tag {
        TAG_CHAIN => {
            let got = u64::from_be_bytes(payload.try_into().map_err(|_| corrupt())?);
            if got != chain_id {
                return Err(StoreError::WrongChain {
                    expected: chain_id,
                    got,
                });
            }
        }
        TAG_FUND if payload.len() == 52 => {
            let wei = U256::from_be_slice(&payload[20..]);
            mem.fund(Address::from_slice(&payload[..20]), wei);
        }
        TAG_SEND => {
            let p = pending_from_raw(payload, chain_id)?;
            mem.lock().admit(p, payload.len())?;
        }
        TAG_MINE if payload.is_empty() => {
            mem.mine();
        }
        _ => return Err(corrupt()),
    }
    Ok(())
}

impl FileMemArkiv {
    /// Opens (or creates) the log at `path` for chain `chain_id` and replays it.
    pub fn open(path: &Path, chain_id: u64) -> Result<Self, StoreError> {
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)
            .map_err(io)?;
        let me = Self {
            mem: MemArkiv::new(chain_id),
            log: Mutex::new(Log { file, consumed: 0 }),
        };
        me.with(|_, out| {
            out.push(record(TAG_CHAIN, &chain_id.to_be_bytes()));
            Ok(())
        })?;
        Ok(me)
    }

    /// Runs `f` on the caught-up fake under the lock, then appends the records it produced
    /// (even if `f` failed after a logged change). A fresh log gets only the chain record.
    fn with<T>(
        &self,
        f: impl FnOnce(&MemArkiv, &mut Vec<Vec<u8>>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut log = self.log.lock().unwrap_or_else(|p| p.into_inner());
        log.file.lock().map_err(io)?;
        let chain_id = now(self.mem.chain_id())?;
        let out = log.catch_up(&self.mem, chain_id).and_then(|()| {
            let mut records = Vec::new();
            let r = f(&self.mem, &mut records);
            let fresh = log.consumed == 0;
            let keep: Vec<u8> = records
                .into_iter()
                .filter(|rec| fresh || rec.first() != Some(&TAG_CHAIN))
                .flatten()
                .collect();
            log.append(&keep)?;
            r
        });
        let _ = log.file.unlock();
        out
    }

    /// Credits `wei` to `a` (logged).
    pub fn fund(&self, a: Address, wei: U256) -> Result<(), StoreError> {
        self.with(|m, out| {
            m.fund(a, wei);
            let mut p = a.to_vec();
            p.extend_from_slice(&wei.to_be_bytes::<32>());
            out.push(record(TAG_FUND, &p));
            Ok(())
        })
    }

    /// Mines one block (logged); returns its number.
    pub fn mine(&self) -> Result<u64, StoreError> {
        self.with(|m, out| {
            out.push(record(TAG_MINE, &[]));
            Ok(m.mine())
        })
    }
}

impl Arkiv for FileMemArkiv {
    async fn chain_id(&self) -> Result<u64, StoreError> {
        self.with(|m, _| now(m.chain_id()))
    }
    async fn head(&self) -> Result<u64, StoreError> {
        self.with(|m, _| now(m.head()))
    }
    async fn base_fee(&self) -> Result<u128, StoreError> {
        self.with(|m, _| now(m.base_fee()))
    }
    async fn query(&self, q: &Query, opts: &QueryOpts) -> Result<QueryPage, StoreError> {
        self.with(|m, _| now(m.query(q, opts)))
    }
    async fn entity_nonce(&self, creator: Address) -> Result<u64, StoreError> {
        self.with(|m, _| now(m.entity_nonce(creator)))
    }
    async fn balance(&self, a: Address) -> Result<U256, StoreError> {
        self.with(|m, _| now(m.balance(a)))
    }
    async fn tx_count(&self, a: Address) -> Result<u64, StoreError> {
        self.with(|m, _| now(m.tx_count(a)))
    }
    async fn tx_count_mined(&self, a: Address) -> Result<u64, StoreError> {
        self.with(|m, _| now(m.tx_count_mined(a)))
    }
    async fn transaction(&self, h: TxHash) -> Result<Option<TxInfo>, StoreError> {
        self.with(|m, _| now(m.transaction(h)))
    }
    async fn mined_tx(&self, h: TxHash) -> Result<Option<crate::MinedTx>, StoreError> {
        self.with(|m, _| now(m.mined_tx(h)))
    }
    async fn block_txs(&self, n: u64) -> Result<Vec<crate::MinedTx>, StoreError> {
        self.with(|m, _| now(m.block_txs(n)))
    }
    async fn estimate(&self, from: Address, ops: &[Operation]) -> Result<u64, StoreError> {
        self.with(|m, _| now(m.estimate(from, ops)))
    }
    async fn send(&self, tx: SignedTx) -> Result<TxHash, StoreError> {
        self.with(|m, out| {
            let raw = tx.raw().to_vec();
            let h = now(m.send(tx))?;
            out.push(record(TAG_SEND, &raw));
            Ok(h)
        })
    }
    async fn receipt(&self, h: TxHash) -> Result<Option<Receipt>, StoreError> {
        self.with(|m, out| {
            if let Some(r) = now(m.receipt(h))? {
                return Ok(Some(r));
            }
            if m.pending() == 0 {
                return Ok(None);
            }
            m.mine();
            out.push(record(TAG_MINE, &[]));
            now(m.receipt(h))
        })
    }
}

#[cfg(test)]
mod tests {
    use alloy_signer_local::PrivateKeySigner;

    use super::*;
    use crate::{ARKIV_ADDRESS, AttributeValue as V, Pred, TxParams, attr, create_op};

    const CHAIN: u64 = 7;

    fn create_tx(key: &PrivateKeySigner, nonce: u64, seq: u64) -> SignedTx {
        let op = create_op(
            &[
                ("ag.kind", V::Str("t".into()), true),
                ("ag.seq", V::U64(seq), true),
            ],
            100,
            seq.into(),
        )
        .expect("op");
        let p = TxParams {
            nonce,
            to: ARKIV_ADDRESS,
            value: U256::ZERO,
            input: crate::execute_calldata(&[op]),
            gas_limit: 1_000_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 0,
        };
        SignedTx::sign(key, CHAIN, p).expect("sign")
    }

    async fn count(a: &FileMemArkiv) -> u64 {
        let q = Query::from(Pred::eq(attr("ag.kind"), "t"));
        let page = a.query(&q, &QueryOpts::count_only()).await.expect("q");
        page.count.expect("count")
    }

    #[tokio::test]
    async fn two_handles_share_one_chain_through_the_log() {
        let dir = tempdir();
        let path = dir.path().join("chain.log");
        let a = FileMemArkiv::open(&path, CHAIN).expect("open");
        let key = PrivateKeySigner::random();
        a.fund(key.address(), U256::from(10u128.pow(20)))
            .expect("fund");
        let h = a.send(create_tx(&key, 0, 1)).await.expect("send");
        let r = a.receipt(h).await.expect("poll").expect("mined on demand");
        assert!(r.success);
        assert_eq!(a.receipt(TxHash::ZERO).await.expect("poll"), None);

        let b = FileMemArkiv::open(&path, CHAIN).expect("reopen");
        assert_eq!(count(&b).await, 1);
        assert_eq!(b.head().await.expect("head"), a.head().await.expect("head"));
        assert_eq!(b.entity_nonce(key.address()).await.expect("nonce"), 1);
        let h2 = b.send(create_tx(&key, 1, 2)).await.expect("send");
        b.mine().expect("mine");
        assert!(b.receipt(h2).await.expect("r").is_some());
        assert_eq!(count(&a).await, 2, "a replays b's appended records");
        assert_eq!(a.tx_count(key.address()).await.expect("n"), 2);
        assert!(a.balance(key.address()).await.expect("bal") < U256::from(10u128.pow(20)));
        assert!(a.base_fee().await.expect("fee") > 0);
        let est = a.estimate(key.address(), &[]).await;
        assert!(est.is_err(), "empty batch reverts");
        assert_eq!(a.chain_id().await.expect("id"), CHAIN);
    }

    #[tokio::test]
    async fn transactions_survive_a_reopen() {
        let dir = tempdir();
        let path = dir.path().join("chain.log");
        let a = FileMemArkiv::open(&path, CHAIN).expect("open");
        let key = PrivateKeySigner::random();
        a.fund(key.address(), U256::from(10u128.pow(20)))
            .expect("fund");
        let tx = SignedTx::sign(
            &key,
            CHAIN,
            TxParams {
                nonce: 0,
                to: Address::repeat_byte(3),
                value: U256::from(7),
                input: Default::default(),
                gas_limit: 21_000,
                max_fee_per_gas: 2_000_000_000,
                max_priority_fee_per_gas: 0,
            },
        )
        .expect("sign");
        let h = a.send(tx).await.expect("send");
        a.mine().expect("mine");
        let b = FileMemArkiv::open(&path, CHAIN).expect("reopen");
        let t = b.transaction(h).await.expect("t").expect("replayed");
        assert_eq!(
            (t.from, t.to, t.value),
            (key.address(), Some(Address::repeat_byte(3)), U256::from(7))
        );
        assert_eq!(b.tx_count_mined(key.address()).await.expect("n"), 1);
    }

    #[test]
    fn refuses_a_log_of_another_chain_and_ignores_a_torn_tail() {
        let dir = tempdir();
        let path = dir.path().join("chain.log");
        let a = FileMemArkiv::open(&path, CHAIN).expect("open");
        a.mine().expect("mine");
        drop(a);
        let err = FileMemArkiv::open(&path, CHAIN + 1).expect_err("wrong chain");
        assert_eq!(err.code(), "E_WRONG_CHAIN");
        let mut f = OpenOptions::new().append(true).open(&path).expect("f");
        f.write_all(&[TAG_MINE, 9, 0, 0]).expect("torn");
        let b = FileMemArkiv::open(&path, CHAIN).expect("torn tail ignored");
        assert_eq!(now(b.head()).expect("head"), 2);
    }

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }
}
