# P5: speed and cost

Status: plan, nothing implemented yet.
Written 2026-11-09.
Section 2 lists the changes the user approved on 2026-11-09.
Section 3 lists further proposals that still need the user's decision, and section 5 lists open decisions.

Labels follow `docs/p0/arkiv-limits.md`: MEASURED (observed live on Rabdi), DERIVED (computed from measured numbers), ESTIMATE (expected effect, to be confirmed by the day-1 bench).

## 1. Baseline and root cause

### 1.1 Measured times

| Path | Time | Label | Cause |
| --- | --- | --- | --- |
| Clone of Charter (1.18 MiB pack) | 13.3 s wall, 0.39 s CPU | MEASURED 2026-11-08 | sequential RPC round trips |
| `git fetch` with nothing new | 5.3 s wall, 0.11 s CPU | MEASURED 2026-11-08 | the full verification walk reruns one query at a time (250 to 700 ms each) |
| `chart program status` | 6.8 s | MEASURED 2026-11-08 | replays every op from the start |
| Small push with a pinned program | 55 to 83 s | MEASURED (`docs/live/private-proofs.md`) | sync, 4 txs each awaited, a chunk check query, the anchor gate re-read, a post-verify sync |
| Posting one op | 4 to 6 s; the proof flow spent about 4 min posting ops | MEASURED (same doc) | one tx at a time, each awaited |
| ZK prove | 53.1 s wall; receipt 223,898 B, posted as about 8 blob ops | MEASURED (`docs/p4/zk-sealed-patch.md`) | proving sits on the push path |

The client is network bound, not CPU bound: every path above spends under 3 percent of its wall time on CPU.

### 1.2 Measured probes

- Self-contained packs (what pushes write today) vs thin packs, over Charter's last 40 first-parent commits: 1,511,784 B vs 768,158 B, so 1.97x (MEASURED).
  A typical commit shrinks 3 to 57x (47,280 B to 825 B; 161,052 B to 6,079 B); one commit that added large new files shrinks only 1.1x.
- Recompressing packs with zstd -19 --long (objects stored raw, whole stream compressed): 4 percent smaller for the full repo, 15 percent smaller for the thin per-commit packs (MEASURED).
- The RPC answers with brotli or gzip when asked; a page of 8 chunk bodies went from 1,049,794 B to 596,721 B with gzip (MEASURED, one sample, wall time not yet compared fairly).
- JSON-RPC batch requests work (two `eth_*` calls in one POST returned both results) (MEASURED); batching `arkiv_query` is not yet checked.
- `eth_sendRawTransactionSync` does not exist on Rabdi (MEASURED), so receipts still need polling or a WebSocket subscription.
- Calldata-only transactions (a zero-value self-transfer carrying the bytes, no entity) cost, by `eth_estimateGas` from a Charter control address: 1 KiB 62,638 gas; 16 KiB 680,899; 64 KiB 2,656,536; 126,000 B 5,088,364 (MEASURED).

### 1.3 Cost model (DERIVED from `arkiv-limits.md`)

- A small push writes 4 entities in 4 txs (chunk, manifest, refs, anchor), about 1.9M gas, of which the anchor at maximum lifetime is about 0.9M.
- Each anchor is kept alive forever: about 6,300 storage units, so about 1.39M gas per anchor per year, growing linearly with the number of pushes.
- A 64 KiB chunk costs 3.92M gas to create at 30 days plus about 1.22M gas per 30-day extension, so about 17.3M gas in its first year.
- The same 64 KiB as calldata only costs 2.66M gas once and nothing after: about 6.5x cheaper in year one and free in later years.
- Large pushes are bound by the 30M gas block: about 7 chunk creates per block at 30 days, about 11 calldata-only 64 KiB txs per block.

## 2. Approved changes (2026-11-09)

### 2.1 Read path, no format change

1. Head probe: one batched request carrying `entityNonce` and the live count of every tracked control address plus anchors above the local high-water mark.
   If nothing changed, the sync ends there; otherwise the remaining queries run in parallel, not in sequence.
   ESTIMATE: no-op fetch from 5.3 s to under 1 s; every push and `program status` start with this sync, so they gain too.
2. DONE on `perf/batch-a` (2.7). Incremental program state: keep the folded state and a cursor (block, entity key) per repo locally, fold only new ops, and refold from scratch only when the pinned program changes.
   ESTIMATE: `program status` from 6.8 s to under 1 s.
3. DONE (brotli and gzip in 2.6, HTTP/2 in 2.7). Transport: enable `reqwest` brotli and gzip, batch independent calls, keep one HTTP/2 connection.
   ESTIMATE: about 43 percent fewer bytes on clone, fewer access-key cost units.

### 2.2 Write path

4. One-tx push (format v2): the chunk ops and the anchor go in one `execute(Operation[])` tx from `control`.
   The v2 anchor carries the refs list (as a diff against the previous anchor) and the chunk hashes inline, so a small push needs no separate `refs` or `manifest` entity.
   The completeness rule still holds because everything `control` creates stays kept alive.
   The anchor gate and post-verify become the head probe plus the receipt; `pending.json` remains only for multi-tx pushes.
   ESTIMATE: a small push from about 60 s to about 5 s.
5. Sign once per push (format v2): the anchor already commits to the refs and manifest hashes, so those carry no signature of their own; this saves about 6.7 KB (two 3,353 B ML-DSA-65 envelopes) per push.
6. Thin packs: push with `git pack-objects --thin` against already anchored tips; clone with `git index-pack --fix-thin`, applying packs in anchor order (requires first, as today).
   MEASURED: 1.97x fewer bytes overall, 3 to 57x for a typical commit.
7. DONE on `perf/batch-a` (2.7), one tx per op on one pipeline because entities in one tx have no defined order. Batch and pipeline ops: several ops per tx (review create plus approve; the blob ops of a receipt), sent on the existing nonce pipeline instead of awaiting each receipt.
   ESTIMATE: the 4-minute proof flow drops to about 10 s.

### 2.3 Long-run cost

8. Anchor checkpoints (needs a security review before code): every N anchors, write a signed checkpoint that commits to the seq, the chain hash and the `entityNonce` of the control address at that point.
   Older anchors may then expire, and the completeness check counts only entities created after the checkpoint.
   Forever-storage then stays at about N anchors per peer instead of growing with every push.

### 2.4 ZK

9. Proving leaves the push path: the sealed commit op goes out first, the proof attaches later, and the program requires the proof before merge, not before push.
   Prove on the rs-lab GPU (CUDA) and switch the guest to RISC Zero's accelerated `sha2`.

### 2.5 Sprint

- Day 1: `CHARTER_TRACE=1` timing spans and an RPC call counter, plus `scripts/bench.sh` (clone, no-op fetch, small push, op post, 10 MiB push on Rabdi) writing a baseline JSON; every target here is checked against it.
- Sprint A, days 2 to 5, no format change, ships as v0.2.x: items 1, 2, 3, 7, 9.
  Exit: no-op fetch under 1 s, `program status` under 1 s, clone 2 to 3x faster, ops batched.
- Sprint B, days 6 to 10, format v2: day 6 is the v2 design doc and an Opus review, then items 4, 5, 6.
  Clients read v1 and v2 (the envelope carries `v`), pushes write v2, and an anchor chain may mix both.
  Exit: small push under 10 s in one tx with 35 to 45 percent less gas; thin-pack round trips pass `git fsck --full`.
- Backlog: item 8 after its security review.

### 2.6 Low-change pass on branch `perf/quick-wins` (2026-11-09, MEASURED)

No format change: a v0.2.0 reader still reads everything this branch writes, as long as thin packs stay off.

What changed:

- Reads run concurrently instead of one after another: peer docs and binds, signer lookups, the completeness and anchor-chain checks of every control address, refs and manifests, peers in concurrent rounds, and the program's peer keys and ref events.
- `connect` caches the chain id it verified, so each command makes one fewer call.
- Responses are brotli or gzip compressed (`reqwest` features); a 2.03 MB chunk page goes over the wire as 1.02 MB.
- The tx pipeline polls every 250 ms instead of 1 s and fetches the head and all pending receipts in one concurrent round.
- Push: the `eth_estimateGas` calls in `cost()` run concurrently, and when every missing chunk fits in one batch the chunks, manifests and refs go out together, so a small push waits on 2 confirmation rounds instead of 4; the landing checks that follow are unchanged.
- Thin packs behind `CHARTER_THIN_PACKS=1`: `pack-objects --thin` on push, `index-pack --fix-thin` on fetch, with index retries while progress is made so a base from another peer can arrive first; it stays off by default until v0.2.0 readers are gone.
- Dropped connections on reads are retried up to 3 times (200, 400, 800 ms); sends are not, because the tx pipeline owns resends.
  Rabdi was dropping about 1 in 10 requests that day (3 of 30 plain `eth_blockNumber` calls), so a clone of about 19 calls failed about 87 percent of the time on v0.2.0.
