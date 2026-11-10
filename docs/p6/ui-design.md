# P6 UI design tokens and component sketches

Status: PROPOSAL for `chart ui`, companion to `docs/p6/ui.md` sections 1, 4.3, 5 and 6.
Date: 2026-11-10.
Design read: developer tool for engineers who live in git, dense and keyboard-driven and calm, sourcehut speed with Linear-level polish, not a GitHub clone.
Dials (design-taste-frontend): variance 4, motion 2, density 8.
The skill targets landing pages, so only its token discipline, contrast checks, one-accent rule, shape lock, dark-mode and reduced-motion rules are applied here.

## 1. Principles

- Verification is the product: the verification panel is the first thing under the repo header on every repo page, and every fact in it links to its block.
- Density over decoration: rows and hairlines, not cards; a table row is 28 px, a list row 32 px.
- Mono carries meaning: hashes, refs, block numbers, signatures and code are mono; prose and labels are sans.
- One accent, four semantic hues: blue is interaction only, green is verified, amber is pending, red is equivocation; no color is ever the only signal (each state also has a word or glyph).
- Zero cost to render: system font stacks, one CSS file, no images, no icon font, inline SVG limited to the state glyphs; target about 6 KB gzipped CSS.
- Keyboard first, mouse fine: every list and tab is reachable with single keys, focus is always visible.
- Calm motion: state changes are instant or under 120 ms, and nothing animates under `prefers-reduced-motion`.

## 2. Color tokens and contrast

Neutrals are slightly warm grays (one palette in both themes), never pure black or white.
Ratios are WCAG 2.x relative-luminance contrast, computed with a python3 script over the exact hex values in section 9.
Body text needs 4.5:1; borders of interactive controls, focus rings and the state glyph need 3:1.

| Token | Light | Dark | Role |
|---|---|---|---|
| `--bg` | #fafaf9 | #121211 | page |
| `--s1` | #ffffff | #191918 | panels, table body |
| `--s2` | #f4f4f2 | #201f1e | table header, hover, code blocks |
| `--s3` | #ebebe8 | #2a2928 | pressed, selected row |
| `--border` | #d4d4cf | #3a3937 | decorative hairlines |
| `--border-strong` | #8a8a84 | #85847f | inputs, buttons, focus-adjacent edges |
| `--text` | #1c1c1a | #ecebe8 | primary |
| `--text-2` | #4a4a46 | #b5b4af | secondary |
| `--text-3` | #62625d | #9a9994 | meta, line numbers |
| `--accent` / `--link` / `--focus` | #1f5fbf | #6ea2f0 / #7fb0f5 / #7fb0f5 | interaction |
| `--ok`, `--ok-bg` | #17693a, #e3f4e8 | #58c27d, #12301d | verified |
| `--warn`, `--warn-bg` | #8a5300, #fcefd2 | #e0a23a, #33260d | pending |
| `--bad`, `--bad-bg` | #b3261e, #fde7e5 | #f27b72, #3a1614 | equivocation, errors |

Text and UI pairs (ratio, light / dark):

