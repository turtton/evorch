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
cp scripts/run-contract-tests.sh scripts/contract-test-filters.sh scripts/ci-test-groups.json \
  scripts/ci-main-only-gui.sh scripts/ci-main-only-gui-tests.json "$work/scripts/"
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
  for suite in cache io remaining ci-remaining main-only-gui; do
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

# The local remaining suite keeps its meaning: it is neither cache nor io, and it
# does not mention the main-only registry.
if "$work/scripts/run-contract-tests.sh" --print-filter remaining | grep -q 'binary(=attention)'; then
  echo "FAIL: remaining must not depend on the main-only registry" >&2
  fails=$((fails + 1))
fi
main_only=$("$work/scripts/run-contract-tests.sh" --print-filter main-only-gui)
if [[ "$("$work/scripts/run-contract-tests.sh" --print-filter ci-remaining)" != *"not ($main_only)" ]]; then
  echo "FAIL: ci-remaining must be remaining without the main-only filter" >&2
  fails=$((fails + 1))
fi
if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "run-contract-tests: list and run agree for every suite and shard"
