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
  scripts/contract-test-filters.sh crates/evorch/src/main.rs; do
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
    crates/evorch-core/tests/core_cache.rs crates/evorch-core/src/main.rs; do
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
  line=$(grep -E 'nextest run .*--test "?'"$1"'"?( |$)' "$ci") || return 1
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
# Every job that runs a product test depends on the sealed artifact job and
# installs it; the artifact job builds it.
ci_job() { awk -v job="$1" '$0 ~ "^  " job ":$" { on = 1; next } on && /^  [a-z0-9-]+:$/ { exit } on' "$ci"; }
job_has() { ci_job "$1" | grep -q -e "$2"; }
for product_job in tests browser-e2e cef-e2e; do
  expect ok "$product_job needs the product artifact" job_has "$product_job" '^    needs: product-artifact$'
  expect ok "$product_job installs the sealed product artifact" job_has "$product_job" 'scripts/product-artifact.sh install'
done
expect ok "the product artifact job builds and seals it" job_has product-artifact 'scripts/product-artifact.sh build'
expect ok "the aggregate gate requires the product artifact job" bash -c "grep -q 'otel, product-artifact, tests\]' $ci"
expect fail "a job that runs no product test does not install the artifact" job_has lint 'scripts/product-artifact.sh install'

if (( fails > 0 )); then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "central file checks and the scope gate behave as documented"
