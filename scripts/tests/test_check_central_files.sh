#!/usr/bin/env bash
# Tests for scripts/check-central-files.sh and scripts/ci-scope-gate.sh with
# hand-written change lists and a throwaway git repository; no cargo. The
# core-client reference check is tested in crates/core-client-refs.
set -euo pipefail
cd "$(dirname "$0")/../.."
central="$PWD/scripts/check-central-files.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

fails=0
expect() { # <ok|fail> <description> <command...>
  local want=$1 what=$2
  shift 2
  if "$@" > "$work/out" 2>&1; then got=ok; else got=fail; fi
  if [[ "$got" != "$want" ]]; then
    echo "FAIL: $what (wanted $want, got $got)" >&2
    sed 's/^/  | /' "$work/out" >&2
    fails=$((fails + 1))
  fi
}
files() { printf '%s\n' "$@" > "$work/files"; }

"$central" --check-data > /dev/null || { echo "FAIL: the data files are inconsistent" >&2; exit 1; }

# O-I owns composition; the three observation workers own disjoint leaves.
files crates/workbench-host/src/areas/browser/observation/source.rs
expect ok "O-S owns the source leaf" "$central" --scope pr16bos --files-from "$work/files"
expect fail "O-H cannot edit the source leaf" "$central" --scope pr16boh --files-from "$work/files"
files crates/workbench-host/src/areas/browser/observation/service.rs
expect ok "O-H owns the service leaf" "$central" --scope pr16boh --files-from "$work/files"
expect fail "O-S cannot edit the service leaf" "$central" --scope pr16bos --files-from "$work/files"
files crates/gui/src/core_client/features/browser/observation.rs
expect ok "O-G owns the GUI observation leaf" "$central" --scope pr16bog --files-from "$work/files"
expect fail "B cannot edit O-G" "$central" --scope pr16bb --files-from "$work/files"
files crates/gui/src/core_client/features/browser.rs
expect fail "B cannot edit the browser composition root" "$central" --scope pr16bb --files-from "$work/files"
expect fail "O-G cannot edit the browser composition root" "$central" --scope pr16bog --files-from "$work/files"
expect ok "O-I owns the browser composition root" "$central" --scope integration --files-from "$work/files"

files crates/browser/src/agent/connection.rs crates/runtime/src/agent_loop.rs crates/runtime/src/agent_loop/browser_observation_tests.rs .github/workflows/ci.yml
expect ok "O-S narrow approved exceptions" "$central" --scope pr16bos --files-from "$work/files"
expect fail "O-G cannot borrow O-S exceptions" "$central" --scope pr16bog --files-from "$work/files"
files crates/runtime/src/lib.rs crates/workbench-host/src/ingress.rs
expect fail "O-S exceptions do not widen composition ownership" "$central" --scope pr16bos --files-from "$work/files"

# A feature PR inside its own area passes.
files crates/workbench-host/src/areas/settings.rs \
  crates/workbench-host/src/areas/settings/save.rs \
  crates/workbench-protocol/src/namespaces/settings.rs \
  crates/workbench-protocol/tests/golden/areas/settings.json \
  crates/gui/src/core_client/features/auth.rs \
  crates/evorch-core/tests/core_settings_cache.rs
expect ok "pr10 changes only its own files" "$central" --scope pr10 --files-from "$work/files"

# A central file is refused whatever the scope would allow.
for central_file in crates/workbench-host/src/areas/mod.rs Cargo.lock \
  crates/workbench-protocol/src/namespaces/mod.rs crates/gui/src/core_client/features/mod.rs \
  scripts/contract-test-filters.sh crates/evorch/src/main.rs \
  scripts/ci-test-groups.json; do
  files crates/workbench-host/src/areas/settings.rs "$central_file"
  expect fail "$central_file is central" "$central" --scope pr10 --files-from "$work/files"
done
# The shared golden files are central, the golden files of an area are not.
files crates/workbench-protocol/tests/golden/commands.json
expect fail "a shared golden file is central" "$central" --scope pr10 --files-from "$work/files"

# Another PR's area is out of scope; so is an unrelated file.
files crates/workbench-host/src/areas/memory.rs
expect fail "pr10 does not own memory" "$central" --scope pr10 --files-from "$work/files"
expect ok "pr13 owns memory" "$central" --scope pr13 --files-from "$work/files"
files crates/runtime/src/lib.rs
expect fail "a file outside every scope" "$central" --scope pr13 --files-from "$work/files"

# 17d and 17e do not share a parser.
files crates/evorch/src/backfill.rs
expect ok "17d owns the online backfill" "$central" --scope pr17d --files-from "$work/files"
expect fail "17e does not own the online backfill" "$central" --scope pr17e --files-from "$work/files"
files crates/evorch/src/maintenance.rs
expect ok "17e owns the offline tool" "$central" --scope pr17e --files-from "$work/files"
expect fail "17d does not own the offline tool" "$central" --scope pr17d --files-from "$work/files"

# An integration PR may change central files.
files Cargo.lock crates/workbench-host/src/areas/mod.rs
expect ok "an integration PR may change central files" "$central" --scope integration --files-from "$work/files"
expect fail "an unknown scope" "$central" --scope nonsense --files-from "$work/files"

# I-14: pr14 owns the storage's management module and its tests, and nothing else
# of the storage; the writer stays central to no one but an integration PR.
files crates/storage/src/management.rs crates/storage/tests/management_cas.rs \
  crates/workbench-host/tests/management_cas_parity.rs crates/workbench-host/src/areas/memory.rs
expect ok "pr14 owns the management module and its tests" "$central" --scope pr14 --files-from "$work/files"
files crates/storage/src/writer.rs
expect fail "pr14 does not own the writer" "$central" --scope pr14 --files-from "$work/files"
files crates/storage/src/management.rs
expect fail "pr10 does not own the management module" "$central" --scope pr10 --files-from "$work/files"
files crates/evorch-core/tests/core_pr_cache.rs
expect ok "pr15 owns its cache target" "$central" --scope pr15 --files-from "$work/files"
expect fail "pr14 does not own the pr15 cache target" "$central" --scope pr14 --files-from "$work/files"
files crates/evorch-core/tests/core_eval_cache.rs
expect ok "pr17c owns its cache target" "$central" --scope pr17c --files-from "$work/files"
expect fail "pr15 does not own the pr17c cache target" "$central" --scope pr15 --files-from "$work/files"
# F2's approved exception grants no access to other central files or scopes.
files crates/workbench-protocol/src/namespaces/mod.rs crates/workbench-protocol/tests/namespaces.rs scripts/feature-scopes.txt \
  scripts/check-central-files.sh scripts/tests/test_check_central_files.sh