- `CHARTER_TRACE=1` prints one stderr line per RPC call with its method, time and size.

Read bench on Charter's own repo, interleaved rounds against `main` plus the same retry patch (so both sides survive drops), 5 rounds each, healthy endpoint, 0 failures:

| Operation | main + retry | perf/quick-wins | Speedup |
|---|---|---|---|
| clone | 8.96 s | 7.76 s | 1.15x |
| no-op fetch | 6.26 s | 3.79 s | 1.65x |
| `program status` | 5.37 s | 3.83 s | 1.40x |

Thin packs over the repo's last 59 first-parent pushes (local `pack-objects`, no chain): median push 13,996 B to 4,927 B (2.81x), all 59 together 2,892,322 B to 1,595,061 B (1.81x).
Chunk calldata and chunk storage scale with bytes, so they fall by about the same factor; the anchor, manifest and refs gas does not change, so a whole small push costs about 1.2x less (DERIVED from the 3.8 measurements, not measured on chain).

Where the remaining 3.8 s of a no-op fetch goes (one traced run): 14 calls in about 10 dependent stages of 0.25 to 0.4 s each.
Two stages are avoidable without a format change:

- `ring_for` (the private-repo check) runs the public identity query that `view::build` runs again right after it; passing the rows through saves about 0.4 s.
- The completeness check is `entityNonce`, count, `entityNonce` in sequence; pinning the count and the nonce to one block with `atBlock` (supported by node v1.0.1) makes it one round, saving about 0.5 s, with a fallback to today's check when a lagging node cannot answer for that block.

Push bench on Rabdi (2026-11-10, MEASURED): one fresh repo per variant seeded with `charter-sync/src` (about 0.4 MB), rounds interleaved, same bench wallet, 0 failures.

| Push | main + retry | perf/quick-wins | Speedup | perf + thin packs | Speedup |
|---|---|---|---|---|---|
| first push, about 0.4 MB (n=1) | 41.11 s | 24.90 s | 1.65x | 25.85 s | 1.59x |
| small one-line change, median (n=3) | 47.19 s | 27.55 s | 1.71x | 25.94 s | 1.82x |
| 256 KiB random blob (n=1) | 53.89 s | 35.68 s | 1.51x | 35.07 s | 1.54x |

Gas per push is not in this table: the wallet balance delta turned out to track top-ups of the per-repo control and bulk addresses, not the gas each push burned, so it needs a per-address receipt sum instead.
Item 1 (one head probe that ends a no-op sync early) and item 2 (incremental program fold) are still the large read-path wins; this pass did not attempt them.

### 2.7 Transport, tx pipeline, incremental fold and op batching on branch `perf/batch-a` (2026-11-11, MEASURED)

No format change: v0.3.0 cloned and pulled a repo this branch pushed (2 MiB and 4 MiB blobs included, `git fsck --full` clean), and this branch cloned the same repo after v0.3.0 pushed a commit on top.
The head probe (item 1) is not in this branch; it moved to the head-model work (3.4).

What changed:

- Item 2: the program fold resumes from a local checkpoint (`arkiv-fold.json` next to the git cache) and runs only the events after it.
  A checkpoint is used only when its version, program hash, repo facts, seed heads and a running SHA-256 over the exact folded events all match, and when every git-ancestry question it asked was answered; anything else folds from scratch.
  It is taken before the obligations run, and a test resumes at every split of every template and compares with a full fold.
- Item 3: `reqwest` speaks HTTP/2, so concurrent calls share one TLS connection; every traced call reports `HTTP/2.0`.
  TLS sessions are resumed within a process (rustls default) but not across processes, so each `chart` or helper process still pays one full handshake on its first call (about 0.35 to 0.9 s).
- Tx pipeline (3.13 step 1): one `eth_estimateGas` per create shape (attribute names, types, value lengths, lifetime, calldata length) prices every chunk of that shape, corrected for zero bytes and with the usual 10 percent headroom; a tx that fails on a reused estimate is re-estimated and resent once.
  The head number and base fee come from one `eth_getBlockByNumber` call, reused for 2 s.
  Inclusion is awaited on a `newHeads` subscription, with the old polling as the fallback when the socket fails.
  A window's txs are signed with consecutive nonces and sent concurrently.
- Item 7: `cob::post_signed` and `cob::post_many` send several ops back to back on one nonce pipeline, one tx each so dependent ops keep their order (entities created in one tx have no defined order); the first program publish and `chart op attach` use it.
- Big pushes: the bulk address keeps up to two blocks of gas in flight (`PipelineConfig::bulk`), chunks go out in batches of 256 instead of 64, and `TxPipeline::run_measured` reports MB/s.

Read bench on Charter's own repo, interleaved rounds against `main`, 5 rounds each, healthy endpoint (warm calls 0.3 to 0.6 s), 0 failures:

| Operation | main | perf/batch-a | Speedup | RPC calls (both) |
|---|---|---|---|---|
| clone | 9.94 s | 10.74 s | none (round spread 9.8 to 13.1 s) | 14 |
| no-op fetch | 5.82 s | 4.87 s | 1.19x | 14 |
| `program status` | 5.17 s | 4.90 s | 1.06x | 14 |

The fold itself (46 events) drops from 89 to 224 ms to 0 to 1 ms once a checkpoint exists.
The 1 s targets for no-op fetch and `program status` are missed: both still make 14 calls in about 10 dependent stages, and only the head probe removes them.

Push bench on Rabdi: one fresh repo per variant seeded with `charter-store/src`, data lifetime 1 day, control lifetime 2 days, same bench wallet, 0 failures.

| Push | main | perf/batch-a | Speedup |
|---|---|---|---|
| first push (n=1, not interleaved) | 23.79 s | 20.77 s | 1.15x |
| small one-line change, median (n=4, interleaved) | 23.04 s | 20.95 s | 1.10x |
| 2 MiB random blob (n=1) | 81.53 s, 0.026 MB/s | 45.23 s, 0.046 MB/s | 1.80x |
| 4 MiB random blob (n=1) | not run | 70.46 s, 0.060 MB/s | - |

A traced small push makes 63 RPC calls instead of 81 (`eth_blockNumber` 15 to 5, receipts 11 to 7, balances 10 to 8); its 8 estimates stay, because each of its txs has a different shape.
The chunk phase alone, taken as the big push minus the small-push median, runs at about 0.036 MB/s on `main` and about 0.085 MB/s here (2.4x).
A 2 MiB push burned about 0.085 to 0.095 tGLM of gas on both sides (summed per address), so the speedup costs no gas; the bulk address now holds a larger float between pushes (up to about 0.06 tGLM).
10 MiB was not run because its gas (about 0.43 tGLM, DERIVED) exceeded the bench budget; at the measured chunk rate it would take about 145 s (ESTIMATE).

In those runs the bulk address was topped up once per window and waited for each top-up to land: the 4 MiB push sent 16 chunks over 2 blocks, then left 3 to 5 blocks idle, so its chunks used 25 percent of the gas of the blocks they spanned.

Follow-up on the same branch (2026-11-11, MEASURED):

- One top-up funds the whole run: one window at the max fee (what the node checks before admitting txs) plus the expected burn of every later job, `gas x (base fee + tip)`.
  If the base fee outgrows that, or the funder cannot spare the whole amount, the per-window top-up still runs as before.
- A send that still fails after its retries, or a tx dropped after every resend, no longer strands the txs above its nonce: the gap is plugged with a zero-value transfer to self priced to replace it, everything in flight lands, and then the run fails with the original error.

| Push (same repo and bulk address) | per-window top-ups | one top-up per run | Speedup |
|---|---|---|---|
| 2 MiB random blob (n=1) | 45.23 s, 0.046 MB/s | 32.70 s, 0.064 MB/s | 1.38x |
| 3 MiB random blob (n=1) | not run | 39.36 s, 0.080 MB/s | - |
| 4 MiB random blob (n=1) | 70.46 s, 0.060 MB/s | not run (budget) | - |
| chunk span, 2 MiB | 12 blocks, 24 percent of their gas | 5 blocks, 58 percent | |
| chunk span, 3 or 4 MiB | 23 blocks, 25 percent (4 MiB) | 8 blocks, 54 percent (3 MiB) | |

