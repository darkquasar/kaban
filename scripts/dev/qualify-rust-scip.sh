#!/usr/bin/env bash
# Explicit R0 experiment only; not the Kaban runner or a production isolation tier.
# No installation, host Cargo configuration, registry sharing or online retry.
set -euo pipefail
if [ "$#" -ne 4 ]; then
  echo 'usage: qualify-rust-scip.sh <absolute-analyzer> <absolute-toolchain> <minimal|workspace> <existing-empty-scratch>' >&2
  exit 2
fi
analyzer=$1; toolchain=$2; fixture=$3; scratch=$4
case "$fixture" in minimal|workspace) ;; *) echo 'unknown qualification fixture' >&2; exit 2;; esac
for path in "$analyzer" "$toolchain" "$scratch"; do
  case "$path" in /*) ;; *) echo 'absolute paths required' >&2; exit 2;; esac
done
[ "$(uname -s):$(uname -m)" = Linux:aarch64 ] || { echo 'qualification requires Linux ARM64 namespaces' >&2; exit 2; }
for tool in bwrap timeout sha256sum; do command -v "$tool" >/dev/null; done
[ -x "$analyzer" ] && [ ! -L "$analyzer" ] || { echo 'real analyzer executable required' >&2; exit 2; }
for binary in cargo rustc; do
  [ -x "$toolchain/bin/$binary" ] && [ ! -L "$toolchain/bin/$binary" ] || {
    echo 'real toolchain required (no rustup proxies)' >&2; exit 2;
  }
done
# Resolve the system compiler's alternatives link without exposing /etc/alternatives.
cc=$(readlink -f /usr/bin/cc)
case "$cc" in /usr/*) ;; *) echo 'system C compiler must resolve under /usr' >&2; exit 2;; esac
[ -x "$cc" ] || { echo 'system C compiler missing' >&2; exit 2; }
[ -d "$scratch" ] && [ ! -L "$scratch" ] || { echo 'existing scratch directory required' >&2; exit 2; }
[ -z "$(ls -A "$scratch")" ] || { echo 'scratch must be empty; existing evidence is retained' >&2; exit 2; }
root=$(cd "$(dirname "$0")/../.." && pwd)
fixtures="$root/fixtures/codegraph/rust-qualification"
umask 077
mkdir "$scratch/home" "$scratch/tmp" "$scratch/out" "$scratch/cargo-home" "$scratch/target"
cp "$fixtures/cargo-config.toml" "$scratch/cargo-home/config.toml"
# Fixture source and tools are immutable inside a fresh mount/network/PID namespace.
# Only scratch is writable (apart from kernel pseudo-filesystems). No /home or
# host registry is mounted. PATH intentionally excludes every rustup proxy.
mounts=(--ro-bind /usr /usr --ro-bind /lib /lib)
[ ! -d /lib64 ] || mounts+=(--ro-bind /lib64 /lib64)
[ ! -f /etc/ld.so.cache ] || mounts+=(--ro-bind /etc/ld.so.cache /etc/ld.so.cache)
sha256sum "$analyzer" "$toolchain/bin/cargo" "$toolchain/bin/rustc" "$cc" > "$scratch/tools.sha256"
set +e
bash "$root/scripts/dev/cargo-slot.sh" timeout --kill-after=5s 120s \
  bwrap --unshare-all --die-with-parent --new-session \
  "${mounts[@]}" --symlink usr/bin /bin --symlink usr/sbin /sbin \
  --proc /proc --dev /dev --ro-bind "$toolchain" /toolchain \
  --ro-bind "$analyzer" /tools/rust-analyzer \
  --symlink "$cc" /tools/cc \
  --ro-bind "$fixtures/$fixture" /work \
  --ro-bind "$fixtures/analyzer-config.json" /config/analyzer.json \
  --bind "$scratch" /scratch --symlink /scratch/tmp /tmp \
  --clearenv --setenv PATH /toolchain/bin:/tools:/usr/bin:/bin \
  --setenv HOME /scratch/home --setenv TMPDIR /scratch/tmp \
  --setenv CARGO_HOME /scratch/cargo-home --setenv CARGO_NET_OFFLINE true \
  --setenv CARGO_TARGET_DIR /scratch/target --setenv CARGO_BUILD_JOBS 4 \
  --setenv CARGO_INCREMENTAL 0 --setenv CARGO_PROFILE_DEV_DEBUG 0 \
  --setenv RUSTC /toolchain/bin/rustc --setenv RUSTDOC /toolchain/bin/rustdoc \
  --setenv LANG C.UTF-8 --chdir /work \
  /tools/rust-analyzer scip /work --output /scratch/out/index.scip \
  --config-path /config/analyzer.json --num-threads 4 \
  > "$scratch/stdout.log" 2> "$scratch/stderr.log"
rc=$?
set -e
printf '%s\n' "$rc" > "$scratch/producer.exit"
echo "producer exit $rc; evidence retained at $scratch; inspect diagnostics and SCIP against the oracle"
exit "$rc"