expect ok "F2 owns the macro cleanup and its gate configuration" "$central" --scope pr16a2 --files-from "$work/files"
expect fail "another feature has no F2 central exception" "$central" --scope pr10 --files-from "$work/files"
files crates/workbench-host/src/ingress.rs crates/workbench-host/src/areas/mod.rs
expect ok "F2 may add the renderer grace accessor and its delegate" "$central" --scope pr16a2 --files-from "$work/files"
expect fail "other scopes cannot change the host ingress or area registry" "$central" --scope pr10 --files-from "$work/files"
files crates/workbench-host/Cargo.toml
expect ok "F2 may promote the host async-trait dependency" "$central" --scope pr16a2 --files-from "$work/files"
expect fail "other scopes cannot change the host manifest" "$central" --scope pr10 --files-from "$work/files"
files crates/storage/src/artifact_refs.rs crates/storage/src/repo/artifact_refs.rs crates/storage/tests/renderer_artifact_refs.rs
expect ok "F2 owns only the thread-scoped artifact reads" "$central" --scope pr16a2 --files-from "$work/files"
files crates/storage/src/writer.rs
expect fail "F2 may not change the storage writer" "$central" --scope pr16a2 --files-from "$work/files"
# The GUI feature files and CLI are still outside pr16a2.
files crates/workbench-host/src/areas/artifacts.rs crates/workbench-host/src/areas/renderer/mod.rs crates/renderer-ipc/src/client.rs crates/evorch-core/tests/core_renderer_cache.rs crates/runtime/src/artifacts/inspect.rs
expect ok "pr16a2 owns the artifact and renderer leaves" "$central" --scope pr16a2 --files-from "$work/files"
expect fail "pr15 does not own the artifact leaf" "$central" --scope pr15 --files-from "$work/files"
for central_file in crates/workbench-host/src/areas/registry_other.rs crates/workbench-host/src/ingress/namespaces/mod.rs crates/runtime/Cargo.toml scripts/ci-test-groups.json; do
  files crates/workbench-host/src/areas/artifacts.rs "$central_file"
  expect fail "pr16a2 may not change the central $central_file" "$central" --scope pr16a2 --files-from "$work/files"
done
files crates/workbench-client/src/transport.rs crates/workbench-client/tests/unix_socket.rs docs/renderer-measurement.md docs/single-core-integration.md crates/evorch-core/src/unix.rs crates/evorch-core/src/renderer.rs
expect ok "pr16a2 owns the frame receive path, the UDS seam and its two documents" "$central" --scope pr16a2 --files-from "$work/files"
for other in docs/providers.md docs/browser.md crates/workbench-client/src/subscription.rs crates/workbench-client/src/client.rs crates/workbench-client/tests/subscriptions.rs crates/workbench-client/tests/connection.rs; do
  files "$other"
  expect fail "pr16a2 does not own $other" "$central" --scope pr16a2 --files-from "$work/files"
done
# Reconnection retention is wired only in F2's two exact lifecycle leaves.
files crates/evorch-core/src/lifecycle.rs crates/evorch-core/src/lifecycle/idle/renderer.rs
expect ok "pr16a2 owns the renderer idle seam and its exact test leaf" "$central" --scope pr16a2 --files-from "$work/files"
expect fail "pr15 does not own the renderer idle seam" "$central" --scope pr15 --files-from "$work/files"
for other in crates/evorch-core/src/lifecycle/signals.rs crates/evorch-core/src/lifecycle/idle/activity.rs crates/evorch-core/src/lifecycle/idle/other.rs crates/workbench-protocol/src/handshake.rs; do
  files "$other"
  expect fail "pr16a2 does not own $other" "$central" --scope pr16a2 --files-from "$work/files"
done
files crates/workbench-protocol/src/handshake.rs
expect ok "only integration may register the renderer wire reason" "$central" --scope integration --files-from "$work/files"
files crates/workbench-client/src/lib.rs crates/gui/src/html_preview/agent_pages.rs crates/gui/tests/artifact_pages_cache.rs
expect ok "F3 owns the client module wiring and the two mechanical open_artifact leaves" "$central" --scope pr16a2 --files-from "$work/files"
expect fail "pr15 does not own the F3 wiring leaves" "$central" --scope pr15 --files-from "$work/files"
for other in crates/gui/src/html_preview/mod.rs crates/gui/src/html_preview/other.rs crates/gui/tests/other.rs crates/workbench-client/src/client.rs; do
  files "$other"
  expect fail "pr16a2 does not own $other" "$central" --scope pr16a2 --files-from "$work/files"
done
files crates/gui/src/core_client/features/artifacts.rs
expect fail "pr16a2 does not own the GUI feature file" "$central" --scope pr16a2 --files-from "$work/files"
files crates/evorch/src/cli/inspect.rs
expect fail "pr16a2 does not own the CLI" "$central" --scope pr16a2 --files-from "$work/files"
# The Evaluator's eval_start spawns `evorch eval run` with `--db`, which 17c
# removes from the CLI: pr17c owns the spawn site and its test, and not the rest
# of the eval crate.
files crates/eval/src/evaluator/groups.rs crates/eval/src/evaluator/tests/start.rs
expect ok "pr17c owns the Evaluator's spawn of eval run and its test" "$central" --scope pr17c --files-from "$work/files"
expect fail "pr15 does not own the Evaluator's spawn of eval run" "$central" --scope pr15 --files-from "$work/files"
files crates/eval/src/driver/tests.rs crates/eval/src/runner/tests.rs
expect ok "pr17c owns the tests of the replay driver and runner" "$central" --scope pr17c --files-from "$work/files"
files crates/eval/src/verify.rs
expect ok "pr17c owns the verification command runner" "$central" --scope pr17c --files-from "$work/files"
expect fail "pr15 does not own the verification command runner" "$central" --scope pr15 --files-from "$work/files"
files crates/eval/src/lib.rs
expect fail "pr17c does not own the rest of the eval crate" "$central" --scope pr17c --files-from "$work/files"

