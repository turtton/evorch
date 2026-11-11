#!/usr/bin/env bash
# scripts/setup-cef.sh and flake.nix (`cefDist`) fetch the same CEF archive in
# the same layout, and cef-rs (Cargo.lock) binds that exact CEF build. A bump of
# any one of them without the others would make CI and Nix test different CEF
# builds, or send cef-dll-sys to download its own copy.
set -euo pipefail
cd "$(dirname "$0")/../.."

value() { sed -n "s/^$1='\\(.*\\)'\$/\\1/p" scripts/setup-cef.sh; }
archive=$(value archive)
hash=$(value hash)
sha1=$(value sha1)
tree_sha256=$(value tree_sha256)
[[ "$tree_sha256" =~ ^[0-9a-f]{64}$ ]] \
  || { echo "setup-cef.sh must pin tree_sha256 (scripts/setup-cef.sh --print-tree-digest)" >&2; exit 1; }
for item in "$archive" "$hash" "$sha1"; do
  [[ -n "$item" ]] || { echo "setup-cef.sh pin is missing" >&2; exit 1; }
  grep -qF -- "$item" flake.nix \
    || { echo "flake.nix does not contain the pin '$item' from scripts/setup-cef.sh" >&2; exit 1; }
done

# cef-dll-sys is versioned `<cef-rs version>+<CEF version>`; the archive name
# starts with `cef_binary_<CEF version>`.
cef_version=$(awk '/^name = "cef-dll-sys"$/ { getline; print; exit }' Cargo.lock \
  | sed -n 's/^version = ".*+\(.*\)"$/\1/p')
[[ -n "$cef_version" ]] || { echo "cef-dll-sys is missing from Cargo.lock" >&2; exit 1; }
[[ "$archive" == "cef_binary_${cef_version}+"* ]] \
  || { echo "Cargo.lock binds CEF $cef_version but setup-cef.sh pins $archive" >&2; exit 1; }
echo "CEF pin consistent: $archive"
