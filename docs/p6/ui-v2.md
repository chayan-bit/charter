# P6 v2: audit and redesign proposal for `chart ui`

Status: slices 0 to 3 and the Settings page are BUILT on branch `ui/v2` (section 10); slices 4 to 8 are still a proposal.
Sections 1 to 9 are the proposal as written before the build; section 10 records the decisions and what was built.
Date: 2026-11-11.
Branch `ui/v2`, from main at bda7571 (v0.4.0).
Builds on `docs/p6/ui.md`, which stays the record of the v1 architecture (axum, askama, htmx, local-first, CSRF and Host guard).

## 0. Scope

Reader: a developer who already has keys and a funded wallet.
The user called the current UI "abysmally bad".
This document says what is actually wrong with it, what is missing, and what to build, in slices.

Onboarding is out of scope until after the mainnet release; see section 8.
Nothing in this proposal needs a new user flow, a faucet or a recovery phrase.

Evidence used:

- the release build of this branch (`target/release/chart`, built with `mac-heavy cargo build --release`);
- a fixture repo `charter-demo` on the file-backed fake chain (`crates/charter-ui/e2e/fixture.sh`): 4 commits, branches and tags, 2 issues with comments and a close, a patch with a review and an approval, a release with 2 artifacts, a pinned bounty program; writes were made here, so no tGLM was spent;
- a read-only scratch clone of the hosted repo (`arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ`) in a home with no keys, read from Rabdi;
- 72 page screenshots plus 72 state screenshots, each in light and dark at 1280x800 and at 390x844 at 2x; page widths were measured on all of them, and `axe-core` was run on 6 pages.

Screenshot root (all file names below are relative to it): `/private/tmp/claude-501/-Users-chayanaggarwal-Code-Arkiv-Github/ee1320ae-45b8-4a79-93f4-e2e71a76e497/scratchpad/ui-audit/shots/`.
`fx-*` is the fixture repo, `st-hosted-*` is the hosted repo, `st-*` is a state (write flow, empty, error).
Names end in `-desktop-light`, `-desktop-dark`, `-phone-light` or `-phone-dark`.

tGLM spent: 0.
The fixture ran on the fake chain and the hosted repo was only read.

## 1. Audit

### 1.1 What is already good

This matters, because the redesign keeps it.

- No horizontal scroll at 390 px on any of the 18 pages in 4 themes and viewports.
- `axe-core` reports 0 violations on home, repo, patch, commits, releases and the confirm dialog.
- Dark and light both work, contrast is sound, and the focus ring is visible.
- Every page is server-rendered, has a skip link, and the keyboard map (`g` then a letter, `j`/`k`, `?`) is a real asset for people who live in git.
- Diffs, highlighting, markdown sanitising and the verification facts are correct and fast.
- The write path already has the right safety shape: one confirm click, cost first, CSRF, keys stay in the process.

So the UI is not broken.
It is a competent read-only skeleton that speaks the protocol's vocabulary to a person who wants to do git work.
The pain is in vocabulary, missing product features, and a write flow that does not tell the user what they are publishing.

### 1.2 Confusion points, ranked by impact

1. **The verification line is protocol text.**
   "Verified identity rev 1, 1 of 1 peers anchored, signatures valid" appears on every page and means nothing to a user; on a phone it is cut to "identity rev 1, 1 of 1 peers anchored, ...".
   The panel behind it says "Delegate 3664a2b ... head seq 1, body 6ea83e9 at block 31, entity 44064d7".
   `fx-repo-desktop-light.png`, `fx-repo-phone-dark.png`, `st-verify-open-desktop-light.png`.
2. **People are 7-hex strings.**
   "by 3664a2b", "Delegate 3664a2b", "opened by 3664a2b" on every issue, patch, review and commit-adjacent view.
   The CLI already records an alias per peer (`PeerBody.alias`), but the UI cache does not carry it.
   `fx-patch-desktop-light.png`, `fx-issue-desktop-light.png`.
3. **Time is block numbers.**
   "Last push: block 486342", "opened at block 20, op ec65041", "Anchored at block 31".
   The only wall-clock times are the author's own clocks ("12 min ago"), which the page itself warns may be wrong.
   `hosted-loading-home-desktop-light.png`, `fx-commit-first-desktop-dark.png`.
4. **The Code page spends half its height on two near-identical ref lists.**
   "Canonical" and "Delegate 3664a2b" list the same refs, the second as raw `refs/heads/...` paths.
   There is no branch or tag switcher; the only way to view another ref is to know the URL.
   `fx-repo-desktop-light.png`, `fx-repo-phone-dark.png`.
5. **The confirm dialog shows plumbing, not the action.**
   Opening one issue shows "Transactions 2", "Wallet top-up 0.000375462 tGLM to the control address, fee at most 0.000042 tGLM", "Entities 1".
   Approving a patch shows "Transactions 4" and "2 transfers ... to the control address".
   Amounts have 9 decimals.
   Nothing says the write is public, permanent, and has a lifetime; the content being published is hidden under "What will be signed".
   On a phone the dialog opens inside the page under the tab bar, so it does not read as a modal.
   `st-confirm-issue-desktop-light.png`, `st-confirm-review-phone-dark.png`.
6. **After Send the user gets a 66-character tx hash.**
   "Sent, waiting for the receipt", a raw transaction id, and a toast at the bottom right, far from where the eyes are.
   No estimate of how long it takes, no step that says when it is visible to others.
   `st-sent-issue-desktop-light.png`.
7. **The patch page cannot answer "is this change good".**
   It has no title body, no commit list, no diff, no changed files, no checks, and revisions appear only as hashes.
   On the hosted repo all 8 patches show "open" and "0 comments" although each was accepted into main under CI-gated rules; the approvals and CI attestations that gate main are invisible there.
   `fx-patch-desktop-light.png`, `st-hosted-patches-desktop-light.png`.
8. **The Program page is raw.**
   "Pinned program rhai/1 f3d22b6", "Ref moves 1 accepted", and a Bounties table whose amounts are bare numbers ("5", "0", no unit).
   It does not say what the rules require, or why a push was accepted.
   `fx-program-desktop-light.png`, `st-hosted-program-desktop-light.png`.
