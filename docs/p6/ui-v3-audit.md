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

### 1.5 Patch files page is one megabyte and 2,000 tab stops (patch files)

`/r/<rid>/patches/<id>/files` on the fixture is 963 KB of HTML (66 KB gzipped), because every diff line carries a `<details>` with one or two textareas for an inline comment.
Keyboard users tab through 2,032 "+" buttons before they reach the review form.

### 1.6 Maintainer name and id duplicated (repo sidebar)

When a peer has no alias the sidebar printed the short id twice: "d72c851 ... d72c851".

### 1.7 Phone screenshots show the bottom nav in the middle of the page

The fixed bottom nav sits over content in full-page captures, and on short pages it covers the last row; content has 120 px of bottom padding but the README end and forms still sit under it on 390x844.

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
