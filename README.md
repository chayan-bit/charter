# Charter

## ETHGlobal Mumbai 2026

Charter is decentralized git hosting on Arkiv: a stock `git push arkiv::<rid>` and `git clone arkiv::<rid>` work end to end on the Rabdi testnet with no server of ours in between.
It is a redesign of the Radicle (heartwood) protocol in which Arkiv replaces gossip and seed nodes, and every client verifies every signature and fails closed on rollback, expiry or missing data.
A live 10 MiB push and clone have passed on Rabdi through the real binaries (numbers below), and the 100 MB gate passes against an in-memory Arkiv.

Submitted to:

- Arkiv: "Build on Arkiv (from scratch)".
- Ethereum Foundation: "Best CROPS App" (see the `CROPS` section below).
- ETHGlobal finalist judging.

Demo videos, every scene recorded live on the Rabdi testnet: the [all-live walkthrough](https://github.com/chayan-bit/charter/raw/demo-assets/arkiv-git-demo-live.mp4) (2:30, chain waits sped up 3x and captioned so) and the [AI agent bounty](https://raw.githubusercontent.com/chayan-bit/charter/demo-assets/agent-bounty-codex-live.mp4) (39 s).

Notes for judges, in `arkiv/`:

- [`arkiv/schema.md`](arkiv/schema.md): every entity kind, its indexed attributes, owner, creation flags, lifetime and queries.
- [`arkiv/friction.md`](arkiv/friction.md): reproducible feedback on building against Arkiv, with a list of what would make it easier.
- [`arkiv/reach.md`](arkiv/reach.md): who this is for.
- [`arkiv/why-arkiv.md`](arkiv/why-arkiv.md): what users would lose if the backend were Postgres.
- [`arkiv/how-you-use-arkiv.md`](arkiv/how-you-use-arkiv.md): how filters, ownership, expiry and permissionless extension change what users see.

## Try it in 60 seconds

Charter's own source is hosted on Arkiv by Charter, and its development runs there: every change to `main` is an Arkiv patch that the repo's pinned program accepts only with a delegate approval and a passing CI attestation posted on Arkiv by the CI runner.
GitHub is a read-only mirror plus the host of the release binaries.

```sh
# 1. install the prebuilt binaries (macOS arm64 or Linux x86_64), see docs/install.md
TAG=v0.6.0 TARGET=aarch64-apple-darwin   # or x86_64-unknown-linux-gnu
mkdir -p ~/.local/bin && curl -fsSL "https://github.com/chayan-bit/charter/releases/download/$TAG/charter-$TAG-$TARGET.tar.gz" | tar xz -C ~/.local/bin git-remote-arkiv chart chart-mcp
export PATH="$HOME/.local/bin:$PATH"

# 2. clone this project from Arkiv, with no keys and no server of ours
git clone arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ charter

# 3. see the program that governs it, its patches and their CI attestations, read straight from Arkiv
cd charter
chart program status --repo arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ
chart op list --repo arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ
```

Reads without an access key are rate limited by Arkiv, so a keyless clone may pause once for about 20 seconds.

Everything below ran live on the Rabdi testnet, with tx hashes and explorer links in each report:

| What | Evidence |
| --- | --- |
| A real AI agent (headless Codex through `chart-mcp`) claims a ZK-proof-gated bounty and the escrow pays it on-chain | [`docs/live/agent-bounty.md`](docs/live/agent-bounty.md), [video, 39 s](https://raw.githubusercontent.com/chayan-bit/charter/demo-assets/agent-bounty-codex-live.mp4) |
| Push, clone, a program refusing a push, patch review, private repo, keyless clone, every scene live | [`docs/live/demo-video.md`](docs/live/demo-video.md), [video, 2:30](https://github.com/chayan-bit/charter/raw/demo-assets/arkiv-git-demo-live.mp4) |
| Multi-peer patches, a 2-of-2 delegate quorum, revert and rewind, branch protection, escrow payout | [`docs/live/collab-escrow.md`](docs/live/collab-escrow.md) |
| Private MLS repos with roles and member removal, programs on private repos, ZK sealed-patch proofs, timelock | [`docs/live/private-proofs.md`](docs/live/private-proofs.md) |
| Developing Charter on Arkiv with a CI-gated `main` | [`docs/p3/arkiv-ci.md`](docs/p3/arkiv-ci.md), [`CONTRIBUTING.md`](CONTRIBUTING.md) |

## What it is

Decentralized git hosting on Arkiv: a redesign of the Radicle (heartwood) protocol where Arkiv replaces gossip and seed nodes.
Arkiv is trusted for ordering, availability and timestamps only; clients verify every signature.

Plan of record: the "Arkiv Git: decentralized code hosting plan" doc (private Claude Doc).

## Phases in this repo

- P0 spike: measure Arkiv limits, confirm OpenMLS X-Wing support, decide fork vs depend on heartwood. Findings in `docs/p0/`.
- P1 core: `git-remote-arkiv`, identity documents, signed refs, ref anchoring, search.
- P2 private: encrypted repos with MLS groups (X-Wing), roles, rotation, blinded annotations.

- P3 collaborative objects and repo programs: on feature branches, not merged to `main` (see "In progress" below).

Testnet only. Never use real funds or mainnet keys.

## Status on main (2026-11-07)

- P0 closed: limits and costs measured on the Rabdi testnet; see `docs/p0/README.md` for numbers, decisions and open questions.
- P1 implemented: `git push arkiv::<rid>` and `git clone arkiv::<rid>` work end to end.
- P1 gate passes against the in-memory Arkiv fake: a 100 MB repo pushes in about 18 s and clones in about 2 s, and a replayed old anchor or refs entity is rejected.
- On Rabdi a 10 MiB gate has passed live (below); the full 100 MB Rabdi run waits on testnet funding (open decision 2).
- Push funding uses rolling top-ups (design 4.2 step 6): the wallet needs the predicted spend plus one window of in-flight max fees, not 4.4 times the spend.

### Live Rabdi gate (2026-11-07)

All numbers MEASURED on Rabdi through the real `git push arkiv::` and `git clone arkiv::` binaries, every entity at a 1 day lifetime.

- Size: 10 MiB of incompressible blobs in 161 chunks, all 161 live after the push.
- Push of 10 MiB: 519.7 s over 261 blocks and 171 txs (161 chunks, manifest, refs, anchor and about 7 wallet top-ups).
- Second small push: 30.0 s over 16 blocks and 4 txs.
- Fresh clone in a second home: 13.2 s; HEAD, tree and blob bytes match and `git fsck --full --strict` is clean.
- Replay: the old anchor and refs re-posted from a foreign address and the old anchor re-posted from the peer's own control address; a fresh clone still gets the newest head.
- Stale head: a client holding the newest mark, served a view that hides the newest anchor with `entityNonce` lowered to match, gets `E_STALE_HEAD` (confirmed by a read-only recheck of the same repo after fixing a bug in the test's hiding proxy, which first made the check fail closed with `E_INCONSISTENT_VIEW`).
- Base fee: 1 gwei at start and at peak during both pushes.
- Rate limits: no 429 waits with the access key.
- Net spend: 0.4352 tGLM for the whole run (preflight predicted 0.4437), funded with rolling top-ups from a 0.59 tGLM wallet, all leftovers swept back.
- The push ran at about 0.6 chunks per block, well under the 4 per block pacing allows; per-tx RPC round trips (estimate, send, receipt polls) are the likely cause (not yet profiled).

### Merged features

Every feature below is on `main`, tested on the in-memory Arkiv in CI, and run live on Rabdi as recorded in `docs/live/`.

- Signed collaborative-object ops (issues, patches, reviews, attestations): `chart op post`, `op list`, `op attach-proof`; format in [`docs/p3/cob-ops.md`](docs/p3/cob-ops.md).
- Repo programs: deterministic Rhai pinned by hash in the identity document, enforced on every fetch and push; templates in `programs/`, contract in [`docs/p3/programs.md`](docs/p3/programs.md).
- Escrow: an accountable agent pays program obligations on-chain, with a public audit; [`docs/p3/escrow.md`](docs/p3/escrow.md).
- Private repos: MLS groups with X-Wing, sealed entities with blinded attributes, roles and member removal; [`docs/p2/private-repos.md`](docs/p2/private-repos.md).
- ZK sealed patches: a RISC Zero receipt proves a sealed patch meets a spec, and the repo program decides whether a proof is required; [`docs/p4/zk-sealed-patch.md`](docs/p4/zk-sealed-patch.md).
- Timelock seal and open with drand quicknet; [`docs/p3/timelock.md`](docs/p3/timelock.md).
- AI agents: scoped, revocable agent grants ([`docs/p3/agents.md`](docs/p3/agents.md)) and an MCP server for Claude, Codex or any MCP client ([`docs/p3/mcp.md`](docs/p3/mcp.md)).
- Arkiv-native CI: a runner posts signed CI attestations and `programs/ci-gated.rhai` gates `main` on them; [`docs/p3/arkiv-ci.md`](docs/p3/arkiv-ci.md).

## How to try it

Everything below needs the repo's dev shell, which provides the pinned Rust 1.97 toolchain, git, cargo-audit and Node 22.

```sh
nix develop
cargo build --release
export PATH="$PWD/target/release:$PATH"   # git finds git-remote-arkiv through PATH
```

### Locally, with no chain

The `mem:` backend is an Arkiv fake that persists to a file.
The end-to-end test drives the real `chart` and `git-remote-arkiv` binaries against it, with two users, `git push`, `git clone` and `git fsck --full --strict`:

```sh
cargo test -p charter-cli --test e2e
```

To do the same by hand, point the CLI at a log file:

```sh
export CHARTER_BACKEND=mem:/tmp/arkiv.log
export CHARTER_HOME=/tmp/charter-demo   # optional, keeps the seed out of ~/.charter
chart keys init                         # prints your nid and wallet address
chart fund --dev 1000                   # dev faucet: credits 1000 tGLM on the fake chain
chart init --name demo                  # prints rid and url
git push arkiv::<rid> main                  # from any git repo with a commit on main
git clone arkiv::<rid> demo-clone
```

`chart fund --dev <amount>` works only with a `mem:` backend; on any other backend it refuses, because real chains are funded from a faucet.
`demo/programs.sh` runs the repo-program walkthrough (branch protection, local refusal, approved push, clone) the same way.

### On the Rabdi testnet

Testnet funds only.
Never use real funds or mainnet keys.

1. Get an Arkiv access key at hub.arkiv.network (wallet sign-in and a captcha, so a person has to do it).
   Anonymous reads run out after about 1,000 queries.
   `export ARKIV_ACCESS_KEY=<your key>` and never commit it.
2. `chart keys init` creates a seed under `$CHARTER_HOME` (default `~/.charter`) and prints your nid and the wallet address.
3. `chart fund --bytes 10485760` prints the wallet, its balance and an estimate of what an init plus a push of that size needs.
   It makes no transfer: send tGLM to the wallet from the faucet at hub.arkiv.network/faucet.
4. `chart init --name demo --lang rust --license MIT` creates the repo with you as its single delegate and prints `rid:` and `url: arkiv::<rid>`.
5. In any git repo: `git remote add arkiv arkiv::<rid>` and `git push arkiv main`.
6. Anywhere else: `git clone arkiv::<rid> demo-clone`.
7. `chart status arkiv::<rid>` shows each peer's head and the minimum expiry in days, and `chart keepalive arkiv::<rid> --dry-run` shows what it would extend.

### Upgrading to v0.6.0

v0.6.0 hardens private repos, adds `chart device revoke` and `chart member sync`, and keeps large ref lists in ref tables (format v4); `docs/install.md` has the full upgrade notes.
A private repo moves to keyring format 2 on its first v0.6.0 membership change, which a delegate of the root identity must make, and from then on a v0.5.0 member stops with `E_UNSUPPORTED_VERSION`.
`chart member sync --repo <url>` removes the MLS leaves of revoked devices in one commit and prints its cost.
A revoke stops a device seed, not an identity: a stolen machine that also held `keys/` can still act as you, so follow "Lost or stolen machine" in `docs/p2/private-repos.md`.
A device past about 550 to 900 refs now writes a format v4 head with a ref table instead of failing with `E_REFS_TOO_LARGE`.
v0.5.0 stops with `E_UNSUPPORTED_VERSION` on a repo with a v4 head, a revoke record or a migrated private keyring, instead of showing stale refs, so upgrade every machine that reads the repo.

### Storage format v3 (v0.5.0)

From v0.5.0 packs are stored brotli-compressed, a head names its pieces as compact block runs, and a push patches only what changed in the head (`docs/p5/head-model.md` section 13).
The first v0.5.0 push of each machine migrates its head automatically; no command is needed.
A v0.4.0 client stops with `E_UNSUPPORTED_VERSION` on a repo once any synced peer has pushed with v0.5.0, instead of showing stale refs, so upgrade every machine that reads the repo.

### Storage format v2 (v0.4.0)

From v0.4.0 a push writes one small transaction: each machine keeps its own signed head entity per repo, small pushes ride in that transaction, larger ones go first as short-lived pieces, and a kept-alive snapshot is refreshed after about 1 MiB of changes or 64 pushes (`docs/p5/head-model.md`).
The first v0.4.0 push to an older repo migrates it automatically; no command is needed, and a second machine with a copy of `keys/` becomes a second device on its first push.
A v0.3.0 client stops with `E_UNSUPPORTED_VERSION` on a repo once any synced peer has pushed with v0.4.0, instead of showing stale refs, so upgrade every machine that reads the repo.

`CHARTER_DATA_LIFETIME=1d` shortens the data lifetime for demos; the live gate used 1 day.
The default is 30 days for data and 182.5 days for the control plane.

## Wallets and a query you can run

The live demo repo on Rabdi, created on 2026-11-07 with 1-day lifetimes (43200 blocks) so it stays readable through judging:

- rid: `z5b15ZhFLNackCqiyAL5y4dMj4MFGfVrtgLmZ9PPDU1AH`, so `git clone arkiv::z5b15ZhFLNackCqiyAL5y4dMj4MFGfVrtgLmZ9PPDU1AH` works with no keys.
- rid in hex (revision 0 hash): `4424958cfcfb43cac8e202206fb5640c30855c2b800151023839fcc3a5bde652`.
- Flow: init, push `v0`, `program pin` of `programs/branch-protection.rhai`, a direct push of `v1` refused locally with `E_PROGRAM_REJECTED`, a patch op and an approve op at `c9cf57a`, then the same push accepted.
- A fresh clone in a second home gets `c9cf57a feat: v1`, and `chart program status` shows the approved patch and `head: refs/heads/main c9cf57a9508c7f6b00e495a03490bc1c2109b0fd`.
- Wallet (pays and tops up the repo addresses): `0xC086980750bc26B4e8F64B85aFe0c7c8bFDc2a7E`.
- Control address (owner and creator of the `id`, `bind`, `anchor` and `cob` entities): `0xc051EAe280800fdF4bAd0A2665aCdB1B4963688F`.
- Bulk address (owner and creator of the `chunk`, `manifest` and `refs` entities): `0x773d7756Fc906D21c4Ac55d7979c3db73C24D6e3`.
- Explorer: https://rabdi.explorer.arkiv.network, for example https://rabdi.explorer.arkiv.network/address/0xc051EAe280800fdF4bAd0A2665aCdB1B4963688F.

Anyone can read a repo's anchor chain straight from Arkiv, without Charter.
The anchor kind fixes each peer's refs head; this query lists the anchors of one repo in sequence order (shape from `docs/p1/design.md` section 3.6):

```text
ag.kind = str('anchor') AND ag.rid = bytes32(0x4424958cfcfb43cac8e202206fb5640c30855c2b800151023839fcc3a5bde652) ORDER BY ag.seq:u64 ASC
```

```sh
curl -s -X POST https://rpc.rabdi.db-chain.testnet.arkiv.network -H 'content-type: application/json' --data-binary @- <<EOF
{"jsonrpc":"2.0","id":1,"method":"arkiv_query","params":["ag.kind = str('anchor') AND ag.rid = bytes32(0x4424958cfcfb43cac8e202206fb5640c30855c2b800151023839fcc3a5bde652) ORDER BY ag.seq:u64 ASC",{"select":{"key":true,"owner":true,"attributes":{"ag.seq":true}},"limit":"0xa"}]}
EOF
```

Expected result: two rows, the anchors with `ag.seq` `0x0` (the `v0` push) and `0x1` (the approved `v1` push), both owned by the control address `0xc051eae280800fdf4bad0a2665acdb1b4963688f`.
For another repo, the rid in hex is the revision hash that `chart id show arkiv::<rid>` prints for revision 0; with an all-zero rid the query is valid and returns no rows.
Add `"select":{"count":true}` to the options to get only a count.
Each row is a hint, not proof: a client checks the signature, the creator address and the chain before trusting it, which `git clone` does for you.

## Development

```sh
nix develop                                   # Rust 1.97 toolchain, git, cargo-audit
cargo test --workspace --all-features         # unit, property, fake-Arkiv and git E2E tests
cargo test --release -p charter-sync --all-features --test gate -- --ignored --nocapture   # 100 MB gate on the fake
CHARTER_RABDI=1 cargo test -p charter-cli --test rabdi_smoke -- --ignored              # live smoke, needs the funded throwaway key
CHARTER_RABDI=1 cargo test -p charter-cli --test rabdi_gate live_rabdi_gate -- --ignored --nocapture   # live 10 MiB gate, about 0.44 tGLM
```

Try it locally without a chain: `export CHARTER_BACKEND=mem:/tmp/arkiv.log`, then `chart keys init`, `chart init --name demo`, and `git push` to the printed `arkiv::` URL.

## How AI was used

This project was built with Claude Code (Anthropic) as a coding agent, orchestrating parallel worker agents under the author's direction.
The author made the architecture and security decisions; the agents did the research, implementation and measurement within them.
The decision log, from `docs/p0/README.md` ("Decided by the user"):

- 2026-11-06: ML-DSA signatures from day one (identities, signed refs, anchors, COBs, MLS credentials).
- 2026-11-06: hybrid composite ML-DSA-65 + Ed25519 for our own signatures (draft-ietf-lamps-pq-composite-sigs); a signature is valid only if both halves verify.
  MLS credentials stay pure ML-DSA because no hybrid MLS suite exists.

`docs/p0/README.md` also lists the recommendations an agent took and flagged as reversible, and the open decisions still waiting for the author.
Every change went through tests, clippy and CI: CI runs `cargo fmt --check`, `cargo clippy -D warnings`, the workspace tests, `cargo audit`, and a check that no tracked secret files or em dashes are in the tree.
Live numbers in this README were measured on Rabdi through the real binaries, and each is labelled as measured, derived or read from source in the docs it comes from.

## CROPS

Censorship resistance, open source, privacy and security, as this project stands today.

- **C (censorship resistance):** we run no server, the data lives on Arkiv, and clients verify everything they read.
  Arkiv runs a single sequencer that can refuse writes, which is censorship; that is outside the P1 trust model, which trusts Arkiv for ordering and availability only.
  A refused push fails visibly (it ends with an error such as `E_UPLOAD_INCOMPLETE` and nothing is anchored) rather than silently.
- **O (open source):** MIT OR Apache-2.0, and anyone can build and run it from the repo alone with `nix develop`.
- **P (privacy):** public repos are public by design.
  Private repos are in progress and store only ciphertext with keyed-hash ids.
  The RPC provider sees which entities a client queries, so use your own Arkiv node or a privacy RPC.
- **S (security):** composite ML-DSA-65 + Ed25519 signatures (both halves must verify), and the seed is stored locally with mode 600.
  Trust assumptions: Arkiv is trusted for ordering and availability only, and nothing else; the client checks every signature, hash and chain link, and keeps its own high-water marks so it refuses to go backwards.
  The EVM keys that own entities are secp256k1 and not post-quantum, which is why the signatures that matter are separate.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
