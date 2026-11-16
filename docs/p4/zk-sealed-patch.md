# P4: zero-knowledge proofs for sealed patches

Status: demo-scale implementation, real proofs (no dev mode).
Date: 2026-11-07.
Code: `crates/arkiv-git-zk-core` (relation), `crates/charter-zk` (verifier, prover behind feature `prove`), `zk/` (guest and demo prover), `scripts/zk-prove.sh`, `chart zk verify`.

## 1. The relation

A sealed patch is a patch `P` encrypted under a key `k` that the author does not reveal yet.
The author proves, without revealing `k` or `P`, that they know `k` and a ciphertext `C` such that:

1. `sha256(k) = H`.
2. `C = nonce || XChaCha20-Poly1305(k, nonce, P)` decrypts and authenticates under `k`, giving `P`.
3. `P`, applied to the public base file `F`, gives a file `R`.
4. `R` satisfies the public spec `S`.

The public statement (the journal, `arkiv_git_zk_core::Public`) is `H`, `sha256(C)`, `sha256(F)`, `sha256(S)` and `S` itself.
The guest computes all four hashes from the witness and commits them, so a receipt for a statement is a receipt that the hashes match.
`k`, `P` and `R` never leave the prover.
`C` and `F` are private inputs to the guest but are public in practice: the op publishes `C`, and `F` is in the repo, so anyone can check them against the hashes.

Patch format (`apply_patch`): one op per line, applied in order with a cursor over the base lines.
`= N` copies the next N base lines, `- TEXT` drops the next base line and requires it to equal TEXT, and `+ TEXT` emits TEXT.
Base lines left after the last op are copied unchanged.
Lines are split on `\n` and rejoined with `\n`, so trailing newlines survive.

Spec format (`spec_holds`): one check per line.
`+ TEXT` requires `R` to contain TEXT, `- TEXT` requires `R` not to contain TEXT, and any other non-empty line fails the spec, so a typo can never pass silently.
A richer spec (a Rhai predicate or a pure function test) slots into `spec_holds` without changing anything else.

## 2. Why RISC Zero

RISC Zero (`risc0-zkvm` 3.0.6, then the latest stable; now pinned at 3.1.0) and SP1 (`sp1-sdk` 6.10) were compared for this relation.
SP1's light verifier crate, `sp1-verifier`, only verifies Groth16 and Plonk wraps, which need a trusted setup and were ruled out.
Verifying a plain SP1 STARK proof needs the SP1 prover stack on the verifying machine.
RISC Zero verifies its succinct STARK receipts with `default-features = false`, so the CLI links a verifier without the prover.
Its toolchain installs without root (`rzup` from crates.io, components into `RISC0_HOME`), and it proves on CPU, with Metal on Apple Silicon.

## 3. Trust assumptions

- STARK soundness: the FRI-based STARK and the recursion circuit that compresses segment proofs into one succinct receipt are sound at RISC Zero's stated security level.
- zkVM circuit correctness: the rv32im circuit faithfully executes the guest ELF, and the guest code (`zk/methods/guest`, which calls `arkiv_git_zk_core::check`) is the relation above.
- Pinned image id: `charter_zk::IMAGE_ID` names the exact guest ELF.
  `verify` checks the receipt against that constant, so a receipt for any other program fails.
  `zk/prover` refuses to prove when the built guest does not hash to the pinned id.
  The guest is built with the local RISC Zero toolchain, not the Docker build, and the id does not depend on the machine: anyone with the same sources and toolchain (`rzup` rust 1.97.0) gets the pinned id.
  `zk/methods/remap-rustc.sh`, the rustc wrapper `zk/methods/.cargo/config.toml` sets for the guest build, makes that hold: it remaps `CARGO_HOME`, the repo root and build-script `OUT_DIR`s to fixed prefixes (panic locations embed source paths), and replaces cargo's `-C metadata` for the in-repo guest crates, which hashes their absolute path, with their name and version.
  Checked 2026-11-16: two Linux x86_64 machines, different checkout directories, `HOME`, `CARGO_HOME` and target directories, produced byte-identical guest ELFs (sha256 `a8debd45...a3de16`) and the pinned id.
  An environment `RUSTC_WRAPPER` overrides the config; `scripts/zk-prove.sh` puts the remap wrapper in its place and chains the original one, and fails before proving if the ELF contains the home or repo path.
- No trusted setup: receipts stay STARKs; there is no Groth16 wrap.
- No dev mode: the verifier enables `risc0-zkvm/disable-dev-mode`, and `verify` also rejects any receipt that is not a succinct STARK receipt.

The proof says nothing about whether `S` is a good spec; a weak spec makes a weak claim.
`H = sha256(k)` lets the author later reveal `k` and anyone check it is the committed key.

## 4. Measured numbers

