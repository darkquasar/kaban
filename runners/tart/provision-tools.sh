#!/usr/bin/env bash
# Candidate guest tool image: reuse the repository's pins/installers, not CI results.
set -euo pipefail
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = aarch64 ] || exit 64
inputs=/tmp/kaban-ci-inputs
cd "$inputs"
python3 - <<'PY'
from pathlib import Path
import hashlib,json
for name,expected in json.loads(Path('source-hashes.json').read_text()).items():
    assert hashlib.sha256(Path(name).read_bytes()).hexdigest()==expected,name
PY

sudo apt-get update
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
  build-essential pkg-config libssl-dev git curl ca-certificates jq sqlite3 strace \
  musl-tools gnupg xz-utils unzip file util-linux gh

download_bootstrap() {
  local tool=$1 output=$2 url checksum
  url=$(python3 -c 'import json,sys;print(json.load(open("bootstrap-inputs.json"))[sys.argv[1]]["url"])' "$tool")
  checksum=$(python3 -c 'import json,sys;print(json.load(open("bootstrap-inputs.json"))[sys.argv[1]]["sha256"])' "$tool")
  curl --fail --silent --show-error --location --retry 3 "$url" --output "$output"
  printf '%s  %s\n' "$checksum" "$output" | sha256sum --check
  chmod 755 "$output"
}
download_bootstrap mise /tmp/kaban-mise-bootstrap
sudo install -m 755 /tmp/kaban-mise-bootstrap /usr/local/bin/mise
download_bootstrap rustup /tmp/rustup-init
/tmp/rustup-init -y --no-modify-path --profile minimal --default-toolchain none
export PATH="$HOME/.cargo/bin:$PATH"
export MISE_DATA_DIR=/opt/kaban-mise
sudo install -d -o "$(id -u)" -g "$(id -g)" -m 755 "$MISE_DATA_DIR"
mise trust "$inputs/mise.toml"
mise install --locked
sudo chmod -R go-w "$MISE_DATA_DIR/installs"

python_bin=$(dirname "$(mise which python)")
node_exe=$(mise which node)
node_bin=$(dirname "$node_exe")
nextest_bin=$(dirname "$(mise which cargo-nextest)")
deny_bin=$(dirname "$(mise which cargo-deny)")
export PATH="$python_bin:$node_bin:$nextest_bin:$deny_bin:$PATH"
root_channel=$(python3 -c 'import tomllib;print(tomllib.load(open("rust-toolchain.toml","rb"))["toolchain"]["channel"])')
producer_channel=$(python3 -c 'import tomllib;print(tomllib.load(open("tools/kaban-rust-scip/rust-toolchain.toml","rb"))["toolchain"]["channel"])')
targets=$(python3 -c 'import tomllib;print(",".join(tomllib.load(open("rust-toolchain.toml","rb"))["toolchain"]["targets"]))')
rustup toolchain install "$root_channel" --profile minimal --component rustfmt --component clippy --target "$targets" --no-self-update
rustup toolchain install "$producer_channel" --profile minimal --component rustfmt --component clippy --no-self-update
rustup default "$root_channel"

# Preserve the setup layout already used by the delivered workflow.
sudo chown root:root "$node_exe"
sudo chmod go-w "$node_exe"
sudo ln -sfn "$node_exe" /usr/bin/node
go_root=$(sudo bash scripts/ci/install-go.sh /usr/lib/kaban-ci-go-1.26.0)
sudo install -d -o "$(id -u)" -g "$(id -g)" -m 755 /opt/kaban-ci-tools
scip_go=$(bash scripts/ci/install-scip-go.sh /opt/kaban-ci-tools)
sudo ln -sfn "$go_root/bin/go" /usr/bin/go
export GOROOT="$go_root" SCIP_GO="$scip_go"
export PATH="$go_root/bin:$PATH"

# Only guest-local tools are exported. No host HOME, source tree or credentials.
profile=$(mktemp)
{
  printf 'export MISE_DATA_DIR=%q\n' "$MISE_DATA_DIR"
  printf 'export GOROOT=%q\n' "$GOROOT"
  printf 'export SCIP_GO=%q\n' "$SCIP_GO"
  printf 'export PATH=%q:"$HOME/.cargo/bin:$PATH"\n' "$go_root/bin:$python_bin:$node_bin:$nextest_bin:$deny_bin"
} > "$profile"
sudo install -m 644 "$profile" /etc/profile.d/kaban-ci-tools.sh
rm "$profile"

export PYTHONDONTWRITEBYTECODE=1
python3 scripts/ci/preflight.py
{
  uname -a
  mise --version
  rustup --version
  rustc --version
  cargo --version
  cargo nextest --version
  cargo deny --version
  python3 --version
  node --version
  env -u GOROOT GOTOOLCHAIN=local go version
  "$SCIP_GO" --version
  (cd tools/kaban-rust-scip && rustup show active-toolchain)
  rustup target list --installed
  df -B1 / /tmp
} | tee "$inputs/installed-tools.txt"
dpkg-query -W > "$inputs/native-packages.txt"
python3 - <<'PY'
from pathlib import Path
import hashlib,json
for name,expected in json.loads(Path('source-hashes.json').read_text()).items():
    assert hashlib.sha256(Path(name).read_bytes()).hexdigest()==expected,name
PY
echo 'Tool image prepared; no product tests or release qualification performed.'