| Foreground on background | Light | Dark | Needs |
|---|---|---|---|
| text on bg | 16.34 | 15.72 | 4.5 pass |
| text on s1 | 17.07 | 14.76 | 4.5 pass |
| text on s2 | 15.50 | 13.80 | 4.5 pass |
| text-2 on bg | 8.52 | 9.03 | 4.5 pass |
| text-2 on s2 | 8.08 | 7.92 | 4.5 pass |
| text-3 on bg | 5.87 | 6.57 | 4.5 pass |
| text-3 on s1 | 6.13 | 6.16 | 4.5 pass |
| text-3 on s3 (lowest case) | 5.13 | 5.09 | 4.5 pass |
| link on bg | 5.83 | 8.42 | 4.5 pass |
| link on s2 | 5.53 | 7.39 | 4.5 pass |
| on-accent (#fff / #0b1a33) on accent button | 6.09 | 6.69 | 4.5 pass |
| ok on s1 | 6.73 | 7.90 | 4.5 pass |
| ok on ok-bg (badge) | 5.89 | 6.43 | 4.5 pass |
| warn on warn-bg | 5.55 | 6.60 | 4.5 pass |
| bad on bad-bg | 5.52 | 6.02 | 4.5 pass |
| bad on s1 | 6.54 | 6.58 | 4.5 pass |
| border-strong on bg | 3.32 | 5.00 | 3 pass |
| border-strong on s1 | 3.47 | 4.70 | 3 pass |
| focus ring on bg | 5.83 | 8.42 | 3 pass |
| focus ring on s1 | 6.09 | 7.90 | 3 pass |
| `--border` on bg (decorative only) | 1.42 | 1.62 | not used for controls |

Diff colors (code text is `--text`):

| Pair | Light | Dark |
|---|---|---|
| text on added line bg (#e6f6e9 / #13291a) | 15.54 | 12.95 |
| text on added gutter (#c8ebd0 / #1b3d27) | 13.49 | 10.10 |
| text on removed line bg (#fdebea / #341715) | 15.15 | 13.77 |
| text on removed gutter (#f7cfcc / #4a1f1c) | 12.24 | 11.71 |
| text-2 line number on added gutter | 6.89 | 5.80 |
| text-2 line number on removed gutter | 6.25 | 6.72 |

Syntax palette, eight roles, ratio on `--s1` / on added-line bg / on removed-line bg (the worst case among the diff backgrounds is the one that must pass):

| Role | Light hex | Light ratios | Dark hex | Dark ratios |
|---|---|---|---|---|
| keyword | #a1307a | 6.52 / 5.82 / 5.67 | #e27cc0 | 6.60 / 5.79 / 6.16 |
| string | #1b6b3a | 6.54 / 5.83 / 5.68 | #74c98f | 8.79 / 7.72 / 8.20 |
| number | #8a5300 | 6.33 / n/a | #e0a23a | 7.87 / n/a |
| comment | #5f5f5a | 6.42 / 5.72 / 5.58 | #9a9994 | 6.16 / 5.41 / 5.75 |
| function | #1f5fbf | 6.09 / 5.43 / 5.29 | #7fb0f5 | 7.90 / 6.94 / 7.37 |
| type | #0e6b72 | 6.24 / n/a | #5cc5cc | 8.64 / n/a |
| operator | #4a4a46 | 8.90 / n/a | #b5b4af | 8.47 / n/a |
| attribute | #8f3d1a | 7.37 / n/a | #e5906a | 7.15 / n/a |

All text pairs pass AA; the lowest text ratio is 5.09 and the lowest UI ratio is 3.32.
Keyword magenta is a syntax role only and never used for UI chrome, so the one-accent rule holds.

## 3. Type scale

- Sans stack: `system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif`.
- Mono stack: `ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, "Liberation Mono", monospace`.
- No webfont is shipped: zero bytes, no CSP surface, no layout shift; this is the default decision.
- If the user later wants a branded mono, the candidate is JetBrains Mono (SIL OFL 1.1) as a single regular-weight woff2 served from the binary; its file size is not checked here and must be measured against the 20 KB budget before adopting it.
- Sizes: 11 px label (uppercase only for table headers), 12.5 px mono, 13 px dense (tables, lists, meta), 14 px body and base, 16 px page title, 20 px repo title; line heights 1.45 for prose, 1.35 for rows, 1.5 for code.
- Weights: 400 body, 500 labels and row titles, 600 headings; headings use tracking -0.01em, never above 20 px.
- Numbers in tables use `font-variant-numeric: tabular-nums`.
- Hashes show 7 characters in mono with the full value in a `title` and one-key copy.

## 4. Spacing, radius, shadow, density

- Spacing scale on a 4 px base: 4, 8, 12, 16, 24, 32 (`--sp-1` to `--sp-6`); page gutter 16 px, content max width 1100 px, left aligned, never centered hero.
- Radius: one rule, 4 px for controls, rows highlights and badges, 6 px for panels, dialogs and toasts; no pills and no other radii.
- Shadows: none on rows or panels; only dialog and toast get `0 8px 24px rgb(20 20 18 / 0.16)` (tinted to the warm neutral, stronger alpha in dark).
- Tables: 28 px rows, 8 px cell padding, header 11 px uppercase `--text-3` on `--s2`, one hairline under each row only in the file tree and commit list (short, scannable); issue and patch lists use hover tint instead of lines when over 12 rows.
- Lists: 32 px rows with two lines max (title, then meta in `--text-3`); no cards, no avatars, a 16 px author glyph at most.
- Hover is `--s2`, selected (keyboard cursor) is `--s3` with a 2 px `--accent` bar on the left edge.

## 5. Component sketches

- Top bar (40 px, `--s1`, bottom hairline): left is `charter` wordmark then breadcrumb `owner / repo / tree / main / src` with each segment a link and the ref in mono; right is a `?` keyboard hint button, the local node state (`synced 12 s ago`), and a theme toggle (system, light, dark).
- Repo nav tabs: Code, Commits, Issues (count), Patches (count), Releases, Program, Verification; text tabs on a 1 px baseline with a 2 px accent underline for the current one, counts in mono `--text-3`; `g` shortcuts mirror them.
- File tree table: columns name (folder glyph, then name; folders first), last commit message (truncated, `--text-2`), age, with the path in the breadcrumb; the row under the keyboard cursor opens on `Enter`; README renders below the table.
- Blob view: header row with path, size, line count, Raw and Copy; body is a two-column grid with right-aligned line numbers in `--text-3` (selectable off), 12.5 px mono, `#L12-L18` anchors highlight with `--warn-bg`; plain text beyond 2,000 lines with a "show all" link per section 6 of the plan.
- Commit list rows: first line is the subject (500 weight), second is `author, age` plus a mono short hash on the right with a small verification glyph (signature checked, unsigned, bad); rows grouped by day with a quiet date header.
- Diff view: per-file header (path, +/- counts, collapse toggle on `x`); unified by default, split behind a toggle; gutters carry both line numbers with the colors in section 2; hunk headers are `--s2` mono; intra-line changes are not highlighted in slice 1.
- Issue and patch list rows: state badge, title, then meta (`#14 opened by a1b2c3d 3 d ago, 4 comments`); badges are 18 px tall, 4 px radius, tinted bg with colored text and a word (open, closed, merged, draft); patches add a mono `rev 3` and a proof chip when a proof is attached.
- Verification panel: a bordered `--s1` panel under the repo header, collapsed to one summary line with a state word and a glyph, expandable with `v`; expanded it is a table, one row per fact.
  - verified (green): identity chain length and delegates, each peer's accepted head (mono) and anchor block (mono, link), ML-DSA status `valid`, last sync time.
  - pending (amber): head seen but anchor block not yet final or signature check still running; shows how many blocks remain.
  - unverified (neutral, `--text-2`): no anchor found for a peer or the cache is stale beyond a threshold; says why in one sentence and offers `chart sync`.
  - equivocation (red): the summary line turns red with the word "Equivocation", the panel is open by default, and both conflicting heads are listed side by side with their block links; this state is never collapsible away on first view.
- Cost-confirmation dialog for writes: modal, 6 px radius, title "Confirm write", then a two-column list (action, repo, entity count, estimated cost `0.0042 tGLM` in mono, balance after); primary button reads `Send 0.0042 tGLM`, secondary `Cancel`; `Enter` is bound to cancel by default and the primary needs an explicit `Cmd+Enter`, so a stray key never spends.
- Toast: bottom-right, 360 px max, `--s1` with a 3 px left bar in state color; two stages in one element, "Sent" with the tx hash then "Confirmed in block 1,284,771" (block link, mono); auto-hides after 6 s, hovering or focusing pauses it, errors stay until dismissed; announced via `role="status"`.

## 6. Keyboard map

| Keys | Action |
|---|---|
| `g h` | home (repo list) |
| `g c` | code, `g l` log, `g i` issues, `g p` patches, `g r` releases, `g v` verification |
| `j` / `k` | move cursor down / up in any list or table |
| `Enter` / `o` | open the row under the cursor |
| `u` | up one directory or back to the list |
| `/` | focus the filter box of the current list |
| `y` | copy the hash or path under the cursor |
| `v` | toggle the verification panel |
| `x` | collapse or expand the current diff file |
| `Esc` | blur input, close dialog or help |
| `?` | show this map |

File finder (`t`) is out of scope.
Keys are ignored inside inputs and with Ctrl, Alt or Meta held; implemented as one small deferred script (htmx stays the only dependency), and every action also exists as a plain link so the no-JS read path keeps working.

## 7. Focus and motion

- Focus ring: 2 px solid `--focus`, 2 px offset, on every focusable element via `:focus-visible`; never removed, and it passes 3:1 on all surfaces.
- Keyboard cursor and focus are separate: the cursor tints the row, focus rings the control.
- Motion budget: opacity and transform only, 80 to 120 ms ease-out for dialog and toast entry, nothing on page load, no skeletons beyond a static "loading" row (pages are server rendered in under 50 ms).
- `@media (prefers-reduced-motion: reduce)` sets all `transition` and `animation` durations to 0.01 ms; the toast then simply appears.
- State is never color only: glyph shapes differ (check, clock, dash, triangle) and each carries its word.

## 8. Themes and CSP notes

- Dark mode follows `prefers-color-scheme`, and `data-theme="light"` or `"dark"` on `<html>` overrides it; the toggle writes the attribute with the same deferred script and a cookie so the server can render the right attribute without a flash.
- `color-scheme: light dark` is set so native controls and scrollbars match.
- CSP has `default-src 'self'` and no `style-src 'unsafe-inline'`, so the critical CSS cannot be an inline `<style>` unless it is hashed; the proposal is one external stylesheet with a long cache lifetime and `Link: rel=preload`, which keeps pages within the 20 KB gzipped budget on repeat views and about 6 KB on the first.
- State glyphs are inline SVG paths with `currentColor`; no third-party icon set.

## 9. Ready-to-paste CSS tokens

```css
:root {
  color-scheme: light dark;
  --bg: #fafaf9; --s1: #ffffff; --s2: #f4f4f2; --s3: #ebebe8;
  --border: #d4d4cf; --border-strong: #8a8a84;
  --text: #1c1c1a; --text-2: #4a4a46; --text-3: #62625d;
  --accent: #1f5fbf; --on-accent: #ffffff; --link: #1f5fbf; --focus: #1f5fbf;
  --ok: #17693a; --ok-bg: #e3f4e8;
  --warn: #8a5300; --warn-bg: #fcefd2;
  --bad: #b3261e; --bad-bg: #fde7e5;
  --add-bg: #e6f6e9; --add-gutter: #c8ebd0; --del-bg: #fdebea; --del-gutter: #f7cfcc;
  --sy-keyword: #a1307a; --sy-string: #1b6b3a; --sy-number: #8a5300; --sy-comment: #5f5f5a;
  --sy-function: #1f5fbf; --sy-type: #0e6b72; --sy-operator: #4a4a46; --sy-attribute: #8f3d1a;
  --shadow: 0 8px 24px rgb(20 20 18 / 0.16);
  --font-sans: system-ui, -apple-system, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
  --font-mono: ui-monospace, "SF Mono", "JetBrains Mono", Menlo, Consolas, "Liberation Mono", monospace;
  --fs-label: 11px; --fs-mono: 12.5px; --fs-dense: 13px; --fs-base: 14px; --fs-title: 16px; --fs-repo: 20px;
  --sp-1: 4px; --sp-2: 8px; --sp-3: 12px; --sp-4: 16px; --sp-5: 24px; --sp-6: 32px;
  --radius: 4px; --radius-lg: 6px;
}

@media (prefers-color-scheme: dark) {
  :root:not([data-theme="light"]) {
    --bg: #121211; --s1: #191918; --s2: #201f1e; --s3: #2a2928;
    --border: #3a3937; --border-strong: #85847f;
    --text: #ecebe8; --text-2: #b5b4af; --text-3: #9a9994;
    --accent: #6ea2f0; --on-accent: #0b1a33; --link: #7fb0f5; --focus: #7fb0f5;
    --ok: #58c27d; --ok-bg: #12301d;
    --warn: #e0a23a; --warn-bg: #33260d;
    --bad: #f27b72; --bad-bg: #3a1614;
    --add-bg: #13291a; --add-gutter: #1b3d27; --del-bg: #341715; --del-gutter: #4a1f1c;
    --sy-keyword: #e27cc0; --sy-string: #74c98f; --sy-number: #e0a23a; --sy-comment: #9a9994;
    --sy-function: #7fb0f5; --sy-type: #5cc5cc; --sy-operator: #b5b4af; --sy-attribute: #e5906a;
    --shadow: 0 8px 24px rgb(0 0 0 / 0.5);
  }
}

:root[data-theme="dark"] {
  color-scheme: dark;
  --bg: #121211; --s1: #191918; --s2: #201f1e; --s3: #2a2928;
  --border: #3a3937; --border-strong: #85847f;
  --text: #ecebe8; --text-2: #b5b4af; --text-3: #9a9994;
  --accent: #6ea2f0; --on-accent: #0b1a33; --link: #7fb0f5; --focus: #7fb0f5;
  --ok: #58c27d; --ok-bg: #12301d;
  --warn: #e0a23a; --warn-bg: #33260d;
  --bad: #f27b72; --bad-bg: #3a1614;
  --add-bg: #13291a; --add-gutter: #1b3d27; --del-bg: #341715; --del-gutter: #4a1f1c;
  --sy-keyword: #e27cc0; --sy-string: #74c98f; --sy-number: #e0a23a; --sy-comment: #9a9994;
  --sy-function: #7fb0f5; --sy-type: #5cc5cc; --sy-operator: #b5b4af; --sy-attribute: #e5906a;
  --shadow: 0 8px 24px rgb(0 0 0 / 0.5);
}

:root[data-theme="light"] { color-scheme: light; }

:focus-visible { outline: 2px solid var(--focus); outline-offset: 2px; }

@media (prefers-reduced-motion: reduce) {
  *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; }
}
```

## 10. Not yet checked

- Rendered appearance: no page has been built from these tokens, so density numbers (28 px rows, 6 KB CSS) are estimates, not measurements.
- Contrast was computed for the listed pairs only; any new pairing (for example text on a hovered `--s2` badge) must be added to the script before use.
- Intra-line diff highlight and a split diff layout are deferred; the JetBrains Mono file size is unmeasured.
