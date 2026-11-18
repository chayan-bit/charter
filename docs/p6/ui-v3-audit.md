# chart ui v3 audit

Status: audit of v0.7.2 (main e8f28a9) before the v3 redesign on branch `ui/v3`.
Date: 2026-11-17.
Sources: the e2e harness on the fixture repo (fake chain) at 1440x900, 1280x800 and 390x844 in light and dark, screenshots in `crates/charter-ui/e2e/out/v3/before/` (not committed), plus a read-only run of the v0.7.2 binary against a copy of the real `CHARTER_HOME` (three repos on Rabdi, among them `arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ`).
Each finding names its screen; functional bugs come first.

## 1. Functional bugs

### 1.1 "Last sync failed" with no reason and no way out (header, every page)

The header chip said "Last sync failed" and nothing else, with no link, tooltip or retry.
Cause: `SyncChip::of` only counted repos whose last round failed; the error string lived in the verify popover of that one repo, which the user had no reason to open.
On the real home the failing repo is `4424958c...`, whose first sync fails with `refs a72064f2... after the program pin` (`ErrorCode::NotLive`): a refs entity it needs has expired on Arkiv, so it can never be rebuilt here.
The `chart ui` output showed only a keepalive warning for another repo, because the per-repo failure went to stderr once per round with a hex id.
Fixed in bb915b6: the chip says how many repos failed and links to the one that did, the repo page shows the error with a plain hint and a Retry now button, and the list says Sync failed with the reason.

### 1.2 A repo whose first sync fails is shown as "Checking, checked never" forever (repo list)

The row of a never-synced repo read "Checking", "Checked never" and "No signed push yet", which looks like work in progress, not a failure.
The attention strip said it "shows what was verified before", which is false when nothing was ever verified.
Its name was the first seven hex digits of its id, with no hint of what it is.
Fixed in bb915b6 with a `failed` state, the reason and the time it failed.

### 1.3 Stale data shown as current until a reload (repo list, repo header)

The coordinator saw the charter repo at "chore(release): bump workspace to 0.5.0", "Updated 5 d ago" and "Verified, 4 d ago" while hosted main was at v0.7.2.
Reproduced cause: `chart ui` renders from the cache at once, and the first round of the session runs in the background; the repo list had no event stream at all, and on repo pages the header chip and "Updated ... by" were not live regions, so they stayed stale after the round finished.
Nothing marked a round in flight, so days-old data was labelled "Verified, on this computer, 4 d ago" with no sign that a refresh was under way.
Rounds also ran in rid order, so a slow round of another repo delayed the repo on screen.
The real home also had a v0.5.0 `chart ui` (started 2026-11-12) still running against the same `CHARTER_HOME`; that older server never refreshes without a page open, which explains the four-day-old cache.
With the v0.7.2 binary on a copy of that home the cache converged to e8f28a9 within seconds, so the sync itself was not broken; the pages were.
Fixed in bb915b6: a running round shows as "checking for changes now", the list follows every repo's rounds over `GET /events` (which holds nothing on screen, so no extra RPC), the chip, "Updated ... by" and the failure notice refresh with the verification panel in one request, and the repo on screen is checked first.

### 1.4 Search nav item never highlighted on desktop (header, `/search`)

On `/search` at 901 px and wider the Search link is hidden (`.primary .s { display: none }`) and the Jump box is removed, so no main destination is marked as current.
Fixed in e153541: the Search link shows in the top bar while it is the current page, and the e2e run checks it.

### 1.5 Patch files page is one megabyte and 2,000 tab stops (patch files)

`/r/<rid>/patches/<id>/files` on the fixture is 963 KB of HTML (66 KB gzipped), because every diff line carries a `<details>` with one or two textareas for an inline comment.
Keyboard users tab through 2,032 "+" buttons before they reach the review form.
Open: making the comment boxes lazy needs a change to the review form contract (one box per line, posted as `c.<file>.<side>.<line>`), so v3 leaves it and the e2e size check reports this page as a warning, like `commit-long`.
Fixed in v4 (9884d28) without touching that contract: large files on the patch files page start collapsed and load on open, so the page went from 964,511 B to 13,191 B of HTML.
The commit page (`commit-long`) has no comment boxes; its size is the diff itself, cut at 2,000 lines behind "Show all".