The 2 MiB chunks landed in 4 blocks, about 0.26 MB/s while they were landing; the rest of the 32.7 s is the push's fixed reads and control txs.
In the 3 MiB push our txs filled blocks to 90 to 99 percent, the base fee rose from 1.00 to 1.37 gwei within 5 blocks, and that rise triggered the one fallback top-up, which cost one idle block.
The step from 2 to 3 MiB added 6.7 s for 1.05 MB (about 0.16 MB/s), so a 10 MiB push would now take about 85 s instead of about 145 s (ESTIMATE).
About 11 chunks of 64 KiB fit in a 30M-gas block (DERIVED), so the pipeline now alternates full and half-full blocks: the per-sender limit of 16 pending txs and the round trip between a receipt and the next send leave the second block of each pair partly empty.

Op batching was not measured on chain; with N ops on one pipeline a post waits about one confirmation round instead of N (ESTIMATE).

### 2.8 Encoding, packing and snapshot policy on branch `perf/b-lane-b` (2026-11-11)

No format change: every head this branch writes is a v2 head that v0.4.0 reads.

What changed:

- Manifest facts cache: the size and git pack trailer of each snapshot manifest are kept by `h` in `git/arkiv-manifests.json` (`h` hashes the signed manifest, so its facts never change).
  The snapshot policy of a push reads them from there, so a piece push no longer queries Arkiv for its own manifests (test `a_piece_push_reads_no_snapshot_manifest_it_signed_itself`).
- Snapshot packs are built with `--no-reuse-delta --window=50`; thin packs stay as they were (MEASURED below).
- Layered snapshots: a due refresh adds one self-contained layer (every object reachable from the merged refs that no kept layer holds, `pack-objects --keep-pack`) to the head's `snapshot` list instead of rewriting the snapshot.
  The 8th refresh, or any refresh whose layer packs are no longer in the local cache, rewrites one compact snapshot instead; that bounds the layer count and the bytes of deleted refs kept alive.
  The refresh threshold is unchanged and counts every layer: piece bytes over `max(1 MiB, 25 percent of all layers)`, 64 packs or 256 pieces.
  v0.4.0 already downloads every manifest a head lists, indexes all downloads in rounds and checks connectivity over all of them, and keepalive extends every listed manifest and its chunks, so layers need no reader change (test `a_refresh_adds_a_layer_that_the_v040_reader_clones`, run against the unchanged v0.4.0 reader code).
- A new cache is configured with one append to its `config` instead of four `git config` spawns.

Snapshot pack settings, bytes and CPU seconds (MEASURED on this Mac):

| Repo | default | `--no-reuse-delta`, window 10 | window 50 | window 250, depth 250 |
|---|---|---|---|---|
| Charter `main` | 1,808,689 | 1,681,736 (0.16 s) | 1,646,039, -9.0% (0.46 s) | 1,642,159 (1.29 s) |
| focus-dock (binary-heavy, 13 MiB) | 12,006,713 | 11,973,110 (0.27 s) | 11,499,578, -4.2% (0.70 s) | 11,498,817 (1.15 s) |

Going from window 10 to 50 saves about 7M gas per CPU second; past 50 it saves under 0.3M gas per CPU second.
Recomputing deltas of thin packs saves 0 to 0.4% on small pushes (and is slightly worse on some), 2% only on 30-commit pushes, so v2 thin packs kept reusing deltas.
Format v3 (2.10) builds thin packs with `--no-reuse-object --compression=0` for brotli, which recomputes their deltas at git's default window 10 as well.

A self-contained layer is 7 to 33% larger than the thin pack of the same commits (Charter `main`, 10 to 60 commits: 786,542 vs 592,193 B and 1,554,014 vs 1,451,613 B), but a v0.4.0 refresh rewrites everything: the full snapshot is 1,645,323 B.

Live on Rabdi with the release build of this branch (MEASURED 2026-11-11, own wallet, `CHARTER_TRACE=1`, gas and tGLM from receipts):

| Step | Txs | Gas | tGLM | Seconds | Head `snapshot` |
|---|---|---|---|---|---|
| small push (3 runs) | 1 | 271,808 to 277,160 | 0.00027 | 9.6 to 11.9 | 1 layer |
| 10 MiB push (snapshot due, adds layer 2) | 169 | 623.5M | 1.079 | 42.9 (resume after a funds stop) | 2 layers |
| 2 MiB push (pieces) | 35 | 85.5M | 0.0887 | 27.6 | 2 layers |
| 1 MiB push (refresh, adds layer 3 of 3 MiB) | 53 | 187.4M | 0.249 | 35.2 | 3 layers |
| small push after the refresh | 1 | 274,786 | 0.00027 | 11.2 | 3 layers |

Before (v0.4.0, profiler traces of the same steps): small push 11.9 to 13.8 s; 2 MiB push 84.9M gas, 25.4 s.
The refresh is the cut: v0.4.0 rewrites the whole 13.1 MiB snapshot, about 210 chunks at 3.88M gas, so about 815M gas and 1.08 tGLM at the same gas price (DERIVED, not run), against 187.4M gas and 0.249 tGLM here, -77%.
The small push drops one RPC (34 instead of 35; the manifest query took 464 to 605 ms) and spawns as many git processes as before (17); its gas is unchanged, as expected without a format change.
A clone of the 3-layer repo with the installed v0.4.0 binary (`chart 0.4.0`, fresh home) took 9 s, `git fsck --full` is clean and `HEAD` matches the pusher.
A v0.4.0 writer on a layered repo is safe (MEASURED on Rabdi with the installed `chart 0.4.0`):
the same device after a downgrade carries all 3 layers on piece pushes and, at its own refresh threshold (which sums every layer), replaces them with one full snapshot of the identity's refs;
a second v0.4.0 device writes no snapshot until its first refresh, reads the first device's layers meanwhile, and its refresh writes a full snapshot while the first device's head keeps its layers.
After each step both readers clone with a clean `git fsck --full`, and `chart keepalive --dry-run` of both versions lists the same keep set, which counts every layer a head lists (and none a head dropped).

### 2.9 Write latency and pacing modes on branch `perf/batch-b` (2026-11-11)

The write path drops round trips without a format change: the chain-id probe rides in the first read's batch, the head tx is signed from what setup and the plan already read (no estimate under 32 KiB, no balance re-read, the tx index from the receipt), the re-check after the head tx reads the peer and the device heads together, the snapshot manifest goes with its last chunks, a first push tops up C_d and the control address in one transfer, and a funded run reads its balance once and counts each landed tx's unused reserve back instead of reading again.
MEASURED on Rabdi against v0.4.0 (lane A build, own wallet, interleaved runs): small push 12.6 / 11.9 / 10.0 s to 9.5 / 9.4 / 6.6 s, refs-only push 12.6 / 11.9 s to 9.0 / 7.9 s, first push 31.4 s to 23.0 s, each at the same gas.

Big uploads (pieces and snapshot chunks) have two pacing modes; the project rule is that no change may make any operation slower or more expensive, so the default keeps v0.4.0's speed.

- Default: up to two blocks of gas unconfirmed, as in v0.4.0, so each block takes all the chunks it can hold (7 of 3.88M gas in a 30M block) and the next block's chunks already wait in the pool; a push never needs more blocks than in v0.4.0.
  Holding chunks back to spread them evenly would mean refilling once per block, and a refill that misses the next block costs a whole block (7 of 63 blocks missed on the 10 MiB run below), so the default does not spread.
  Full blocks raise the base fee 12.5% per block (1.00 to 2.64 gwei over a 10 MiB push), so the default keeps v0.4.0's fee for big pushes; it keeps every saving that costs no time: the single balance read, the manifest with the chunks and the single top-up.
- Economy, `CHARTER_PACE=economy`: up to the block gas target (15M, half the limit) unconfirmed, refilled as each block lands, so the base fee never rises.
  MEASURED: a 10 MiB push cost 0.625 tGLM in 134.0 s against v0.4.0's 1.218 tGLM in 96.9 s (-49% fee, +38% time); a 2 MiB push cost 0.127 instead of 0.155 tGLM, its content phase 28.9 s instead of 23.1 s.

