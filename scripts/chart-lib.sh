# shellcheck shell=bash
# Shared helpers for the chart-*.sh scripts (docs/p3/arkiv-ci.md). Needs chart, git, jq.

die() {
  echo "$(basename "$0"): $*" >&2
  exit 1
}

for tool in chart git jq; do
  command -v "$tool" >/dev/null || die "$tool not found on PATH"
done

# A native jq.exe under Git Bash writes CRLF, and the \r would end every id it prints.
case $OSTYPE in msys* | cygwin*) jq() { command jq -b "$@"; } ;; esac

ops_json() { chart op list --repo "$1" --json; }

my_nid() { chart keys show | sed -n 's/^nid:[[:space:]]*//p'; }

# Reads `op list --json`; prints "patch author head" per patch with the author's latest
# create/revise head. Ids and oids are validated, since they reach git command lines.
patch_heads() {
  jq -r '
    reduce (.[] | select(.type == "xyz.arkiv.patch")) as $o ({};
      if $o.action == "create" then .[$o.object] = {author: $o.author, head: $o.payload.head}
      elif $o.action == "revise" and .[$o.object].author == $o.author
      then .[$o.object].head = $o.payload.head
      else . end)
    | to_entries[]
    | select((.value.head | type) == "string"
        and (.value.head | test("^([0-9a-f]{40}|[0-9a-f]{64})$"))
        and (.value.author | test("^z[1-9A-HJ-NP-Za-km-z]+$")))
    | "\(.key) \(.value.author) \(.value.head)"'
}

# fetch_head <url> <author nid> <head>: fetches the author's branches into the current
# repo and checks that the head commit arrived.
fetch_head() {
  git fetch -q "$1/$2" "+refs/heads/*:refs/arkiv/$2/*" && git cat-file -e "$3^{commit}"
}

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1"; else shasum -a 256 "$1"; fi | cut -d' ' -f1; }

# This attester's xyz.arkiv.attest object, created on first use; needs $url, $me, $ops and
# sets $attest_obj.
# shellcheck disable=SC2154 # globals of the sourcing script
attest_object() {
  if [ -z "$attest_obj" ]; then
    attest_obj=$(jq -r --arg me "$me" 'map(select(.type == "xyz.arkiv.attest"
      and .action == "create" and .author == $me)) | first | .id // empty' <<<"$ops")
  fi
  if [ -z "$attest_obj" ]; then
    # The payload label is on-chain data in existing attest objects, kept unchanged by the Charter rename.
    attest_obj=$(chart op post --repo "$url" --type xyz.arkiv.attest --action create \
      --payload-json '{"runner":"arkiv-ci"}') || return 1
  fi
}