# Every binary the cache gate names is a test target that exists: nextest rejects
# a binary name that matches nothing, and a registered placeholder is a file.
# `filter_binaries <file>` prints the `binary(=...)` names of a filter file's
# CACHE_CONTRACT_FILTER; `missing_targets` prints those without a target.
filter_binaries() {
  (
    # shellcheck disable=SC1090
    . "$1"
    printf '%s\n' "$CACHE_CONTRACT_FILTER" | grep -o 'binary(=[^)]*)' | sed -e 's/^binary(=//' -e 's/)$//' | sort -u
  )
}
missing_targets() {
  local name found
  while IFS= read -r name; do
    found=$(git ls-files --cached --others --exclude-standard "crates/*/tests/$name.rs" "crates/*/tests/$name/main.rs")
    [[ -n "$found" ]] || echo "$name"
  done < <(filter_binaries "$1")
}
no_missing_targets() { [[ -z "$(missing_targets "$1")" ]]; }
expect ok "the cache gate names core_pr_cache" bash -c "grep -q 'binary(=core_pr_cache)' scripts/contract-test-filters.sh"
expect ok "core_pr_cache has a test target" test -f crates/evorch-core/tests/core_pr_cache.rs
expect ok "the cache gate names core_eval_cache" bash -c "grep -q 'binary(=core_eval_cache)' scripts/contract-test-filters.sh"
expect ok "core_eval_cache has a test target" test -f crates/evorch-core/tests/core_eval_cache.rs
expect ok "the cache gate names core_cli_run_cache" bash -c "grep -q 'binary(=core_cli_run_cache)' scripts/contract-test-filters.sh"
expect ok "core_cli_run_cache has a test target" test -f crates/evorch-core/tests/core_cli_run_cache.rs
expect ok "the cache gate names core_renderer_cache" bash -c "grep -q 'binary(=core_renderer_cache)' scripts/contract-test-filters.sh"
expect ok "core_renderer_cache has a test target" test -f crates/evorch-core/tests/core_renderer_cache.rs
expect ok "every binary of the cache gate has a test target" no_missing_targets scripts/contract-test-filters.sh
printf "CACHE_CONTRACT_FILTER='package(=x) & binary(=no_such_cache_target)'\n" > "$work/bad-filter.sh"
expect fail "a binary without a target is detected" no_missing_targets "$work/bad-filter.sh"

# A scope pattern is not expanded against the files that exist. The sandbox is a
# git repository with real files under the patterns: an unquoted loop would
# replace `offline*` with the names it finds and refuse the nested ones.
repo="$work/repo"
mkdir -p "$repo/scripts" "$repo/crates/storage/src/maintenance/offline" "$repo/docs/config"
cp scripts/check-central-files.sh scripts/central-files.txt scripts/feature-scopes.txt "$repo/scripts/"
printf x > "$repo/crates/storage/src/maintenance/offline.rs"
printf x > "$repo/crates/storage/src/maintenance/offline/lease_tests.rs"
printf x > "$repo/docs/config/evorch-core.md"
sandbox_git() { git -C "$repo" -c user.name=fixture -c user.email=fixture@example.invalid -c commit.gpgsign=false "$@"; }
sandbox_git init -q -b main
sandbox_git add -A
sandbox_git commit -q -m base
sandbox_git checkout -q -b feature
printf y > "$repo/crates/storage/src/maintenance/offline/lease_tests.rs"
printf y > "$repo/docs/config/evorch-core.md"
sandbox_git commit -q -am change
sandbox_check() { (cd "$repo" && scripts/check-central-files.sh "$@"); }
expect ok "a nested file under a glob scope is in scope (pr17e)" sandbox_check --scope pr17e --base main
expect fail "pr10 does not own the nested offline file" sandbox_check --scope pr10 --base main
# A base that cannot be diffed is an error, never an empty change set.
expect fail "an unknown base is an error" sandbox_check --scope pr17e --base no-such-ref
expect fail "an unknown base is an error for an integration PR" sandbox_check --scope integration --base no-such-ref
expect fail "an unreadable file list is an error" sandbox_check --scope pr17e --files-from "$work/missing"
expect fail "an unknown scope in a real diff" sandbox_check --scope nonsense --base main

# The CI gate derives the scope from the head branch.
gate="$PWD/scripts/ci-scope-gate.sh"
scope_is() { [[ "$("$gate" --scope-of "$1")" == "$2" ]]; }
expect ok "feat/pr10-settings is pr10" scope_is feat/pr10-settings pr10
expect ok "feat/pr17e is pr17e" scope_is feat/pr17e pr17e
expect ok "feat/pr16a1-measure is pr16a1" scope_is feat/pr16a1-measure pr16a1
expect ok "feat/s0-integration is an integration PR" scope_is feat/s0-integration integration
expect ok "feat/integration-s1 is an integration PR" scope_is feat/integration-s1 integration
expect ok "a branch without a scope is not checked" scope_is fix/gui-paging ""
expect ok "a branch without a type is not checked" scope_is pr10 ""
cp scripts/ci-scope-gate.sh "$repo/scripts/"
sandbox_git branch -m feat/pr10-settings
expect fail "the gate refuses a pr10 branch that changes pr17e files" bash -c "cd '$repo' && scripts/ci-scope-gate.sh --head-ref feat/pr10-settings --base main"
expect ok "the gate accepts a pr17e branch for the same files" bash -c "cd '$repo' && scripts/ci-scope-gate.sh --head-ref feat/pr17e-offline --base main"
expect ok "the gate leaves an unscoped branch alone" bash -c "cd '$repo' && scripts/ci-scope-gate.sh --head-ref fix/other --base main"
expect fail "the gate fails when the base is missing" bash -c "cd '$repo' && scripts/ci-scope-gate.sh --head-ref feat/pr17e-offline --base origin/main"