Live sanity run of `perf/batch-b` 4e3895d (lanes A, B and C merged, default pacing) against v0.4.0, interleaved on fresh repos in one session (MEASURED 2026-11-11, own wallets, `CHARTER_TRACE=1`, gas and tGLM from receipts):

| Step | v0.4.0 s | batch-b s | v0.4.0 tGLM | batch-b tGLM |
|---|---|---|---|---|
| first push (64 KiB) | 34.7 | 24.8 | 0.006659 | 0.006665 |
| small push (3 runs) | 13.0 / 13.8 / 13.0 | 8.0 / 6.4 / 6.2 | 0.000326 to 0.000328 | 0.000325 to 0.000330 |
| refs-only push | 14.0 | 7.8 | 0.000260 | 0.000260 |
| no-op fetch (3 runs) | 5.5 / 4.9 / 5.5 | 2.1 / 2.1 / 1.4 | 0 | 0 |
| clone | 4.5 | 1.5 | 0 | 0 |
| 2 MiB push | 37.0 (content 24.6) | 25.4 (content 18.3) | 0.1530 | 0.1513 |

Both clones match the pusher's `HEAD` and pass `git fsck --full`.
Small-push gas moves by about 2k either way with the random payload; the first push's manifest tx costs about 6k gas more (+0.09% of the push, seen in 2 of 2 runs) when it lands in the block of its chunks, which the 2 MiB push's manifest does not show.
Cause (found 2026-11-11, head-model 13.2): GolemDB charges an index-term create for every term it does not find in the commit the block started from, so a manifest in its chunks' block pays again for terms those chunks create there (a 2 MiB push's chunks span several blocks, so the terms exist by its last batch); format v3 creates the manifest in the head tx instead, a block later.

Format v3 (brotli, lane B) cuts the bytes of source repos by 28 to 35%; once it lands the default gets both faster and cheaper without any pacing, since fewer chunks need fewer blocks and less gas.

### 2.10 Format v3 on Rabdi (2026-11-11, MEASURED)

`perf/batch-b` with format v3 wired (head-model 13) against the installed v0.4.0, interleaved on fresh repos in one session, one wallet, `CHARTER_TRACE=1`.
Gas is the sum of `gasUsed` over every tx the run's addresses sent, read from receipts block by block (v0.4.0 traces name no tx hashes); every price was 1 gwei except the random 2 MiB pushes, which filled blocks and paid about 1.22 gwei on average (0.201161 tGLM for v0.4.0, 0.185485 for v3).
Clones ran from empty homes and match the pusher's `HEAD` and pass `git fsck --full --strict`.

| Operation | v0.4.0 s | v3 s | v0.4.0 gas | v3 gas |
|---|---|---|---|---|
| first push (200 KB source tree) | 31.2 | 24.8 | 5,837,736 | 4,843,037 |
| small push (3 runs) | 9.8 / 9.3 / 9.3 | 4.6 / 6.2 / 6.4 | 312,894 / 313,784 / 316,835 | 299,297 / 300,724 / 301,829 |
| refs-only push | 9.2 | 4.6 | 258,746 | 223,444 |
| 2 MiB source push (pieces) | 18.8 | 14.3 | 25,596,016 | 19,004,664 |
| 2 MiB random push (snapshot refresh; v3 adds a layer) | 34.8 | 24.5 | 165,282,443 | 152,506,363 |
| no-op fetch (3 runs) | 4.8 / 3.5 / 3.7 | 1.7 / 1.1 / 1.2 | 0 | 0 |
| clone right after a push | 7.3 | 4.0 | 0 | 0 |
| clone after the pieces expired (3 runs) | 6.8 / 8.3 / 7.1 | 5.8 / 5.4 / 3.8 | 0 | 0 |
| migration push (v2 repo with 2 carried packs, issue, pinned `ci-gated`) | 11.4 | 5.9 | 280,840 | 263,191 |
| program fold after it (`chart program status`) | 5.7 | 2.9 | 0 | 0 |
| private first push | 28.1 | 21.4 | 7,031,349 | 6,904,631 |
| private small push (3 runs) | 10.3 / 9.5 / 9.7 | 6.5 / 6.0 / 5.9 | 283,907 / 285,827 / 283,931 | 283,919 / 283,919 / 283,871 |
| private clone | 5.7 | 3.5 | 0 | 0 |

- The migration push compares with the same push by v0.4.0 on an identical repo; it is 4.5% of v0.4.0's first push (5,837,556 gas).
  After it v0.4.0 stops with `E_UNSUPPORTED_VERSION` on `chart program status` and on `git clone`; v3 clones and fsck pass, the issue op lists, and the fold shows the same state and heads as v0.4.0's fold of the twin repo.
- Private entities are padded to power-of-two buckets, so a smaller head body only helps across a bucket edge; the private small pushes differ by single zero bytes of ciphertext (12 gas each way, two v0.4.0 inits of identical repos differ by 96 gas).
- The first push's head tx with its manifest used 1,257,712 gas against 1,370,127 for v0.4.0's manifest tx plus head tx.
- Snapshot pack cost on a 118 MiB repo (MEASURED on this Mac, 10 cores, 284,496 objects, 12,293 commits, one ref; local only, a live push of it is over the 1 tGLM budget).
  v0.4.0's first push builds the thin pack it never sends (1.25 s) and two zlib packs reusing deltas (1.95 s; 2.6 CPU s in all): 133.9 MB stored.
  v3 builds no thin pack, then three raw packs with `--no-reuse-delta --window=50` (11.2 s, 71 CPU s) and brotli q6 (4.4 s): 77.7 MB stored, -42%.
  Window 10 instead would take 9.8 s (35 CPU s) plus 6.4 s of brotli for 97.4 MB in five packs, so window 50 is both faster end to end and smaller.
  The 12.4 s of extra CPU buys 56 MB fewer bytes on chain, which at the measured 10 MiB push rate (about 0.1 MB/s, 2.7) is several minutes and about 3.4 tGLM less (3.92M gas per 64 KiB chunk, section 1), so the setting stays unconditional.
- A push whose snapshot is due with any new piece (64 carried packs, 256 pieces, or carried pieces alone over the refresh threshold) builds no thin pack and runs no brotli; a refs-only push right at that threshold therefore refreshes one push early.
- A thin pack whose zlib fallback still passes the 100 MiB decode cap is sent as a snapshot instead.
- A v3 reader's cache holds text objects without zlib (4.2 MB against 2.9 MB for this repo); repacking them in the fetch would cost the clone time it saves, so it is not done (head-model 13.3).

#### Rerun after the review fixes (2026-11-11, MEASURED)

Same method, fresh repos, after the fixes for the independent review (fresh `at`, zlib fallback over the decode cap, owner-only state files, op probe, head gas bound).

| Operation | v0.4.0 s | v3 s | v0.4.0 gas | v3 gas |
|---|---|---|---|---|
| first push (200 KB source tree) | 32.1 | 22.5 | 5,837,652 | 4,843,463 |
| small push (2 runs) | 13.8 / 13.9 | 6.8 / 5.7 | 273,146 / 273,625 | 259,834 / 258,963 |
| no-op fetch (2 runs) | 4.6 / 4.6 | 2.1 / 1.5 | 0 | 0 |
| clone right after the pushes | 7.8 | 4.1 | 0 | 0 |
| 10 MiB push, 7 MiB Rust source and 3 MiB random (snapshot due) | 52.7 | 43.5 | 281,697,780 (0.423 tGLM) | 256,186,188 (0.349 tGLM) |
| private first push | 35.2 | 27.4 | 7,033,155 | 6,904,325 |
| private small push | 13.3 | 8.0 | 283,847 | 281,843 |

- Both clones match the pusher's `HEAD` and pass `git fsck --full --strict`.
- The 10 MiB push sent one head tx from `C_d` with status 1; its upload took 36 s (about 18 blocks), so it did not reach the 64-block window the fresh `at` protects, which test `a_push_that_uploads_for_many_blocks_signs_a_fresh_at_and_sends_no_follow_up_head` covers.
- A 100 MiB push was measured locally (the 118 MiB repo above): live, v0.4.0 alone would store 133.9 MB, about 15 tGLM at this run's 10 MiB rate, far over its 1 tGLM budget.

## 3. Proposals for a much larger cut (need the user's decision)

### 3.1 Storage tiers: hot entities, cold calldata

Observation: Rabdi keeps block history even though entity state is head-only.
After an entity expires, `eth_getTransactionByHash` still returns its full create calldata, and Rabdi still serves block 1 (MEASURED, `arkiv-limits.md` section 2).
So bytes written once as calldata stay readable without any storage gas.