9. **Releases speak program jargon and raw bytes.**
   "not tracked by the program", "valid under the pinned program", sizes like "21983268 B", a 12-character hash with no copy button, and a footer telling the user to go to the CLI to verify.
   `fx-releases-phone-light.png`, `st-hosted-releases-phone-dark.png`.
10. **Phone chrome is cramped and clips content.**
    The tab bar cuts "Program" to "Pr" with no scroll hint; the clone box shows only `git clone arkiv::z32QC1rhuaDVw7`, so the address cannot be read; the header buttons "theme" and "?" are text buttons at full size; the verification card is truncated.
    `fx-repo-phone-dark.png`, `fx-blob-phone-light.png`, `fx-log-phone-light.png`.
11. **Dead ends and "restart the program" instructions.**
    The empty home says to use the CLI; an unknown or unfetched repo is a bare "404 / No such repo in this $CHARTER_HOME"; with no keys, every write form says "Run `chart keys init`, fund the wallet, then restart `chart ui`", because keys are loaded once at startup.
    `st-empty-home-phone-dark.png`, `st-bad-rid-desktop-light.png`, `st-writes-off-desktop-light.png`.
12. **A 3,200-line file is a 40,328 px page.**
    Highlighting is capped at 2,000 lines but the whole file is rendered into the DOM.
    `fx-blob-long-desktop-light.png`.

Smaller items:

- The theme button says "theme" with no hint of the current mode.
- Issue and patch rows give no author alias, no comment counts on the issue list, and no filter or search.
- The "Fund the bounty" form has an amount of 0 that shows no visible feedback on submit (page unchanged; not investigated further).
- Times such as "Times are the authors' own clocks" appear as a disclaimer on every thread; correct, but it is a symptom of point 3.

### 1.3 Empty, loading and error states

| State | What a user sees today | Verdict |
|---|---|---|
| No repos | One sentence telling them to use `git clone` or `chart ui <rid>` | Dead end, no action on the page |
| Repo not cached | 404 page, no hint that it can be fetched | Dead end |
| First sync running | By source (`verify.rs` `unsynced`): "Pending: not verified by chart ui yet; the first sync round is running", "synced never" | Not seen on screen: the first sync of the hosted repo finished in seconds, so this state was read from source, not captured |
| Sync paused by RPC quota | "paused: RPC quota, resumes in N min" in the verification line (by source) | Not exercised; plausible but easy to miss |
| Arkiv unreachable | Not exercised (no way to cut the network from the backend options); by source there is a per-round error in the panel | Unverified |
| No keys | "Writes are off ..." message in place of every form | Clear, but needs a restart to fix |
| Write failed | Not exercised on the fake chain | Unverified |
| Equivocation | Red "Equivocation" word and a red row in the panel | Correct but jargon; no page-level warning |

## 2. Missing features, ranked by user value

Ranked against what a person coming from GitHub or Radicle expects and against what `chart` already does.
"UI today" is what the 11 current routes support.

| # | Gap | UI today | CLI or protocol | Value |
|---|---|---|---|---|
| 1 | Branch and tag switcher, all-branches page | None; URL only | Refs per peer in `view.json` | Daily use |
| 2 | Patch review: files changed, commits, revision compare, inline comments | Hashes only | Revisions and heads are in ops | Daily use |
| 3 | Checks on patches and main: approvals needed, CI attestation, signed commits | None | `programs/ci-gated.rhai`, attestation ops, folded state | Charter's differentiator |
| 4 | Issue and patch writes beyond open and comment: close, reopen, create a patch, revise, merge | Open issue, comment, review, mark merged, claim, fund | `op post` supports the full set (the fixture uses `issue close`) | Daily use |
| 5 | Plain-language authors: aliases instead of 7-hex ids | Hex only | `PeerBody.alias` | Daily use |
| 6 | Wallet: balance, spend history, low-balance warning | Balance only inside the confirm dialog | `chart fund` | Cost awareness |
| 7 | Expiry and keep-alive: "kept until", warnings, renew with a cost preview | None | `chart status`, `chart keepalive --dry-run` | Prevents data loss |
| 8 | Wall-clock time for blocks | None | Block times are on the chain | Daily use |
| 9 | Search and filter: repos, issues, patches, commits, files | None | `chart search` (network); local cache is searchable | Daily use |
| 10 | Maintainers and identity: show maintainers by name, propose and accept revisions, rename, threshold | Hex list in the verification panel | `chart id show/propose/accept` | Team use |
| 11 | Private repos: lock badge, members, roles | Not shown at all (`private` is cached but unused in pages) | `chart member ...` | Team use |
| 12 | Commit tools: file history, blame, compare, archive download | Log and single commit | Plain git on the cache | Daily use |
| 13 | Activity feed: new pushes, patches awaiting your review, bounties, expiring repos | None | Derived from the sync loop and ops | Daily use |
| 14 | Rules page that explains the program and shows why each push was accepted or refused | Raw table | `chart program status` | Trust |
| 15 | Release verification in the page and humane sizes and hashes | "Verify with the CLI" text | `chart release verify` | Trust |

Lower: escrow status and audit view (`chart escrow audit`), agent grants and devices (head model section 4), attached proofs (`op attach-proof`), timelock, ZK receipts, create and clone a repo (deferred with onboarding), markdown preview in forms, issue labels.

## 3. Redesign

Design read: a developer tool for engineers who know git and may be new to Arkiv; a calm, trustworthy, quietly dense product UI, not a landing page.
Dials: variance 3, motion 2, density 5.
The design-taste skill is written for landing pages, so only its product-safe rules are applied: one accent, one neutral ramp, no AI-purple, no centered hero, no serif, no decorative motion, a single shape rule, full state coverage, label above input, button contrast checked, and no hand-rolled icons in the shipped build.

### 3.1 Principles

1. Say what the action does, in git words.
   "Approve this patch", "Comment", "Renew", never "anchor", "entity" or "control address" outside a "Technical details" disclosure.
