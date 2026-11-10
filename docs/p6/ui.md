# P6: A light, fast web UI for Charter (`chart ui`)

Status: PLAN, approved in direction by the user on 2026-11-10 ("Yes, write the plan"); implementation waits for a go-ahead per slice.
Date: 2026-11-10.
Builds on `docs/p1/design.md` (cache layout 3.9, verified view 4.3), `docs/p3/cob-ops.md` (ops), `docs/p3/releases.md`, `docs/p3/programs.md` and `docs/p5/speed-and-cost.md` section 3.13 (latency plan).

## 1. Goal and non-goals

Goal: a web UI that feels instant (every page under 50 ms locally), weighs about 20 KB per page, works with JavaScript off for reading, and makes Charter's verification visible.
Users open it with `chart ui`; it uses the keys already in `$CHARTER_HOME`, so there is no wallet, extension or WalletConnect step.

Non-goals for now: a hosted multi-user forge with accounts, CI, code search across all of Arkiv, a desktop app, a mobile app, a GitHub look-alike.

Design read: a developer tool for engineers who live in git, with a dense, keyboard-driven, calm language; sourcehut speed with Linear-level polish, dark and light themes, monospace where it carries meaning.

## 2. Why local-first

The slow part of Charter is the chain, not rendering: about 320 ms per round trip to Rabdi and several reads per view (`speed-and-cost.md` 3.13).
A UI that reads the chain on page load (a browser dapp) inherits that latency on every click.
Charter already keeps a verified local copy per repo under `$CHARTER_HOME/repos/<rid-hex>/` (`git/`, `state.json`).
So pages render from local disk, and a background sync loop keeps the copy fresh; the chain never sits on the request path.

## 3. Survey (reuse before writing)

| Candidate | What it is | Verdict |
|---|---|---|
| `radicle-dev/radicle-gui` explorer (Svelte, pushed 2026-11-10) | Closest prior art: repos, issues, patches, diffs over `radicle-httpd` | GPL-3.0, so its code cannot enter Charter (MIT OR Apache-2.0); it is built around Radicle's git-stored COBs, while Charter's ops are Arkiv entities (`cob-ops.md`). Borrow UX ideas only. |
| `radicle-httpd` (Rust, MIT OR Apache-2.0) | JSON API over Radicle storage | License fits, data model does not: it expects Radicle identity refs and git COBs. |
| Forgejo, Gitea | Full forges | Server plus database plus accounts; far heavier than the problem. |
| sourcehut, cgit | Server-rendered, almost no JavaScript | The model for speed and weight; different stack, so copy the approach, not code. |
| Browser dapp reading Arkiv directly | No server | Slow first load, heavy bundle (ML-DSA verify and pack parsing in the browser), and it needs a wallet connection, the path that crashed the Arkiv hub (`docs/p5/upstream-hub-wallet-crash.md`). Rejected for v1. |
| Desktop app (Tauri, as Radicle desktop) | Native shell | Per-platform builds and signing; no need yet. |

Libraries picked (all checked on crates.io / npm 2026-11-10):

| Need | Pick | Version, license | Why over the alternative |
|---|---|---|---|
| HTTP server | `axum` | 0.8.9, MIT | Built on `hyper` 1 and `tower-http` 0.6, both already in `Cargo.lock` via `reqwest`. |
| Templates | `askama` | 0.16.1 (2026-09), MIT OR Apache-2.0 | Compile-time checked, plain HTML files; `maud` last released 2025-02. |
| Partial updates, live refresh | `htmx` | 2.0.11, 0BSD, about 14 KB gzipped | One vendored file; progressive enhancement, pages still work without it. |
| Syntax highlighting | `syntect` with `fancy-regex` | 5.3.0, MIT | Pure Rust, mature; `tree-sitter-highlight` needs a C grammar per language. |
| Markdown | `pulldown-cmark` + `ammonia` | 0.13.6, MIT; 4.2.4, MIT OR Apache-2.0 | Standard pair: render, then sanitize. |
| Git reads | the existing `charter_sync::git::Git` wrapper (git CLI) | in tree | Already how Charter reads git; switch to `gix` (0.90, MIT OR Apache-2.0) only if a page misses the budget. |
| Static assets | `include_bytes!` | std | A handful of files; no embed crate needed. |

## 4. Architecture

