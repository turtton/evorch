#!/usr/bin/env bash
# Install the CEF distribution that cef-rs pins, in the flat layout cef-dll-sys
# reads through CEF_PATH. `gui` enables CEF by default, so every cargo build of
# it needs this directory (docs/browser.md).
#
# Usage: scripts/setup-cef.sh <dest>
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
base_url="${CEF_DOWNLOAD_URL:-https://cef-builds.spotifycdn.com}"

dest="${1:?usage: setup-cef.sh <dest>}"
if [[ -e "$dest" ]] && [[ -n "$(ls -A "$dest")" ]]; then
  echo "setup-cef: $dest is not empty" >&2
  exit 2
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
curl --fail --silent --show-error --location --retry 5 --retry-all-errors \
  --output "$work/cef.tar.bz2" "$base_url/$archive"

expected="$(printf '%s' "${hash#sha256-}" | base64 -d | od -An -v -tx1 | tr -d ' \n')"
actual="$(sha256sum "$work/cef.tar.bz2" | cut -d' ' -f1)"
if [[ "$actual" != "$expected" ]]; then
  echo "setup-cef: sha256 mismatch for $archive: expected $expected, got $actual" >&2
  exit 1
fi

mkdir "$work/src"
tar -xjf "$work/cef.tar.bz2" -C "$work/src" --strip-components=1 \
  --wildcards '*/Release' '*/Resources' '*/include'
mkdir -p "$dest"
cp -r "$work/src/Release/." "$work/src/Resources/." "$dest/"
cp -r "$work/src/include" "$dest/"
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