2. Verification is a state, not a paragraph.
   One chip says Verified; it turns amber or red only when something is wrong, and a page-level banner appears for conflicts.
3. Money is always visible and never a surprise.
   The wallet balance sits in the header; every write shows the worst-case cost, what is published, and that it is public and lasting.
4. Time is wall-clock time; blocks are details.
5. Keep what works: server-rendered pages, htmx, the keyboard map, the guard, the 20 KB per page budget.

### 3.2 Information architecture

Global header (always): logo, Repositories, Activity, a "Jump to" command box (`/`), a quiet sync indicator, the wallet chip, the theme toggle.
On phones the three destinations move to a bottom bar (Repositories, Activity, Wallet) and the header keeps only the logo, sync dot, wallet chip and theme.

```text
/                          Repositories (with attention strip: expiring, awaiting your review)
/activity                  New pushes, reviews waiting for you, bounties, expiring repos
/wallet                    Balance, address, spend history, renew-all
/r/<rid>                   Code (branch picker, files, last commit, README, side panel)
/r/<rid>/tree|blob/...     File browser: tree sidebar, file view, history link
/r/<rid>/branches          All branches and tags with last push and author
/r/<rid>/commits/<ref>     Commits grouped by day, filter, signed mark
/r/<rid>/commit/<sha>      Commit and diff
/r/<rid>/issues, /<id>     List with Open or Closed filter, thread with sidebar
/r/<rid>/patches, /<id>    List with checks column; patch with Overview, Files changed, Commits
/r/<rid>/releases          Releases with allowed-by-rules mark, humane sizes, copy hash
/r/<rid>/rules             Rules and CI: what is enforced, recent decisions, bounties
/r/<rid>/settings          Maintainers, identity history, storage and keep-alive, access (private)
```

Renames: Program becomes "Rules and CI"; Verification stops being a tab and becomes the chip plus a popover; "Delegate" becomes "Maintainer" in the UI.
Old URLs keep working (redirects), so links in docs and the CLI output do not break.

### 3.3 Layout and the pages that change most

- **Repositories.**
  Rows, not a table: name and visibility, description, "Updated 12 min ago by ravi", "Kept until 14 Jun 2027", and one status chip (Verified, Syncing first copy, Expires in 12 days).
  An attention strip shows only exceptions; when there are none it does not exist.
- **Repo page.**
  Header row: name, Public or Private, the Verified chip (click for a plain-language popover, details behind a disclosure), Clone button with a popover that holds the full `git clone` line.
  Below: tabs, then a two-column layout (main, then a 296 px side panel with Maintainers, Storage, Activity); one column on phones.
  The branch picker replaces the "Branches and tags" section.
- **File browser.**
  Tree sidebar (hidden behind a button below 960 px), breadcrumb, last commit bar, Copy path, History, Raw.
  Files over 2,000 lines render only the first screen and load the rest on demand.
- **Patch.**
  Title, state, "ravi wants to merge 2 commits into main from fix/empty-ledger", a revision picker, then Overview (checks panel, timeline), Files changed, Commits.
  The right panel holds Your review (Approve, Comment), Merge, Reviewers.
  Checks are derived from the pinned program and the attestation ops: maintainer approval (n of m), CI passed (who attested, which commit), commits signed.
- **Issue.**
  Thread plus side panel: Bounty (pledged amount with unit, fund), Linked patches, Participants; Close and Comment in the composer with a cost hint.
- **Releases and Rules and CI.**
  A release card shows "Allowed by repo rules" and a table of downloads with platform, size in MB and a copy button for the full hash.
  Rules and CI states the rule in a sentence, lists recent decisions as Change, Who, Checks, Result, and shows bounties with units.

### 3.4 The cost-confirm dialog (writes)

The dialog already exists; this redesign changes what it says, not when it appears.
Every on-chain write still shows it, with no "do not ask again" option in this proposal (see open question 2).

```text
Approve this patch?
This publishes your approval on Arkiv. Anyone can read it. It cannot be edited or deleted.

 What you publish
 Approval of "Fix empty ledger crash", revision 2, commit 1a87e4f

 Most you can pay                              0.0044 tGLM
 Test tokens, no real value

 Funds now      12.4780 tGLM        Takes      about 10 s
 After          about 12.4736       Kept for   about 6 months, then it needs a renewal

 > Breakdown and what will be signed   (technical: records, transactions, top-ups, fee, payload)

 [Cancel]                          [Approve for up to 0.0044 tGLM]
 Esc cancels, Cmd+Enter sends. Your keys never leave this computer.
```

Rules:

- The headline is the action in a question; the sentence under it says public, permanent and what is published.
- One number: the most the user can pay, rounded to 4 decimals; the top-up transfers and the fee are in the breakdown.
- The primary button repeats the amount, so the consent is on the button.
- Cancel is the default focus and Enter does not send (kept from today); `Esc` cancels; `Cmd+Enter` sends.
- Variants: low balance (red notice, button disabled, "Nothing was spent", copy the wallet address); RPC quota (amber notice with a countdown, button enabled when it ends); renew shows the new end date.
- After Send the same dialog becomes a three-step progress: signed on this computer, in a block (with the block number and seconds), visible after the next sync; it can be closed and keeps going.
- On phones it is a bottom sheet with full-width buttons.

Measured on the fake chain (fees are not Rabdi fees): open issue at most 0.002164 tGLM in 2 transactions; approve patch at most 0.004358 tGLM in 4 transactions.
The mockup shows rounded values.

### 3.5 Tokens

One accent, one neutral ramp, one shape rule.
Shape: controls 6 px, containers 8 px, chips pill; nothing else.

| Token | Light | Dark |
|---|---|---|
| `--bg` | `#f6f7f9` | `#0e1014` |
| `--s1` (surface) | `#ffffff` | `#15181e` |
| `--s2`, `--s3` | `#eff1f4`, `#e3e6eb` | `#1c2027`, `#272c35` |
| `--line`, `--line-strong` | `#dde0e6`, `#8b929e` | `#292e38`, `#6d7584` |
| `--text`, `--text-2`, `--text-3` | `#14171c`, `#434955`, `#5a606c` | `#e9ebef`, `#b6bbc6`, `#9199a6` |
| `--accent` (links, primary) | `#2152c4` | `#7ba3ff` |
| `--ok`, `--warn`, `--bad` | `#14683a`, `#80500a`, `#b0241c` | `#5ac882`, `#e3a53f`, `#f58179` |

