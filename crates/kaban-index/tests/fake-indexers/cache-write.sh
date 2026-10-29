#!/bin/sh
# The shared-cache fake Go indexer (CG4.4 `caches_shared_ro_in_namespaces`,
# `ns_indexer_succeeds_with_ro_cache`, `caches_dedicated_when_configured`): prints the cache
# variables, reads `seed.txt` from `$GOMODCACHE` (an un-vendored repository reads its module
# cache; `-mod=readonly GOPROXY=off` never writes it), then tries one write into the cache and
# reports whether it landed (overlayfs) or failed (plain read-only bind). Exits 0 when the read
# worked: the write outcome is reported, not required.
echo "GOMODCACHE=$GOMODCACHE"
echo "GOCACHE=$GOCACHE"
echo "GOFLAGS=$GOFLAGS GOPROXY=$GOPROXY"
echo "HOME=$HOME TMPDIR=$TMPDIR"
if [ -f "$GOMODCACHE/seed.txt" ]; then
  echo "seed=$(cat "$GOMODCACHE/seed.txt")"
else
  echo "seed=absent"
fi
# the cache's parent (the caller's home in the usual layout) is not an input: a canary file
# beside the cache must be absent inside the view (gofix1-1)
if [ -e "$GOMODCACHE/../canary.txt" ]; then
  echo "parent canary=visible"
else
  echo "parent canary=absent"
fi
# Go creates its module cache directory itself; a plain read-only bind refuses the
# `mkdir` and the write alike, so both errors are captured (the subshell's stderr is
# redirected before the failing redirection inside it is attempted).
if ( mkdir -p "$GOMODCACHE" && echo "written by the indexer" > "$GOMODCACHE/indexer-wrote.txt" ) 2>"$TMPDIR/cache-write.err"; then
  echo "cache write landed"
else
  echo "cache write failed: $(cat "$TMPDIR/cache-write.err")"
fi
mkdir -p "$GOCACHE" && echo "build cache ok" > "$GOCACHE/build.txt" && echo "build cache write ok"
[ -f "$GOMODCACHE/seed.txt" ]
