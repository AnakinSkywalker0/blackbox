#!/bin/sh
# blackbox installer for Linux and macOS.
#
#   curl -fsSL https://raw.githubusercontent.com/AnakinSkywalker0/blackbox/main/install.sh | sh
#
# Downloads the latest release for your system from GitHub, checks its SHA-256 against
# the published checksum, and runs `bb install` (copies bb to ~/.local/bin, starts
# recording, and starts it at login). No root needed. Read it before you run it.
set -eu

REPO="AnakinSkywalker0/blackbox"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  plat="linux-x86_64" ;;
  Darwin-arm64)  plat="macos-arm64" ;;
  Darwin-x86_64) plat="macos-x86_64" ;;
  *) echo "blackbox doesn't have a build for $(uname -s) $(uname -m) yet." >&2; exit 1 ;;
esac

base="https://github.com/$REPO/releases/latest/download"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "Downloading blackbox ($plat)..."
curl -fsSL "$base/bb-$plat.tar.gz" -o "$tmp/bb.tar.gz"
curl -fsSL "$base/bb-$plat.tar.gz.sha256" -o "$tmp/bb.sha256"

want="$(awk '{print $1}' "$tmp/bb.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  got="$(sha256sum "$tmp/bb.tar.gz" | awk '{print $1}')"
else
  got="$(shasum -a 256 "$tmp/bb.tar.gz" | awk '{print $1}')"
fi
if [ "$want" != "$got" ]; then
  echo "Checksum mismatch, so nothing was installed." >&2
  echo "  expected $want" >&2
  echo "  got      $got" >&2
  exit 1
fi
echo "Checksum OK. Installing..."

tar -xzf "$tmp/bb.tar.gz" -C "$tmp"
"$tmp"/bb-v*/bb install