- Type: Geist for text and Geist Mono for hashes, paths and amounts; 14/21 base, 13 dense, 12.5 mono, 22/28 page title.
- Status colours only mean status: green verified or passing, amber waiting or expiring, red conflict or refused; blue is the one accent.
- Contrast was checked against the backgrounds for body text; all of `--text-3` is at least 4.5:1.
- Motion: one live dot that pulses slowly while synced, a row flash when a live update lands, nothing else; all of it off under `prefers-reduced-motion`.

### 3.6 Live sync and verification without noise

- Header: a small dot and "Synced 12 s ago".
  Amber "Paused: rate limited" and red "Cannot reach Arkiv, showing data from 4 min ago" appear only when true.
- The Verified chip is the only per-repo verification element on normal pages.
  Click opens a popover in plain sentences, with a Technical details disclosure for revisions, peers, block numbers and signature suites.
- Conflicting history: the chip turns red ("Conflicting history") and a page-level banner appears on every page of that repo.
- Updates arrive over the existing event stream (`/r/<rid>/events`); new rows flash once, and the wallet chip updates after a write.
- The mockup has switches for these states (sync paused, offline, conflict).

### 3.7 Libraries

The stack stays axum, askama and htmx, with no SPA.
The proposal adds only two vendored assets and no Rust dependency.

| Need | Pick | License | Why |
|---|---|---|---|
| Typeface | Geist and Geist Mono, self-hosted woff2 (latin subset, about 70 KB, embedded like `htmx.min.js`) | OFL-1.1 | Readable at 13 px, has a real mono; the existing CSP forbids third-party fonts, so it must be embedded |
| Icons | Phosphor icons, about 20 glyphs as an inline SVG sprite included by askama | MIT | Maintained set; the skill forbids hand-drawn icons; the mockup uses placeholder glyphs only |
| Popovers, dialogs, details | Native `<dialog>`, `<details>` and the `popover` attribute | platform | No dependency, works with JS off for reading |
| Command box | About 60 lines of vanilla JS in `keys.js` | none | It only filters a list the page already has |

Library versions must be re-checked on crates.io, npm and the vendor sites at slice time, since none is pulled by Cargo.

## 4. Backend and endpoint changes the redesign needs

These are sketches; nothing is implemented.
`RepoView` is the per-repo `view.json` written by the sync loop.

1. **People and time.**
   Add `alias: Option<String>` per peer and `time_ms` per anchored block to `RepoView` (block time from the receipt or header already read by the sync loop).
   Pages show alias or short id, and "12 min ago" derived from block time.
2. **Expiry.**
   Add `expiry: { min_block, min_ms, checked_ms }` to `RepoView`, computed with the same function `chart status` uses.
   `GET /r/{rid}/settings` shows it; the home row shows "Kept until".
3. **Cost preview.**
   `POST /r/{rid}/write/preview` returns JSON:
   `{ "action": "approve", "publishes": "...", "visibility": "public|sealed", "max_wei": "...", "tx_count": 4, "records": 2, "eta_s": 10, "keep_days": 182, "balance_wei": "...", "after_wei": "...", "problems": ["low_balance" | "quota_paused"], "breakdown": {...}, "payload": "..." }`.
   The existing prepare handler renders the dialog from it, so the numbers have one source.
4. **Wallet.**
   `GET /wallet` and `GET /wallet/chip` (htmx fragment for the header).
   Append every confirmed write to `$CHARTER_HOME/ui/spend.json` (`{ts_ms, block, repo, action, wei, tx}`); the page lists it.
   Add a `wallet` event to the event stream.
5. **Write kinds.**
   Add `issue-close`, `issue-reopen`, `patch-create`, `patch-revise` to `writes/form.rs`, mapped onto the ops the CLI already posts; add `keepalive` that calls the keep-alive function with the dry-run estimate as its cost.
6. **Checks.**
   `GET /r/{rid}/patches/{id}/checks` (fragment): from the folded program state and attestation ops, return approvals needed and present, CI attestation (attester, commit, time), and signed-commit status.
7. **Branches and search.**
   `GET /r/{rid}/branches` from the existing git reader; `GET /search?q=` over repo names, issue and patch titles and commit subjects in the local cache only (substring match; no new index crate).
8. **Keys loaded late.**
   Load `Keys` on demand per write instead of only at startup, so the "restart `chart ui`" instruction goes away.
9. **Activity.**
   `GET /activity` folds new pushes, reviews waiting for the local identity and expiring repos from the cache.

Security model is unchanged: bind 127.0.0.1, Host check, session token cookie, CSRF on every write, strict CSP (fonts and icons are embedded, no third-party requests), keys never reach the browser.

## 5. Slices

Effort is one engineer, in working days, and includes tests and Playwright screenshots in light and dark at desktop and phone width.
Each slice ends with a short real-flow recording on the fake chain, per the project rule on PR demo videos.
Total about 34 days; slices 0 to 3 (about 14 days) give the biggest visible change and should go first.

