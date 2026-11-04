#!/usr/bin/env bash
# Fixture tests for scripts/check-workbench-boundaries.sh: one passing graph and
# one failing graph per rule. Fixtures are generated as `cargo metadata`-shaped
# JSON from a compact spec so no workspace build is needed.
set -euo pipefail
cd "$(dirname "$0")/../.."

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
checker=scripts/check-workbench-boundaries.sh

# Spec on stdin: {"workspace": [names], "external": [names], "deps": [{from, to, kind?, optional?, target?, resolved?}]}
# `resolved: false` models a declaration the resolved graph leaves out
# (an optional dependency, or one for another target).
make_metadata() {
  jq '
    . as $s
    | ($s.workspace + $s.external) as $names
    | def is_ws($n): $s.workspace | index($n) != null;
      def id($n): if is_ws($n) then "path+file:///fixture/crates/\($n)#0.1.0"
                  else "registry+https://example.invalid/index#\($n)@1.0.0" end;
      def kind($d): if ($d.kind // "normal") == "normal" then null else $d.kind end;
    {
      workspace_members: [$s.workspace[] | id(.)],
      packages: [$names[] | . as $n | {
        id: id($n), name: $n, version: (if is_ws($n) then "0.1.0" else "1.0.0" end),
        source: (if is_ws($n) then null else "registry+https://example.invalid/index" end),
        dependencies: [$s.deps[] | . as $d | select($d.from == $n) | {
          name: $d.to,
          source: (if is_ws($d.to) then null else "registry+https://example.invalid/index" end),
          kind: kind($d), optional: ($d.optional // false), target: ($d.target // null) }]
      }],
      resolve: { nodes: [$names[] | . as $n | {
        id: id($n),
        deps: [$s.deps[] | . as $d | select($d.from == $n and ($d.resolved != false)) | {
          name: $d.to, pkg: id($d.to), dep_kinds: [{kind: kind($d), target: ($d.target // null)}] }]
      }] }
    }'
}

pass=0
fail=0
# run_case <name> <expect: ok|fail> <expected output substring or ""> <<< spec
run_case() {
  local name=$1 expect=$2 needle=$3
  make_metadata > "$work/$name.json"
  local status=0
  "$checker" "$work/$name.json" > "$work/$name.out" 2>&1 || status=$?
  local ok=1
  if [[ $expect == ok && $status -ne 0 ]]; then ok=0; fi
  if [[ $expect == fail && $status -ne 1 ]]; then ok=0; fi
  if [[ -n $needle ]] && ! grep -qF -- "$needle" "$work/$name.out"; then ok=0; fi
  if [[ $ok -eq 1 ]]; then
    pass=$((pass + 1))
    echo "ok   - $name"
  else
    fail=$((fail + 1))
    echo "FAIL - $name (expected $expect${needle:+ containing '$needle'}, exit $status)"
    sed 's/^/       | /' "$work/$name.out"
  fi
}

WS='"workspace": ["workbench-protocol","workbench-client","runtime","storage","config","gui","test-support"]'
EXT='"external": ["serde","serde_json","thiserror","tokio","regex"]'
GOOD='{"from":"workbench-protocol","to":"serde"},{"from":"workbench-protocol","to":"serde_json"},{"from":"workbench-protocol","to":"thiserror"},
      {"from":"workbench-client","to":"workbench-protocol"},{"from":"workbench-client","to":"tokio"},{"from":"workbench-client","to":"thiserror"},
      {"from":"gui","to":"workbench-client"},{"from":"runtime","to":"storage"},{"from":"runtime","to":"serde"}'

run_case clean_graph ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

run_case clean_graph_with_dev_deps_on_third_parties ok "" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"serde_json","kind":"dev"}]}
SPEC

run_case protocol_depends_on_runtime fail "workbench-protocol -> runtime [normal; resolved]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-protocol","to":"runtime"}]}
SPEC

run_case protocol_reaches_storage_through_config fail "workbench-protocol -> config [normal; resolved] -> storage [normal; resolved]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-protocol","to":"config"},{"from":"config","to":"storage"}]}
SPEC

run_case protocol_depends_on_client fail "workbench-protocol -> workbench-client" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-protocol","to":"workbench-client"}]}
SPEC

run_case protocol_optional_dependency_declared_only fail "declared, optional" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-protocol","to":"runtime","optional":true,"resolved":false}]}
SPEC

run_case protocol_target_specific_build_dependency fail "[build; declared, target cfg(unix)]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-protocol","to":"storage","kind":"build","target":"cfg(unix)","resolved":false}]}
SPEC

run_case protocol_dev_dependency_on_workspace_crate fail "workbench-protocol -> test-support [dev;" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-protocol","to":"test-support","kind":"dev"}]}
SPEC

run_case client_dev_dependency_on_workspace_crate fail "workbench-client -> config [dev;" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"config","kind":"dev"}]}
SPEC

run_case client_depends_on_storage fail "workbench-client -> storage" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"storage"}]}
SPEC

run_case client_reaches_storage_through_config fail "workbench-client -> config [normal; resolved] -> storage" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"config"},{"from":"config","to":"storage"}]}
SPEC

run_case runtime_depends_on_protocol fail "core crate runtime must not depend on workbench-protocol" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"workbench-protocol"}]}
SPEC

run_case runtime_reaches_protocol_through_a_workspace_crate fail "runtime -> test-support [normal; resolved] -> workbench-protocol" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"test-support"},{"from":"test-support","to":"workbench-protocol"}]}
SPEC

run_case storage_reaches_client_through_gui fail "core crate storage must not depend on workbench-client (directly or transitively): storage -> gui" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"storage","to":"gui"}]}
SPEC

run_case core_crate_reaching_the_protocol_in_two_hops_through_the_client fail "config -> gui [normal; resolved] -> workbench-client [normal; resolved] -> workbench-protocol" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"config","to":"gui"}]}
SPEC

run_case dev_dependencies_of_a_core_dependency_are_not_followed ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"test-support"},{"from":"test-support","to":"workbench-client","kind":"dev"}]}
SPEC

run_case storage_dev_depends_on_client fail "core crate storage must not depend on workbench-client" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"storage","to":"workbench-client","kind":"dev"}]}
SPEC

run_case protocol_third_party_outside_allowlist fail "workbench-protocol declares third-party dependency regex" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-protocol","to":"regex"}]}
SPEC

run_case client_third_party_outside_allowlist fail "workbench-client declares third-party dependency regex" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"regex","optional":true,"resolved":false}]}
SPEC

run_case protocol_package_missing fail "workspace member 'workbench-protocol' not found" <<SPEC
{"workspace": ["workbench-client","runtime"], $EXT, "deps": [{"from":"workbench-client","to":"tokio"}]}
SPEC

echo "$pass passed, $fail failed"
[[ $fail -eq 0 ]]