New crate `crates/charter-ui` (library) plus one subcommand `chart ui` in `charter-cli`.
No existing crate changes behaviour; `charter-sync` gains two small additions listed below.

```
browser ── HTTP ──> charter-ui (axum, askama)
                       │ reads (no lock)            │ writes (slice 2)
                       ▼                            ▼
          $CHARTER_HOME/repos/<rid>/          charter_sync::cob::post, release, program
            git/  state.json  cobs.json              │
                       ▲                            ▼
                 sync loop (one task) ── charter_sync::sync, cob::list ── Arkiv RPC
```

### 4.1 Data plane

- Repos: the directories under `$CHARTER_HOME/repos/`; each page reads `git/` with git plumbing and `state.json` for peers, anchors and equivocations.
- Ops: `cob::list` reads the chain today, so the UI adds a per-repo local op cache `cobs.json` (verified ops only, ordered by block, with the last block seen) written by the sync loop.
- Program state, bounties and escrow are folded from the cached ops with the existing `charter-program` fold, not re-read from the chain.
- Releases are `xyz.arkiv.release` ops, so they come from the same cache.

### 4.2 Sync loop and the repo lock

`Repo::open` blocks on a file lock held for the life of the `Repo` (`repo.rs`), and `git push` through the remote helper takes the same lock.
The UI must never make a push wait on it, so:

- Page handlers never take the lock; they only read `git/` (safe for concurrent readers) and the JSON files, which are replaced atomically.
- The sync loop opens the repo with a new non-blocking `Repo::try_open`, runs one round (`sync` plus a `cob::list` from the last block seen), writes the files, and drops the lock; if the lock is busy it skips the round.
- Interval: a round runs only when a one-query probe (newest `ag.rid` entity and the count) changed, since Rabdi bills an `arkiv_query` 10 units of a per-IP 10,000 (`docs/p0/arkiv-limits.md`); the repo on screen is probed every 10 s (with a `newHeads` socket, at the first block after that), other repos every 5 min while any page is open, and nothing with no page open (`crates/charter-ui/src/pace.rs`).
- Quota: the RPC client keeps the latest `RateLimit` and `Arkiv-Quota-Used-Percent` headers; the loop slows from 50 % used, pauses at 80 % or on a 429 until the reset, never sleeps out a `Retry-After` inside a round, and the pages show "paused: RPC quota".

Additions to `charter-sync` (both small, both tested): `Repo::try_open`, and `cob::list_since(rid, from_block)` so the loop reads only new ops.

### 4.3 Pages (slice 1, read-only)

| Route | Shows |
|---|---|
| `/` | Repos in this `$CHARTER_HOME`: name, last push, sync freshness, verification state |
| `/r/<rid>` | README, branches and tags per peer, clone command, verification panel |
| `/r/<rid>/tree/<ref>/<path>` | Directory listing |
| `/r/<rid>/blob/<ref>/<path>` | Highlighted file, line anchors, raw link |
| `/r/<rid>/log/<ref>` | Commit list, paged |
| `/r/<rid>/commit/<sha>` | Message, signatures, diff with per-file collapse |
| `/r/<rid>/issues`, `/r/<rid>/issues/<id>` | Issue list and thread from cached ops |
| `/r/<rid>/patches`, `/r/<rid>/patches/<id>` | Patch list, revisions, reviews, attached proofs |
| `/r/<rid>/releases` | Releases with artifact hashes and their verification |
| `/r/<rid>/program` | Pinned program, bounties, accepted and rejected ops with reasons |

The verification panel is Charter's edge and sits on every repo page: identity chain length and delegates, each peer's accepted head and anchor block, ML-DSA signature status, equivocations flagged in red, and the time of the last sync.
Each fact links to the block or entity it came from.

### 4.4 Writes (slice 2)

- Open an issue, comment, review or merge a patch, publish a release, fund or claim a bounty, through the same `charter-sync` functions the CLI calls, signed with `Keys` from `$CHARTER_HOME`.
- Every write shows its cost first (the existing `push_send::Cost` estimate, in tGLM) and needs one confirm click; nothing is signed on page load.
- After a write, the page shows "sent" at once and "confirmed in block N" when the receipt lands.

### 4.5 Live updates (slice 3)

