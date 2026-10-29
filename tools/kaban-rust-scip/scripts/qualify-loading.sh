#!/usr/bin/env bash
# Trusted loading qualification only: not production runner isolation evidence.
set -euo pipefail
[ "$#" = 4 ] || [ "$#" = 5 ] || [ "$#" = 6 ] || exit 2
[ "$(uname -s):$(uname -m)" = Linux:aarch64 ] || exit 2
producer=$1
toolchain=$2
fixture=$3
scratch=$4
expect_output=${5:-no-output}
checkout=${6:-/work}
case "$checkout" in /work|/checkout-one|/checkout-two) ;; *) exit 2;; esac
root=$(cd "$(dirname "$0")/../../.." && pwd)
expected='semantic workspace loaded'
source="$root/fixtures/codegraph/rust-qualification/$fixture"
case "$fixture" in
  minimal|workspace) ;;
  metadata-failure|build-failure)
    source="$root/tools/kaban-rust-scip/tests/fixtures/$fixture"
    expected="semantic ${fixture%-failure} loading failed";;
  target-contexts|semantic-extra|cargo-contexts|local-contexts|operator-ranges|required-features|block-references|shared-contexts|member-collisions|cfg-conflict|derive-impls) source="$root/tools/kaban-rust-scip/tests/fixtures/$fixture";;
  # HD-1 (astra ri4-3bc-1), hd1-2: the workspace fixture plus a C file compiled with -flto through the admitted CC
  # (so cc1, as, collect2, lto-wrapper and ld all run), built with an `ld`/`as`/`collect2`/`lto-wrapper` that writes a
  # marker placed first on the child PATH (`/tools`); the pinned backend chain must never execute any of them.
  shadow-backend) source="$root/tools/kaban-rust-scip/tests/fixtures/$fixture";;
  cfg-body-conflict) source="$root/tools/kaban-rust-scip/tests/fixtures/$fixture"; expected='unsupported incompatible cfg/body';;
  block-function|block-impl|block-generated|block-attribute) source="$root/tools/kaban-rust-scip/tests/fixtures/$fixture"; expected='unsupported active block-item scope';;
  block-derive) source="$root/tools/kaban-rust-scip/tests/fixtures/block-attribute"; expected='unsupported active block-item scope';;
  shared-root) source="$root/tools/kaban-rust-scip/tests/fixtures/$fixture"; expected='unsupported shared-source Cargo target root';;

  missing-server) source="$root/fixtures/codegraph/rust-qualification/minimal"; expected='proc-macro server resource is unavailable';;
  missing-sysroot) source="$root/fixtures/codegraph/rust-qualification/minimal"; expected='matching rust-src resources are unavailable';;
  sysroot-metadata-failure) source="$root/tools/kaban-rust-scip/tests/fixtures/sysroot-metadata-failure"; expected='semantic sysroot metadata loading failed';;
  sysroot-manifest-failure) source="$root/tools/kaban-rust-scip/tests/fixtures/sysroot-metadata-failure"; expected='semantic sysroot metadata loading failed';;
  *) exit 2;;