| Slice | Delivers | Days | Acceptance | Files touched |
|---|---|---|---|---|
| 0 | Tokens, shell, fonts, icon sprite, phone bottom bar, theme toggle with state, sync indicator, wallet chip placeholder | 3 | No horizontal scroll at 390 px on all pages; axe 0 violations; header fits one line at 1024 px; page weight still under 20 KB gzipped plus cached assets | `assets/app.css`, `assets/fonts/*`, `assets/icons.svg`, `templates/base.html`, `src/assets.rs` |
| 1 | Repo list rows and attention strip; repo header, Clone popover, branch picker, branches page, side panel; Code page without the ref lists | 4 | Switch branch and tag from the picker; every ref of the fixture reachable without typing a URL; phone tabs show a scroll hint | `templates/index.html`, `repo.html`, `home.html`, `tree.html`, `src/pages/repos.rs`, `code.rs`, `mod.rs` |
| 2 | Vocabulary and time: aliases, wall-clock times, Verified chip and popover, "Maintainer", Rules and CI naming, conflict banner | 3 | No page shows "delegate", "rev", "anchored", "entity" or a bare block number outside a Technical details disclosure; times read "12 min ago" or a date; an equivocation fixture shows the red banner on every page | `src/cache.rs`, `sync_loop.rs`, `pages/verify.rs`, `templates/verify.html`, `thread.html`, `commit.html`, `program.html` |
| 3 | Cost dialog v2, write progress, wallet page and chip, spend ledger, low-balance and quota variants | 4 | Dialog shows what is published, max cost, balance after and lifetime; no tx hash on the success path; low balance disables Send; the e2e write flow still passes and `chart op list` shows the op | `templates/confirm.html`, `status.html`, `toast.html`, `src/writes/*`, `pages/write.rs`, new `pages/wallet.rs`, `assets/live.js` |
| 4 | Issues and patches v2: filters, close and reopen, patch Overview, Files changed, Commits, revision picker, checks panel | 6 | A patch shows its diff and checks without leaving the page; the hosted repo's patches show approvals and CI attestation; closing an issue from the UI verifies with `chart op list` | `templates/threads.html`, `thread.html`, new `patch.html`, `src/pages/cobs.rs`, `fold.rs`, `writes/form.rs` |
| 5 | Releases and Rules and CI pages, humane sizes, copy hash, plain-language rule text, decisions table | 3 | Sizes in MB or KB; the full hash is copyable; each accepted or refused push links to its patch and checks | `templates/releases.html`, `program.html`, `src/pages/program.rs` |
| 6 | Activity page, local search, "Jump to" box, large-file paging | 4 | Search finds a repo, issue, patch and commit subject from the fixture; a 3,200-line file renders in under 50 ms and under 200 KB; Activity lists a pending review | new `pages/activity.rs`, `pages/search.rs`, `assets/keys.js`, `pages/code.rs`, `highlight.rs` |
| 7 | Settings: maintainers, identity history, propose and accept, storage and renew with cost preview | 5 | Renew shows the dry-run cost and extends the expiry; adding a maintainer creates an identity proposal visible to `chart id show` | new `pages/settings.rs`, `writes/*`, `cache.rs` |
| 8 | Phone and accessibility pass, empty and error states for every page, e2e screenshots, performance budget re-check | 2 | Every page has a designed empty, loading and error state; the `ui.md` section 6 budgets pass; axe 0 violations | `templates/*`, `assets/app.css`, `e2e/e2e.py` |

Private-repo views (lock badge, members, roles) are small and ride along in slices 1 and 7; they need `RepoView.private`, which is already cached.

## 6. Mockup

`/private/tmp/claude-501/-Users-chayanaggarwal-Code-Arkiv-Github/ee1320ae-45b8-4a79-93f4-e2e71a76e497/scratchpad/ui-audit/mockup.html`.
One self-contained file with inline CSS and JS; the only external request is Google Fonts (Geist), which the shipped build embeds instead.
It is clickable: repository list, repo code page with branch picker and Clone popover, file browser, commits, issue, patch with checks and a Files changed tab, releases, Rules and CI, and the cost dialog with four variants and a sending then done sequence.
It follows `prefers-color-scheme` and has a theme toggle; the bottom "Mockup" bar switches screens, opens dialog variants and simulates sync states (paused, offline, conflict).
Checked in Chrome at 1280 px and 390 px in light and dark: no horizontal scroll on any screen or dialog variant, no console errors.
Mockup screenshots are in `scratchpad/ui-audit/mock-shots/` (for example `patch-desktop-dark.png`, `dialog-approve-desktop-light.png`, `code-phone-light.png`).
All data in the mockup is made up, including the repo address; it uses no real key, phrase or hosted-repo data.

## 7. Open questions

Answered on 2026-11-11; see section 10.1 for who decided what.

1. Name aliases: show the peer alias from the identity, or let each user keep a local nickname for peers, or both?
2. Should small writes (for example a comment under 0.005 tGLM) ever skip the dialog after a one-time opt-in?
   The proposal says no, because the user asked for an explicit dialog before every on-chain write.
3. May we embed Geist and Phosphor in the binary (OFL-1.1 and MIT, about 90 KB together), or should we keep system fonts and a smaller custom icon set?
4. Patch states: today a patch is open until a `merged` op; the hosted repo shows all 8 patches open although main moved.
   Should the UI infer "merged" from the accepted ref move, or show only what ops say?
5. Is block time acceptable as the displayed time (accurate, chain-sourced), or do we also show the author's own clock?
6. Should Activity and Search stay local-cache only, or later query Arkiv (`chart search`) for repos not on this computer?
7. Is the order slices 0 to 3 first right, or should patches (slice 4) come before the cost dialog?

### 7.1 Question 6 revised: search can also query Arkiv (slice 6 design)

The user, on 2026-11-11: "for 6 one can also using quering arkiv as its free right? its slow for now but free right?"
So slice 6 search gets a second source, Arkiv, next to the local cache.
This is a design only; nothing of it is built in slices 0 to 3, and the Settings control for it is added only when slice 6 is built.
Everything below was read from the code at main 53f7774 (v0.5.0) and from two live read probes on 2026-11-11, one anonymous and one with the access key.

**What `arkiv_query` can filter on.**
An attribute is indexed unless it is `ag.v` or a `bytes` value (`charter-core/src/attrs.rs` `Attribute::indexed`).
The query language has `=`, `<`, `<=`, `>`, `>=`, `BETWEEN`, `STARTSWITH`, `EXISTS`, `TYPEOF`, `AND`, `OR` and `NOT`, and nothing else (`arkiv-query` lexer at the pinned rev 2a3526d); `charter-store` `Pred` exposes `Eq`, `Ge`, `Le`, `StartsWith`, `And` and `Or`.
So matching is equality, range and prefix only: there is no substring, word or case-insensitive match, and "ledger" cannot find "fix-empty-ledger".

