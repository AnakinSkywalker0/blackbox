#!/usr/bin/env bash
# Installs bb for the blackbox GitHub Action and exports BB_BIN. Never fails the job:
# if anything is wrong it warns and leaves BB_BIN empty, and the action then posts nothing.
#
# Inputs (environment): BB_VERSION ("latest" or like 0.6.0 / v0.6.0), BB_PATH (use this
# bb instead of downloading), RUNNER_TEMP, GITHUB_ENV.
set -u

REPO="AnakinSkywalker0/blackbox"

warn() { echo "::warning::blackbox: $*"; }

export_bin() { [ -n "${GITHUB_ENV:-}" ] && echo "BB_BIN=$1" >>"$GITHUB_ENV"; echo "blackbox: using $1"; }

# `bb explain` is what the action needs. Releases before it was added can't help.
usable() { "$1" explain --help >/dev/null 2>&1; }

main() {
  if [ -n "${BB_PATH:-}" ]; then
    if [ -x "$BB_PATH" ] && usable "$BB_PATH"; then export_bin "$BB_PATH"; else warn "bb-path '$BB_PATH' isn't a usable bb (it needs 'bb explain')."; fi
    return 0
  fi

  local version="${BB_VERSION:-latest}"
  case "$version" in
    latest) base="https://github.com/$REPO/releases/latest/download" ;;
    v[0-9]*.[0-9]*.[0-9]* | [0-9]*.[0-9]*.[0-9]*)
      case "$version" in *[!0-9.v]*) warn "bb-version '$version' isn't a plain version number."; return 0 ;; esac
      base="https://github.com/$REPO/releases/download/v${version#v}"
      ;;
    *) warn "bb-version must be 'latest' or a version like 0.6.0."; return 0 ;;
  esac

  local plat
  case "$(uname -s)-$(uname -m)" in
    Linux-x86_64) plat="linux-x86_64" ;;
    Darwin-arm64) plat="macos-arm64" ;;
    Darwin-x86_64) plat="macos-x86_64" ;;
    *) warn "no blackbox build for $(uname -s) $(uname -m). Run this on an ubuntu or macos runner."; return 0 ;;
  esac

  local tmp="${RUNNER_TEMP:-/tmp}/blackbox-$$"
  mkdir -p "$tmp" || { warn "no temporary folder."; return 0; }
  curl -fsSL --retry 3 "$base/bb-$plat.tar.gz" -o "$tmp/bb.tar.gz" \
    && curl -fsSL --retry 3 "$base/bb-$plat.tar.gz.sha256" -o "$tmp/bb.sha256" \
    || { warn "couldn't download bb ($version, $plat)."; return 0; }

  local want got
  want="$(awk '{print $1}' "$tmp/bb.sha256")"
  if command -v sha256sum >/dev/null 2>&1; then got="$(sha256sum "$tmp/bb.tar.gz" | awk '{print $1}')"; else got="$(shasum -a 256 "$tmp/bb.tar.gz" | awk '{print $1}')"; fi
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    warn "the download's checksum didn't match, so it was not used."
    return 0
  fi

  tar -xzf "$tmp/bb.tar.gz" -C "$tmp" || { warn "couldn't unpack bb."; return 0; }
  local bin
  bin="$(ls "$tmp"/bb-v*/bb 2>/dev/null | head -n 1)"
  if [ -z "$bin" ] || [ ! -x "$bin" ]; then warn "the download didn't contain bb."; return 0; fi
  if ! usable "$bin"; then
    warn "this bb release has no 'bb explain' yet. Set the action's bb-version to a newer release."
    return 0
  fi
  export_bin "$bin"
}

main
exit 0