One real proof of the demo in `zk/demo/` (a one-line fix to a Rust `add` function) with `risc0-zkvm` 3.0.6, on an Apple Silicon MacBook Air (10 cores, 16 GB), 2026-11-07, while other cargo builds were running (load average about 10).

| What | Value |
|---|---|
| Guest execution | one segment, 2^17 cycles padded (`SEGMENT_PO2 = 18`) |
| Prove (segment proof, lift to succinct) | 53.1 s wall, 272 s CPU |
| Peak memory | 1.49 GB max RSS, 2.0 GB peak footprint |
| Receipt (bincode succinct `Receipt`) | 223,898 bytes |
| Verify, release build (in `zk-prover`) | 18.5 ms |
| `chart zk verify`, debug build, whole process | 0.64 s |
| Image id | `IMAGE_ID` in `crates/charter-zk/src/lib.rs` |

The upgrade to `risc0-zkvm` 3.1.0 (2026-11-16) was measured on a shared Linux server (AMD EPYC 9454, CPU only, `RAYON_NUM_THREADS=16`), five interleaved runs of each version.
These CPU numbers are not comparable with the Metal numbers above.

| What | 3.0.6 | 3.1.0 |
|---|---|---|
| Prove wall, mean of 5 (range) | 14.87 s (14.72 to 15.06) | 14.88 s (14.79 to 15.01) |
| Prove CPU, mean of 5 | 231.5 s | 231.4 s |
| Peak memory (max RSS) | 1.44 GB | 1.44 GB |
| Receipt (bincode succinct `Receipt`) | 223,898 bytes | 223,898 bytes |
| Verify, release build (in `zk-prover`) | 15.0 ms | 15.0 ms |

Receipts are tied to the verifier version: a 3.1.0 verifier rejects 3.0.6 receipts ("parameters digest" mismatch), so the upgrade re-proved the fixtures.
The path remapping changed the image id but not the speed: three interleaved runs gave 15.03 s before and 15.06 s after (mean prove wall), with the same peak memory, receipt size and verify time.
The fixture receipt is `crates/charter-zk/tests/fixtures/demo.receipt` (sha256 `5e3c3e523d41662d851a66fa9adc488a691399266220af5d9d1841312654f3a5`).
Building the prover on macOS compiles RISC Zero's Metal kernels; that needs `scripts/macos-shims/xcrun` (nix xcrun cannot see the Metal toolchain, and risc0 kernels need a macOS 14 Metal language target), which `scripts/zk-prove.sh` puts on `PATH`.
The verifier-only build links about 180 crates (`cargo tree -p charter-zk -e normal`), mostly the RISC Zero verifier and its field and hash code.

## 5. How repo programs use it

A sealed patch travels as an `xyz.arkiv.sealed` `commit` op whose `commitment` is `H` and whose payload carries a proof attachment (docs/p3/programs.md, "Proofs").
The receipt bytes travel in `xyz.arkiv.blob` ops; the attachment names them by hash and carries the public statement (`H`, `sha256(C)`, `sha256(F)`, `sha256(S)`, `S`).
Before every fold the client resolves the receipt, runs `charter_zk::journal` and compares the journal with the claimed statement exactly.
The program sees the result as `op.proofs[i]` with `ok`, the verified `image_id` and the statement, never the receipt.
The program never verifies proofs itself; it decides policy.
`programs/sealed-patch.rhai` lets an issue require, allow or ignore proofs, and counts a required-proof commit only when `ok` holds, the image id is the pinned guest, `spec_hash` is the issue's and `key_hash` is the commitment.
Results are cached by receipt hash, so re-folds do not verify again.
Checking `sha256(C)` against a published blob and `sha256(F)` against the file at a commit is left to the program or a later client step; the receipt already binds both hashes.
When the author later reveals `k`, clients check `sha256(k) = H`, decrypt `C` and apply `P`, and get the file the proof promised.

## 6. Running it

Verify (cheap, any machine): `chart zk verify --receipt demo.receipt --public demo.public.json`.
Prove (heavy): `mac-heavy scripts/zk-prove.sh`, which installs the toolchain into `.zk-toolchain/` on first run, builds the guest and prover, and writes `receipt.bin`, `public.json` and `sealed.bin` to `zk/out/`.
The prover generates the demo key in memory and never writes it anywhere.
`ZK_PROVE_TIMEOUT_S` bounds the proving run (default 1800 s).
The root workspace links only the verifier, so `cargo test --workspace --all-features` needs no shim; proving code lives in `zk/prover` (outside the workspace) and only `scripts/zk-prove.sh` puts `scripts/macos-shims` on `PATH`.
After any change to the guest or `arkiv-git-zk-core`, the image id changes: rebuild, copy the new `SEALED_PATCH_ID` into `IMAGE_ID`, re-prove and replace the fixtures.