- One server-sent-events stream per open page; the sync loop pushes "new push", "new comment" and "sync state" events and htmx swaps the affected fragment.
- The loop moves from polling to `eth_subscribe newHeads` over WebSocket (measured: blocks arrive about 0.3 s after their timestamp), shared with the `chart node` work in `speed-and-cost.md` 3.13.

### 4.6 Public read node (slice 4)

- The same binary with `chart ui --public --bind <addr>`: read-only, no keys loaded, sealed (private) repos never served, a configured repo list or any repo found by `search`.
- It doubles as the fast read node in 3.13; pages state "verified against Arkiv at block N" so readers can re-check with the CLI.
- Slice 5 (later): a WASM build of the verifier so the public pages can be checked in the browser without trusting the host.

## 5. Security

- Bind `127.0.0.1` on a random port; print one URL carrying a random session token (as Jupyter does); the token becomes an `HttpOnly`, `SameSite=Strict` cookie.
- Reject any request whose `Host` header is not the bound address (DNS rebinding) and any write without a per-session CSRF token.
- Strict `Content-Security-Policy`: `default-src 'self'`, no inline script, no third-party requests; fonts and htmx are served from the binary.
- All repo content (markdown, file names, commit messages, op payloads) is untrusted: markdown goes through `ammonia`, everything else through askama's escaping; raw files are served as `text/plain` or with `Content-Disposition: attachment`.
- Sealed repos are decrypted only in the local mode and only for members; `--public` refuses to start if it would serve one.
- Keys never reach the browser; signing happens in the server process.

## 6. Performance budget and how it is checked

| Metric | Budget | Check |
|---|---|---|
| Server time per page, local cache | under 20 ms p50, under 50 ms p99 | `oha` against a fixture repo (via `nix-shell`), in CI on the fake store |
| HTML + CSS per page | under 20 KB gzipped, critical CSS inline | a test that sums response sizes |
| JavaScript | htmx only, deferred; reading works with JS off | Playwright run with JS disabled |
| First page after `chart ui` | under 1 s on a synced repo | timed in the E2E suite |
| Large files and diffs | highlight only the first 2,000 lines, then plain text with a "show all" link | unit test on a generated file |

## 7. Testing and demo

- TDD per handler: a failing test first, against the existing `charter-store` `fake` feature, so no chain or funds are needed.
- Template snapshot tests for each page with fixture data, including empty, error and very long states.
- Playwright E2E (Python Playwright from nix, as used for the RainbowKit demo) for the main flows, with screenshots in light and dark at desktop and phone widths, checked pixel by pixel per the project rule.
- Accessibility: keyboard reachability of every action, visible focus, contrast at WCAG AA, `axe-core` run in the E2E suite.
- Each slice's PR carries a short browser recording of the real flow against Rabdi, pushed to the orphan `demo-assets` branch.

## 8. Slices and acceptance

| Slice | Delivers | Done when |
|---|---|---|
| 1 | Read-only local UI: repo list, repo home, tree, blob, log, commit diff, issues, patches, releases, program, verification panel; sync loop with `try_open` and the op cache | Charter's own repo browses fully from `chart ui`; budgets in section 6 pass; a concurrent `git push` is never blocked longer than one sync round |
| 2 | Writes with cost confirmation | An issue, comment, patch review and release made in the UI verify with `chart op list` and `chart release verify` |
| 3 | Live updates over SSE, `newHeads` subscription | A push from another terminal appears on an open page within one block |
| 4 | `--public` read node | Serves Charter's repo read-only with keys absent; sealed repos refused |
| 5 | WASM verifier for public pages | Later; scoped separately |

Rough size of slice 1: one new crate of about 1,500 to 2,500 lines including templates and tests, plus the two `charter-sync` additions.

## 9. Open decisions for the user

- Name and command: `chart ui` (proposed) or `chart web`.
- Visual identity: whether to reuse the pitch deck's colours and type or define fresh tokens; a one-page design proposal comes before slice 1's templates.
- Whether the public read node (slice 4) is something we host, and where.
- Whether slice 2's writes should also be possible from the public mode through a browser wallet later; this plan assumes no.

## 10. Not yet checked

- How long a cold `sync` plus a full `cob::list` takes for a repo with many ops; it sets how fresh the first page is.
- `syntect`'s startup cost for the default syntax set on this Mac (loaded lazily on first highlight).
- Whether git CLI calls per page stay within the budget for a 10,000-entry tree; the fallback is `gix`.