| Entity | Indexed attributes (searchable on chain) | Only inside the signed body `ag.body` (download to search) |
|---|---|---|
| Repo identity (`id`, `charter-core/src/body/id.rs`) | `ag.kind`, `ag.rid`, `ag.h`, `ag.parent`, `ag.nid` (signer), `ag.seq` (revision), `ag.name`, `ag.lang`, `ag.lic` | description, default branch, maintainers (delegates) and threshold, program pin |
| Peer key (`peer`) | `ag.kind`, `ag.nid`, `ag.h` | alias, public key |
| Bind (`bind`) | `ag.kind`, `ag.rid`, `ag.nid`, `ag.seq` | control and bulk addresses |
| Refs, anchor (`refs`, `anchor`) | `ag.rid`, `ag.nid`, `ag.h`; anchors also `ag.seq` and `ag.at` (head block seen when signing) | branch and tag names, commit ids |
| Op (`cob`: issue, patch, comment, review, release, program) | `ag.kind`, `ag.rid`, `ag.nid`, `ag.h`, `ag.cob` (object id), `ag.type` (for example `xyz.arkiv.issue`), `ag.action` (for example `create`, `merged`) | issue and patch title, text and comments, labels, release tag and notes, the author's clock `ts_ms` |

Consequences:

- Repo name, language and licence are searchable on chain, by exact value or by prefix (`chart search` already does this: `charter-sync/src/search.rs`).
- Owner or alias is not: an alias lives only in the peer body.
  A repo can be found by a maintainer's peer id (`ag.nid` on `bind`), which `chart search --peer` does, but not by a typed alias.
- Op kind and action are searchable (for example every `xyz.arkiv.issue` `create` on the network, or every `merged` patch of one repo), but titles and text are not.
  Searching titles needs the op rows downloaded and matched on this computer: one query returns up to 100 ops with their bodies, and there are no cursors, only `offset`.
- Private repos are sealed: `ag.body`, `ag.rid`, `ag.name`, `ag.lang` and `ag.lic` are hidden, and `ag.rid` and `ag.name` are replaced by blinded values (`charter-private/src/entity.rs` `HIDDEN`).
  Only a member who holds the repo key can compute the blinded name, so private repos never appear in an Arkiv search; a member finds them in the local cache.
- Every hit is a hint from permissionless data until it is verified: `search.rs` re-reads the repo's identity chain and drops hits whose head no longer matches.
  That costs 1 query for the identity plus 2 per signer (peer key and bind chain), so 3 queries for a one-maintainer repo.

**The setting (added with slice 6).**
"Search scope": "Local only" or "Local + Arkiv".

- Local results render at once from the cache, as the slice 6 acceptance already requires.
- With "Local + Arkiv", Arkiv results stream in after them over the same page (an htmx fragment), each marked "On Arkiv, not on this computer" with a Fetch button that starts the existing first sync for that repo.
- Arkiv is queried only when the user submits a search (Enter or the Search button), never while typing and never from the sync loop, so the `pace.rs` bounds and the sync loop's query count do not change.
- Each search has a query budget: at most 1 candidate query per term plus verification of the first 10 candidates, about 31 queries or 310 cost units for one-maintainer repos, and a "Verify 10 more" button for the rest.
  A search also stops early, and says so, when the quota the RPC reports reaches 60 percent used, well under `pace.rs` `PAUSE_AT` (80 percent), so a search can never push the shared quota into the sync pause.
- A title search on Arkiv (issues and patches across the network) downloads op pages and matches them locally; it gets the same budget, so it reads at most a few hundred of the newest ops and says how far it got.

**The default is left open for the user.**
What each choice costs:

- Arkiv reads cost no gas and no tGLM, so "free" is right in money.
- They are not free in quota.
  Measured 2026-11-11: every `arkiv_query` carries `Arkiv-Cost: 10`.
  The `RateLimit` header counts requests, not cost units: 2,000 requests per 10 s without a key and 5,000 per 10 s with the access key, each call taking 1 off `remaining`.
  The cost units go to a separate budget: without a key it is 10,000 units, so about 1,000 queries, per window of several minutes, and running it out returns HTTP 429 `ANON_COST_LIMITED` for about 6.5 minutes (`docs/p0/arkiv-limits.md`, measured 2026-11-06); with the key it is the key's monthly quota, reported only as `Arkiv-Quota-Used-Percent` (8 percent on 2026-11-11), whose absolute size the headers do not give.
- They are slow: a warm query took about 480 to 550 ms from this computer on 2026-11-11 (about 1.1 s with a new TLS connection), and `arkiv-limits.md` measured 250 to 730 ms; a full search with verification is about 1 to 3 s.
- "Local only" costs nothing and is instant, but finds only repos already on this computer.
- "Local + Arkiv" finds repos not on this computer, at about 310 cost units and 1 to 3 s per submitted search, and spends the same quota the sync loop uses.

## 8. Deferred to after mainnet: onboarding

The user decided on 2026-11-11 that onboarding waits until after the mainnet release; Charter stays in its dev phase until then.
This section only lists what that work would cover.
It is not designed here, nothing in this document depends on it, and no new-user flow was walked.

The onboarding work would cover:

- a first-run wizard inside `chart ui`;
- creating the identity and keys, and the recovery phrase flow (show, back up, restore);
- the wallet address and QR code, and how a new user gets funds;
- the funding path, including the public faucet route, and a live balance wait;
- cost estimates for the first actions;
- creating or cloning a first repository from the UI, and the first push;
- the second-computer story;
- plain-language error handling for each of those steps.

One fact from the audit that bears on it: the seed is raw 32 bytes in `keys/seed`, so a recovery phrase would be a new encoding of it, not a change to key derivation.

## 9. Not verified

- The first-sync "Pending" state, the RPC-quota pause and the Arkiv-unreachable state were read from source, not captured on screen.
- The write-failure state and a real Rabdi write were not exercised; the cost figures come from the fake chain.
- Keyboard and accessibility coverage is `axe-core` on 6 pages plus the shortcut help dialog; there was no screen-reader pass.
- The "fund 0" submit showing no feedback was seen once and not investigated.
- The mockup was checked in Chrome only, not Safari or Firefox.