# I0: the next-wave scopes. Each owner passes on its leaves (`foo.rs` and `foo/*`
# are separate registrations), and refuses the central files, the shared contract
# and every other owner's files; no scope has a central-file exception.
C=crates/evorch-core
T=crates/test-support
H=crates/workbench-host
X=crates/core-client-e2e
G=crates/gui
next_wave_scopes=(pr16a2f4p pr16a2f4e pr16bov1 pr16bov2 pr16bov3)
owned_samples=(
  "$T/src/product_exec.rs $T/src/product_exec/fixture.rs $C/shared-test-support/product_exec.rs $C/shared-test-support/product_exec/launch.rs $C/tests/renderer_support/product_exec.rs $C/tests/renderer_support/product_exec/launch.rs $C/tests/f4_product_fixture.rs $C/measurements/f4-product/manifest.json $C/measurements/f4-product/report/summary.md"
  "$C/tests/f4_product_exec.rs $C/tests/f4_product_exec/handoff.rs $C/tests/f4_product_cef.rs $C/tests/f4_product_cache.rs"
  "$C/tests/agent_observation_transport.rs $C/tests/agent_observation_transport/auth.rs $H/tests/agent_observation_lifecycle.rs $H/src/areas/browser/observation/source.rs $H/src/areas/browser/observation/source/lifecycle.rs"
  "$C/tests/agent_observation_chromium.rs $X/tests/agent_observation_gui.rs $X/tests/agent_observation_gui/frames.rs $G/src/core_client/features/browser/observation.rs $G/src/core_client/features/browser/observation/view.rs"
  "$C/tests/agent_observation_restart.rs $C/tests/agent_observation_restart/reconnect.rs $C/tests/agent_observation_cache.rs"
)
for i in "${!next_wave_scopes[@]}"; do
  read -ra owned <<< "${owned_samples[$i]}"
  files "${owned[@]}"
  expect ok "${next_wave_scopes[$i]} owns its registered leaves" "$central" --scope "${next_wave_scopes[$i]}" --files-from "$work/files"
  for j in "${!next_wave_scopes[@]}"; do
    [[ "$i" != "$j" ]] || continue
    for one in "${owned[@]}"; do
      files "$one"
      expect fail "${next_wave_scopes[$j]} does not own $one" "$central" --scope "${next_wave_scopes[$j]}" --files-from "$work/files"
    done
  done
  for refused in Cargo.lock Cargo.toml crates/evorch-core/Cargo.toml crates/test-support/Cargo.toml crates/core-client-e2e/Cargo.toml \
    .github/workflows/ci.yml scripts/feature-scopes.txt scripts/contract-test-filters.sh scripts/check-central-files.sh \
    scripts/tests/test_check_central_files.sh scripts/ci-test-groups.json scripts/product-artifact.sh \
    crates/test-support/src/lib.rs crates/test-support/src/product_artifact.rs \
    crates/evorch-core/tests/core_cache.rs crates/evorch-core/src/main.rs crates/workbench-host/src/ingress.rs; do
    files "${owned[0]}" "$refused"
    expect fail "${next_wave_scopes[$i]} may not change $refused" "$central" --scope "${next_wave_scopes[$i]}" --files-from "$work/files"
  done
done
# A neighbour of a registered leaf is not part of it.
files $C/tests/agent_observation_transport_extra.rs
expect fail "O-V1 does not own a sibling of its transport leaf" "$central" --scope pr16bov1 --files-from "$work/files"
files $C/tests/f4_product_cache/other.rs
expect fail "F4-E owns the cache binary file only, not a directory of it" "$central" --scope pr16a2f4e --files-from "$work/files"
# The I0 integration PR itself is an integration PR and may change them all.
files scripts/feature-scopes.txt .github/workflows/ci.yml scripts/contract-test-filters.sh crates/test-support/src/lib.rs crates/test-support/Cargo.toml
expect ok "an integration PR may register the next wave" "$central" --scope integration --files-from "$work/files"
# Branch names classify into the intended scopes.
expect ok "test/pr16a2f4p-product-fixture is pr16a2f4p" scope_is test/pr16a2f4p-product-fixture pr16a2f4p
expect ok "test/pr16a2f4e-product-exec is pr16a2f4e" scope_is test/pr16a2f4e-product-exec pr16a2f4e
expect ok "test/pr16bov1-observation-transport is pr16bov1" scope_is test/pr16bov1-observation-transport pr16bov1
expect ok "test/pr16bov2-observation-chromium-gui is pr16bov2" scope_is test/pr16bov2-observation-chromium-gui pr16bov2
expect ok "test/pr16bov3-observation-restart-cache is pr16bov3" scope_is test/pr16bov3-observation-restart-cache pr16bov3
expect ok "chore/integration-next-wave-registration is an integration PR" scope_is chore/integration-next-wave-registration integration

# I0: the two independent cache binaries and the ignored tests CI selects. A
# nextest filter that matches nothing fails, so every target exists, every ignored
# one is ignored, and ci.yml selects it with `--run-ignored only --no-tests fail`.
for cache_binary in f4_product_cache agent_observation_cache; do
  expect ok "the cache gate names $cache_binary" bash -c "grep -q 'binary(=$cache_binary)' scripts/contract-test-filters.sh"
  expect ok "$cache_binary has its own test target" test -f "$C/tests/$cache_binary.rs"
  expect ok "$cache_binary is a binary no other cache target shares" bash -c "[[ \$(grep -c 'binary(=$cache_binary)' scripts/contract-test-filters.sh) == 1 ]]"
done
ci=.github/workflows/ci.yml
ci_selects_ignored() { # <test target>: one nextest line selecting it as an ignored test that must exist
  local line
  line=$(grep -E '(nextest|guard-cargo\.sh) run .*--test "?'"$1"'"?( |$)' "$ci") || return 1
  grep -q -e '--run-ignored only' <<< "$line" && grep -q -e '--no-tests fail' <<< "$line"
}
is_ignored_target() { grep -q '^#\[ignore' "$1"; }
for ignored in "$C/tests/f4_product_cef.rs" "$C/tests/agent_observation_restart.rs" "$C/tests/agent_observation_chromium.rs" "$X/tests/agent_observation_gui.rs"; do
  name=$(basename "$ignored" .rs)
  expect ok "$name is an ignored placeholder" is_ignored_target "$ignored"
  expect ok "ci.yml selects $name with --run-ignored only --no-tests fail" ci_selects_ignored "$name"
done
host_lifecycle=$(sed -n 's/^ *HOST_LIFECYCLE_TEST: *//p' "$ci")
expect ok "ci.yml names the host lifecycle test once" test -n "$host_lifecycle" -a "$(printf '%s\n' "$host_lifecycle" | wc -l)" = 1
expect ok "the host lifecycle test exists and is ignored" is_ignored_target "$H/tests/$host_lifecycle.rs"
expect ok "ci.yml selects the host lifecycle test with --run-ignored only --no-tests fail" ci_selects_ignored '\$HOST_LIFECYCLE_TEST'
# D-3a2-A: explicitly register both new binaries and lib-unit cells in the
# runtime remainder. Exercise the same no-cargo inventory guard CI calls.
source scripts/contract-test-filters.sh
watchdog_inventory="$work/watchdog-inventory"
mkdir "$watchdog_inventory"
printf 'evorch-core\tevorch-core\trestart::shutdown_watchdog_tests::cell\tfalse\nevorch-core\tevorch-core::planned_shutdown_watchdog\tcold_negative\tfalse\nevorch-core\tevorch-core::planned_shutdown_watchdog_composition\tcold_flush\tfalse\n' > "$work/watchdog-rows"
cp "$work/watchdog-rows" "$watchdog_inventory/remaining.tsv"
: > "$watchdog_inventory/cache.tsv"
: > "$watchdog_inventory/io.tsv"
expect ok "watchdog cells remain in the existing remaining suite" check_planned_shutdown_watchdog_inventory "$watchdog_inventory"
for required in 'restart::shutdown_watchdog_tests::' $'::planned_shutdown_watchdog\t' $'::planned_shutdown_watchdog_composition\t'; do
  grep -v -F "$required" "$work/watchdog-rows" > "$watchdog_inventory/remaining.tsv"
  expect fail "missing watchdog inventory selection: $required" check_planned_shutdown_watchdog_inventory "$watchdog_inventory"
