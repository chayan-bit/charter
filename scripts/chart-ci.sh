#!/usr/bin/env bash
# CI runner on Arkiv (docs/p3/arkiv-ci.md). Tests every patch head that is not in main and
# has no attestation from this runner yet, in a clean detached worktree, and posts a signed
# xyz.arkiv.attest `ci` op {head, status, suite, log_sha256, duration_s}.
# An interrupted or signal-killed run posts nothing, so a pass is only ever posted for a suite that exited 0.
# Usage: scripts/chart-ci.sh --repo <arkiv-url> [--once]
# Env: CHARTER_HOME  runner identity (signs the attestations, pays the gas)
#      CHARTER_CI_CMD    suite, run with sh -c (default: the cargo steps of .github/workflows/ci.yml)
#      CHARTER_CI_SUITE  suite name in the payload (default: the command)
#      CHARTER_CI_DIR    cache: git repo, logs, cargo target (default: ~/.cache/charter-ci,
#                        or ~/.cache/arkiv-ci if only that pre-rename cache exists)
#      CHARTER_CI_INTERVAL  seconds between rounds without --once (default: 60)
set -euo pipefail
# The cache holds clones of possibly private repos and their test logs: owner only.
umask 077
# shellcheck source=scripts/chart-lib.sh
. "$(dirname "$0")/chart-lib.sh"

usage() { die "usage: chart-ci.sh --repo <arkiv-url> [--once]"; }
url="" once=""
while [ $# -gt 0 ]; do
  case $1 in
    --repo) [ $# -ge 2 ] || usage; url=$2; shift 2 ;;
    --once) once=1; shift ;;
    *) usage ;;
  esac
done
[ -n "$url" ] || usage

steps='cargo fmt --all --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --all-features'
if command -v mac-heavy >/dev/null; then steps="mac-heavy sh -c '$steps'"; fi
# Pre-rename ARKIV_CI_* names still work; the CHARTER_CI_* name wins.
cmd=${CHARTER_CI_CMD:-${ARKIV_CI_CMD:-$steps}}
suite=${CHARTER_CI_SUITE:-${ARKIV_CI_SUITE:-$cmd}}
cache=$HOME/.cache/charter-ci
[ ! -d "$cache" ] && [ -d "$HOME/.cache/arkiv-ci" ] && cache=$HOME/.cache/arkiv-ci
dir=${CHARTER_CI_DIR:-${ARKIV_CI_DIR:-$cache}}/$(printf %s "$url" | tr -c 'A-Za-z0-9' _)
repo=$dir/repo
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$dir/target}
mkdir -p "$dir/logs"
[ -d "$repo/.git" ] || git init -q "$repo"
git -C "$repo" worktree prune

wt=""
cleanup() { if [ -n "$wt" ]; then git -C "$repo" worktree remove --force "$wt" || true; fi; }
trap cleanup EXIT
trap 'echo "chart-ci: interrupted, no attestation posted" >&2; exit 130' INT TERM HUP

me=$(my_nid)
[ -n "$me" ] || die "no identity in CHARTER_HOME; run chart keys init"
attest_obj="" main=""

# run_one <patch> <author> <head>: every step before the suite must succeed, so a broken
# checkout can never be reported as a pass (or a fail).
run_one() {
  local author=$2 head=$3 log rc start status payload
  # Merged heads arrive with main, so they are skipped without fetching their branch.
  if [ -n "$main" ] && git -C "$repo" merge-base --is-ancestor "$head" "$main" 2>/dev/null; then
    echo "chart-ci: skip $head, already in main"
    return 0
  fi
  (cd "$repo" && fetch_head "$url" "$author" "$head") || { echo "chart-ci: cannot fetch $head" >&2; return 1; }
  echo "chart-ci: patch $1 head $head"
  wt=$(mktemp -d "$dir/wt.XXXXXX") || return 1
  git -C "$repo" worktree add -q --detach --force "$wt" "$head" || return 1
  [ "$(git -C "$wt" rev-parse HEAD)" = "$head" ] || return 1
  log=$dir/logs/$head.log
  start=$(date +%s)
  rc=0
  (cd "$wt" && sh -c "$cmd") >"$log" 2>&1 </dev/null || rc=$?
  cleanup
  wt=""
  if [ "$rc" -gt 128 ]; then
    echo "chart-ci: suite killed by signal $((rc - 128)), no attestation" >&2
    return 1
  fi
  status=pass
  [ "$rc" -eq 0 ] || status=fail
  payload=$(jq -cn --arg h "$head" --arg s "$status" --arg suite "$suite" \
    --arg l "$(sha256 "$log")" --argjson d "$(($(date +%s) - start))" \
    '{head: $h, status: $s, suite: $suite, log_sha256: $l, duration_s: $d}')
  attest_object || return 1
  chart op post --repo "$url" --type xyz.arkiv.attest --action ci --object "$attest_obj" \
    --parent "$attest_obj" --payload-json "$payload" >/dev/null || return 1
  echo "chart-ci: $status $head (log $log)"
}

# One pass over the open patch heads; returns 1 if any head could not be attested.
round() {
  local done_heads p author head failed=0
  ops=$(ops_json "$url") || return 1
  main=$(cd "$repo" && git fetch -q "$url" main && git rev-parse FETCH_HEAD) || main=""
  done_heads=$(jq -r --arg me "$me" '.[] | select(.type == "xyz.arkiv.attest"
    and .action == "ci" and .author == $me) | .payload.head' <<<"$ops") || return 1
  while read -r -u 3 p author head; do
    grep -qxF "$head" <<<"$done_heads" && continue
    run_one "$p" "$author" "$head" || failed=1
  done 3< <(patch_heads <<<"$ops")
  return "$failed"
}

if [ -n "$once" ]; then
  round
  exit
fi
while :; do
  round || echo "chart-ci: round incomplete, retrying next round" >&2
  sleep "${CHARTER_CI_INTERVAL:-${ARKIV_CI_INTERVAL:-60}}"
done