esac
source=${KABAN_QUALIFICATION_SOURCE:-$source}
[ -d "$source" ] || exit 2
for path in "$producer" "$toolchain" "$scratch"; do case "$path" in /*) ;; *) exit 2;; esac; done
[ -x "$producer" ] && [ ! -L "$producer" ] || exit 2
for binary in cargo rustc; do
  [ -x "$toolchain/bin/$binary" ] && [ ! -L "$toolchain/bin/$binary" ] || exit 2
done
[ -d "$scratch" ] && [ ! -L "$scratch" ] && [ -z "$(ls -A "$scratch")" ] || exit 2
cc=$(readlink -f /usr/bin/cc)
umask 077
mkdir "$scratch/home" "$scratch/tmp" "$scratch/out" "$scratch/cargo-home" "$scratch/target"
cp "$root/fixtures/codegraph/rust-qualification/cargo-config.toml" "$scratch/cargo-home/config.toml"
# Contract v2: the linker is the canonical system compiler; /usr is bound whole, so the path is valid inside the sandbox.
printf '{"protocol_version":2,"target":"aarch64-unknown-linux-gnu","cargo":"/toolchain/bin/cargo","rustc":"/toolchain/bin/rustc","sysroot":"/toolchain","proc_macro_server":"/toolchain/libexec/rust-analyzer-proc-macro-srv","linker":"%s"}\n' "$cc" > "$scratch/prepared.json"
sha256sum "$producer" "$toolchain/bin/cargo" "$toolchain/bin/rustc" "$toolchain/libexec/rust-analyzer-proc-macro-srv" > "$scratch/tools.sha256"
find "$source" -type f -exec sha256sum {} + > "$scratch/source.sha256"
mounts=(--ro-bind /usr /usr --ro-bind /lib /lib)
[ ! -d /lib64 ] || mounts+=(--ro-bind /lib64 /lib64)
[ ! -f /etc/ld.so.cache ] || mounts+=(--ro-bind /etc/ld.so.cache /etc/ld.so.cache)
masks=()
source_masks=()
if [ "$fixture" = missing-server ]; then masks+=(--tmpfs /toolchain/libexec); fi
if [ "$fixture" = missing-sysroot ]; then masks+=(--tmpfs /toolchain/lib/rustlib/src); fi
if [ "$fixture" = sysroot-metadata-failure ]; then
  masks+=(--ro-bind "$source/sysroot.toml" /toolchain/lib/rustlib/src/rust/library/Cargo.toml
    --ro-bind "$source/core.toml" /toolchain/lib/rustlib/src/rust/library/core/Cargo.toml)
fi
if [ "$fixture" = sysroot-manifest-failure ]; then
  masks+=(--ro-bind "$source/malformed.toml" /toolchain/lib/rustlib/src/rust/library/Cargo.toml)
fi
if [ "$fixture" = block-derive ]; then
  source_masks+=(--ro-bind "$source/derive.rs" "$checkout/src/lib.rs")
fi
shadow_marker="$scratch/tmp/shadow-backend-ran"
if [ "$fixture" = shadow-backend ]; then
  # The shadow records its execution, then runs the real backend so that only the marker tells the two apart.
  mkdir "$scratch/shadow"
  for tool in ld as collect2 lto-wrapper; do
    real=$(readlink -f "$(env -i PATH=/usr/bin:/bin "$cc" -print-prog-name=$tool)")
    for name in "$tool" "aarch64-linux-gnu-$tool"; do
      printf '#!/bin/sh\necho "%s" >> /scratch/tmp/shadow-backend-ran\nexec %s "$@"\n' "$name" "$real" > "$scratch/shadow/$name"
      chmod 0700 "$scratch/shadow/$name"
      masks+=(--ro-bind "$scratch/shadow/$name" "/tools/$name")
    done
  done
fi
invocation=(/tools/kaban-rust-scip index --workspace "$checkout" --output /scratch/out/index.scip --config /scratch/prepared.json --threads 4)
if [ "$expect_output" = --cargo-proof ]; then
  invocation=(/usr/bin/env -u RUSTC_WRAPPER -u CARGO_REGISTRY_TOKEN CARGO=/toolchain/bin/cargo RUSTC=/toolchain/bin/rustc PATH=/toolchain/bin:/tools:/usr/bin:/bin /toolchain/bin/cargo test --all-targets --no-run --locked --offline --verbose)
fi
set +e
timeout --kill-after=5s 120s bwrap --unshare-all --die-with-parent --new-session \
  "${mounts[@]}" --symlink usr/bin /bin --symlink usr/sbin /sbin \
  --proc /proc --dev /dev --ro-bind "$toolchain" /toolchain "${masks[@]}" \
  --ro-bind "$producer" /tools/kaban-rust-scip --symlink "$cc" /tools/cc \
  --ro-bind "$source" "$checkout" "${source_masks[@]}" \
  --bind "$scratch" /scratch --symlink /scratch/tmp /tmp --clearenv \
  --setenv HOME /scratch/home --setenv TMPDIR /scratch/tmp \
  --setenv CARGO_HOME /scratch/cargo-home --setenv CARGO_TARGET_DIR /scratch/target \
  --setenv CARGO /host-cargo-proxy --setenv RUSTC_WRAPPER /host-wrapper \
  --setenv CARGO_REGISTRY_TOKEN invented-must-not-reach-child \
  --chdir "$checkout" "${invocation[@]}" \
  > "$scratch/stdout.log" 2> "$scratch/stderr.log"
rc=$?
set -e
printf '%s\n' "$rc" > "$scratch/producer.exit"
find "$source" -type f -exec sha256sum {} + > "$scratch/source-after.sha256"
cmp "$scratch/source.sha256" "$scratch/source-after.sha256"
sha256sum --check "$scratch/tools.sha256" > "$scratch/tool-input-check.log"
if [ -e "$shadow_marker" ]; then
  printf 'shadow backend executed: %s\n' "$(tr '\n' ' ' < "$shadow_marker")" >&2; exit 1
fi
if [ "$expect_output" = --cargo-proof ]; then
  [ "$rc" = 0 ] && [ ! -e "$scratch/out/index.scip" ]
elif [ "$expect_output" = --expect-scip ]; then
  [ "$rc" = 0 ] || { printf 'complete SCIP must exit0, actual=%s\n' "$rc" >&2; exit 1; }
  [ -s "$scratch/out/index.scip" ] && [ ! -s "$scratch/stdout.log" ]
else
  [ "$rc" = 1 ] && [ ! -s "$scratch/stdout.log" ] && [ ! -e "$scratch/out/index.scip" ] || { printf 'refused profile must exit1 without output, actual=%s\n' "$rc" >&2; exit 1; }
  grep -q "$expected" "$scratch/stderr.log"
fi
if [ "$fixture" = workspace ] || [ "$fixture" = shadow-backend ]; then
  [ "$(find "$scratch/target" -name build-script-ran.txt | wc -l)" -gt 0 ]
  [ -f "$scratch/tmp/proc-macro-ran.txt" ]
fi
if [ "$fixture" = shadow-backend ]; then
  # the C compile really ran, so the backend tools had the chance to be the shadows
  [ "$(find "$scratch/target" -name c-compiled.txt | wc -l)" -gt 0 ] || { echo 'shadow-backend: the C compile did not run' >&2; exit 1; }
fi
printf 'qualification passed: %s\n' "$expect_output"
