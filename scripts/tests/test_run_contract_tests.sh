#!/usr/bin/env bash
# The inventory lists a suite and the shard runs it through two code paths of
# scripts/run-contract-tests.sh (cargo nextest list, scripts/guard-cargo.sh run).
# A suite listed with one filter or partition and run with another would make the
# merge gate vouch for tests that never run. This copies the scripts next to a
# stub cargo and a stub guard and compares what each path is given. No build.
set -euo pipefail
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/scripts" "$work/bin"
cp scripts/run-contract-tests.sh scripts/contract-test-filters.sh scripts/ci-test-groups.json "$work/scripts/"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$@" > "$STUB_OUT"\n' > "$work/scripts/guard-cargo.sh"
printf '#!/usr/bin/env bash\nprintf "%%s\\n" "$@" > "$STUB_OUT"\n' > "$work/bin/cargo"
chmod +x "$work/scripts/guard-cargo.sh" "$work/bin/cargo" "$work/scripts/run-contract-tests.sh"

# The arguments of -E and --partition in a recorded invocation.
selected() { # <file>
  awk '$0 == "-E" || $0 == "--partition" { print; getline; print }' "$1"
}

fails=0
for group in runtime gui storage-tools support; do
  shards=$(jq -r --arg g "$group" '.groups[$g].shards' scripts/ci-test-groups.json)
  for suite in cache io remaining; do
    for ((k = 1; k <= shards; k++)); do
      export STUB_OUT="$work/list.out"
      PATH="$work/bin:$PATH" "$work/scripts/run-contract-tests.sh" --group "$group" --shard "$k/$shards" --list "$suite"
      export STUB_OUT="$work/run.out"
      "$work/scripts/run-contract-tests.sh" --group "$group" --shard "$k/$shards" "$suite"
      if [[ "$(selected "$work/list.out")" != "$(selected "$work/run.out")" || -z "$(selected "$work/run.out")" ]]; then
        echo "FAIL: $group $k/$shards $suite: list and run are given different filters or partitions" >&2
        diff <(selected "$work/list.out") <(selected "$work/run.out") >&2 || true
        fails=$((fails + 1))
      fi
      if ! grep -qx -- "hash:$k/$shards" "$work/run.out"; then
        echo "FAIL: $group $k/$shards $suite: the run is not partitioned" >&2
        fails=$((fails + 1))
      fi
    done
  done
done

# What is listed and run does not depend on the event: a pull request and a push
# to main select the same tests, so the inventory they merge is the same.
for group in runtime gui storage-tools support; do
  shards=$(jq -r --arg g "$group" '.groups[$g].shards' scripts/ci-test-groups.json)
  for suite in cache io remaining; do
    for event in "pull_request refs/pull/762/merge" "push refs/heads/main" "workflow_dispatch refs/heads/other"; do
      export STUB_OUT="$work/event.${event%% *}.out"
      GITHUB_EVENT_NAME=${event%% *} GITHUB_REF=${event#* } PATH="$work/bin:$PATH" \
        "$work/scripts/run-contract-tests.sh" --group "$group" --shard "1/$shards" --list "$suite"
    done
    if ! cmp -s "$work/event.pull_request.out" "$work/event.push.out" || ! cmp -s "$work/event.push.out" "$work/event.workflow_dispatch.out"; then
      echo "FAIL: $group $suite: the listed selection depends on the event" >&2
      fails=$((fails + 1))
    fi
  done
done
for script in run-contract-tests.sh run-test-group.sh verify-test-inventory.sh; do
  if grep -qE 'GITHUB_(EVENT_NAME|REF|BASE_REF|HEAD_REF)' "scripts/$script"; then
    echo "FAIL: scripts/$script reads the event; the inventory must not depend on it" >&2
    fails=$((fails + 1))
  fi
done

# remaining is what neither cache nor io selects, and no suite is left to a main push:
# the removed main-only suites are refused instead of being read as something else.
for removed in ci-remaining main-only-gui; do
  if "$work/scripts/run-contract-tests.sh" --print-filter "$removed" 2> /dev/null; then
    echo "FAIL: the removed suite $removed is still accepted" >&2
    fails=$((fails + 1))
  fi
done
if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "run-contract-tests: list and run agree for every suite and shard"
