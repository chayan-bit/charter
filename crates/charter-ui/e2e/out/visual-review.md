# chart ui visual review

Source: screenshots in `crates/charter-ui/e2e/out/` from one harness run (debug build, fixture repo `charter-demo`), light and dark, desktop 1280x800 and phone 390x844 at 2x.
Every page was looked at in at least one theme and one viewport; the pages with the most findings were looked at in all four combinations.
Selectors refer to `crates/charter-ui/assets/app.css` and `crates/charter-ui/templates/`.
Ranked by user impact.

## 1. Repo header, verification panel and tabs eat the phone viewport (high)

Files: `repo-light-phone.png`, `blob-light-phone.png`, `log-light-phone.png`, `issues-light-phone.png`, `releases-light-phone.png`.
On every repo page at 390 px the content starts at about y=630 of 844 CSS px: title, description, a three-line verification card, a two-row tab bar, then the page.
The verification summary alone is 90 px because `.vp summary` wraps into three stacked lines with loose line height.
Fix: on `max-width: 600px` make `.vp summary` `flex-wrap: nowrap` with `.sum` truncated (`overflow: hidden; text-overflow: ellipsis; white-space: nowrap; flex: 1`) and hide `.meta`; hide `.repo .desc` and shrink `.repo h1` to 16 px; put the tab bar on one row (item 2).
Consider rendering the repo header once and swapping only `<main>` content with htmx boost, so the big header is not a per-page cost on small screens.

## 2. Tab bar wraps to two rows on phone and the underline lands on row one only (high)

Files: `repo-light-phone.png`, `log-light-phone.png`, `issues-light-phone.png` (same on every repo page).
"Program" and "Verification" fall onto a second row; the active-tab underline and the hairline sit on different rows, so the bar looks broken.
Cause: `.tabs { flex-wrap: wrap }` (app.css line 91).
Fix: `.tabs { flex-wrap: nowrap; overflow-x: auto; scrollbar-width: none; gap: var(--sp-3); }` and `.tabs a { white-space: nowrap; flex: none; }`; scrollable tabs keep one baseline.
Also make the "Verification" tab open the panel without JS: `<a href="#verify">` only scrolls, it does not set `details[open]`; use `details:target` styling or a `:has(:target)` rule, or link to a real page.

## 3. Phone top bar clips the breadcrumb when it wraps (high)

Files: `blob-light-phone.png`, `blob-wide-light-phone.png`, `blob-dark-phone.png`.
On `/blob/main/src/main.rs` the crumbs wrap to two lines inside a fixed 40 px bar; the first line touches the top edge and "main.rs" hangs below the wordmark.
Cause: `.top { height: 40px }` plus `.crumbs { flex-wrap: wrap }`.
Fix: `.top { min-height: 40px; height: auto; padding-block: 6px; }` and `.crumbs { flex-wrap: nowrap; overflow: hidden; }` with only the last two segments visible on phone (`.crumbs > :not(:nth-last-child(-n+3)) { display: none }`), or make the crumbs a second row below the wordmark on phone.

## 4. Release table breaks hashes and URLs mid-token on phone (high)

File: `releases-light-phone.png`.
The sha256 column shows `59bd16` with a lone `d` on the next line, the location URL breaks as `https://example.co` / `m/demo.tgz`, and the size column is a bare `15` with no unit.
A 7-character prefix of a sha256 is also not useful for a release page whose point is artifact verification.
Fix: `td code { white-space: nowrap }` for hashes; show the first 12 characters with a copy affordance and the full value in a `title` and in the `data-copy` attribute; render URLs with `overflow-wrap: anywhere` and ellipsis; format size as `15 B`; on phone turn each artifact into a stacked block (name, hash, size, locations) instead of a four-column table.

## 5. Verification panel gives no hint that it expands (medium)