done
sed 's/false/true/g' "$work/watchdog-rows" > "$watchdog_inventory/remaining.tsv"
expect fail "ignored watchdog cells cannot satisfy default selection" check_planned_shutdown_watchdog_inventory "$watchdog_inventory"
cp "$work/watchdog-rows" "$watchdog_inventory/remaining.tsv"
for suite in cache io; do
  cp "$work/watchdog-rows" "$watchdog_inventory/$suite.tsv"
  expect fail "watchdog cells cannot enter $suite" check_planned_shutdown_watchdog_inventory "$watchdog_inventory"
  : > "$watchdog_inventory/$suite.tsv"
done
expect ok "watchdog guard is wired into CI's existing inventory step" bash -c "grep -q 'check_planned_shutdown_watchdog_inventory \"\$EVORCH_TEST_INVENTORY_DIR\"' $ci"

# Package inventory is the existing ci-test-groups registration; the explicit
# guard must fail if the public UDS binary is absent, ignored or misclassified.
expect ok "public restart contracts belong to the runtime package inventory" jq -e '.groups.runtime.packages | index("evorch-core") != null' scripts/ci-test-groups.json
expect ok "public restart contracts have bootstrap support" jq -e '.groups.runtime.features | index("evorch-core/bootstrap-test-support") != null' scripts/ci-test-groups.json
public_inventory="$work/public-inventory"
mkdir -p "$public_inventory"
printf '%s\n' 'evorch-core	evorch-core::public_restart_contracts	contract_0	false' 'evorch-core	evorch-core::public_restart_contracts	contract_1	false' 'evorch-core	evorch-core::public_restart_contracts	contract_2	false' 'evorch-core	evorch-core::public_restart_contracts	contract_3	false' 'evorch-core	evorch-core::public_restart_contracts	contract_4	false' 'evorch-core	evorch-core::public_restart_contracts	contract_5	false' 'evorch-core	evorch-core::public_restart_contracts	contract_6	false' 'evorch-core	evorch-core::public_restart_contracts	contract_7	false' > "$public_inventory/remaining.tsv"
: > "$public_inventory/cache.tsv"
: > "$public_inventory/io.tsv"
expect ok "all eight public restart contracts remain selected" check_public_restart_contracts_inventory "$public_inventory"
cp "$public_inventory/remaining.tsv" "$public_inventory/cache.tsv"
expect fail "public authority contracts cannot enter cache" check_public_restart_contracts_inventory "$public_inventory"
: > "$public_inventory/cache.tsv"
cp "$public_inventory/remaining.tsv" "$public_inventory/io.tsv"
expect fail "public authority contracts cannot enter I/O" check_public_restart_contracts_inventory "$public_inventory"
: > "$public_inventory/io.tsv"
: > "$public_inventory/remaining.tsv"
expect fail "a missing public restart contract inventory is rejected" check_public_restart_contracts_inventory "$public_inventory"
expect ok "public guard is wired into CI inventory" grep -q check_public_restart_contracts_inventory "$ci"
watchdog_manifest_is_feature_gated() {
  local block
  block=$(grep -A4 '^name = "planned_shutdown_watchdog_composition"$' "$1")
  grep -qx 'harness = false' <<< "$block" &&
    grep -Fxq 'required-features = ["bootstrap-test-support"]' <<< "$block"
}
expect ok "dedicated watchdog target stays feature-gated and harness-free" watchdog_manifest_is_feature_gated crates/evorch-core/Cargo.toml
printf 'name = "planned_shutdown_watchdog_composition"\nharness = false\nrequired-features = ["wrong-feature"]\n' > "$work/wrong-watchdog-manifest"
expect fail "another feature cannot enable the watchdog target" watchdog_manifest_is_feature_gated "$work/wrong-watchdog-manifest"

# Every job that runs a product test depends on the sealed artifact job and
# installs it; the artifact job builds it.
ci_job() { awk -v job="$1" '$0 ~ "^  " job ":$" { on = 1; next } on && /^  [a-z0-9-]+:$/ { exit } on' "$ci"; }
job_has() { ci_job "$1" | grep -q -e "$2"; }
# A job that runs a product test is not ordered after the artifact job by `needs`:
# it waits for that job's success and installs the artifact before its first
# guard-cargo.sh / run-test-group.sh run (comment lines are ignored).
product_gate_in() { # <ci.yml> <job>
  awk -v job="$2" '
    $0 ~ "^  " job ":$" { on = 1; next }
    on && /^  [a-z0-9-]+:$/ { exit }
    on && $0 !~ /^[[:space:]]*#/ {
      n++
      if ($0 ~ /scripts\/wait-product-artifact\.sh$/ && !wait) wait = n
      if ($0 ~ /scripts\/product-artifact\.sh install/ && !install) install = n
      if ($0 ~ /(guard-cargo\.sh|run-test-group\.sh) run( |$)/ && !first) first = n
    }
    END { exit !(wait && install && first && wait < install && install < first) }
  ' "$1"
}
sed '/scripts\/wait-product-artifact\.sh$/d' "$ci" > "$work/ci-no-wait"
sed '/scripts\/product-artifact\.sh install/d' "$ci" > "$work/ci-no-install"
sed '/- name: Wait for the product artifact job/i\      - run: scripts/guard-cargo.sh run -p early' "$ci" > "$work/ci-run-first"
for product_job in tests browser-e2e cef-e2e; do
  expect ok "$product_job waits for the producer and installs the sealed product artifact before its first test run" product_gate_in "$ci" "$product_job"
  expect fail "$product_job without the wait for the producer" product_gate_in "$work/ci-no-wait" "$product_job"
  expect fail "$product_job without the install" product_gate_in "$work/ci-no-install" "$product_job"
  expect fail "$product_job running a test before the wait and install" product_gate_in "$work/ci-run-first" "$product_job"