## 10. Built: slices 0 to 3 and Settings

### 10.1 Decisions

The user said on 2026-11-11: "go with the defaults and have a settings tab where i can customize them".
So every question in section 7 took its proposed default, and each default is a choice on the Settings page.

| Question | Decided | By | Settings choice |
|---|---|---|---|
| 1. Peer names | Both: the local nickname with the alias after it, else the alias, else a short id | User (defaults) | Peer names, plus a nickname per peer |
| 2. Small writes | Always show the cost dialog | User (defaults) | Small writes: always, or skip under a limit up to 0.1 tGLM |
| 3. Fonts and icons | Embedded (Schibsted Grotesk, IBM Plex Mono, Phosphor) | User (defaults) | Fonts: Charter's or this computer's |
| 4. Patch status | Infer "merged" when the default branch contains the patch head | User (defaults) | Patch status: infer, or only a merged op |
| 5. Times | Block time only | User (defaults) | Times: block, or block plus the author's clock (`ts_ms`) |
| 6. Activity and Search scope | Local cache only for now; Arkiv as a second source in slice 6 (7.1) | User | None yet; the control comes with slice 6 |
| 7. Order | Slices 0 to 3 first | User (defaults) | - |
| Visual direction | Folio (design A): light-first, reads like a document, one green accent meaning only "proven", ink buttons | Coordinator | Theme: system, light or dark |

Folio replaced the Geist-based look of section 3.5 and the mockup of section 6: Schibsted Grotesk replaced Geist, body text is 15 px, and rules and space separate content (boxes only for dialogs, popovers and warnings).

### 10.2 Settings (`/settings`)

- Local only: `$CHARTER_HOME/ui/prefs.json`, written atomically with mode 0600 through charter-sync `write_json`.
- Unknown fields are refused; a bad value or a nickname over 40 characters is refused with the reason and the file is not touched.
- A corrupt file falls back to the defaults with a "Settings reset" notice on every page until a good file is saved; it is never overwritten silently.
- Saving applies to the next page without a restart; each option shows which one is the default.
- POSTs go through the same CSRF token and Origin and Host guard as every write; the header theme toggle posts to `/settings/theme`, so the theme lives in one place.
- Settings never reads the chain: the peer table comes from the cached views.
- A write skipped under the small-write limit lands on its status page with "Sent without asking", shows the actual cost, and is recorded in the spend ledger with `skipped: true`.
  The limit applies only when the write moves no funds (no escrow deposit), its estimate is at most the limit, the balance covers it and no RPC quota pause is on.
- Future work: a search-scope choice (local, or local plus Arkiv) when slice 6 is built.

### 10.3 What was built

- Slice 0: Folio tokens and CSS, embedded fonts (63 KB woff2, OFL) and a 31-symbol Phosphor sprite (MIT), licences in `assets/licenses/`, a favicon, the header with Repositories, Wallet and Settings, a sync indicator, the wallet chip, the theme toggle and a help button, and a phone bottom bar.
- Slice 1: repo list rows with an attention strip (conflicts and failed syncs only), repo header with the Verified seal and a Clone popover, a branch and tag picker on the code pages, a `/r/{rid}/branches` page, a side panel (maintainers, the rule for the default branch, the address) and the last commit bar.
- Slice 2: peer aliases (section 10.4), wall-clock times from block numbers (section 10.5), the Verified popover in plain sentences, "Maintainers", "Rules and CI" (`/r/{rid}/rules`, `/program` kept), and the red conflict banner on every repo page and the repo list.
  Block numbers, entity keys, sequence numbers and ML-DSA details stay inside Technical details disclosures and tooltips.
- Slice 3: the cost dialog v2 (what is published, the most it can cost, wallet now and after, how long it takes and how long it is kept, a breakdown and what will be signed), the three-step status card (signed, in a block, visible here) that shows the actual fee from the receipts, the `/wallet` page with Refresh and the spend ledger (`ui/spend.json`, newest first, never overwritten when unreadable), the low-balance warning, and Send disabled when the balance cannot cover the worst case or an RPC quota pause is on.
  The dialog's "Funds now" is the wallet plus the repo's paying address, the same pool the not-enough-funds check uses, so it can be above the header chip.
  "After, at least" is "Funds now" less the most it can cost: top-ups only move tokens between the two, and fees and escrow deposits leave the pool.
- Also fixed while there: `just now` instead of `0 s ago`, ordered-list markers and large amounts no longer spaced out by tabular figures, the issue-list state column aligned, table cells no longer touching, the status card no longer re-animating on every one-second poll, the author's clock no longer broken mid-phrase in the timeline margin, op fields as phrases ("Bounty: 0.5 tGLM", "Closed as fixed"), "Markdown supported" as the composer hint, and `refs/heads/` dropped from ref names outside Technical details.

### 10.4 Peer aliases

A peer document already carried an `alias`, but `chart` never set it, so every alias on chain was empty.
`CHARTER_ALIAS` (trimmed, at most 64 bytes) now sets it when a peer document is written.
charter-core gained `peer_doc`, which returns the key and the alias from the peer lookup the sync already does, and charter-sync keeps the aliases it sees in a process-wide map, so naming peers costs no extra query.
The UI stores them in `view.json` (`aliases`), merging old entries with this round's peers and op authors.
An author whose ops were cached before this change gets a name only when a later round sees one of their ops or peer heads again.

### 10.5 Times

A page reads no chain to show a time: a block's time is estimated as the view's sync time minus 2 s per block behind the synced head.
On Rabdi that is accurate to a few seconds; on the file-backed fake chain, which makes a block only when something is written, the estimates are compressed and read too recent.
The block number stays in the tooltip ("2026-11-11 09:41 UTC, block 1,204,331") and in Technical details.
The RPC-quota line keeps its wording ("paused: RPC quota, resumes in N min") but does not count down live; a reload shows the new value.

### 10.6 Performance, before and after

Measured on the test fixture with the same tests before the build (main 53f7774) and after; render times are release builds, 60 runs per page.

