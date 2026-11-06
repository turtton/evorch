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
  WORKBENCH_BOUNDARY_CORE_CRATES="${CASE_CORE_CRATES:-runtime storage config}" \
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

WS='"workspace": ["workbench-protocol","workbench-client","workbench-host","workbench-web","runtime","storage","config","gui","test-support"]'
EXT='"external": ["serde","serde_json","thiserror","tokio","regex","egui","wgpu","axum","http","futures-core"]'
GOOD='{"from":"workbench-protocol","to":"serde"},{"from":"workbench-protocol","to":"serde_json"},{"from":"workbench-protocol","to":"thiserror"},
      {"from":"workbench-client","to":"workbench-protocol"},{"from":"workbench-client","to":"tokio"},{"from":"workbench-client","to":"thiserror"},
      {"from":"gui","to":"workbench-client"},
      {"from":"workbench-host","to":"workbench-protocol"},{"from":"workbench-host","to":"runtime"},{"from":"workbench-host","to":"tokio"},{"from":"gui","to":"workbench-host"},
      {"from":"workbench-web","to":"workbench-protocol"},{"from":"workbench-web","to":"axum"},{"from":"workbench-web","to":"http"},{"from":"workbench-web","to":"futures-core"},{"from":"workbench-web","to":"serde"},{"from":"workbench-web","to":"serde_json"},{"from":"workbench-web","to":"thiserror"},{"from":"workbench-web","to":"tokio"},
      {"from":"runtime","to":"storage"},{"from":"runtime","to":"serde"}'

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

# A core crate that was renamed must make the check fail, not silently stop
# covering it. These cases list the crates the fixture was written against.
CASE_CORE_CRATES="runtime storage config"
run_case core_crate_renamed fail "core crate 'storage' is not a workspace member" <<SPEC
{"workspace": ["workbench-protocol","workbench-client","runtime","storage-v2","config"], $EXT,
 "deps": [{"from":"workbench-protocol","to":"serde"},{"from":"workbench-client","to":"workbench-protocol"}]}
SPEC

CASE_CORE_CRATES="runtime storage config"
run_case core_crate_missing_from_a_small_workspace fail "core crate 'config' is not a workspace member" <<SPEC
{"workspace": ["workbench-protocol","workbench-client","runtime","storage"], $EXT,
 "deps": [{"from":"workbench-protocol","to":"serde"},{"from":"workbench-client","to":"workbench-protocol"}]}
SPEC

# The built-in list is what CI uses; a graph that lacks its crates fails.
CASE_CORE_CRATES="agents apply-patch browser browser-cef catalog config event-bus js-engine model providers recorder routing runtime sandbox secret-guard storage tools"
run_case built_in_core_list_requires_every_listed_crate fail "core crate 'agents' is not a workspace member" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_CORE_CRATES=""
run_case host_depends_on_gui fail "workbench-host must not depend on GUI or client crate gui" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-host","to":"gui"}]}
SPEC

run_case host_reaches_client_through_a_workspace_crate fail "workbench-host -> test-support [normal; resolved] -> workbench-client" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-host","to":"test-support"},{"from":"test-support","to":"workbench-client"}]}
SPEC

run_case host_reaches_egui_through_a_core_crate fail "GUI or client crate egui@1.0.0" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"egui"}]}
SPEC

run_case host_declares_a_rendering_crate_only_for_another_target fail "workbench-host declares GUI or client dependency wgpu [normal; declared, optional" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-host","to":"wgpu","optional":true,"resolved":false}]}
SPEC

run_case host_reaches_an_optional_rendering_crate_declared_by_a_core_crate fail "workbench-host must not reach GUI or client dependency egui declared by runtime [normal; declared, optional]: workbench-host -> runtime" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"egui","optional":true,"resolved":false}]}
SPEC

run_case host_reaches_a_target_specific_rendering_crate_two_hops_away fail "declared by test-support [normal; declared, target cfg(unix)]: workbench-host -> test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-host","to":"test-support"},{"from":"test-support","to":"wgpu","target":"cfg(unix)","resolved":false}]}
SPEC

run_case a_dev_only_rendering_crate_of_a_host_dependency_is_not_followed ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"egui","kind":"dev","resolved":false}]}
SPEC

run_case a_rendering_crate_outside_the_host_graph_is_not_a_violation ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"egui"},{"from":"gui","to":"wgpu","optional":true,"resolved":false}]}
SPEC

run_case host_dev_dependency_on_gui fail "workbench-host -> gui [dev;" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-host","to":"gui","kind":"dev"}]}
SPEC

run_case dev_dependencies_of_a_host_dependency_are_not_followed ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-host","to":"test-support"},{"from":"test-support","to":"gui","kind":"dev"}]}
SPEC

run_case host_package_missing fail "workspace member 'workbench-host' not found" <<SPEC
{"workspace": ["workbench-protocol","workbench-client","runtime"], $EXT, "deps": [{"from":"workbench-protocol","to":"serde"},{"from":"workbench-client","to":"workbench-protocol"}]}
SPEC

# workbench-web: protocol adapter that must not know the core or any GUI.
run_case web_depends_on_runtime fail "workbench-web must not depend on workspace crate runtime: workbench-web -> runtime" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"runtime"}]}
SPEC

run_case web_depends_on_host fail "workbench-web must not depend on workspace crate workbench-host" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"workbench-host"}]}
SPEC

run_case web_reaches_storage_through_config fail "workbench-web -> config [normal; resolved] -> storage" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"config"},{"from":"config","to":"storage"}]}
SPEC

run_case web_dev_dependency_on_workspace_crate fail "workbench-web -> test-support [dev;" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"test-support","kind":"dev"}]}
SPEC

run_case web_depends_on_gui fail "workbench-web must not depend on GUI or client crate gui" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"gui"}]}
SPEC

run_case web_depends_on_client fail "workbench-web must not depend on workspace crate workbench-client" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"workbench-client"}]}
SPEC

run_case web_reaches_egui_through_a_dependency fail "workbench-web must not depend on GUI or client crate egui@1.0.0" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"regex"},{"from":"regex","to":"egui"}]}
SPEC

run_case web_declares_an_optional_rendering_crate fail "workbench-web declares GUI or client dependency wgpu" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"wgpu","optional":true,"resolved":false}]}
SPEC

run_case web_third_party_outside_allowlist fail "workbench-web declares third-party dependency regex" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"regex"}]}
SPEC

run_case web_dev_dependency_on_a_third_party_crate_is_allowed ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"regex","kind":"dev"}]}
SPEC

run_case core_crate_depending_on_web fail "core crate runtime must not depend on workbench-web" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"workbench-web"}]}
SPEC

run_case web_package_missing fail "workspace member 'workbench-web' not found" <<SPEC
{"workspace": ["workbench-protocol","workbench-client","workbench-host","runtime"], $EXT, "deps": [{"from":"workbench-protocol","to":"serde"},{"from":"workbench-client","to":"workbench-protocol"},{"from":"workbench-host","to":"workbench-protocol"}]}
SPEC

echo "$pass passed, $fail failed"
[[ $fail -eq 0 ]]