done
expect ok "the product artifact job builds and seals it" job_has product-artifact 'scripts/product-artifact.sh build'
expect ok "the aggregate gate requires the product artifact job" bash -c "grep -q 'otel, product-artifact, tests\]' $ci"
expect fail "a job that runs no product test does not install the artifact" job_has lint 'scripts/product-artifact.sh install'

# 5a-I: the public restart management leaves (R, Q1, Q2, CN, CW). Each owner passes on
# its leaves (`foo.rs` and `foo/*` are separate registrations) and refuses every
# other owner's files, the central files and the shared contracts; no scope has a
# central-file exception.
W=crates/workbench-web
S=crates/storage
PRO=crates/workbench-protocol/src/namespaces/lifecycle
HOL=$H/src/ingress/namespaces/lifecycle
leaf_scopes=(pr07b5ar pr07b5aq1 pr07b5aq2 pr07b5acn pr07b5acw)
leaf_samples=(
  "$C/src/restart.rs $C/src/restart/public.rs $C/src/restart/public/coordinator.rs $HOL/restart.rs $HOL/restart/targets.rs $PRO/restart.rs $PRO/restart/request.rs $C/tests/public_restart_coordinator.rs $C/tests/public_restart_coordinator/capture.rs"
  "$HOL/quarantine.rs $HOL/quarantine/list.rs $PRO/quarantine.rs $PRO/quarantine/detail.rs $S/src/recovery/quarantine.rs $S/src/recovery/quarantine/discard.rs $H/tests/quarantine_admin.rs $H/tests/quarantine_admin/cas.rs $S/tests/quarantine_admin.rs $S/tests/quarantine_admin/cas.rs"
  "$HOL/owned_graphs.rs $HOL/owned_graphs/list.rs $H/src/bootstrap/recovery_disposal.rs $H/src/bootstrap/recovery_disposal/cas.rs crates/runtime/src/runtime/recovery_disposal.rs crates/runtime/src/runtime/recovery_disposal/phase.rs $C/src/recovery_admin.rs $C/src/recovery_admin/resume.rs $C/tests/recovery_disposal.rs $S/src/recovery/owned_graphs.rs $S/src/recovery/owned_graphs/cas.rs $H/tests/owned_graphs_admin.rs $H/tests/owned_graphs_admin/predicate.rs $S/tests/owned_graphs_admin.rs $H/tests/recovery_disposal.rs crates/runtime/tests/recovery_disposal.rs"
  "crates/core-launcher/src/handoff_diagnostics.rs crates/core-launcher/src/handoff_diagnostics/report.rs crates/workbench-client/src/restart.rs crates/workbench-client/src/restart/confirmation.rs crates/evorch/src/cli/core_admin.rs crates/evorch/src/cli/core_admin/restart.rs $G/src/core_client/restart.rs $G/src/core_client/restart/view.rs crates/workbench-client/tests/restart_admin.rs crates/workbench-client/tests/restart_admin/confirm.rs crates/evorch/tests/restart_admin.rs $C/tests/cli_continuity_cache.rs"
  "$W/src/server.rs $W/src/server/restart.rs $W/src/sse.rs $W/src/sse/admin.rs $W/src/backend.rs $W/src/assets.rs $W/src/restart.rs $W/src/restart/status.rs $W/assets/js/app.js $W/assets/js/api.js $W/assets/js/store.js $W/assets/js/sse.js $W/assets/js/views.js $W/assets/js/restart.js $W/assets/js/restart/view.js $W/tests/public_restart.rs $W/tests/public_restart/flow.rs $W/tests/public_restart_chromium.rs $W/tests/public_restart_chromium/page.rs"
)
for i in "${!leaf_scopes[@]}"; do
  read -ra owned <<< "${leaf_samples[$i]}"
  for one in "${owned[@]}"; do
    files "$one"
    expect ok "${leaf_scopes[$i]} owns $one" "$central" --scope "${leaf_scopes[$i]}" --files-from "$work/files"
    for j in "${!leaf_scopes[@]}"; do
      [[ "$i" != "$j" ]] || continue
      expect fail "${leaf_scopes[$j]} does not own $one" "$central" --scope "${leaf_scopes[$j]}" --files-from "$work/files"
    done
  done
  for refused in Cargo.lock Cargo.toml crates/evorch-core/Cargo.toml crates/workbench-authority/Cargo.toml crates/workbench-web/Cargo.toml \
    .github/workflows/ci.yml scripts/feature-scopes.txt scripts/contract-test-filters.sh scripts/ci-test-groups.json \
    scripts/check-central-files.sh scripts/check-workbench-boundaries.sh scripts/workbench-boundary-allowlist.txt scripts/tests/test_check_central_files.sh \
    crates/workbench-protocol/src/envelope.rs crates/workbench-protocol/src/namespaces/mod.rs crates/workbench-host/src/ingress.rs; do
    files "${owned[0]}" "$refused"
    expect fail "${leaf_scopes[$i]} may not change $refused" "$central" --scope "${leaf_scopes[$i]}" --files-from "$work/files"
  done
done
# public_restart_cache.rs is owned by 5a-V (integration), never by a leaf.
files $C/tests/public_restart_cache.rs
for sharing in pr07b5ar pr07b5acw; do
  expect fail "$sharing does not own the integration public restart cache binary" "$central" --scope "$sharing" --files-from "$work/files"
done
for other in pr07b5aq1 pr07b5aq2 pr07b5acn pr16a2f4e pr16bov3; do
  expect fail "$other does not own the public restart cache binary" "$central" --scope "$other" --files-from "$work/files"
done
# The shared contracts and the roots belong to no leaf scope (an integration PR changes
# them): the authority, the host admin service, the lifecycle roots, the audit storage,
# the ingress and the neighbours of the registered leaves.
shared_contracts=(
  crates/workbench-authority/src/lib.rs crates/workbench-authority/src/tests.rs
  $H/src/admin/mod.rs $H/src/admin/audit.rs $H/src/admin/bindings.rs
  crates/workbench-protocol/src/namespaces/lifecycle.rs $PRO/contracts.rs
  crates/storage/src/admin_audit.rs crates/storage/src/admin_audit/writer.rs crates/storage/src/recovery.rs
  crates/evorch-core/src/unix.rs $C/tests/public_restart_contracts.rs $H/tests/admin_contracts.rs $W/tests/public_restart_contracts.rs
  $W/src/api.rs $W/src/restart_observer.rs
  crates/workbench-protocol/tests/golden/areas/lifecycle.json crates/workbench-protocol/tests/common/areas/lifecycle.rs
)
for scope in "${leaf_scopes[@]}"; do
  for shared in "${shared_contracts[@]}"; do
    files "$shared"
    expect fail "$scope does not own the shared contract $shared" "$central" --scope "$scope" --files-from "$work/files"
  done
