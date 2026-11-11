#!/usr/bin/env bash
# Release artifact attester (docs/p3/releases.md). Reads release <tag> from the pinned
# program's state, downloads each artifact from its first working location, and posts a
# signed xyz.arkiv.attest `artifact` op {tag, commit, name, sha256} only when the sha256 and
# size match the release record. Artifacts this key already attested are skipped.
# It attests bytes, not a rebuild: reproducible rebuilds are the stronger future mode.
# Usage: scripts/chart-release-attest.sh --repo <arkiv-url> --tag <tag>
# Env: CHARTER_HOME  attester identity (listed in the program's attesters(), funded)
set -euo pipefail
# Downloads of a private repo's release artifacts stay owner only.
umask 077
# shellcheck source=scripts/chart-lib.sh
. "$(dirname "$0")/chart-lib.sh"
command -v curl >/dev/null || die "curl not found on PATH"

usage() { die "usage: chart-release-attest.sh --repo <arkiv-url> --tag <tag>"; }
url="" tag=""
while [ $# -gt 0 ]; do
  case $1 in
    --repo) [ $# -ge 2 ] || usage; url=$2; shift 2 ;;
    --tag) [ $# -ge 2 ] || usage; tag=$2; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$url" ] && [ -n "$tag" ] || usage

me=$(my_nid)
[ -n "$me" ] || die "no identity in CHARTER_HOME; run chart keys init"
state=$(chart program status --repo "$url" | sed -n 's/^state: //p')
rel=$(jq -c --arg t "$tag" '.releases[$t] // empty' <<<"$state")
[ -n "$rel" ] || die "no release $tag in the program state"
commit=$(jq -r .commit <<<"$rel")
ops=$(ops_json "$url")
attest_obj=""
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# attest_one <artifact json>: the downloaded file is always $tmp/artifact, never a path
# built from the on-chain name.
attest_one() {
  local name sha size loc got payload
  name=$(jq -r .name <<<"$1") sha=$(jq -r .sha256 <<<"$1") size=$(jq -r .size <<<"$1")
  if jq -e --arg me "$me" --arg t "$tag" --arg c "$commit" --arg n "$name" --arg s "$sha" \
    'any(.[]; .type == "xyz.arkiv.attest" and .action == "artifact" and .author == $me
      and .payload == {tag: $t, commit: $c, name: $n, sha256: $s})' <<<"$ops" >/dev/null; then
    echo "chart-release-attest: already attested $name"
    return 0
  fi
  while read -r loc; do
    # Locations are chain data: only a URL, never something curl would take as an option.
    case $loc in http://* | https://* | file://*) ;; *) continue ;; esac
    rm -f "$tmp/artifact"
    curl -fsSL --proto '=http,https,file' -o "$tmp/artifact" "$loc" || continue
    got=$(sha256 "$tmp/artifact")
    if [ "$got" != "$sha" ] || [ "$(wc -c <"$tmp/artifact" | tr -d ' ')" != "$size" ]; then
      echo "chart-release-attest: MISMATCH $name at $loc (sha256 $got), not attesting" >&2
      return 1
    fi
    payload=$(jq -cn --arg t "$tag" --arg c "$commit" --arg n "$name" --arg s "$sha" \
      '{tag: $t, commit: $c, name: $n, sha256: $s}')
    attest_object || return 1
    chart op post --repo "$url" --type xyz.arkiv.attest --action artifact \
      --object "$attest_obj" --parent "$attest_obj" --payload-json "$payload" >/dev/null || return 1
    echo "chart-release-attest: attested $name $sha from $loc"
    return 0
  done < <(jq -r '.locations[]' <<<"$1")
  echo "chart-release-attest: no location of $name could be downloaded" >&2
  return 1
}

failed=0
while read -r -u 3 art; do
  attest_one "$art" || failed=1
done 3< <(jq -c '.artifacts[]' <<<"$rel")
exit "$failed"
