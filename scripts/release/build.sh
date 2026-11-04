#!/usr/bin/env bash
# One selected build, existing protocol smoke, ordinary CLI checks, then archive.
set -euo pipefail
cd "$(dirname "$0")/../.."
target=${1:?usage: build.sh TARGET [OUTPUT_DIRECTORY]}
output=${2:-dist}
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-unknown-linux-musl) ;;
  *) echo "unsupported release target: $target" >&2; exit 2 ;;
esac
python3 scripts/release/licenses.py check --target "$target"
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
commit=$(git rev-parse HEAD)
host=$(rustc -vV | sed -n 's/^host: //p')
args=(build --release --locked -p kaban -p kaban-index)
binary_dir=${CARGO_TARGET_DIR:-target}/release
case "$target" in
  *-musl)
    [ "$host" = x86_64-unknown-linux-gnu ]
    args+=(--target "$target")
    binary_dir=${CARGO_TARGET_DIR:-target}/$target/release
    ;;
  *) [ "$host" = "$target" ] ;;
esac
cargo "${args[@]}"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
stage=$scratch/kaban-v$version-$target
mkdir "$stage"
cp -p "$binary_dir/kaban" "$binary_dir/kaban-index" "$stage/"
cp LICENSE NOTICE "$stage/"
cp -R "licenses/release/$target" "$stage/third-party"
# The release's own paths are used for every check and are never rebuilt afterward.
export KABAN_HOME=$scratch/home
python3 - "$stage" "$version" "$scratch" <<'CLI'
import json, subprocess, sys
from pathlib import Path
stage, version, scratch = Path(sys.argv[1]), sys.argv[2], Path(sys.argv[3])
# subprocess closes inherited runner descriptors, matching the existing CI launcher.
for name in ('kaban', 'kaban-index'):
    subprocess.run([str(stage / name), '--help'], stdout=subprocess.DEVNULL, check=True)
    actual = subprocess.check_output([str(stage / name), '--version'], text=True).strip()
    assert actual == f'{name} {version}', actual
report = json.loads(subprocess.check_output([str(stage / 'kaban-index')], text=True))
assert report['ok'] is True and report['verb'] == 'scaffold' and report['exit'] == 0, report
# Missing policy must be refused before any external producer can be invoked.
refusal = subprocess.run([str(stage / 'kaban-index'), '--policy', str(scratch / 'absent-policy.yaml'), 'doctor'], capture_output=True, text=True)
report = json.loads(refusal.stdout)
assert refusal.returncode == 4 and report['ok'] is False and report['class'] == 'policy_invalid' and report['exit'] == 4, report
CLI
python3 scripts/mcp/mcp_smoke.py "$stage/kaban" "$scratch/smoke.db" examples/policy.yaml
python3 scripts/release/package.py --stage "$stage" --target "$target" --version "$version" --commit "$commit" --output "$output"
