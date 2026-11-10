#!/usr/bin/env bash
# The CEF GUI is the only supported GUI build: `gui` must refuse to compile
# without its `cef` feature rather than produce a partial GUI.
set -euo pipefail
cd "$(dirname "$0")/../.."

log="$(mktemp)"
trap 'rm -f "$log"' EXIT
if cargo check -p gui --no-default-features >"$log" 2>&1; then
  echo "gui compiled without the cef feature" >&2
  exit 1
fi
if ! grep -q 'gui requires the `cef` feature' "$log"; then
  echo "gui failed without the cef feature for another reason:" >&2
  tail -n 40 "$log" >&2
  exit 1
fi
echo "gui refuses to build without the cef feature"
