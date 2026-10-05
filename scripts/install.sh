#!/usr/bin/env sh
# Install the parallax binary from this repo's GitHub releases.
#
#   scripts/install.sh [TAG]     # default: the latest release
#
# The repo is private, so this uses an authenticated `gh` CLI. Installs to
# $PARALLAX_INSTALL_DIR, or ~/.local/bin.
set -eu

repo=pixelbadger/parallax
dir=${PARALLAX_INSTALL_DIR:-"$HOME/.local/bin"}

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  *)
    echo "no prebuilt parallax for $(uname -s) $(uname -m); use: cargo install --path ." >&2
    exit 1
    ;;
esac

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# shellcheck disable=SC2086 # an empty TAG means the latest release
gh release download ${1:-} --repo "$repo" --dir "$tmp" \
  --pattern "parallax-*-$target.tar.gz" --pattern SHA256SUMS
if command -v sha256sum > /dev/null; then sha=sha256sum; else sha="shasum -a 256"; fi
(cd "$tmp" && grep -- "-$target.tar.gz" SHA256SUMS | $sha -c - > /dev/null)
mkdir -p "$dir"
tar -xzf "$tmp"/parallax-*-"$target".tar.gz -C "$dir" parallax
echo "installed $("$dir/parallax" --version) to $dir/parallax"