### 1.6 Maintainer name and id duplicated (repo sidebar)

When a peer has no alias the sidebar printed the short id twice: "d72c851 ... d72c851".
Fixed in e153541: the id is printed only when it differs from the name.

### 1.7 Phone screenshots show the bottom nav in the middle of the page

The fixed bottom nav sits over content in full-page captures, and on short pages it covers the last row; content has 120 px of bottom padding but the README end and forms still sit under it on 390x844.
Fixed in e153541: the bottom nav is sticky at the end of the page, so it never covers content; full-page captures still draw it where the first screen ends, which is how Playwright captures sticky elements.

### 1.8 The Commits tab of an empty repo is a 404 (commits)

The Commits tab links `/r/<rid>/log/<default branch>`, and a repo with no commit yet has no such branch, so the tab opened the "No such branch, tag or commit" error page.
Fixed in 8f10adc: the default branch of a repo with no commits shows the empty log; any other missing ref is still a 404.

### 1.9 "Expired on Arkiv" for a record that is only late or that contradicts itself (repo list, repo page)

The round maps `E_NOT_LIVE`, `E_ID_NOT_LIVE` and `E_CONTROL_NOT_LIVE` to "Expired on Arkiv", keeps the repo off the attention strip and does not re-run the round until the probe sees a change.
`E_CONTROL_NOT_LIVE` also covers a device head that contradicts its own writes (`HeadRollback`: an older head than its address wrote, `DuplicateHead`: two valid heads), where nothing expired; those now say so, with their own hint, and stay on the attention strip (fix/batch-0.7.3).
A record written moments ago that Arkiv has not indexed yet is a missing row, exactly like an expired one, and the client cannot tell them apart without another read.
It is left as "Expired on Arkiv", with a hint sentence that a record written in the last minute can look the same; the label heals on its own, because the probe sees the row arrive and the next round clears the error.

### 1.10 "Renew by" was amber from the first push (repo sidebar)

The storage line turned amber under 30 days, but a push writes refs, manifests and chunks for exactly 30 days, so every repo not yet kept alive was amber from its first push.
It now turns amber where `git fetch` starts warning (7 days, `WARN_EXPIRY_BLOCKS`) and red once the date passed, with unit tests for the date and both boundaries (fix/batch-0.7.3).

### 1.11 Ids wrapped in the verify popover (repo header)

On a phone the address in Technical details broke mid-string and "ML-DSA-65" broke at its hyphens.
The address is now the same one-line copy box as the sidebar and short ids never wrap (fix/batch-0.7.3).
Technical details stays a disclosure inside the popover on purpose: it keeps protocol words (peers, seq, entity keys) out of the plain-language view, which `tests/v2.rs` checks.

### 1.12 Issue and patch state pills (issues, patches, threads)

The v0.7.2 pill was grey for open and closed alike.
Fixed in v3 (e153541): open is green with a dashed circle, merged violet with a merge icon, closed grey with a check (issues) or a cross (patches).

## 2. Global visual and UX problems

- Hierarchy is inverted: the repo name is a 38 px display headline while the content the user came for (files, issues) starts below the fold on a 1280x800 screen.
- Type is too large for a dense tool: 15 px body, 20 px repo row titles, 28 px page titles; rows are 41 to 104 px tall.
- The document style (hairlines everywhere, no containers) makes lists, the README and the sidebar run together; nothing groups the file list with its last-commit header.
- The page is warm-grey "paper" with a green accent and black ink buttons, which reads as an editorial page, not a developer tool, and gives interactive elements no colour.
- Links are underlined body-colour text, so links and plain text are hard to tell apart outside tables.
- Header: 60 px tall, with a 280 px search box, a sync dot, a wallet chip and two icon buttons; the sync text has no affordance.
- The verification seal (the product) is a 32 px icon with tiny 12 px subtext floating top right, detached from the content.
- Tabs have no icons and no count style for zero, and the active underline sits 1 px off the rule.
- Empty states are a bare paragraph with no icon or next step on most pages.
- Focus ring is green on green-tinted states; fine for contrast but inconsistent with the black buttons.
- The phone layout stacks title, description, two meta lines, the seal and Clone before the tabs; content starts at about y=470.

