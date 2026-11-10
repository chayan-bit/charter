#!/usr/bin/env bash
# Builds a fixture repo on the file-backed fake chain (no network, no funds).
#   fixture.sh <root-dir>      writes <root-dir>/env.sh (CHARTER_HOME, CHARTER_BACKEND, REPO_URL, ...)
# Env: CHART_BIN_DIR (dir with `chart` and `git-remote-arkiv`, default: worktree target/debug).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../../.." && pwd)"
bin="${CHART_BIN_DIR:-$repo_root/target/debug}"
root="$(mkdir -p "${1:?usage: fixture.sh <root-dir>}" && cd "$1" && pwd)"

export PATH="$bin:$PATH"
export HOME="$root/home"
export CHARTER_HOME="$root/home/.charter"
export CHARTER_BACKEND="mem:$root/chain.log"
export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1
export GIT_AUTHOR_NAME=alice GIT_AUTHOR_EMAIL=alice@example.com
export GIT_COMMITTER_NAME=alice GIT_COMMITTER_EMAIL=alice@example.com
mkdir -p "$HOME"

chart keys init >/dev/null
chart fund --dev 1000000 >/dev/null
url="$(chart init --name charter-demo --description 'E2E fixture repo' | sed -n 's/^url: *//p')"
rid="${url#arkiv::}"

work="$root/work"
rm -rf "$work"
git init -q -b main "$work"
g() { git -C "$work" -c commit.gpgsign=false "$@"; }

# Commit 1: README with markdown (incl. a script that must be sanitized), nested dirs.
mkdir -p "$work/src/deep/er/est" "$work/docs"
cat > "$work/README.md" <<'MD'
# Charter demo

A **fixture** repo for the `chart ui` E2E harness, with [a link](https://example.com/docs) and `inline code`.

## Usage

```rust
fn main() {
    println!("hello, charter");
}
```

- one
- two with a [relative link](docs/guide.md)

> A quote with <script>window.__pwned = 1; alert('xss')</script> embedded.

<script>window.__pwned = 2</script>
<img src="/assets/app.css" alt="not an image" onerror="window.__pwned = 3">

![architecture diagram](docs/diagram.png)
MD
printf 'fn main() {\n    println!("<hi> & \\"bye\\"");\n}\n' > "$work/src/main.rs"
printf 'pub fn add(a: i32, b: i32) -> i32 { a + b }\n' > "$work/src/lib.rs"
printf 'deep file\n' > "$work/src/deep/er/est/leaf.txt"
printf '# Guide\n\nSome docs.\n' > "$work/docs/guide.md"
# A 480x160 gradient PNG (stdlib only), referenced from the README with a relative path.
python3 - "$work/docs/diagram.png" <<'PY'
import struct, sys, zlib
w, h = 480, 160
rows = b"".join(b"\0" + bytes(v for x in range(w) for v in (40 + x * 200 // w, 90 + y * 120 // h, 200 - x * 100 // w)) for y in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
open(sys.argv[1], "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
PY
g add -A && g commit -q -m "feat: initial import" -m "The first commit, with a body paragraph that explains why."

# Commit 2: a long file (3,000+ lines, past the 2,000-line highlight cap).
awk 'BEGIN { for (i = 0; i < 3200; i++) printf "let value_%d = compute(%d, \"line %d\"); // generated\n", i, i, i }' > "$work/src/long.rs"
g add -A && g commit -q -m "feat: add a long generated file"

# Commit 3: modify + delete + a very long unbroken line (phone-width stress).
printf 'fn main() {\n    println!("hello");\n}\n' > "$work/src/main.rs"
g rm -q docs/guide.md
printf 'x%.0s' $(seq 1 300) > "$work/src/wide.txt"; printf '\n' >> "$work/src/wide.txt"
g add -A && g commit -q -m "fix: smaller main, drop the guide, add a very long single-line file that wraps badly on narrow screens"
g tag v0.1.0

# Commit 4 on a second branch.
g checkout -q -b feat/side
printf 'side branch\n' > "$work/SIDE.md"
g add -A && g commit -q -m "feat: side branch work"
g checkout -q main
g push -q "$url" main feat/side v0.1.0
head="$(g rev-parse main)"
first="$(g rev-list --max-parents=0 main)"

post() { chart op post --repo "$url" --type "$1" --action "$2" ${3:+--object "$3" --parent "$3"} --payload-json "$4"; }

issue="$(post xyz.arkiv.issue create "" '{"title":"Crash on <b>start</b> with a very long title that should wrap or truncate cleanly on a phone screen","body":"It **crashes**. <script>x()</script>\n\nSteps:\n\n1. run it","bounty":5}')"
post xyz.arkiv.issue comment "$issue" '{"body":"seen it too, on `main`"}' >/dev/null
post xyz.arkiv.issue comment "$issue" '{"body":"a second comment\n\n```\nbacktrace here\n```"}' >/dev/null
issue2="$(post xyz.arkiv.issue create "" '{"title":"Second issue, still open","body":"Nothing yet."}')"
post xyz.arkiv.issue close "$issue" '{"reason":"fixed"}' >/dev/null

patch="$(post xyz.arkiv.patch create "" "{\"title\":\"Fix the crash\",\"head\":\"$head\",\"base\":\"$first\",\"closes\":\"$issue\"}")"
review="$(post xyz.arkiv.review create "" "{\"patch\":\"$patch\"}")"
post xyz.arkiv.review approve "$review" "{\"patch\":\"$patch\",\"head\":\"$head\"}" >/dev/null

# A release of one real artifact.
mkdir -p "$root/dist"; printf 'artifact bytes\n' > "$root/dist/demo.tgz"
chart release publish --repo "$url" --tag v0.1.0 --commit "$head" \
  --artifact "demo.tgz=$root/dist/demo.tgz@x86_64" \
  --artifact "charter-v0.1.0-x86_64-unknown-linux-gnu.tar.gz=$root/dist/demo.tgz@x86_64-unknown-linux-gnu" \
  --location "demo.tgz=https://example.com/demo.tgz" \
  --location "charter-v0.1.0-x86_64-unknown-linux-gnu.tar.gz=https://github.com/example-org/charter/releases/download/v0.1.0/charter-v0.1.0-x86_64-unknown-linux-gnu.tar.gz" \
  --notes "First release" >/dev/null

# Pin a program (bounty.rhai), when the CLI accepts it.
if ! chart program pin --repo "$url" --file "$repo_root/programs/bounty.rhai" >/dev/null 2>"$root/pin.err"; then
  echo "warning: program pin failed, program page will be empty: $(head -c 200 "$root/pin.err")" >&2
fi

# A long ref name pushed after the pin (an accepted ref move, and a ref that must wrap at slashes on a phone).
g checkout -q -b feat/chore/rename-everything-for-the-narrow-layout-stress/with-more-segments-after-it
printf 'long ref\n' > "$work/LONGREF.md"
g add -A && g commit -q -m "feat: a commit on a branch with a very long name"
g push -q "$url" feat/chore/rename-everything-for-the-narrow-layout-stress/with-more-segments-after-it || echo "warning: long-ref push failed" >&2
g checkout -q main

cat > "$root/env.sh" <<ENV
export CHART_BIN_DIR="$bin"
export HOME="$root/home"
export CHARTER_HOME="$root/home/.charter"
export CHARTER_BACKEND="mem:$root/chain.log"
export REPO_URL="$url"
export RID="$rid"
export HEAD_SHA="$head"
export FIRST_SHA="$first"
export ISSUE_ID="$issue"
export PATCH_ID="$patch"
ENV
echo "fixture ready: $root/env.sh (rid $rid)"
