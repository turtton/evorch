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
# `nix print-dev-env --offline` and cached under $TMPDIR. When
# EVORCH_PROCESS_GUARD_BUNDLE is unset, the process-guard helper bundle that
# tests starting guarded children need is prepared once per helper source
# revision under $TMPDIR (scripts/guard-artifact.sh prepare) and reused.
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

# Process-guard helper bundle (what CI and flake.nix export). An already set
# EVORCH_PROCESS_GUARD_BUNDLE is never overwritten. Otherwise the bundle lives
# in a directory keyed by the content of everything the helper is built from
# (a deleted or renamed file counts as a change), so a changed helper source
# gets a fresh bundle. An existing bundle is re-verified (manifest, helper hash
# and ABI) and replaced when broken or without a manifest. It is prepared in a
# private directory and moved into place, so concurrent invocations never see
# a half-written bundle. Everything runs in a subshell that only reports the
# manifest path on stdout: whatever goes wrong there only warns, and the
# requested command always runs.
prepare_guard_bundle() {
  local script="$root/scripts/guard-artifact.sh" key dir f
  if [[ ! -x "$script" ]]; then
    echo "with-dev-env: $script not found" >&2
    return 1
  fi
  key="$({
    git ls-files -z --cached --others --exclude-standard -- crates/process-guard \
      Cargo.toml Cargo.lock rust-toolchain.toml scripts/guard-artifact.sh | sort -zu |
      while IFS= read -r -d '' f; do
        if [[ -f "$f" ]]; then sha256sum -- "$f"; else printf 'missing  %s\n' "$f"; fi
      done
  } | sha256sum | cut -c1-16)"
  dir="$TMPDIR/evorch-guard-$key"
  if [[ -e "$dir" ]] && ! "$script" verify "$dir/guard-bundle.json" > /dev/null 2>&1; then
    echo "with-dev-env: guard bundle $dir is broken, rebuilding" >&2
    rm -rf "$dir"
  fi
  if [[ ! -e "$dir" ]]; then
    guard_tmp="$(mktemp -d "$TMPDIR/evorch-guard-$key.tmp.XXXXXX")"
    trap 'rm -rf "$guard_tmp"' EXIT
    "$script" prepare --profile release --out "$guard_tmp/bundle" > /dev/null
    # Another invocation may have finished first; then its bundle stays.
    mv -T "$guard_tmp/bundle" "$dir" 2> /dev/null || true
  fi
  [[ -e "$dir/guard-bundle.json" ]] || return 1
  printf '%s\n' "$dir/guard-bundle.json"
}
if [[ -z "${EVORCH_PROCESS_GUARD_BUNDLE:-}" ]]; then
  if guard_bundle="$(prepare_guard_bundle)" && [[ -n "$guard_bundle" ]]; then
    export EVORCH_PROCESS_GUARD_BUNDLE="$guard_bundle"
  else
    echo "with-dev-env: EVORCH_PROCESS_GUARD_BUNDLE not set (process-guard bundle unavailable)" >&2
  fi
fi
exec "$@"