## 3. Per screen

### Repo list (`/`)

- Rows are 104 px tall with a 20 px title, a description, a meta line and a right-aligned state column; ten repos fill three screens.
- The state column repeats "Checked just now" on every row.
- Counts read "0 issues, 0 patches" even when there is nothing to say.
- No count of repos in the page head; the intro sentence is two lines of marketing-like copy.

### Repo code page (`/r/<rid>`)

- 38 px title, 17 px description and a 14 px meta line take 200 px before the tabs.
- File rows are 41 px; the last-commit row looks like a file row; sizes and ages float right with no column alignment.
- README has no container and no clear start; its code block is wider than the text column.
- Sidebar: "Maintainers", "Changing main" and "Address" are plain text blocks with a lot of empty space below; the address wraps in a grey box.
- The Clone popover duplicates the address shown in the sidebar.

### Tree and blob

- The path bar repeats the repo name as a link and uses a 17 px mono heading.
- Blob: line numbers and code sit on the page background with no container; "Copy path", "History" and "Raw" are three equal buttons.

### Commits (`/log`) and commit

- Day headings are 17 px bold over a black rule; each commit row is 82 px with the hash first in underlined mono.
- Commit page: the subject is a 28 px headline; facts are a two-column list with mixed mono and sans; diffs have no file container.

### Branches

- Two stacked tables with "As alice pushed them" headings; the default branch chip and tag chip look identical.

### Issues and patches lists

- State is a grey "open" or "closed" pill with no colour or icon, so open and closed look the same.
- Rows are 70 px with the title, labels and a meta line that wraps on desktop.
- "New issue" floats alone in a right-aligned row above the list.

### Issue and patch threads

- The 28 px title wraps to two lines; the state pill is grey.
- The timeline puts times in a right-aligned mono margin with diamond markers; comments have no container, so a comment and an event ("closed this") look alike.
- Bounty shows as "Bounty: 5 wei" in the body meta line.
- The comment form, Fund, Claim and Labels sections are stacked full width with disclosure triangles; triage has no sidebar.

### Patch files and review

- Each file header is a hairline row; the diff runs full width with no frame; inline "+" buttons are black squares.
- See 1.5 for weight.

### Releases and new release

- The tag is a 28 px headline; the artifact table shows the full sha256 in grey mono that wraps on phone.

### Rules and CI

- Section headings are 17 px with tables that have no containers; the JSON state is a raw `<pre>`.

### Verify popover

- Opens from the seal; good content, but "Technical details" is a second disclosure inside a popover and the facts table wraps ids.

### Search (local and Arkiv)

- The page head plus a 44 px input and a "Local only" line; results sections are hairline lists like everything else.
- The Search nav highlight bug (1.4).

### Wallet

- Balance is a 28 px number in the main column with "Refresh" far to the right; the address sidebar is mostly empty.

### Settings

- Seven sections, each 120 px or more, with a 260 px label column; radio cards have no visible container, so the clickable area is unclear.

### Manage, new issue, new release

- Forms are full width up to 720 px with labels and hints on one line; the primary button is black.

### Confirm dialog, sent and confirmed status

- Clear content; the 30 px mono price competes with the title; the primary is a black button labelled with the price.

### Toasts

- A black pill at the bottom centre; failed writes stay with a Dismiss button; fine, but it does not match the rest of the palette.

### Error page and not found

- A large "404" heading and one sentence, no icon, no link styling.

### Empty states

- Empty repo: "Nothing on main yet" paragraph with no clone hint near it; empty issues, patches and releases are one paragraph each.

### Keyboard shortcut dialog

- A plain table with full-width hairlines; keys and descriptions are not aligned in columns on phone.

## 4. Accessibility and performance baseline