Design:

- Cold data: pack bytes go out as zero-value self-transfers from `bulk`, up to 126,000 B of calldata per tx, with no entity.
- The anchor lists `(tx hash, sha256)` for every cold piece, so integrity comes from the signed anchor, exactly as today.
- Clone fetches cold pieces with batched `eth_getTransactionByHash`, checks each sha256 against the anchor and the pack against its id, then runs `index-pack` as today.
- Hot data: only what must be queryable stays an entity (identity, binds, the newest anchor and checkpoint per peer), plus an optional hot copy of the current snapshot.
- A per-repo policy selects the tier: `hot` (today's behaviour), `warm` (current snapshot hot, history cold), `cold` (all pack bytes cold).
- Anyone can re-materialise a cold piece as a hot entity later, since the bytes are verified by hash.

Effect (DERIVED): 64 KiB costs 2.66M gas once instead of 17.3M in year one and 14.6M per later year; big pushes fit about 11 pieces per block instead of 7, so they are about 1.5x faster too.

Trade-off, stated plainly: an entity is a priced availability promise that ends at its expiry; block history is kept by node operators today but is not a priced promise, and a pruned node would lose it.
Mitigations: `warm` keeps the current snapshot hot; any archive node or mirror can serve history, verified by hash; a cold piece can be promoted to hot at any time.

### 3.2 Upload ahead of push

A background step (post-commit hook or idle daemon, opt-in per repo) uploads each new commit's thin pack as cold data as soon as it exists.
At push time only the anchor tx remains, so a push takes one block (about 2 to 4 s) whatever its size.
Commits that are rewritten before pushing waste their upload; thin packs keep that waste small.
Private repos upload sealed bytes only, as today.

### 3.3 Example: Charter's own repo over one year (DERIVED, ESTIMATE)

- Today: 35 chunks (about 2.2 MiB written) created and kept alive, 12 anchors, refs and manifests: about 630M gas per year (about 0.63 tGLM).
- With thin packs, one-tx push, checkpoints and the `cold` tier: about 1.1 MiB written once (about 46M gas) plus 12 one-tx anchors (about 12M) plus about 2 hot anchors kept alive (about 3M per year): about 61M gas in year one, about 3M per later year.
- That is about 10x cheaper in year one and about 200x cheaper per later year.

### 3.4 One mutable head per peer, history as a transaction log

Status: IMPLEMENTED for v0.4.0 as format v2, with the changes decided on 2026-11-11 (per-device heads, rollback check by `entityNonce`, snapshot plus pieces, collaboration ops left as entities); the design is `docs/p5/head-model.md` and the measured run is section 3.15.
The design list below is the original proposal and is kept for the record.

Measured 2026-11-09 by `eth_estimateGas` on Rabdi (impersonating each entity's owner, nothing sent):

- Charter's anchors are read-only entities: a patch reverts with `ReadOnlyEntity`.
- A same-size patch of a mutable entity with 174 days left: 300 B costs 45,509 gas, which is the calldata price; the long remaining lifetime is not charged again.
- Growing the value is charged for the remaining lifetime: 300 B to 1,324 B costs 190,584 gas, to 4,396 B 639,121 gas; shrinking refunds nothing.
- A 64 KiB create with a 20-block lifetime costs 2,663,459 gas and with a 1-block lifetime 2,664,257 gas, the same as calldata only (2,656,536); a 4 KiB create with 20 blocks costs 200,986 gas.

Design:

- Each peer keeps one mutable head entity per repo, created once with a fixed-size slot (padding is zero bytes, 10 gas per byte in calldata instead of 40), kept alive like any control-plane entity.
- A push is one tx from `control`: `[create piece(s) with a 20-block lifetime, patch head]`.
  The pieces become calldata in block history; the head body carries seq, the refs root, the piece hashes, the hash of the previous push tx and skip pointers (previous tx hashes at distances 1, 2, 4, 8, ...), plus the signature.
- Storage no longer grows with pushes: each repo costs a constant number of live entities per peer, whatever its history.
- Discovery is one query (`ag.kind = str('head') AND ag.rid = ...`) returning every peer's current head, so a no-op fetch is one round trip and a clone is that query plus batched `eth_getTransactionByHash` for the pieces.
- Completeness: `control` now also creates short-lived pieces, so the head records the running count of pieces created, and the check becomes `live control-plane count = entityNonce - pieces created`, using a positive `OR` of control-plane kinds (Rabdi has no `NOT`).
- Rollback: a key holder could patch its head back to an older body; immutable (read-only) checkpoint entities every N pushes cap that window, and clients that saw the newer head reject the older one by seq, as today.
- Collaboration ops go into each author's own head log the same way, so they stop being one kept-alive entity each.
- A fork's head can reference the parent repo's piece tx hashes (pieces are verified by hash, wherever they sit), so a fork uploads no data.

### 3.5 Hash-chain signatures between checkpoints (needs a security review)

- At each checkpoint the composite ML-DSA-65 + Ed25519 signature also commits to `K_0`, the tip of a hash chain `K_i = H(K_{i+1})` derived from the master seed.
- Push i carries `HMAC(K_i, body_i)` instead of a 3,353 B signature; a tiny reveal patch in the next block publishes `K_i`, and clients accept push i once `H(K_i) = K_{i-1}` and the MAC checks.
- This is TESLA-style broadcast authentication with chain order standing in for clock sync: `K_i` is published only after push i is final, so nobody can forge a body with it, and hashes are post-quantum.
- The newest push is final at once but authenticated one block later, when its key is revealed.

### 3.6 Per-push estimate, 4 KiB thin pack (DERIVED from the measurements above)

| | Today | Head model | Head model plus hash chain |
| --- | --- | --- | --- |
| Txs | 4 | 1 | 1 plus a 1-block reveal |
| Gas | about 1.9M | about 0.37M | about 0.25M |
| Storage added, kept forever | about 2.7M gas per year | none | none |
| Push latency | 55 to 83 s | about 1 block | about 1 block |

Constant cost per peer per repo: about 21,000 storage units live (head, peer, bind, identity), about 4.6M gas (0.005 tGLM) per year.

### 3.7 Explored and rejected

- Cost-aware entropy coding that favours zero bytes (10 gas) over non-zero bytes (40 gas): the best possible gain is about 4.5 percent (optimum near 22 percent zero bytes), and it lengthens the data.
- A trained zstd dictionary for thin packs: 1.3 percent beyond plain zstd on Charter's history (MEASURED).
- Rebuilding tree objects from path edits: git's tree deltas already send roughly that.

### 3.8 Funded run on Rabdi (2026-11-09, MEASURED)

Scripts and raw results: the session scratchpad `headprobe/` (first run) and `headprobe2/` (clean chain); key: the spike's throwaway testnet key, never printed; spent 0.0279 tGLM in all.

- Head create, 4 KiB zero slot, 30 days: 292,953 to 294,957 gas.
- Five real Charter thin packs pushed as one tx each (`[create piece, 20 blocks] + [patch head]`, head carrying a 3,353 B random stand-in for the signature): 1,936 B 269,854 gas; 3,591 B 323,572; 5,475 B 398,792; 1,195 B 250,359; 6,079 B 421,953; receipt latency 1.5 to 3.3 s each.
- A 606,425 B push as 4 piece txs plus the head tx: 24.49M gas in all, mined over 4 blocks.
- Hash-chain variant, 675 B pack: 196,631 gas plus a 55,996 gas reveal patch.
- Same 3,591 B pack the current way, by estimate with the same attribute sets: chunk 338,587 + manifest 354,185 + refs 342,952 + anchor 949,991 = 1,985,715 gas in 4 txs, leaving 18,176 storage units alive, so about 4.0M gas per year for as long as the repo lives.
- So the head model is 6.1x cheaper per push and about 18x cheaper over the first year, and adds no recurring cost.
- After all 11 pieces expired (the owner's live entities dropped to the 2 heads plus 2 older spike entities), the client rebuilt all 5 pushes from the head entity and block history alone in 2.0 s: every piece and pack sha256 matched, `git index-pack --fix-thin` applied them on top of the base commit, `git fsck --full` passed and the final tree equalled the original.
- Charter's last 59 first-parent thin packs: median 4,927 B, p90 42,858 B, max 606,425 B.

Lessons from the run:

- The 4 KiB zero padding costs about 41,000 gas on every push (10 gas per zero byte), so the slot should be sized to the real head (about 1 KiB with a signature root).
- The hash-chain signature saved only about 45,000 gas net per push once the reveal patch is paid; it is not worth its security-review cost now.
- `updatedAt` on the head points at the last patch, which was the reveal, so finding the newest push's same-tx piece needs the body block recorded in the head (or no separate reveal).

### 3.9 What users would notice (downsides of 3.1 to 3.5)

Data and availability:

- Old history lives in block history, which node operators keep today but nobody is paid to keep; if Rabdi or other providers prune old blocks, cloning history fails with "piece unavailable" even though the repo looks alive.
  Mitigations: periodic snapshot packs, a `warm` tier that keeps the current snapshot as a paid entity, archive mirrors serving hash-verified pieces.
- Thin packs form a chain: one lost piece breaks every later push until the next snapshot, where today a missing self-contained pack loses only its own objects.
- A first clone of a long history walks many transactions; without snapshots and skip pointers, thousands of pushes mean thousands of fetches.
- Data was always permanent in block history, also today, so an accidentally pushed secret could never be erased; expiry only hid it from queries. Nothing changes here, but users may assume otherwise.

Visibility and tooling:

- Old pushes, pieces and collaboration ops stop being entities, so `arkiv_query` (and the hub or explorer entity views) no longer show them; counting chunks or listing issues by query, as in today's demo, stops working.
- Issue and patch lists need a local index or an indexer service, because ops live in each author's log rather than as queryable entities; the first `chart op list` on a new machine is slower.

Safety semantics:

- The head is mutable, so a key holder can patch it back to an older state; clients that saw the newer head reject that by seq, but a fresh client only detects rollbacks past the last immutable checkpoint.
- Arkiv has no compare-and-swap on patch: two concurrent pushes with one key (a copied key on two machines) make the second silently overwrite the first; today that is a visible `E_LOST_RACE`; the client must check after the receipt that its push is the one in the head.
- If the head expires because nobody extends it, the whole peer history becomes unreachable even though the bytes remain in blocks; heads should be created with `permissionlessExtension` so delegates and mirrors can keep them alive.
- With hash-chain signatures, the newest push is authenticated one block late, and a crash before the reveal leaves it unverified until the next push.

Limits and migration:

- A fixed head slot caps the inline refs list; repos with many branches or tags keep refs in a piece (one more fetch to list refs), and growing the slot later is charged for its whole remaining lifetime (measured: +1 KiB on a 174-day entity costs about 145,000 gas).
- Format v2 cannot be read by v0.2.0 clients; existing repos need a migration push; the demo queries, docs and deck numbers change.
- The `control` address now pays for pieces too, so funding moves from `bulk` to `control`.
- With upload ahead of push, commits rewritten before pushing waste their upload.

### 3.10 Small extras

- zstd recompression of thin packs: 15 percent fewer bytes (MEASURED); worth it only once the larger items have landed.
- Refs as a diff against the previous anchor keeps anchor size flat for repos with many branches and tags (part of item 4).

### 3.11 Outlook: Arkiv fixes and mainnet (2026-11-10, ESTIMATE)

Cost in money is gas x (base fee + tip) x GLM price, so each Arkiv-side change moves only one factor.

| Arkiv-side change | Effect on cost | Effect on time | Confidence |
|---|---|---|---|
| Block builder honours the 200M genesis gas limit (known bug) | None per op at idle; under load it keeps blocks under half full, so the base fee stays at the floor instead of rising 12.5 percent per full block | Big pushes about 6.7x faster (30M to 200M gas per block) | SOURCE-VERIFIED limit, DERIVED speedup |
| RPC stops dropping about 1 in 10 requests | None | Removes retries and stalls; reads stop failing | MEASURED today |
| Calldata repriced from the 10-gas-per-token floor (40.8 gas per byte) to plain 16 gas per non-zero byte | Calldata about 2.5x cheaper; a head-model push (3.4) about 1.6x cheaper, since about 60 percent of it is calldata | None | SPECULATIVE: no sign Arkiv plans it |
| Mainnet base-fee floor below Rabdi's 1 gwei | Linear: 0.1 gwei is 10x cheaper, 0.01 gwei 100x | None | UNKNOWN: no mainnet fee schedule is published |
| Mainnet storage rate (`capacity_rate`, 14 on Rabdi) | Linear in either direction; the docs call today's weights placeholders, and storage is the scarce resource, so it may rise | None | UNKNOWN |

At Rabdi's prices with GLM at 0.109 USD (the P0 assumption, not re-checked), costs are already small in money terms:

| Workload | v0.2.0 today | Head model (3.4), measured gas |
|---|---|---|
| One small push (3.6 KB thin pack) | 1.99M gas, about 0.0002 USD, plus 4.0M gas (about 0.0004 USD) per year kept alive, growing with every push | 0.32M gas, about 0.00004 USD, no growing upkeep |
| 100 MiB of history for one year | about 23.8 GLM (about 2.6 USD) as entities, every year | about 4.3 GLM (about 0.47 USD) as calldata, paid once |

So Arkiv's fixes mostly buy speed and steadier fees, not cheaper gas.
The money cost on mainnet rides on two numbers nobody has published yet (the base-fee floor and the storage rate) plus the GLM price.
The cuts Charter controls are the design changes: about 6x per push and no per-year upkeep with the head model, about 1.8x fewer bytes with thin packs, and they multiply with whatever Arkiv's mainnet pricing turns out to be.

### 3.12 Head model vs centralized storage vs IPFS (2026-11-10, ESTIMATE)

Prices: Arkiv at Rabdi's weights, 1 gwei, GLM 0.109 USD (recent quotes range 0.09 to 0.13); S3 Standard us-east-1 at 0.023 USD per GB-month, 0.005 USD per 1,000 PUTs, 0.09 USD per GB egress after the free 100 GB per month; IPFS pinning at the 20 USD per month for 1 TB tier that Pinata (Picnic) and Filebase (Starter) both list, with storage overage about 0.035 to 0.05 USD per GB-month and bandwidth overage about 0.015 USD per GB.
All are third-party or vendor-blog figures found on 2026-11-10, not checked against each provider's live price page; Lighthouse (IPFS plus Filecoin) lists almost the same rate, 120 USD per year for 500 GB.

IPFS is a protocol, not a store: bytes stay reachable only while some node pins them, so its cost is whoever pins.

| Per GiB of git history | Year 1 | 10 years | Reads |
|---|---|---|---|
| Charter v0.2.0 (chunks kept alive as entities) | about 31 USD | about 268 USD | free on the public RPC, rate limited |
| Charter head model (history in calldata, paid once) | 4.78 USD | 4.78 USD, if block history stays retrievable | same |
| S3 Standard | 0.30 USD | 2.96 USD | 0.097 USD per full clone after the free tier |
| IPFS on a pinning service, plan fully used | about 0.26 USD | about 2.6 USD, while the subscription is paid | about 1 TB per month included, then about 0.016 USD per GiB |
| IPFS pinned by the users themselves (every clone pins, Radicle-style seeding) | 0 USD | 0 USD | free, but only reachable while some seeder is online |
| Head model with the bytes on IPFS and only the head and CIDs on Arkiv | about 0.26 USD plus about 0.00002 USD per push | about 2.6 USD | IPFS gateway plus the RPC |

Plan granularity matters for small users: a repo under the free tiers (about 1 GB on Pinata) costs nothing, but the first paid tier is a 20 USD per month minimum, so one 2 GiB repo pays 240 USD per year on IPFS, not 0.5 USD; the per-GiB rows assume a host that fills the plan with many repos.

| Per small push (3.6 KB thin pack) | Cost |
|---|---|
| Charter v0.2.0 | 0.00022 USD, plus 0.00044 USD per year of upkeep that grows with every push |
| Charter head model | 0.000035 USD, no upkeep |
| S3 | about 0.000005 USD (one PUT; the storage is negligible) |
| IPFS | inside the plan quota, so about 0 at the margin |

Reading the tables:

- Per push the head model is about 6x cheaper than v0.2.0 but still about 7x dearer than an S3 PUT; per GiB-decade it is in the same range as S3 and pinned IPFS (4.78 vs about 2.6 to 3 USD), and it overtakes S3 after about 16 years of storage, or within about 4 years for a repo cloned in full 10 times a year once S3 egress is billed.
- What the extra cents buy: a public, ordered, signed record that the bytes were published at a block, readable by anyone without an account, which no single operator can delete or rewrite.
  S3 is one operator's say-so.
  IPFS adds content addressing (a CID proves the bytes are the right ones) but nothing about order, publication time, or which ref points where; mutable pointers need IPNS, which is slow to resolve and keyed to one publisher, and availability still rests on someone paying to pin.
- Latency: an Arkiv push confirms in one or two 2 s blocks; S3 answers in about 100 ms; an IPFS add is immediate on the pinning node, but first fetches by CID from elsewhere can take seconds to minutes while the DHT finds a provider.
- Biggest caveat for the head model: calldata is only storage while block history is served.
  Rabdi's history retention is unchecked (section 6), so a hash-verified mirror is needed either way, and IPFS is the natural one because a CID already is the hash; that makes the hybrid row (Arkiv for order and signatures, IPFS for bytes) the likely end state for large repos.
- Arkiv mainnet prices are unknown (section 3.11); every Arkiv number here scales linearly with the base-fee floor, the storage rate and the GLM price.

### 3.13 GitHub-class latency with the head model (2026-11-10, needs the user's decision)

The bar, measured from this Mac on the same 10 MB repo (`chayan-bit/charter`), 5 rounds each:

| | GitHub HTTPS | GitHub SSH | Charter perf branch (2.6) |
|---|---|---|---|
| clone | 3.26 s | 7.04 s | 7.76 s |
| no-op fetch | 0.72 s | 2.61 s | 3.79 s |
| `ls-remote` | 0.68 s | 2.59 s | n/a |

Latency floors on Rabdi (MEASURED 2026-11-10):

- One round trip on a warm connection: 312 to 328 ms; the first call of a process pays about 940 ms for TCP plus TLS.
- JSON-RPC batches work, `arkiv_query` included: 3 calls in one request took 357 ms, so a batch costs one round trip.
- WebSocket works on the same host: `eth_subscribe newHeads` delivers each block about 0.3 s after its timestamp, every 2.0 s; a request over the socket takes 309 ms.
- `eth_sendRawTransactionSync` (EIP-7966) is not available; the `pending` block tag is.

Root cause: latency is round trips times depth, plus one block for writes.
Today a no-op fetch is about 10 dependent round trips and a push reads the whole repo view before it writes; the round trip itself (about 320 ms) is longer than GitHub's, so depth is the lever we control and distance is Arkiv's.

The design combines four known pieces and adds one property only the head model has:

1. Zero-read push (new).
   Each head has exactly one writer, so the previous head, the nonce and the fee inputs are all local knowledge: a push signs and sends one tx with no reads before it, and its latency is one inclusion.
   To keep one writer per head when a person uses several machines, each device gets its own head under the identity (a device key the identity delegates), so a blind write can never conflict.
   Fees come from the published formula (3.11) and the last base fee seen, with headroom, instead of an `eth_estimateGas` round trip.
2. One-round-trip reads (head probe, item 1 of 2.1).
   One batched request returns every head of the repo plus the block number; if nothing moved, a fetch ends there.
   Verified state is cached per head hash, so only new heads are checked, and the program fold is incremental (item 2).
3. Race and verify (from git's `bundle-uri` and Radicle's seeding).
   Trust comes from the head; bytes come from whichever source answers first, requested in parallel: the local replica, a peer, an IPFS gateway, a CDN, or block calldata over the RPC.
   Every piece is checked against the hash in the signed head, so a fast untrusted source cannot change what is accepted.
   The head also carries the hash of a periodic snapshot bundle, so a clone is one download plus the thin packs since it.
4. Chain-driven local node (Radicle's model, driven by the chain).
   An optional `chart node` keeps one warm connection and a `newHeads` subscription, prefetches the packs of followed repos within about 0.3 s of a push landing, and serves `git fetch` from local disk.
   It also holds a durable outbox: `git push` can return once the signed tx is accepted by the RPC, and the node confirms it and resends if needed; the CLI reports "accepted" and "confirmed" separately, and the default stays "confirmed".

Latency budget per operation (ESTIMATE, built from the measured floors above: 0.62 s connect, 0.32 s per round trip, 1.0 s average wait for the next 2 s block, 0.3 s block notification):

| Operation | GitHub HTTPS | Charter today | Head model, plain CLI | Head model with `chart node` |
|---|---|---|---|---|
| no-op fetch | 0.72 s | 3.79 s | about 0.94 s (connect plus one probe); about 0.62 s with TLS session resumption | about 0.05 s (local) |
| small push, confirmed | not measured; about 1.5 s expected | about 60 s (1.3) | about 2.3 s (connect, send, one block, notification) | about 1.6 s |
| small push, accepted | n/a | n/a | n/a | about 0.32 s |
| clone, 10 MB | 3.26 s | 7.76 s | about 2.3 s (connect, probe, snapshot from the fastest source, index) | about 2 s, or local if followed |
| `program status` | n/a | 3.83 s | about 0.94 s | about 0.05 s |

Limits that design alone cannot remove:

- Confirmed pushes wait for a block: 2 s blocks put the average at about 1 s and the worst case at about 2 s on top of the send.
  Arkiv-side fixes would close it: preconfirmations (soft confirmation in under 0.5 s, as some L2s offer), shorter blocks, or `eth_sendRawTransactionSync`.
- Distance: 320 ms per round trip against GitHub's lower figure; a regional Arkiv RPC, or a read node we run near users, divides every read by roughly the ratio of the round trips.
- Write bandwidth: 30M gas per 2 s block is about 350 KiB/s of calldata for the whole chain, shared by every user (about 2.3 MiB/s if the 200M limit is honoured).
  A 10 MB push as calldata needs about 15 blocks (30 s) today, most of that gap is chain settings, not physics: see 3.14.

Order of work, smallest change first:

1. No format change (DONE on `perf/batch-a` except the head probe and cross-process TLS resumption, see 2.7): head probe, TLS session resumption, local fee formula, `newHeads` instead of receipt polling, one batch per stage. Target: no-op fetch under 1 s.
2. Format v2 with the head model: zero-read single-tx push, device heads, snapshot hash in the head, race-and-verify clone. Target: confirmed push about 2 s, 10 MB clone under GitHub's 3.26 s.
3. `chart node`: warm connection, subscription, prefetch, outbox. Target: fetch local, accepted push about 0.3 s.
4. Ask Arkiv: regional RPC, preconfirmations or `eth_sendRawTransactionSync`, and the 200M block gas fix.

Open risks: the outbox mode reports success before inclusion (needs honest UX and a node that survives restarts); device heads add key management; snapshots need a producer (the pushing peer or a CI job) and a retention rule.

### 3.14 Big pushes at GitHub speed, bytes on chain (2026-11-10, needs the user's decision)

Goal: a big push is limited by the user's upload link, not by the chain, and the push returns only when the bytes are on chain.
The bar (MEASURED): this Mac uploads at about 10.7 MB/s, so GitHub takes a 10 MB push in about 2 to 3 s.

Why the chain is slow (MEASURED on Rabdi and READ in `Arkiv-Network/arkiv` at 2a3526d):

| Cause | Kind | Today | What it costs us |
|---|---|---|---|
| Block gas target 30M; genesis says 200M | operator setting | the limit fell by 1/1024 per block from 200M (block 0) to 75M (block 1,000) to 30M (block 10,000 onward); the payload builder targets `--builder.gaslimit`, else the genesis value | 6.7x |
| 40 gas per data byte | placeholder price | Arkiv's own schedule charges 16 per byte (`G_BYTE`, marked provisional in `gas.rs`), but compute is floored at the EIP-7623 minimum of 40 per nonzero byte, which wins; we measured 40.8 | 2.5x vs 16 |
| Storage rent counted as block gas | design | `gas_used = compute + storage + purge`, and `gas_used` fills the block; a chunk kept 1 year pays about 264 gas per byte, of which about 223 is rent | 6.5x for entity chunks |
| Block size on the consensus network | physics of one chain | Lighthouse gossips blocks with the Ethereum default limit of 10 MiB per message (a per-network setting); with 2 s slots that is about 5 MiB/s | the real ceiling of one chain |

So one Rabdi chain carries about 55 KiB/s of year-long entity chunks and about 359 KiB/s of calldata today, against a physical ceiling near 5 MiB/s.
The first three rows are settings and a pricing choice, not limits of the hardware.
The EIP-7623 floor exists on Ethereum to bound block size for about a million gossiping validators; Rabdi has one sequencer, one follower and no L1 data posting (`kurtosis/arkiv-chain.yaml`: "There is no L1, L2, settlement chain").
Rent is a payment for disk over time, not work done inside the block, so it should not use block space; this is the multidimensional-gas idea Ethereum applied to blobs (EIP-4844 meters blob gas separately).

Checked and ruled out: blobs (the public RPC caps request bodies at 256 KiB over HTTPS and WebSocket, and a one-blob tx is about 262.5 KB as hex; blobs are also pruned and unreadable by the entity engine); parallel wallets (the limit is per block, not per sender); zero-byte encodings (the floor charges 10 per zero byte and 40 per nonzero byte, so the best encoding gains about 4%).

Proposal, in order:

1. Measure the real ceiling on a private devnet (the repo's kurtosis devnet, built and run on staging-entity): stock config, then `--builder.gaslimit 200000000`, then a patch that meters rent separately from block gas and prices data at `G_BYTE`, then a larger gossip limit.
   Drive it with Charter's own push path and record MB/s per step.
2. Make the client saturate whatever the chain offers: pipelined nonces, many 128 KiB txs in flight over parallel connections, one signature per push (already approved), no per-chunk reads; target at least 80 txs per second sustained.
3. Take the numbers and the patch to Arkiv as an issue and PR on `Arkiv-Network/arkiv` (only with the user's OK): restore the 200M target, meter rent apart from block gas, replace the EIP-7623 floor with a data price fit for a db-chain, raise the RPC body limit.
   The schedule is explicitly provisional, so this is the moment to change it.
4. Beyond one chain: stripe a push's chunks across several db-chains (Arkiv's design is many db-chains under one L2) and commit the per-chain roots in the head, sent after every stripe lands.
   Throughput adds per chain, the way GitHub adds file servers, so the user's link stays the limit as load grows.

| Push | GitHub (DERIVED) | Today, entity chunks | Today, calldata tier (3.1) | 200M target | + rent off the block meter, 16 gas/B (one chain, 5 MiB/s) | + striping over 2 to 4 chains |
|---|---|---|---|---|---|---|
| 10 MB | about 2 to 3 s | about 3 min | about 28 s | about 4 to 5 s | about 2 to 3 s | about 2 to 3 s |
| 100 MB | about 10 to 12 s | about 29 min | about 4.7 min | about 43 s | about 20 s | about 10 to 12 s |
| 1 GB | about 1.6 to 2 min | about 5 h | about 49 min | about 7.3 min | about 3.4 min | about 1.6 to 2 min |

The last two columns are ESTIMATES until step 1 measures them; execution speed in GolemDB and follower bandwidth are not yet measured and could lower the ceiling.

### 3.15 Format v2 on Rabdi (2026-11-11, MEASURED)

Release build of v0.4.0 rebased on `main` 611da1f, against the v0.3.0 release, both on Rabdi with 1 gwei base fee; design in `docs/p5/head-model.md`.

| Scenario | v0.3.0 | v0.4.0 (format v2) |
|---|---|---|
| (a) new repo, init plus first push | - | 38.1 s, 19 txs, 3.85M gas |
| (b) small push, 5 interleaved rounds | 24.4 to 28.1 s, 6 txs, about 968k gas | 11.7 to 13.6 s, 1 tx, 340k to 354k gas |
| (c) 10 MiB push (snapshot refresh) | - | 72.9 s, 164 txs, 432M gas, 0.697 tGLM |
| (c) 2 MiB push (pieces) | - | 26.6 s, 34 txs, 84.9M gas, 0.0947 tGLM |
| (d) migration push of a v0.3.0 repo | - | 38.4 s, 8 txs, 1.13M gas |
| (e) two devices, different branches | - | both land, 12.6 s and 14.7 s |
| (e) two devices, same branch | - | winner 11.7 s; loser `E_LOST_RACE` |
| (f) clone after the pieces expired | - | 18.3 s for 12 MiB; 11.3 s for a small repo |
| (h) private repo, push and clone | - | 31.9 s first push, 12.2 s next push, 4.2 s clone |

- (b) A small push is 2x faster and 2.8x cheaper.
  Most of the remaining 12 s is round trips: about 13 s of RPC calls at 300 to 500 ms each in the trace (the verified sync before the push, then the re-check after the receipt), of which about 3 s is the re-check; the head transaction lands in one block.
- (c) The 10 MiB push cost more than section 3.14 estimated, because a snapshot refresh writes the whole pack as 30-day entity chunks; the base fee also rose to about 1.6 gwei during it.
- (d) After the migration v0.3.0 fetch and clone stop with `E_UNSUPPORTED_VERSION`; the v0.4.0 fetch and a fresh clone match, the issue is still listed, `git fsck` is clean.
- (h) No ref name, commit message or file content appears in plaintext in the calldata of the private repo's transactions.
- (i) CI-gated program on a v2 repo: a contributor's patch pushed to its own namespace, a CI attestation by the runner key, the delegate's merge to `main`, a release and its artifact attestation, and `chart release verify` reports it valid; a push of an unreviewed commit to `main` is refused with `E_PROGRAM_REJECTED` before any transaction.
- (j) `chart ui` shows the device head (seq, body, block) and the merged refs, and picks up a new push within one sync round.
- The live probe for the rollback rule and the block-history depth are in `docs/p5/head-model.md` section 12.

Re-run on the build with the review fixes (`aefc0b0`: pinned head nonce, fresh-read race check, signed base check, v1 refs keepalive):

- (b) 3 interleaved rounds: v0.3.0 26.3 to 27.9 s, 6 txs, about 915k gas; v0.4.0 12.4 to 13.5 s, 1 tx, 265k to 271k gas.
- (d) A v0.3.0 repo with the CI-gated program pinned before migration, a patch, a CI pass and a merge in v1: the migration push took 37.7 s (8 txs, 1.13M gas); a fresh v0.4.0 home folds it in 7.7 s to the same `main`; v0.3.0 stops with `E_UNSUPPORTED_VERSION`.
  The fold still reads the v1 refs entities from the pin onward, so `chart keepalive` now keeps them: its keep set for that repo is 16 entities, against 14 without them.
- (e) Different branches from two devices: both land (12.1 s and 11.6 s); same branch: winner 13.3 s, loser `E_LOST_RACE`.
- (e) A fresh device whose first push started before another device landed `main` gets `E_LOST_RACE` ("moved refs/heads/main after git checked it") and leaves the landed commit in place.
- (e) Two machines with one copied device seed pushing at once: one lands, the other stops with `E_CONCURRENT_PUSH` before a second head transaction, and its retry lands with both branches listed.
- (i) Patch 27.2 s, CI 24.3 s, merge 46.3 s, release 8.7 s, attestation 19.6 s, verify from a fresh home valid; an unreviewed push gives `E_PROGRAM_REJECTED` and leaves no staging refs.

## 4. Rejected

- Creating chunks with a short lifetime and extending them later: the total gas is the same; it only moves the payment and adds a data-loss risk if the extension fails to land.
- Dropping or downgrading ML-DSA.
- Anything EVM-first.
- Accepting a push before its bytes are on chain (seeds now, chain later): the user rejected it on 2026-11-10.

## 5. Open decisions

- Storage tiers (section 3.1) and the default tier for new repos.
- Upload ahead of push (section 3.2).
- Big pushes on chain at upload speed (section 3.14): devnet measurement, client pipelining, the Arkiv asks, striping across db-chains.
- Hash-chain signatures between checkpoints (section 3.5); the head model of section 3.4 is decided and implemented.
- Groth16 wrap of ZK receipts: 223,898 B to under 1 KB and about 8 ops to 1, at the price of a trusted setup that `docs/p4/zk-sealed-patch.md` deliberately avoids.
- An untrusted read cache (CDN or GitHub Releases) serving hash-verified pieces: GitHub-speed clones with an unchanged trust model; this is a product choice.
- Ask Arkiv upstream to lift the block builder from 30M to the 200M gas its genesis configures: big pushes about 6x faster for free.

## 6. Not yet checked

- Whether a multi-op `execute` tx is all-or-nothing on Rabdi (item 4 assumes it).
- Whether `arkiv_query` works inside a JSON-RPC batch.
- Whether Rabdi's RPC watchers will prune block history later: on 2026-11-11 `eth_getTransactionByHash` still served transactions back to block 1, about 11 days (head-model section 12).
- A fair wall-time comparison of compressed vs uncompressed responses.
- Section 3.8 covers one key and one repo shape; section 3.15 exercises two devices of one identity, a snapshot refresh and the rollback rule, but not yet a repo with several delegates.
