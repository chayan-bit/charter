#!/usr/bin/env bash
# Pushes a branch to your own namespace of an Arkiv repo and posts its xyz.arkiv.patch op,
# or revises the op when the branch head moved. Prints the patch id. Run inside a clone.
# Usage: scripts/chart-patch.sh <arkiv-url> <branch> [title [body-file]]
# The optional body file (Markdown) becomes the patch description.
set -euo pipefail
# shellcheck source=scripts/chart-lib.sh
. "$(dirname "$0")/chart-lib.sh"

url=${1:?usage: chart-patch.sh <arkiv-url> <branch> [title]}
branch=${2:?usage: chart-patch.sh <arkiv-url> <branch> [title]}
head=$(git rev-parse --verify "$branch^{commit}")
title=${3:-$(git log -1 --format=%s "$head")}
body=${4:+$(cat "$4")}

git push -q "$url" "$branch:refs/heads/$branch"
me=$(my_nid)
ops=$(ops_json "$url")
patch=$(jq -r --arg me "$me" --arg b "$branch" 'map(select(.type == "xyz.arkiv.patch"
  and .action == "create" and .author == $me and .payload.branch == $b)) | last | .id // empty' \
  <<<"$ops")
payload=$(jq -cn --arg t "$title" --arg h "$head" --arg b "$branch" --arg d "$body" \
  '{title: $t, head: $h, branch: $b} + (if $d == "" then {} else {body: $d} end)')
if [ -z "$patch" ]; then
  patch=$(chart op post --repo "$url" --type xyz.arkiv.patch --action create \
    --payload-json "$payload")
elif ! patch_heads <<<"$ops" | grep -qxF "$patch $me $head"; then
  chart op post --repo "$url" --type xyz.arkiv.patch --action revise --object "$patch" \
    --parent "$patch" --payload-json "$payload" >/dev/null
fi
echo "$patch"
