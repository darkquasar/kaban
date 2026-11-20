#!/usr/bin/env bash
# Install a pinned Go toolchain in a new private directory outside the checkout.
set -euo pipefail
destination=${1:?usage: install-go.sh /absolute/new-directory [version]}
go_version=${2:-1.26.0}
case "$destination" in
  /*) ;;
  *) echo "the Go directory must be absolute" >&2; exit 64 ;;
esac
repository=$(cd "$(dirname "$0")/../.." && pwd -P)
parent=$(dirname "$destination")
mkdir -p -m 700 "$parent"
parent=$(cd "$parent" && pwd -P)
destination="$parent/$(basename "$destination")"
case "$destination/" in
  "$repository/"*) echo "install Go outside the checkout" >&2; exit 64 ;;
esac
if [ -e "$destination" ] || [ -L "$destination" ]; then
  echo "the Go directory must not already exist" >&2
  exit 64
fi
case "$(uname -m)" in
  x86_64) architecture=amd64 ;;
  aarch64|arm64) architecture=arm64 ;;
  *) echo "unsupported Go architecture" >&2; exit 69 ;;
esac
case "$go_version/$architecture" in
  1.26.0/amd64) checksum=aac1b08a0fb0c4e0a7c1555beb7b59180b05dfc5a3d62e40e9de90cd42f88235 ;;
  1.26.0/arm64) checksum=bd03b743eb6eb4193ea3c3fd3956546bf0e3ca5b7076c8226334afe6b75704cd ;;
  1.27.3/amd64) checksum=65efbc80fdc1fa2cd2631cad5b481777a68e8725f34e89f50fc6ac714e379b40 ;;
  1.27.3/arm64) checksum=a06e99309dbcbdca1a8ccd2282c3ee146e55930886e26b4f1193269587165f49 ;;
  *) echo "unsupported Go version: $go_version" >&2; exit 69 ;;
esac
scratch=$(mktemp -d "$parent/.kaban-go.XXXXXXXX")
trap 'rm -rf "$scratch"' EXIT
archive="$scratch/go.tar.gz"
curl --fail --silent --show-error --location --retry 3 \
  "https://dl.google.com/go/go${go_version}.linux-${architecture}.tar.gz" --output "$archive"
printf '%s  %s\n' "$checksum" "$archive" | sha256sum --check >&2
tar -xzf "$archive" -C "$scratch" --no-same-owner
chmod -R go-w "$scratch/go"
version=$(env -u GOROOT GOTOOLCHAIN=local "$scratch/go/bin/go" version)
if [ "$version" != "go version go$go_version linux/$architecture" ]; then
  echo "unexpected Go version: $version" >&2
  exit 1
fi
mv "$scratch/go" "$destination"
echo "$version installed" >&2
printf '%s\n' "$destination"