done
expect ok "an integration PR may change the shared contracts" "$central" --scope integration --files-from "$work/files"
# R and Q never edit a root dispatch: the lifecycle roots are in none of their scopes
# (pr18a and pr16a2 keep theirs; the roots of the new leaves are not theirs either).
for root in $H/src/ingress/namespaces/lifecycle.rs crates/workbench-protocol/src/namespaces/lifecycle.rs $C/src/lifecycle.rs; do
  files "$root"
  for scope in pr07b5ar pr07b5aq1 pr07b5aq2 pr07b5acn pr07b5acw; do
    expect fail "$scope does not edit the root dispatch $root" "$central" --scope "$scope" --files-from "$work/files"
  done
done
files "$PRO/owned_graphs.rs" "$PRO/owned_graphs/view.rs"
expect fail "Q2 leaf cannot re-edit the protocol contract" "$central" --scope pr07b5aq2 --files-from "$work/files"
# envelope.rs is a central file and no scope has an exception for it.
envelope=crates/workbench-protocol/src/envelope.rs
expect fail "the scope data names no envelope.rs" bash -c "sed 's/#.*//' scripts/feature-scopes.txt | grep -q envelope"
expect fail "check-central-files.sh has no exception for envelope.rs" grep -q 'envelope' scripts/check-central-files.sh
files "$envelope"
while IFS= read -r every_scope; do
  expect fail "scope $every_scope has no exception for envelope.rs" "$central" --scope "$every_scope" --files-from "$work/files"
done < <(sed -e 's/#.*//' scripts/feature-scopes.txt | awk 'NF { print $1 }' | sort -u)
expect ok "an integration PR may change envelope.rs" "$central" --scope integration --files-from "$work/files"
# The I0 scopes this PR does not change keep their leaves (F4-E, O-V2, O-V3).
files $C/tests/f4_product_exec.rs $C/tests/f4_product_exec/handoff.rs $C/tests/f4_product_cef.rs $C/tests/f4_product_cache.rs
expect ok "F4-E keeps its leaves" "$central" --scope pr16a2f4e --files-from "$work/files"
files $C/tests/agent_observation_chromium.rs $X/tests/agent_observation_gui.rs $X/tests/agent_observation_gui/frames.rs $G/src/core_client/features/browser/observation.rs $G/src/core_client/features/browser/observation/view.rs
expect ok "O-V2 keeps its leaves" "$central" --scope pr16bov2 --files-from "$work/files"
files $C/tests/agent_observation_restart.rs $C/tests/agent_observation_restart/reconnect.rs $C/tests/agent_observation_cache.rs
expect ok "O-V3 keeps its leaves" "$central" --scope pr16bov3 --files-from "$work/files"
# Branch names classify into the scopes.
expect ok "feat/pr07b5ar-restart is pr07b5ar" scope_is feat/pr07b5ar-restart pr07b5ar
expect ok "feat/pr07b5aq1-quarantine is pr07b5aq1" scope_is feat/pr07b5aq1-quarantine pr07b5aq1
expect ok "feat/pr07b5aq2-owned-graphs is pr07b5aq2" scope_is feat/pr07b5aq2-owned-graphs pr07b5aq2
expect ok "feat/pr07b5acn-native is pr07b5acn" scope_is feat/pr07b5acn-native pr07b5acn
expect ok "feat/pr07b5acw-web is pr07b5acw" scope_is feat/pr07b5acw-web pr07b5acw
expect ok "test/pr16a2f4e-product-exec is pr16a2f4e" scope_is test/pr16a2f4e-product-exec pr16a2f4e
expect ok "test/pr16bov2-observation-gui is pr16bov2" scope_is test/pr16bov2-observation-gui pr16bov2
expect ok "test/pr16bov3-observation-restart is pr16bov3" scope_is test/pr16bov3-observation-restart pr16bov3
expect ok "feat/integration-07b5a-contracts is an integration PR" scope_is feat/integration-07b5a-contracts integration
expect ok "feat/integration-07b5a-contracts-registration is an integration PR" scope_is feat/integration-07b5a-contracts-registration integration

# 5a-I: the two independent cache binaries of the public restart management, the web
# placeholder CI selects as an ignored test, and the contract and test registrations.
for cache_binary in public_restart_cache cli_continuity_cache; do
  expect ok "the cache gate names $cache_binary" bash -c "grep -q 'binary(=$cache_binary)' scripts/contract-test-filters.sh"
  expect ok "$cache_binary has its own test target" test -f "$C/tests/$cache_binary.rs"
  expect ok "$cache_binary is a binary no other cache target shares" bash -c "[[ \$(grep -c 'binary(=$cache_binary)' scripts/contract-test-filters.sh) == 1 ]]"
done
expect ok "the web public restart browser placeholder is an ignored test" is_ignored_target "$W/tests/public_restart_chromium.rs"
expect ok "ci.yml selects public_restart_chromium with --run-ignored only --no-tests fail" ci_selects_ignored public_restart_chromium
expect ok "the web public restart placeholder exists" test -f "$W/tests/public_restart.rs"
expect ok "the web contract binary exists" test -f "$W/tests/public_restart_contracts.rs"
expect ok "the core contract binary exists" test -f "$C/tests/public_restart_contracts.rs"
# 5a-V V0-reg: explicit test-target registrations and the shared submodule roots.
for target in public_restart_exec public_restart_deny public_restart_product; do
  expect ok "evorch-core registers $target" grep -q -A1 "^name = \"$target\"$" "$C/Cargo.toml"
done
expect ok "core-client-e2e registers public_restart_native" grep -q -A1 '^name = "public_restart_native"$' "$X/Cargo.toml"
for target in \
  "$C/tests/public_restart_exec.rs" "$C/tests/public_restart_deny.rs" \
  "$C/tests/public_restart_product.rs" "$X/tests/public_restart_native.rs"; do
  expect ok "V0-reg placeholder $target exists" test -f "$target"
