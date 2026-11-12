#!/usr/bin/env bash
# Print one sha256 digest of a directory tree: every path with its type, every
# symlink with its target, the executable bit of every regular file, and the
# sha256 of every regular file. Adding, removing, renaming or retargeting
# anything changes the digest; mtimes and the other permission bits do not.
#
# Usage: scripts/cef-tree-digest.sh <dir>
#
# scripts/setup-cef.sh compares this digest of the extracted CEF archive with the
# one it pins, so a cached copy of that tree is only used when it is exactly what
# the pinned archive extracts to.
set -euo pipefail

dir="${1:?usage: cef-tree-digest.sh <dir>}"
[[ -d "$dir" ]] || { echo "cef-tree-digest: $dir is not a directory" >&2; exit 2; }
cd "$dir"
# The line format below cannot represent these names or symlink targets
# unambiguously.
if [[ -n "$(find . \( -name $'*\n*' -o -lname $'*\n*' \) -print -quit)" ]]; then
  echo "cef-tree-digest: a path or symlink target under $dir contains a newline" >&2
  exit 1
fi
{
  find . -mindepth 1 -printf '%y %p -> %l\n'
  find . -type f -perm /111 -printf 'x %p\n'
  find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum | sed 's/^/h /'
} | LC_ALL=C sort | sha256sum | cut -d' ' -f1
