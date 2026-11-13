#!/usr/bin/env bash
# One full-profile build; archive/extract once and execute those exact shipped bytes.
set -euo pipefail
cd "$(dirname "$0")/../.."
target=${1:?usage: build.sh TARGET [OUTPUT_DIRECTORY]}
output=${2:-dist}
case "$target" in
  x86_64-unknown-linux-gnu|aarch64-unknown-linux-gnu|x86_64-unknown-linux-musl|aarch64-apple-darwin) ;;
  *) echo "unsupported release target: $target" >&2; exit 2 ;;
esac
python3 scripts/release/licenses.py check --target "$target"
version=$(python3 -c 'import tomllib; print(tomllib.load(open("Cargo.toml", "rb"))["workspace"]["package"]["version"])')
commit=$(git rev-parse HEAD)
host=$(rustc -vV | sed -n 's/^host: //p')
args=(build --release --locked -p kaban -p kaban-index --features kaban-ingest/pdf,kaban-ingest/md-splitter)
binary_dir=${CARGO_TARGET_DIR:-target}/release
case "$target" in
  *-musl)
    [ "$host" = x86_64-unknown-linux-gnu ]
    args+=(--target "$target")
    binary_dir=${CARGO_TARGET_DIR:-target}/$target/release
    ;;
  *) [ "$host" = "$target" ] ;;
esac
if [ "$target" = aarch64-apple-darwin ]; then
  test "$(uname -m)" = arm64
  export MACOSX_DEPLOYMENT_TARGET=15.0
fi
cargo "${args[@]}"
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
stage=$scratch/kaban-v$version-$target
mkdir "$stage"
cp -p "$binary_dir/kaban" "$binary_dir/kaban-index" "$stage/"
cp LICENSE NOTICE "$stage/"
# Regenerate notices from the locked sources and pinned rustc; refuse unreviewed drift.
python3 scripts/release/licenses.py stage --target "$target" --output "$stage/third-party"
python3 - "$stage" "$target" "$version" "$commit" "${args[@]}" <<'BUILD'
import hashlib, json, sys
from pathlib import Path
stage, target, version, commit = Path(sys.argv[1]), *sys.argv[2:5]
record = {'target': target, 'version': version, 'commit': commit,
          'features': ['default', 'kaban-ingest/pdf', 'kaban-ingest/md-splitter'],
          'cargo_args': sys.argv[5:],
          'binaries': {n: {'sha256': hashlib.sha256((stage/n).read_bytes()).hexdigest(),
                           'mode': (stage/n).stat().st_mode & 0o777}
                       for n in ('kaban', 'kaban-index')}}
(stage/'BUILD.json').write_text(json.dumps(record, indent=2) + '\n')
BUILD
python3 scripts/release/package.py --stage "$stage" --target "$target" --version "$version" --commit "$commit" --output "$output" --extract "$scratch/extracted"
checked_stage=$stage
stage="$scratch/extracted/$(basename "$stage")"
# Every runtime consumer uses extracted shipped bytes; there is no later rebuild.
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
python3 - "$stage" "$scratch" <<'FEATURES'
import json, os, shutil, subprocess, sys
from pathlib import Path
stage, scratch = map(Path, sys.argv[1:])
repo = scratch/'feature-probe'
repo.mkdir()
subprocess.run(['git', 'init', '-q', str(repo)], check=True)
(repo/'long.md').write_text('# Package splitting\n\n' + 'Package splitting marker sentence. ' * 600 + '\n')
shutil.copyfile('crates/kaban-ingest/tests/fixtures/pdf/born1.pdf', repo/'born.pdf')
env = dict(os.environ, KABAN_DB=str(scratch/'features.db'), KABAN_WORKSPACE='package-features')
def cli(*args):
    return json.loads(subprocess.check_output([str(stage/'kaban'), *args], cwd=repo, env=env, text=True))
report = cli('ingest', 'long.md', 'born.pdf')
assert report['documents'] == 2 and report['failed'] == 0 and report['partial'] == 0, report
assert report['chunks_upserted'] >= 4, report
assert cli('recall', 'Package splitting marker', '--kind', 'Chunk')['count'] > 0
assert cli('recall', 'second page heading', '--kind', 'Chunk')['count'] > 0
cli('doctor')
print('Extracted full-profile Markdown/PDF ingest, chunk recall and doctor passed:', report)
FEATURES
python3 - "$checked_stage" "$stage" "$output" "$version" "$target" <<'UNCHANGED'
import importlib.util, json, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location('package', 'scripts/release/package.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
staged, extracted, output = map(Path, sys.argv[1:4])
module.verify_extracted(staged, extracted)
archive = output/f'kaban-v{sys.argv[4]}-{sys.argv[5]}.tar.gz'
assert (output/(archive.name + '.sha256')).read_text().split()[0] == module.sha256(archive)
print('Final extracted pair/notices/modes and archive checksum remain unchanged')
UNCHANGED