Files: `repo-light-desktop.png`, `repo-verify-open-light-desktop.png`, `repo-verify-open-dark-phone.png`.
The collapsed summary looks like a static banner; there is no chevron or "details" cue, and the expanded table (the product's differentiator) is invisible unless a user guesses to click or knows the `v` key.
Fix: add a `::after` chevron on `.vp summary` that rotates on `[open]`, plus a text "Details" at the right on desktop; keep the `list-style: none` but add `cursor: pointer` and a hover tint `var(--s2)`.
The expanded `.facts` table also uses `ok` green text without the glyph for "ML-DSA-65 + Ed25519 valid"; prefix it with the check glyph so state is not color only (design doc section 7).

## 6. Links inside text are distinguished by color only: axe `link-in-text-block` on 5 pages (medium, accessibility)

Files: `home-light-desktop.png`, `repo-light-desktop.png` (README link), `blob-long-cap-light-desktop.png` ("Show all"), also on `commit-long`.
Links inside running text have no underline, so they fail WCAG 1.4.1 against the surrounding text.
Fix: `.readme a, .md a, .more a, .desc a, td > a + .dim, p a { text-decoration: underline; text-underline-offset: 2px; }`, or underline all links inside `p`, `.more`, `.last` and `.dim` containers; table-cell primary links can stay plain if they differ in weight, but the repo name on the home page needs an underline or `font-weight: 500`.

## 7. README relative images and links do not resolve (medium, correctness)

Files: `repo-light-desktop.png`, `repo-dark-desktop.png`.
`![architecture diagram](docs/diagram.png)` renders a broken image and the browser requests `/r/docs/diagram.png` (404, a console error the harness flags on every repo page load); the relative link `docs/guide.md` resolves the same wrong way.
Fix: after `ammonia`, rewrite relative `src` to `/r/<rid>/raw/<ref>/<dir>/<src>` and relative `href` to `/r/<rid>/blob/<ref>/<dir>/<href>` (use `ammonia::Builder::attribute_filter` or `url_relative(UrlRelative::Custom(...))`), and add `loading="lazy"` plus a `max-width: 100%` that already exists.
Raw images must be served with a safe type (SVG as attachment or with `Content-Security-Policy: sandbox`).

## 8. Diff gutters and code start offsets differ between files (medium)

Files: `commit-light-desktop.png`, `commit-first-dark-phone.png`.
`docs/guide.md` (deleted file) has one wide gutter and code starts at x=57; `src/main.rs` has two narrow gutters and code starts at x=72; whole-file adds show `+# Guide` with the sign glued to the text while modified files show a separated sign column.
Hunk header text sits at a third offset.
Fix: always render two number columns (`.diff .ln { width: 3.5ch; min-width: 3.5ch }`) and a fixed-width sign column `td.sg { width: 1.5ch; user-select: none }`, so code starts at the same x in every file; for whole-file adds and deletes leave the unused number column empty.

## 9. Tree rows lack last-commit message and age; ".." row looks like debris (medium)

Files: `tree-nested-light-desktop.png`, `tree-nested-dark-phone.png`, `repo-light-desktop.png`.
The design promises name, last commit message, age per row; the table shows name and byte size only, with one commit line above the table instead.
The `..` up link is a lone mono blue "..", dotted at phone size, floating with its own margin, unlike a table row.
File sizes are raw bytes (`182270 B`).
Fix: render `..` as the first `<tr>` of the table with a folder glyph and the text "(parent directory)"; humanize sizes (`178 KB`); add the per-row commit subject and age columns, hidden below 600 px; align the `.last` line with the table padding (`padding-inline: var(--sp-2)`).
Folder glyph is a filled shape and file glyph an outline in a different grey, so the two columns of icons look inconsistent; use the same stroke style and `color: var(--text-3)` for both.

## 10. Content column is left-aligned at 1100 px while the top bar spans the full width (medium)

Files: `repo-light-desktop.png`, `home-light-desktop.png` (all desktop shots).
At 1280 px the buttons "theme" and "?" sit at x=1264 while the content right edge is x=1084, so the header actions float 180 px away from the content column.
Fix: wrap `.top` content in the same max-width container (`.top > * ` inside a `max-width: 1100px; margin-inline: 0` wrapper), or put `margin-inline: auto` on both `main` and the top-bar inner wrapper.
The design says left aligned content, which is fine, but then the top bar needs the same right edge.

## 11. Patch page lacks a summary and shows raw ops (medium)

File: `patch-dark-desktop.png`.
A stray `rev 1  08365dc` row sits between the "Opened by" line and the first card with partial hairlines and no header; base, closes and head show as full 40-character hashes in `<code>` without links (the closed issue and the commit are not clickable); reviews appear as `create` and `approve` ops with the patch id repeated, instead of "approved at head 08365dc".
Fix: add a header strip (state badge, base..head with links to `/commit/`, closes link to `/issues/<id>`), a revisions table with a header row, and fold review ops into one "Reviews" list (verdict badge, reviewer, head, time); keep the raw ops behind a `<details>`.

## 12. Issue thread typography and spacing (low-medium)

File: `issue-light-desktop.png`, `issues-light-phone.png`.
Ordered list inside a comment shows "1. run it" with an odd inset and no hanging number (`.md ol` padding with `list-style-position: inside`).
A comment card ("seen it too") carries an empty trailing line, so its bottom padding is about 3x its top padding.
Op headers use protocol words ("create by 0652f1d", "comment by") instead of "opened", "commented", "closed", and "(author clock)" repeats on every card.
On phone the badge column wastes 150 px of 390 and the meta line (`800ebdc by 0652f1d at block 13, 0 comments`) wraps as "0 / comments".
Fix: `.md > :last-child { margin-bottom: 0 }`; `.md ol, .md ul { padding-left: 1.5em; list-style-position: outside }`; map actions to past-tense verbs; on phone put the badge inline before the title (`.threads .st { display: inline; padding: 0 }`) and make meta `font-size: 12px; white-space: nowrap` for the comment count with `<span class="nowrap">0 comments</span>`.

## 13. Help dialog has no backdrop and is hard to see in dark mode (low-medium)

Files: `home-help-dialog-dark-phone.png` (also the other three variants).
The dialog background equals the page background and there is no `::backdrop` dim, so it reads as a bordered region of the page; the "Close" button sits left of the table content (8 px outdent) and wraps the key list "g c / l / i / p / r / v" onto two lines.
Fix: `dialog { background: var(--s1); box-shadow: var(--shadow); } dialog::backdrop { background: rgb(0 0 0 / .4) }` (the shadow token exists but is not applied), align `form[method=dialog] { margin-top: var(--sp-3); text-align: right }`, and render the g-chords as separate rows or allow `flex-wrap`.

## 14. Home table cramped on phone, "Last push" is a block number (low-medium)

File: `home-help-dialog-dark-phone.png` (background), `home-light-phone.png`.
"LAST PUSH" header wraps to two lines, the repo description wraps below the name, and "block 6" gives no wall-clock meaning.
Fix: hide the "Synced" column on phone or stack name/description in one cell with `display: block`; show "block 6 (3 min ago)" using the block timestamp; set `th { white-space: nowrap }`.

## 15. Blob page: 200 px of chrome before the first line of code (low-medium)

Files: `blob-light-desktop.png`, `blob-dark-phone.png`.
Title, description, verification card and tabs repeat above every file; a 3-line file starts at y=221.
The line-number gutter is tight against the code (4 px) and left-biased for short files.
Fix: collapse the repo header to one line (name + verification chip + tabs) on non-home pages; `.code .ln { padding-right: var(--sp-3) }`; add a sticky `.filehead` (`position: sticky; top: 0`) so Raw stays reachable in long files.
Singular/plural: "1 lines" should be "1 line".

## 16. Long blob page weighs 2.2 MB and the tab order has 2,000 stops (high for large files, performance and accessibility)

File: `blob-long-cap-light-desktop.png` (the cap boundary renders correctly: "1200 more lines. Show all (lines past 2,000 are plain text)").
Each highlighted line is a table row with nested `<span class="sy-...">` elements (about 1.1 KB per line); 2,000 lines are 2.2 MB raw, 47 KB gzipped, and in the debug build take 5 to 8 seconds server side.
Every line number is a link, so the page has 2,017 tab stops; keyboard users must tab through all of them to reach the footer.
Fix: collapse adjacent spans with the same scope stack (syntect `ClassedHTMLGenerator` already supports flat classes: use `ClassStyle::SpacedPrefixed` and strip empty or duplicate scopes), use `<pre>` with one `<span id="L12">` per line instead of a table, make line numbers non-focusable anchors (`tabindex="-1"` or CSS counters plus `#L12` handled by a click on the line), and lower the cap to 1,000 lines.
`commit-long` has the same shape at 300 KB (28 KB gzipped) and a "Show all" link; cap hunks in total, not per file.

## 17. Commit page "Anchored" row runs refs together (low)

File: `commit-light-desktop.png`.
"via refs/heads/feat/side refs/heads/main refs/tags/v0.1.0" has no separators, and the "ML-DSA-65 + Ed25519 valid" text wraps under it.
Fix: render refs as `<code>` chips separated by commas or `<ul class="inline">`, and put the signature status on its own row with the check glyph.
Also add the commit time (only the date is shown) and a "root commit" row styled like the others (it is dim on phone, good).

## 18. Blockquote has no visual treatment (low)

Files: `repo-light-desktop.png`, `repo-dark-desktop.png`.
The README blockquote renders as indented plain text with no bar or tint, so it reads as a stray indent (it also follows a list with no spacing).
Fix: `.readme blockquote, .md blockquote { margin: var(--sp-3) 0; padding-left: var(--sp-3); border-left: 3px solid var(--border-strong); color: var(--text-2) }`.
The README card also has an extra 24 px above its first heading: `.readme > :first-child { margin-top: 0 }`.

## 19. Program page: header/number misalignment and bare hashes (low)

File: `program-light-desktop.png`.
"AMOUNT" header is left-aligned over right-aligned numbers; bounty rows show only `800ebdc` and `b783ac9` with no issue titles; the State JSON dump uses unlinked 64-character hashes and no empty-state wording for "0 accepted, 0 rejected" (rejected ops with reasons are an advertised feature).
Fix: `th.num { text-align: right }`, join the issue title into the bounty row, link ids in the JSON block with a small post-processing pass or hide the raw state behind `<details>`, and add "No ref moves yet" empty text.

## 20. Smaller items

- `repo-focus-dark-desktop.png`: the focus ring on "Code" is clear and 2 px, but it overlaps the active-tab underline and is clipped at the bottom by `.tabs a { margin-bottom: -1px }`; add `outline-offset: -2px` for `.tabs a:focus-visible`.
- `not-found-dark-phone.png`: the "404" heading is dim grey (reads as disabled) and there is no repo or page context; use `color: var(--text)` and link to the repo when the missing page is inside one.
- Clone box on phone (`repo-light-phone.png`) truncates the command after 40 characters with no copy button; add a visible "Copy" `.btn` (the `y` key is not discoverable) and allow the input to scroll.
- The code `--` ligature in `releases-light-phone.png` ("verify --tag") merges the two dashes in the mono stack; add `font-variant-ligatures: none` to `code, pre, .mono`.
- `blob-wide-light-phone.png`: a 300-character unbroken line wraps at phone width instead of scrolling; acceptable, but the wrap hides that it is one line; consider `overflow-x: auto` plus `white-space: pre` for code.
- No skip-to-content link exists (`<a class="skip" href="#main">` before the header would save about 12 tab stops per page).

## Not covered

Keyboard cursor row (`j`/`k`), theme toggle states, split diff, error states other than 404, empty repo list, equivocation and pending verification states, and a repo with hundreds of commits (pager) were not captured; the fixture has none of them.
