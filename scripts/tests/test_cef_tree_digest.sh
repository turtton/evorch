#!/usr/bin/env bash
# scripts/cef-tree-digest.sh must change for every tamper a cached CEF extraction
# could suffer, and scripts/setup-cef.sh must refuse a cache that does not match
# its pinned digest instead of using it.
set -euo pipefail
cd "$(dirname "$0")/../.."
digest=scripts/cef-tree-digest.sh
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

make_tree() { # make_tree <dir>
  rm -rf "$1"
  mkdir -p "$1/Release/locales" "$1/include"
  printf 'lib' > "$1/Release/libcef.so"
  printf 'pak' > "$1/Release/locales/en-US.pak"
  printf '#pragma once' > "$1/include/cef.h"
  printf '#!/bin/sh' > "$1/Release/tool"
  chmod 755 "$1/Release/tool"
  ln -s libcef.so "$1/Release/libcef.link"
}

tree="$work/tree"
make_tree "$tree"
base=$("$digest" "$tree")
[[ "$base" =~ ^[0-9a-f]{64}$ ]] || { echo "digest is not a sha256: $base" >&2; exit 1; }
[[ "$("$digest" "$tree")" == "$base" ]] || { echo "digest is not stable" >&2; exit 1; }

expect_same() { # expect_same <what> <command...>
  local what=$1; shift
  make_tree "$tree"
  (cd "$tree" && "$@")
  [[ "$("$digest" "$tree")" == "$base" ]] || { echo "digest changed by: $what" >&2; exit 1; }
}
expect_changed() {
  local what=$1; shift
  make_tree "$tree"
  (cd "$tree" && "$@")
  [[ "$("$digest" "$tree")" != "$base" ]] || { echo "digest did not notice: $what" >&2; exit 1; }
}

expect_same "touching mtimes" touch -d '2001-01-01 00:00:00 UTC' Release/libcef.so include/cef.h
expect_same "group/other write bits" chmod g+w Release/libcef.so
expect_changed "changed file content" sh -c "printf 'LIB' > Release/libcef.so"
expect_changed "added file" touch include/extra.h
expect_changed "removed file" rm include/cef.h
expect_changed "renamed file" mv include/cef.h include/cef2.h
expect_changed "added symlink" ln -s /etc/passwd Release/evil
expect_changed "retargeted symlink" ln -sfn include/cef.h Release/libcef.link
expect_changed "file replaced by symlink with the same bytes" sh -c "rm include/cef.h && ln -s ../Release/locales/en-US.pak include/cef.h"
expect_changed "added executable bit" chmod +x Release/libcef.so
expect_changed "removed executable bit" chmod -x Release/tool
expect_changed "added empty directory" mkdir include/empty

# Names the digest's line format cannot represent are refused, not hashed.
for what in file link-target; do
  make_tree "$tree"
  if [[ "$what" == file ]]; then
    : > "$tree/include/$(printf 'a\nb')"
  else
    ln -s "$(printf 'a\nb')" "$tree/Release/newline.link"
  fi
  if "$digest" "$tree" > /dev/null 2>&1; then
    echo "digest accepted a newline in a $what" >&2
    exit 1
  fi
done

# A cache that does not match the pin is discarded and never used.
mkdir "$work/bin"
printf '#!/bin/sh\nexit 22\n' > "$work/bin/curl"
chmod +x "$work/bin/curl"
make_tree "$work/cache"
dest="$work/dest"
if PATH="$work/bin:$PATH" scripts/setup-cef.sh "$dest" --src-cache "$work/cache" 2> "$work/err"; then
  echo "setup-cef.sh accepted a cache that does not match its pinned digest" >&2
  exit 1
fi
grep -q 'does not match the pinned digest' "$work/err" \
  || { echo "setup-cef.sh did not report the mismatching cache:" >&2; cat "$work/err" >&2; exit 1; }
[[ ! -e "$work/cache" ]] || { echo "the mismatching cache was not discarded" >&2; exit 1; }
[[ ! -e "$dest" ]] || { echo "a mismatching cache was installed into $dest" >&2; exit 1; }
echo "cef tree digest tests passed"
