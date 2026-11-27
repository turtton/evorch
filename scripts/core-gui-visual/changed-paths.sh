#!/usr/bin/env bash
# Whether a PR changes anything the new GUI's screens can depend on.
#
# Usage: changed-paths.sh <before-sha> <after-sha>
#
# Prints `relevant=true` or `relevant=false` and, when true, the first path that
# matched. The paths are those of the diff between the two sides the evidence
# compares, with renames split so that both the old and the new name count. A diff
# that cannot be computed is an error, never "not relevant". The workflow runs this
# in the capture job and not as an `on.pull_request.paths` filter, because a filter
# would also keep the closed run (the cleanup) from starting.
set -euo pipefail

before=${1:?before sha}
after=${2:?after sha}

# The crates cover the GUI and everything it links; the manifests, the vendored
# patches and the Cargo configuration decide how they build; the rest is this
# evidence's own tooling and the setup of the CEF distribution it links against.
is_relevant() {
  case "$1" in
    crates/* | vendor/* | .cargo/* | Cargo.toml | Cargo.lock | rust-toolchain.toml) return 0 ;;
    scripts/ci-test-groups.json | scripts/setup-cef.sh | scripts/cef-tree-digest.sh) return 0 ;;
    scripts/core-gui-visual/* | .github/workflows/core-gui-visual.yml) return 0 ;;
    .github/actions/setup-cef/* | .github/actions/apt-install/*) return 0 ;;
  esac
  return 1
}

list=$(mktemp)
trap 'rm -f "$list"' EXIT
# Not a process substitution: a failing git must stop the script.
git diff --name-only --no-renames -z "$before" "$after" > "$list"

while IFS= read -r -d '' path; do
  if is_relevant "$path"; then
    echo "relevant=true"
    echo "first=$path"
    exit 0
  fi
done < "$list"
echo "relevant=false"