- axe (WCAG 2.0 and 2.1 A and AA) found no violations on any page in either theme; keyboard checks pass except the patch files tab-stop count (1.5).
- CSS: `app.css` 45,930 B (10,180 B gzipped), `search.css` 1,366 B on the search page only.
- JS: `htmx.min.js` 52,182 B, `keys.js` 9,315 B, `live.js` 2,393 B; fonts 63,408 B.
- Server time (debug build, p50): repo page 2.5 ms, issue 2.7 ms, patch files 15.7 ms, blob-long 393 ms.

## 5. v4 direction: ink and signal

Audit of v3 against the design-taste checklist.
v3 is cleaner than v2 but reads as a GitHub reskin: a grey canvas with white bordered cards, a GitHub tab bar and file table, blue links, pill badges everywhere.
The product (cryptographic verification) is a 40 px tinted box in the corner, so it does not set the identity of the page.
Concrete defects the review found, all confirmed on the v3 screenshots:

- The "Verified / checked just now" badge is a boxed pill in the top right, detached from the content it vouches for.
- The address wraps mid-string in the sidebar.
- "Permanent on Arkiv" and the "Public" badge say the same thing twice.
- Every file row repeats the same truncated commit message, and the list reads as noise.
- The maintainer is "you" plus a bare hex, with no hint what the hex is.
- The sidebar is three short text blocks with a screenful of dead space under them.
- Release sha256 values wrap, and the patch files page is about 1 MB and 6,000 px tall.

Dials: DESIGN_VARIANCE 5, MOTION_INTENSITY 3, VISUAL_DENSITY 7.
Reading it as: a local developer tool for git users who care about provenance, with a calm and dense language, leaning toward native CSS, Geist and Geist Mono, and one signal colour.

Direction, in five lines:

1. Palette: one flat canvas (`#fbfcfb`, near-black `#0b0d0c` in dark) with green-tinted greys, hairlines instead of cards, no page-background panels. One accent, a deep signal green (`#0a6b4d`, `#3dd29a` in dark) that means "proven" and also marks what you can act on. Warn amber, error red and merged violet are status colours only.
2. Type: Geist for text and Geist Mono (zero slashed) for hashes, counts and addresses, both self-hosted variable fonts (53 KB together, 10 KB less than v3). Body 14 px, titles 24 px at -0.025em, small labels 12 px sentence case.
3. Shape and spacing: three radii only (4 chips and keys, 6 controls, 12 dialogs and the band); 4 px grid; rows 34 to 40 px; lists and tables are flush with a hairline above and below, not boxed.
4. Signature element: the stamp (a solid green square with a check; amber, red or hollow for the other states) and the provenance band under the repo title. The band is a full-width signature line: stamp, state, and the head hash, block number and signing quorum as mono cells. The repo list is a column of stamps. Nothing else on the page is green-filled, so verification is the first thing the eye lands on.
5. Information design: one fact once. Visibility lives in the header, storage and network in a spec sheet, the address is one copyable line that never wraps, each file row shows a commit message only when it differs from the latest-commit line above it, and large diffs start collapsed.

Constraints kept: every URL, feature and htmx behaviour; one accent; WCAG AA in both themes (palette pairs checked, axe in the e2e); loading, empty and error states; `prefers-reduced-motion`; no gradients, glows or emoji; no runtime CDN.

### Measured result (kratos, debug e2e and release render bench)

- Fixed: boxed pill (now the provenance band), address wrapping, public and permanent duplication, repeated file-row messages, sha256 wrapping, patch files page size, sparse sidebar, bare maintainer hex.
- Patch files page: 964,511 B to 13,191 B of HTML (68,203 B to 14,802 B with CSS, gzipped); large files load on open and a no-JS link opens them.
- Every other page: HTML plus CSS gzipped within +0.3 percent of v3 (+40 to +110 B); fonts are 10 KB lighter.
- Server time (release, 60 runs, p50): unchanged within 0.01 ms on every page except the repo home, 0.23 to 0.35 ms, far under the 20 ms budget.
- e2e: 2,868 checks, 0 failures; axe WCAG 2.1 A and AA 0 violations on 81 page and theme pairs. The three warnings (blob-long, commit-long size and the debug-build time of blob-long) are the stress pages and predate v4.