| Measure | Before | After |
|---|---|---|
| `app.css` gzipped | 5,766 B | 8,962 B |
| Largest page, HTML + CSS gzipped (budget 20 KB) | about 8.4 KB (repo home) | 12,165 B (repo home) |
| Smallest page, HTML + CSS gzipped | about 6.9 KB (`/`) | 10,302 B (`/wallet`) |
| Repo home render p50 / p99 | 0.35 / 0.93 ms | 0.29 / 0.52 ms |
| Commit render p50 / p99 | 0.32 / 0.78 ms | 0.25 / 0.40 ms |
| Repo list render p50 / p99 | 0.03 / 0.15 ms | 0.05 / 0.09 ms |
| Branches, wallet, settings p50 | not served | 0.10, 0.01, 0.04 ms |
| Tree render p50 / p99, A/B | 0.21 / 0.25 to 0.33 ms | 0.13 to 0.14 / 0.15 to 0.20 ms |
| Blob render p50 / p99, A/B | 0.14 to 0.15 / 0.17 to 0.23 ms | 0.15 / 0.20 to 0.27 ms |
| Repo home render p50, A/B | 0.28 to 0.30 ms | 0.23 to 0.24 ms |
| History walks per tree / blob render | one walk of up to 2,000 commits for the entries plus one unbounded walk for the folder bar / 0 | 0 / 0 |
| Commit objects read per Code page for the branch picker | one per ref | 0 (from the index) |
| Chain queries: first sync, then per hour with no page open | 9, then 0 | 9, then 0 |
| Chain queries per hour: idle, on screen, on screen with one change | 17, 365, 371 | 17, 365, 371 |

The repo list's p50 rose by about 20 microseconds from its richer rows; its p99 fell, and issue and patch counts are now counted once per sync round into `view.json` instead of reading every repo's `cobs.json` on each render, so the list no longer grows with op count.
Fonts and the icon sprite (63 KB and 5.4 KB gzipped) load once and are cached as immutable; the 20 KB budget of `ui.md` section 6 counts HTML and CSS.
The A/B rows ran the before (`425df78`, main's code) and after binaries alternately four times each in one session, because the machine stayed loaded (load average 4 to 10); the ranges are across those runs.
Tree, blob and repo home pages walk no history while rendering: the sync round writes `last.json`, one index per ref tip keyed by commit id (`charter-ui/src/last.rs`), and the page looks paths up in it, loaded once per sync.
A tip that did not move keeps its index; a moved tip is indexed again from one walk, bounded at 20,000 commits, that stops once every path of the tip's tree is placed.
With no index yet (a cold start before the first round) the pages render without the last-commit bar.
The blob page's p50 matches main with the bar; taking the bar out measured the same, and the remaining p99 spread is within run-to-run noise on this machine.
The writer reads the wallet balance once at start, once when pricing (as before), once after a write and on Refresh; none of these are in the sync loop.

### 10.7 Known issues found during the build

- Fixed: a push that only moved refs never reached an open page, nor the UI's sync loop (on main since v0.5.0).
  Format v3 heads (`8d5f1e4`) are patched in place, and the UI's one-query probe (`charter-ui/src/pace.rs`) saw only the repo's entity count and newest key.
  The probe now sorts the repo's entities by `ag.dev`, which only heads carry; rows without the sort attribute come first in creation order (Arkiv `docs/api-spec.md` "Sorting", checked on Rabdi), so one page of the same query holds the newest non-head entity and every head with its `$updatedAt`.
  It stays one query when nothing changed and two when something did, as before; the `tests/pace.rs` counts are unchanged (first sync 9, then 0 with no page open; 17 an hour idle; 365 on screen; 371 with one change), and a refs-only push costs one probe query per 10 s until it is seen, then one round.
  The `chart` CLI's memo probe (`charter-sync/src/view/memo.rs`) already lists every head's `(key, $updatedAt)` for every repo, and a refs-only push now has a test there too.
- Fixed: the phone tab strip fades its right edge only while tabs sit past it.
- Fixed: the blob page has the last-commit bar, read from the per-tip index of section 10.6 like the tree page's, so neither page walks history while rendering.
- Fixed: amounts use one format everywhere (wallet and its chip, the cost dialog and its breakdown, the write status, the ledger, Rules and the timeline), from `charter-ui/src/pages/money.rs`.
  From 0.0001 tGLM up it shows at most four decimals with trailing zeros dropped, rounded up for what you may pay and down for what you hold; below that it shows gwei, or wei under 1 gwei (`5 wei`); the exact tGLM value is the tooltip.
  The ops listed under "what will be signed" keep the exact value.
  In columns (the ledger, the dialog's breakdown, Rules bounties) tGLM and gwei are padded to four decimals in tabular numerals and right-aligned on a unit slot as wide as "tGLM", so `0.0030 tGLM` sits above `0.0139 tGLM` and `5 wei` lines up on the unit; running text keeps the short form.
- Writes to a private repo are priced and sent sealed only once `sec/private-plumbing` lands; that branch picks the backend inside `writes/run.rs`, and this build left that choice alone.
  The dialog reads every figure from the `Estimate` the writer produces, so it shows the sealed cost with no change here.

### 10.8 Verification

- Workspace tests (581), `cargo fmt --check` and `cargo clippy -D warnings` pass; `tests/pace.rs` adds the refs-only push from another device (it failed before the probe fix); `tests/v2.rs` adds 8 tests (vocabulary and no bare block numbers on 14 pages, aliases, branches and picker, the conflict banner, Settings applied, kept across a restart, mode 0600, bad input and missing CSRF refused, corrupt-file reset, the small-write skip, the dialog fields and the ledger, and the unfunded case).
- The e2e run on the fake chain: 1,495 checks pass, 3 warn (two synthetic long pages over 20 KB and debug-build timing on one of them), 0 fail, including the live update of section 10.7 and the tab fade at both ends on 76 phone pages; axe reports 0 WCAG 2.1 A and AA violations on 50 page and state runs, including the confirm dialog with its breakdown open.
- `e2e/run.sh` now exits 0 when every check passes and `E2E_WORK` keeps the work directory (its cleanup trap's last test used to set the exit status to 1).
