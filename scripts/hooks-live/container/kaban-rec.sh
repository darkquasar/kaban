#!/usr/bin/env bash
# Recording shim for test-pi-extension.sh (R4-4): appends the envelope the extension sends (one
# line per call) to $KBN_ENVELOPES (tmpfs inside the container), then runs the real static binary
# on the same bytes and returns its exit status. Test-only; never used by run.sh.
set -u
real=/opt/kaban/kaban
rec="${KBN_ENVELOPES:-/capture/envelopes.jsonl}"
{ tee -a "$rec"; printf '\n' >> "$rec"; } | "$real" "$@"
exit "${PIPESTATUS[1]}"
