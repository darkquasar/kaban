#!/usr/bin/env bash
# Offline behaviour test of hooks/pi/kaban-hooks.ts inside the kaban-hooks-live image (OWN-7,
# R4-4). No pi, no model, no credentials: a fake `pi` object fires invented events at the
# extension (test-pi-extension.mjs, loaded with jiti as pi does), a recording shim in front of
# the static binary (container/kaban-rec.sh) keeps every envelope the extension sends (tmpfs), and the test asserts the
# M4-7 intake allowlist on those envelopes plus the W2-D2 behaviour. Also runs node's syntax
# check (`--experimental-strip-types --check`) on the extension.
#
#   bash scripts/hooks-live/test-pi-extension.sh --kaban <static kaban binary>
#
# Output: reduced JSON lines only (booleans, counts, the extension's own key names). Exit 1 on
# any failed assertion: the node syntax check, the behaviour test (test-pi-extension.mjs) and the
# recalled state (failures >= 1, pending_fix, evidence_runs >= 1, has_secret == false) all
# participate in the exit status (RL-3); none is only printed.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
IMAGE_NAME="${IMAGE_NAME:-kaban-hooks-live:m4}"
kaban_bin=""
while [ $# -gt 0 ]; do
  case "$1" in
    --kaban) kaban_bin="$2"; shift 2 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
[ -n "$kaban_bin" ] || { echo "--kaban <static kaban binary> is required" >&2; exit 2; }
kaban_bin="$(readlink -f "$kaban_bin")"
[ -x "$kaban_bin" ] || { echo "kaban binary not executable: $kaban_bin" >&2; exit 2; }
command -v docker >/dev/null || { echo "docker not available" >&2; exit 2; }

inner='
set -u
export KABAN_HOME=$HOME/.kaban KABAN_DB=/work/kaban.db KABAN_POLICY=/policy.yaml
mkdir -p "$KABAN_HOME" /capture
cd /work && git init -q . && git config user.email t@example.invalid && git config user.name t
printf "# scratch\n" > README.md && git add README.md && git commit -q -m init
SENT="KBN-DEC-$(head -c 4 /dev/urandom | od -An -tx1 | tr -d " \n")"
seed="$(/opt/kaban/kaban --json remember Decision "$SENT: notes.txt holds exactly one line; never append a second line to it." --path-scope notes.txt --importance 0.9 --props "{\"rationale\":\"fake-pi test seed\"}" 2>/dev/null)"
id="$(printf "%s" "$seed" | jq -r ".id // empty")"
acc="$(/opt/kaban/kaban --json accept "$id" --reason seed --path-scope notes.txt 2>/dev/null)"
echo "{\"seeded\": $([ -n "$id" ] && echo true || echo false), \"accepted\": $(printf "%s" "$acc" | jq -c "(.node.governance == \"accepted\")" 2>/dev/null || echo false)}"
: > /capture/envelopes.jsonl
JITI="$(ls /usr/local/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/jiti/lib/jiti.mjs 2>/dev/null || ls /usr/local/share/npm-global/lib/node_modules/@earendil-works/pi-coding-agent/node_modules/jiti/lib/jiti.mjs)"
node --experimental-strip-types --check /work/hooks/pi/kaban-hooks.ts 2>/dev/null; node_rc=$?
echo "{\"node_check_rc\": $node_rc}"
KBN_ENVELOPES=/capture/envelopes.jsonl KABAN_BIN=/harness/container/kaban-rec.sh EXT=/work/hooks/pi/kaban-hooks.ts WORK=/work SENT="$SENT" JITI="$JITI" \
  node /harness/test-pi-extension.mjs 2>/dev/null
rc=$?
echo "{\"envelope_lines\": $(wc -l < /capture/envelopes.jsonl | tr -d " ")}"
recall="$(/opt/kaban/kaban --json recall "failed" --kind Failure --full-text --limit 5 2>/dev/null \
  | jq -c --arg s AKIAQWERTYUIOPASDFGH "{failures: (.count // 0), pending_fix: ([.results[]?.props.pending_fix] | any), evidence_runs: ([.results[]?.props.evidence_runs // [] | length] | max // 0), has_secret: (tostring | contains(\$s))}")"
[ -n "$recall" ] || recall="{\"failures\":0,\"pending_fix\":false,\"evidence_runs\":0,\"has_secret\":null}"
# the recalled state is asserted, not only printed: one Failure recorded, pending fix, one success
# evidence run appended, the planted secret nowhere in it
printf "%s" "$recall" | jq -e ".failures >= 1 and .pending_fix == true and .evidence_runs >= 1 and .has_secret == false" >/dev/null 2>&1; recall_rc=$?
echo "$recall" | jq -c --argjson ok "$([ $recall_rc -eq 0 ] && echo true || echo false)" ". + {recall_ok: \$ok}"
final=0; [ $node_rc -eq 0 ] && [ $rc -eq 0 ] && [ $recall_rc -eq 0 ] || final=1
echo "{\"test_rc\": $rc, \"recall_rc\": $recall_rc, \"node_check_rc\": $node_rc, \"exit\": $final}"
exit $final
'
exec docker run --rm \
  --tmpfs /home/node:rw,uid=1000,gid=1000,mode=0700,size=64m \
  --tmpfs /work:rw,uid=1000,gid=1000,mode=0755,size=64m \
  --tmpfs /capture:rw,uid=1000,gid=1000,mode=0700,size=16m \
  -v "$here:/harness:ro" \
  -v "$repo/hooks:/work/hooks:ro" \
  -v "$repo/examples/policy.yaml:/policy.yaml:ro" \
  -v "$kaban_bin:/opt/kaban/kaban:ro" \
  --entrypoint bash "$IMAGE_NAME" -c "$inner"
