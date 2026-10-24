#!/usr/bin/env bash
set -euo pipefail

attic="$PWD/result-attic/bin/attic"
attempts="${ATTIC_PUSH_ATTEMPTS:-4}"
attempt_timeout="${ATTIC_PUSH_TIMEOUT:-20m}"
status=0

# Upload closures so toolchains, vendored sources and crane artifacts are reusable.
# The patched client tolerates slow server storage; one NAR at a time limits load.
for result in "$@"; do
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    # Bound the entire pipeline, including path-info. The kill grace makes a
    # PR's 290s attempt finish within five minutes even if termination is ignored.
    # Positional parameters expand in the pipeline's shell.
    # shellcheck disable=SC2016
    if timeout --kill-after=10s "$attempt_timeout" bash -o pipefail -c '
      nix path-info --recursive "$1" | "$2" push --stdin --jobs 1 home
    ' bash "$result" "$attic"; then
      break
    fi
    if [ "$attempt" -eq "$attempts" ]; then
      echo "attic push of $result failed after $attempts attempts" >&2
      status=1
      break
    fi
    delay=$((attempt * 60 + RANDOM % 60))
    echo "attic push of $result failed (attempt $attempt/$attempts); retrying in ${delay}s"
    sleep "$delay"
  done
done

exit "$status"