done
# 5a-V's split targets admit each planned leaf to its owner only. The shared
# module roots and V-cache's integration-owned binary remain outside every leaf.
v_scope_keys=(pr5avexec pr5avdeny pr5avprod pr07b5acn)
v_leaf_owners=(
  pr5avexec pr5avdeny pr5avprod pr07b5acn
)
v_leaf_paths=(
  "$C/tests/public_restart_exec.rs"
  "$C/tests/public_restart_deny.rs"
  "$C/tests/public_restart_product.rs"
  "$X/tests/public_restart_native.rs"
)
for i in "${!v_leaf_paths[@]}"; do
  owner=${v_leaf_owners[$i]}
  leaf=${v_leaf_paths[$i]}
  files "$leaf"
  expect ok "$owner owns its 5a-V leaf $leaf" "$central" --scope "$owner" --files-from "$work/files"
  for other in "${v_scope_keys[@]}"; do
    [[ "$other" != "$owner" ]] || continue
    expect fail "$other cannot edit $owner's 5a-V leaf $leaf" "$central" --scope "$other" --files-from "$work/files"
  done
done
for shared_root in \
  "$C/tests/public_restart_cache.rs" "$C/Cargo.toml" scripts/ci-test-groups.json .github/workflows/ci.yml; do
  files "$shared_root"
  for scope in "${v_scope_keys[@]}"; do
    expect fail "$scope cannot edit the shared 5a-V root or registration $shared_root" "$central" --scope "$scope" --files-from "$work/files"
  done
  expect ok "the integration PR owns the shared 5a-V root or registration $shared_root" "$central" --scope integration --files-from "$work/files"
done
expect ok "evorch-core remains in the runtime test group" bash -c "jq -e '.groups.runtime.packages | index(\"evorch-core\")' scripts/ci-test-groups.json"
expect ok "core-client-e2e remains in the GUI test group" bash -c "jq -e '.groups.gui.packages | index(\"core-client-e2e\")' scripts/ci-test-groups.json"
expect ok "the admin publish boundary test and gate run in CI" bash -c "grep -q 'bash scripts/tests/test_check_admin_publish_boundary.sh' '$ci' && grep -q 'bash scripts/check-admin-publish-boundary.sh' '$ci'"
expect ok "the public restart product target shares the sealed CEF product job" bash -c "grep -q -- '--test f4_product_cef --test agent_observation_restart --test public_restart_product --run-ignored only' '$ci'"
expect ok "the CLI route recognizes core and preserves bare restart as Run" bash -c "grep -q 'Some(\"core\") => Route::CoreAdmin' crates/evorch/src/cli/mod.rs && grep -q 'route_of(&\\[\"restart\"\\]), Route::Run' crates/evorch/src/cli/mod.rs"
# Every test binary the registration names exists as a target of the crate it belongs to.
for target in "$H/tests/admin_contracts.rs" "$H/tests/quarantine_admin.rs" "$H/tests/owned_graphs_admin.rs" \
  "$S/tests/quarantine_admin.rs" crates/workbench-client/tests/restart_admin.rs; do
  expect ok "$target exists" test -f "$target"
done
# The authority is in exactly one test group (the support group), and its compile-fail
# doctests run alone in CI, apart from the workspace-wide doctest run.
expect ok "workbench-authority is in the support test group" bash -c "jq -e '.groups.support.packages | index(\"workbench-authority\")' scripts/ci-test-groups.json"
expect ok "workbench-authority is in no other test group" bash -c "[[ \$(jq '[.groups[].packages[] | select(. == \"workbench-authority\")] | length' scripts/ci-test-groups.json) == 1 ]]"
expect ok "CI runs the authority doctests alone, offline, without default features" grep -q -e 'cargo test --offline --locked -p workbench-authority --doc --no-default-features' "$ci"
expect ok "CI sets CARGO_BUILD_GC_ENABLED=false for the authority doctests" bash -c "ci_step() { awk '/- name: Authority compile-fail doctests/ { on = 1; next } on && /^      - / { exit } on' $ci; }; ci_step | grep -q 'CARGO_BUILD_GC_ENABLED: \"false\"'"
expect ok "the workspace doctests exclude the authority" bash -c "[[ \$(grep -c 'cargo test --workspace --exclude workbench-authority --doc --locked' $ci) == 2 ]]"
expect fail "no workspace-wide doctest run includes the authority" bash -c "grep -E 'cargo test --workspace --doc' $ci"

# Q2 integration: the independent disposal targets join their existing crate groups.
for target in crates/runtime/tests/recovery_disposal.rs crates/runtime/tests/owned_graphs_admin.rs crates/storage/tests/owned_graphs_admin.rs "$H/tests/recovery_disposal.rs" "$C/tests/recovery_disposal.rs"; do
  expect ok "Q2 target $target exists" test -f "$target"
done
expect ok "Q2 runtime is in exactly one CI group" bash -c "[[ \$(jq '[.groups[].packages[] | select(. == \"runtime\")] | length' scripts/ci-test-groups.json) == 1 ]]"
expect ok "Q2 host is in the support group" bash -c "jq -e '.groups.support.packages | index(\"workbench-host\")' scripts/ci-test-groups.json"
expect ok "Q2 core is in the runtime group" bash -c "jq -e '.groups.runtime.packages | index(\"evorch-core\")' scripts/ci-test-groups.json"
expect ok "Q2 cache targets are separately registered for runtime and core" bash -c "[[ \$(grep -c 'binary(=recovery_disposal)' scripts/contract-test-filters.sh) == 2 ]]"
expect fail "removed owner-retention target stays absent" grep -q 'name = "physical_ownership_retention"' crates/runtime/Cargo.toml
expect ok "owner-free graph targets use custom harnesses" bash -c 'grep -A2 "name = \"owned_graphs_admin\"" crates/runtime/Cargo.toml | grep -q "harness = false" && grep -A2 "name = \"q2_disposal_retention\"" crates/workbench-host/Cargo.toml | grep -q "harness = false"'
expect ok "owner-free disposal authority remains central" grep -qx 'crates/runtime/src/runtime/disposal_authority.rs' scripts/central-files.txt
expect ok "physical fixtures remain integration-owned" bash -c 'grep -qx "crates/runtime/shared-test-support/physical_ownership.rs" scripts/central-files.txt && grep -qx "crates/runtime/shared-test-support/physical_test_harness.rs" scripts/central-files.txt'
expect ok "CI runs the owner-free disposal boundary gate" grep -q 'bash scripts/check-q2-boundaries.sh' "$ci"

if (( fails > 0 )); then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "central file checks and the scope gate behave as documented"
