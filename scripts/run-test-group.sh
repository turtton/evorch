#!/usr/bin/env bash
# Build, list and run one shard of a CI test group (scripts/ci-test-groups.json).
#
# Usage: scripts/run-test-group.sh inventory <group> <K/N> <outdir>
#        scripts/run-test-group.sh run <group> <K/N> <cache|io|remaining> <outdir>
#
# inventory  Lists the group's tests with the exact arguments the run uses
#            (scripts/run-contract-tests.sh --list) and writes, into <outdir>:
#              all.tsv default.tsv cache.tsv io.tsv remaining.tsv   (whole group)
#              cache.part.tsv io.part.tsv remaining.part.tsv        (this shard)
#              meta.json                                            (written last)
#            Rows are `package<TAB>binary-id<TAB>test<TAB>ignored`, sorted.
#            The first listing also builds the group's test binaries.
#            A failing listing fails the job; meta.json is only written when
#            every listing succeeded, and the merge gate rejects a shard
#            without it.
# run        Runs the shard's part of one suite. A suite that is empty for the
#            whole group is skipped only when ci-test-groups.json lists it in
#            allow_empty_suites; a shard whose partition happens to be empty is
#            skipped because the merge gate checks that the shards of the group
#            together cover the suite. Everything else runs with `--no-tests
#            fail`.
set -euo pipefail
cd "$(dirname "$0")/.."
groups_file=scripts/ci-test-groups.json

mode="${1:?expected inventory or run}"
group="${2:?expected a group name}"
shard="${3:?expected K/N}"
[[ "$shard" =~ ^([1-9][0-9]*)/([1-9][0-9]*)$ ]] || { echo "shard must look like K/N: $shard" >&2; exit 2; }
shard_k="${BASH_REMATCH[1]}"
shard_n="${BASH_REMATCH[2]}"
configured_n=$(jq -er --arg g "$group" '.groups[$g].shards' "$groups_file") \
  || { echo "unknown test group: $group" >&2; exit 2; }
[[ "$shard_n" == "$configured_n" ]] || { echo "group $group has $configured_n shards, not $shard_n" >&2; exit 2; }
(( shard_k <= shard_n )) || { echo "shard $shard_k is out of range 1..$shard_n" >&2; exit 2; }

count() { wc -l < "$1" | tr -d ' '; }

case "$mode" in
  inventory)
    out="${4:?expected an output directory}"
    command -v jq > /dev/null || { echo "run-test-group: jq is required" >&2; exit 2; }
    rm -rf "$out"
    mkdir -p "$out"
    work=$(mktemp -d)
    trap 'rm -rf "$work"' EXIT
    # Identity lines of the tests nextest would run (filter-match "matches"),
    # from a JSON listing on stdin.
    select_tests() {
      jq -r '
        .["rust-suites"][] | . as $suite
        | .testcases | to_entries[]
        | select(.value["filter-match"].status == "matches")
        | [$suite["package-name"], $suite["binary-id"], .key, (.value.ignored | tostring)]
        | @tsv' | LC_ALL=C sort
    }
    list_to() { # list_to <file> <run-contract-tests args...>
      local file=$1
      shift
      scripts/run-contract-tests.sh "$@" > "$work/list.json"
      select_tests < "$work/list.json" > "$file"
      echo "listed $(count "$file") tests -> ${file##*/}"
    }
    for set_name in all default cache io remaining; do
      list_to "$out/$set_name.tsv" --group "$group" --list "$set_name"
    done
    for suite in cache io remaining; do
      list_to "$out/$suite.part.tsv" --group "$group" --shard "$shard" --list "$suite"
    done
    config_hash=$(
      {
        cat "$groups_file" scripts/contract-test-filters.sh .config/nextest.toml
        env | LC_ALL=C sort | grep -E '^(RUSTFLAGS|CARGO_PROFILE_[A-Z_]+|CARGO_BUILD_[A-Z_]+|RUSTDOCFLAGS)=' || true
      } | sha256sum | cut -d' ' -f1
    )
    sha="${GITHUB_SHA:-$(git rev-parse HEAD)}"
    jq -n --arg group "$group" --argjson shard "$shard_k" --argjson shards "$shard_n" \
      --arg sha "$sha" --arg config_hash "$config_hash" \
      --argjson packages "$(jq -c --arg g "$group" '.groups[$g].packages | sort' "$groups_file")" \
      --argjson features "$(jq -c --arg g "$group" '.groups[$g].features // []' "$groups_file")" \
      '{status: "ok", group: $group, shard: $shard, shards: $shards, sha: $sha,
        config_hash: $config_hash, packages: $packages, features: $features}' > "$out/meta.json"
    {
      echo "### Test inventory: $group $shard"
      echo
      echo "| set | tests |"
      echo "| --- | ---: |"
      for f in all default cache io remaining; do echo "| $f | $(count "$out/$f.tsv") |"; done
      for f in cache io remaining; do echo "| $f (this shard) | $(count "$out/$f.part.tsv") |"; done
    } > "$out/summary.md"
    cat "$out/summary.md"
    if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
      cat "$out/summary.md" >> "$GITHUB_STEP_SUMMARY"
    fi
    ;;
  run)
    suite="${4:?expected cache, io or remaining}"
    out="${5:?expected the inventory directory}"
    [[ -f "$out/meta.json" ]] || { echo "no inventory in $out: run the inventory phase first" >&2; exit 2; }
    case "$suite" in cache | io | remaining) ;; *) echo "unknown suite: $suite" >&2; exit 2 ;; esac
    if [[ "$(count "$out/$suite.tsv")" == 0 ]]; then
      if jq -e --arg g "$group" --arg s "$suite" '.groups[$g].allow_empty_suites | index($s)' "$groups_file" > /dev/null; then
        echo "group $group has no $suite tests (listed in allow_empty_suites); skipping"
        exit 0
      fi
      echo "group $group has no $suite tests and $groups_file does not allow that" >&2
      exit 1
    fi
    if [[ "$(count "$out/$suite.part.tsv")" == 0 ]]; then
      echo "shard $shard of group $group has none of the $(count "$out/$suite.tsv") $suite tests; skipping"
      exit 0
    fi
    echo "running $(count "$out/$suite.part.tsv") of $(count "$out/$suite.tsv") $suite tests (group $group, shard $shard)"
    exec scripts/run-contract-tests.sh --group "$group" --shard "$shard" "$suite"
    ;;
  *)
    echo "unknown mode: $mode" >&2
    exit 2
    ;;
esac
