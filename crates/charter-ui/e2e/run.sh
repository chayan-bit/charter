#!/usr/bin/env bash
# E2E + accessibility harness for `chart ui`.
#   ./run.sh                 build, make a fixture, start `chart ui`, run e2e.py, stop the server
# Env: E2E_OUT=<dir> write screenshots and reports there (default e2e/out); NO_BUILD=1 reuse target/debug; KEEP=1 keep the temp fixture dir; E2E_WORK=<dir> use (and keep) a fixture dir;
#      REUSE_FIXTURE=1 with E2E_WORK skip fixture creation; CHART_BIN_DIR=<dir> binaries to use.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../../.." && pwd)"

# Python Playwright comes from nix only (no global installs).
if ! python3 -c 'import playwright' 2>/dev/null; then
  exec nix-shell -p python3Packages.playwright --run "$(printf '%q ' "$0" "$@")"
fi

if [[ -z "${NO_BUILD:-}" && -z "${CHART_BIN_DIR:-}" ]]; then
  heavy=(); command -v mac-heavy >/dev/null && heavy=(mac-heavy)
  (cd "$repo_root" && "${heavy[@]}" cargo build -p charter-cli)
fi

keep="${KEEP:-}"
if [[ -n "${E2E_WORK:-}" ]]; then work="$E2E_WORK"; keep=1; else work="$(mktemp -d "${TMPDIR:-/tmp}/charter-ui-e2e.XXXXXX")"; fi
mkdir -p "$work"
server_pid=""
cleanup() {
  [[ -n "$server_pid" ]] && kill "$server_pid" 2>/dev/null || true
  [[ -z "$keep" && -d "$work" ]] && rm -rf -- "$work"
}
trap cleanup EXIT

if [[ -z "${REUSE_FIXTURE:-}" || ! -f "$work/fx/env.sh" ]]; then
  rm -rf -- "$work/fx"
  "$here/fixture.sh" "$work/fx"
fi
# shellcheck disable=SC1091
source "$work/fx/env.sh"
export PATH="$CHART_BIN_DIR:$PATH"

now() { if [[ -n "${EPOCHREALTIME:-}" ]]; then echo "${EPOCHREALTIME/,/.}"; else python3 -c 'import time; print(time.time())'; fi; }
out="${E2E_OUT:-$here/out}"
mkdir -p "$out"; rm -f "$out/first-page.json"
t0="$(now)"
# Slow receipt polls so the e2e sees a write in its sent state before the fake mines it.
CHARTER_MEM_POLL_MS=1500 chart ui "$RID" >"$work/server.out" 2>"$work/server.err" &
server_pid=$!
for _ in $(seq 1 1000); do
  [[ -s "$work/server.out" ]] && break
  kill -0 "$server_pid" 2>/dev/null || { echo "chart ui exited early:"; cat "$work/server.err"; exit 1; }
  sleep 0.01
done
url="$(head -n1 "$work/server.out")"
[[ "$url" == http://127.0.0.1:*/\?token=* ]] || { echo "unexpected URL line: $url"; exit 1; }

status=0
export E2E_URL="$url" E2E_T0="$t0" E2E_OUT="$out" CHART_BIN_DIR
python3 "$here/e2e.py" first-page || true   # time to the first verified repo page, before any browser starts
E2E_URL="$url" E2E_T0="$t0" E2E_OUT="$out" E2E_WORK="$work" \
  python3 "$here/e2e.py" || status=$?
echo "server stderr (tail):"; tail -n 5 "$work/server.err" || true
exit "$status"
