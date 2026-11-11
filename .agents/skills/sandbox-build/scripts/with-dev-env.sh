#!/usr/bin/env bash
# Run a command inside the flake's default dev shell environment, from a sandbox
# that has no `nix` on PATH and does not inherit LIBCLANG_PATH, CEF_PATH or
# LD_LIBRARY_PATH.
#
# Usage (from the checkout or worktree to build):
#   with-dev-env.sh <command> [args...]
#   with-dev-env.sh cargo nextest run -p providers
#
# The dev shell environment is generated once per flake revision with
# `nix print-dev-env --offline` and cached under $TMPDIR.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

nix_bin="$(command -v nix || true)"
if [[ -z "$nix_bin" ]]; then
  # The sandbox hides the host profile, but /nix/store is readable and the
  # daemon socket works: use the newest nix in the store.
  nix_bin="$(printf '%s\n' /nix/store/*-nix-[0-9]*/bin/nix |
    awk '{ v = $0; sub(/.*-nix-/, "", v); sub(/\/bin\/nix$/, "", v); print v, $0 }' |
    sort -V | tail -n1 | cut -d' ' -f2)"
fi
if [[ ! -x "$nix_bin" ]]; then
  echo "with-dev-env: no nix binary found" >&2
  exit 2
fi

key="$(cat flake.nix flake.lock rust-toolchain.toml | sha256sum | cut -c1-16)"
cache="${TMPDIR:-/tmp}/evorch-devenv-$key.sh"
if [[ ! -s "$cache" ]]; then
  "$nix_bin" --extra-experimental-features 'nix-command flakes' \
    print-dev-env --offline "$root" > "$cache.tmp"
  mv "$cache.tmp" "$cache"
fi

# Source from a directory without `.git`, so the shellHook skips `lefthook
# install`, and keep our TMPDIR: the script points it at a fresh mktemp dir.
saved_tmpdir="${TMPDIR:-/tmp}"
set +eu
cd "$saved_tmpdir"
# shellcheck disable=SC1090
. "$cache" > /dev/null 2>&1
set -eu
export TMPDIR="$saved_tmpdir"
cd "$root"

if [[ -z "${LIBCLANG_PATH:-}" ]]; then
  echo "with-dev-env: dev shell environment did not load ($cache)" >&2
  exit 2
fi
exec "$@"
