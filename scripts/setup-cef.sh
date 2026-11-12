#!/usr/bin/env bash
# Install the CEF distribution that cef-rs pins, in the flat layout cef-dll-sys
# reads through CEF_PATH. `gui` enables CEF by default, so every cargo build of
# it needs this directory (docs/browser.md).
#
# Usage: scripts/setup-cef.sh <dest> [--src-cache <dir>]
#        scripts/setup-cef.sh --print-tree-digest
#
# --src-cache keeps the verified extraction of the archive (before the changes
# below) in <dir>, for CI to cache. The script uses <dir> only when
# scripts/cef-tree-digest.sh of it equals the `tree_sha256` pinned here, which
# comes from the checked-out repository, never from the cache; anything else is
# discarded and the archive is fetched again. A fresh extraction is stored only
# when it matches the pin, and `<dir>.fresh` is created then so the caller knows
# there is something to save.
# --print-tree-digest fetches and verifies the archive and prints the value for
# `tree_sha256`. Run it whenever `archive` changes.
#
# <dest> holds libcef.so, the paks, ICU data, the V8 snapshot, the `en-US` and
# `ja` locales and the headers. flake.nix builds the same layout as `cefDist`;
# scripts/tests/test_setup_cef_pin.sh keeps the pinned archive in step with it.
#
# Both cargo and nextest start test binaries without libcef.so on the loader
# path, so also export LD_LIBRARY_PATH. Use a directory holding only a
# libcef.so link for tests that never start CEF: the distribution bundles a
# Vulkan loader (libvulkan.so.1) that would shadow the system one for wgpu.
# CEF finds its resources beside the libcef.so it was loaded from, so a test
# that starts CEF needs <dest> itself on LD_LIBRARY_PATH.
set -euo pipefail

archive='cef_binary_154.0.34+g14c5a08+chromium-154.0.8037.98_linux64_minimal.tar.bz2'
hash='sha256-6H4sdjBDct0ouJnOtu+ks1P1JwBZTdw1ioI37AfsZJI='
sha1='15c91ad767097217b2e82ba8b933fe30b134be69'
tree_sha256='9e07f74633355157c04d82b5e79ca0a5fa6a98eb7ab5303d3cf99e2360d5e0ec'
base_url="${CEF_DOWNLOAD_URL:-https://cef-builds.spotifycdn.com}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Download the archive, check its pinned sha256 and extract what the layout
# needs into $1.
fetch_and_extract() {
  curl --fail --silent --show-error --location --retry 5 --retry-all-errors \
    --output "$work/cef.tar.bz2" "$base_url/$archive"
  local expected actual
  expected="$(printf '%s' "${hash#sha256-}" | base64 -d | od -An -v -tx1 | tr -d ' \n')"
  actual="$(sha256sum "$work/cef.tar.bz2" | cut -d' ' -f1)"
  if [[ "$actual" != "$expected" ]]; then
    echo "setup-cef: sha256 mismatch for $archive: expected $expected, got $actual" >&2
    exit 1
  fi
  mkdir "$1"
  tar -xjf "$work/cef.tar.bz2" -C "$1" --strip-components=1 \
    --wildcards '*/Release' '*/Resources' '*/include'
}

if [[ "${1:-}" == --print-tree-digest ]]; then
  fetch_and_extract "$work/src"
  "$here/cef-tree-digest.sh" "$work/src"
  exit 0
fi

dest="${1:?usage: setup-cef.sh <dest> [--src-cache <dir>]}"
shift
src_cache=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --src-cache) src_cache="${2:?--src-cache needs a directory}"; shift 2 ;;
    *) echo "setup-cef: unknown argument: $1" >&2; exit 2 ;;
  esac
done
if [[ -e "$dest" ]] && [[ -n "$(ls -A "$dest")" ]]; then
  echo "setup-cef: $dest is not empty" >&2
  exit 2
fi

src=""
if [[ -n "$src_cache" && -d "$src_cache" && -n "$(ls -A "$src_cache")" ]]; then
  actual="$("$here/cef-tree-digest.sh" "$src_cache")"
  if [[ "$actual" == "$tree_sha256" ]]; then
    echo "setup-cef: using the cached extraction; it matches the pinned digest"
    src="$src_cache"
  else
    echo "setup-cef: the cached extraction does not match the pinned digest (got $actual); discarding it" >&2
    rm -rf "$src_cache" "$src_cache.fresh"
  fi
fi
if [[ -z "$src" ]]; then
  fetch_and_extract "$work/src"
  src="$work/src"
  if [[ -n "$src_cache" ]]; then
    actual="$("$here/cef-tree-digest.sh" "$work/src")"
    if [[ "$actual" == "$tree_sha256" ]]; then
      rm -rf "$src_cache"
      mkdir -p "$(dirname "$src_cache")"
      mv "$work/src" "$src_cache"
      src="$src_cache"
      : > "$src_cache.fresh"
    else
      echo "::warning::the extracted CEF archive does not match the pinned tree_sha256 ($actual); not caching it. Update tree_sha256 in scripts/setup-cef.sh." >&2
    fi
  fi
fi

mkdir -p "$dest"
cp -r "$src/Release/." "$src/Resources/." "$dest/"
cp -r "$src/include" "$dest/"
# The user-namespace sandbox is used (ADR 0034), and the GUI ships two locales.
rm -f "$dest/chrome-sandbox"
find "$dest/locales" -name '*.pak' ! -name en-US.pak ! -name ja.pak -delete
strip --strip-debug "$dest/libcef.so" "$dest/libvk_swiftshader.so" "$dest/libvulkan.so.1"
cat > "$dest/archive.json" <<EOF
{"type":"minimal","name":"$archive","sha1":"$sha1"}
EOF
# cef-dll-sys re-runs (and rebuilds its C++ wrapper) when a file under CEF_PATH is
# newer than its cached build output, which a fresh extraction always is.
find "$dest" -exec touch -h -d '2000-01-01 00:00:00 UTC' {} +
