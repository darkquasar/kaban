#!/usr/bin/env bash
# Dependency allowlist for the v4 crates (stage1-spec G1-E3; cg-join-v1 §2 "Dependency edges";
# stage1-plan CG2.4; GM2a RG2-7, RG2-8). Exit 0 when no forbidden edge exists, 1 naming every
# forbidden edge found, and 1 whenever the dependency evidence could not be obtained (`cargo
# metadata` failing, malformed or empty metadata): the gate never reports "ok" without evidence.
#
# Edges come from one `cargo metadata` call, with their kinds read explicitly from
# `resolve.nodes[].deps[].dep_kinds[].kind` (`null` = normal, "build", "dev"). A rule names the
# kinds admitted at the root package; the closure then follows normal and build edges of the
# dependencies (a dependency's own dev-dependencies are never built, as `cargo tree` treats them).
#
#   kaban-codegraph    must not list kaban-policy, kaban-store-sqlite, kaban-api, scip   (any kind)
#   kaban-index        must not list kaban-store-sqlite, kaban-api                      (normal, build)
#   kaban-store-sqlite must not list kaban-codegraph                                    (normal, build;
#                                                                                        the dev edge is allowed, SO-3)
#   kaban-core         must not list kaban-codegraph                                    (any kind)
#
# Every rule applies only to workspace members present at run time; a member not yet present
# prints "skipped: <member> not a workspace member yet" (informational, not a failure), but only
# after the metadata itself was validated, so an absent member is never the result of a failed
# command.
#
# Usage: bash scripts/ci/deps_allowlist.sh   (CARGO=<path> substitutes the cargo executable)
set -euo pipefail
cd "$(dirname "$0")/../.."

CARGO="${CARGO:-cargo}"
fail=0

metadata=$("$CARGO" metadata --format-version 1) || {
  echo "FAILED: cargo metadata did not succeed (no dependency evidence)" >&2
  exit 1
}

# the metadata is a JSON object with the three tables the rules read, each a non-empty array
if ! printf '%s' "$metadata" | jq -e '
      type == "object"
      and (.packages | type == "array" and length > 0)
      and (.workspace_members | type == "array" and length > 0)
      and (.resolve | type == "object")
      and (.resolve.nodes | type == "array" and length > 0)
    ' > /dev/null 2>&1; then
  echo "FAILED: cargo metadata output is malformed or empty (no dependency evidence)" >&2
  exit 1
fi

# workspace member names, one per line
members=$(printf '%s' "$metadata" | jq -r '
  (.packages | map({key: .id, value: .name}) | from_entries) as $name
  | .workspace_members[] | $name[.]')

has_member() {
  printf '%s\n' "$members" | grep -qx "$1"
}

# names of every package reachable from workspace member <pkg> through edges whose kind is in
# <kinds> at the root and normal/build below it (the root excluded), one per line
closure_names() {
  local kinds=$1 pkg=$2
  printf '%s' "$metadata" | jq -r --arg root "$pkg" --arg kinds "$kinds" '
    (.packages | map({key: .id, value: .name}) | from_entries) as $name
    | (.resolve.nodes | map({key: .id, value: .deps}) | from_entries) as $deps
    | ($kinds | split(",")) as $k
    | [.workspace_members[] | select($name[.] == $root)] as $roots
    | if ($roots | length) != 1 then error("root package not found once: " + $root) else . end
    | def kind_ok($dk; $root_level):
        ($dk.kind // "normal") as $kk
        | (($k | index($kk)) != null) and ($root_level or $kk != "dev");
    def step($state; $root_level):
        ([ $state.frontier[] as $f
           | ($deps[$f] // [])[]
           | select(any(.dep_kinds[]; kind_ok(.; $root_level)))
           | .pkg ]
         | unique
         | map(select(. as $p | ($state.seen | index($p)) == null))) as $new
        | {seen: ($state.seen + $new), frontier: $new};
    step({seen: $roots, frontier: $roots}; true)
    | until(.frontier == []; step(.; false))
    | .seen[] | select(. as $p | ($roots | index($p)) == null) | $name[.]'
}

# forbid <edge kinds at the root> <pkg> <forbidden>...
forbid() {
  local kinds=$1 pkg=$2
  shift 2
  if ! has_member "$pkg"; then
    echo "skipped: $pkg not a workspace member yet"
    return 0
  fi
  local names
  names=$(closure_names "$kinds" "$pkg") || {
    echo "FAILED: dependency closure of $pkg ($kinds) could not be computed" >&2
    return 1
  }
  local f hit=0
  for f in "$@"; do
    if printf '%s\n' "$names" | grep -qx "$f"; then
      echo "FAILED: forbidden edge $pkg -> $f ($kinds)" >&2
      fail=1
      hit=1
    fi
  done
  [ $hit -eq 0 ] && echo "deps: $pkg ($kinds) lists none of: $*"
  return 0
}

forbid normal,build,dev kaban-codegraph kaban-policy kaban-store-sqlite kaban-api scip || exit 1
forbid normal,build kaban-index kaban-store-sqlite kaban-api || exit 1
forbid normal,build kaban-store-sqlite kaban-codegraph || exit 1
forbid normal,build,dev kaban-core kaban-codegraph || exit 1

# informational: the one admitted edge of the store to the code graph is dev-only (SO-3)
if has_member kaban-store-sqlite && closure_names dev kaban-store-sqlite | grep -qx kaban-codegraph; then
  echo "deps: kaban-store-sqlite -> kaban-codegraph is a dev edge (allowed, SO-3)"
fi

if [ $fail -ne 0 ]; then
  echo "FAILED: dependency allowlist (G1-E3, cg-join-v1 §2)" >&2
  exit 1
fi
echo "dependency allowlist: ok"
