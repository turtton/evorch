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

# The gui/evorch dependency ratchet reads an allowlist; cases get this one unless
# they set CASE_ALLOWLIST. It covers what the passing graph (GOOD) declares for gui.
DEFAULT_ALLOWLIST='# comment lines and blanks are ignored

gui workbench-host normal 19b   # trailing comment'
CASE_ALLOWLIST=$DEFAULT_ALLOWLIST

pass=0
fail=0
# run_case <name> <expect: ok|fail> <expected output substring or ""> <<< spec
run_case() {
  local name=$1 expect=$2 needle=$3
  if [[ -n ${CASE_METADATA:-} ]]; then
    cp "$CASE_METADATA" "$work/$name.json"
  else
    make_metadata > "$work/$name.json"
  fi
  printf '%s\n' "$CASE_ALLOWLIST" > "$work/$name.allow"
  local status=0
  WORKBENCH_BOUNDARY_CORE_CRATES="${CASE_CORE_CRATES:-runtime storage config}" \
    WORKBENCH_BOUNDARY_ALLOWLIST="${CASE_ALLOWLIST_PATH:-$work/$name.allow}" \
    WORKBENCH_BOUNDARY_STALE_ALLOWLIST="${CASE_STALE:-warn}" \
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

WS='"workspace": ["workbench-protocol","workbench-client","workbench-host","workbench-web","evorch-core","runtime","storage","config","gui","evorch","core-instance","core-launcher","test-support"]'
EXT='"external": ["serde","serde_json","thiserror","tokio","regex","egui","wgpu","axum","http","futures-core","http-body-util","hyper","hyper-util"]'
GOOD='{"from":"workbench-protocol","to":"serde"},{"from":"workbench-protocol","to":"serde_json"},{"from":"workbench-protocol","to":"thiserror"},
      {"from":"workbench-client","to":"workbench-protocol"},{"from":"workbench-client","to":"tokio"},{"from":"workbench-client","to":"thiserror"},
      {"from":"gui","to":"workbench-client"},
      {"from":"workbench-host","to":"workbench-protocol"},{"from":"workbench-host","to":"runtime"},{"from":"workbench-host","to":"tokio"},{"from":"gui","to":"workbench-host"},
      {"from":"workbench-web","to":"workbench-protocol"},{"from":"workbench-web","to":"axum"},{"from":"workbench-web","to":"http"},{"from":"workbench-web","to":"http-body-util"},{"from":"workbench-web","to":"hyper"},{"from":"workbench-web","to":"hyper-util"},{"from":"workbench-web","to":"futures-core"},{"from":"workbench-web","to":"serde"},{"from":"workbench-web","to":"serde_json"},{"from":"workbench-web","to":"thiserror"},{"from":"workbench-web","to":"tokio"},
      {"from":"runtime","to":"storage"},{"from":"runtime","to":"serde"}'

run_case clean_graph ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

run_case core_binary_is_a_gui_free_composition_root ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch-core","to":"runtime"},{"from":"evorch-core","to":"workbench-host"},{"from":"evorch-core","to":"workbench-web"},{"from":"evorch-core","to":"workbench-client","kind":"dev"}]}
SPEC

run_case core_binary_cannot_reach_gui fail "evorch-core -> gui" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch-core","to":"gui"}]}
SPEC

run_case core_binary_cannot_reach_optional_rendering fail "evorch-core declares GUI or client dependency egui" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch-core","to":"egui","optional":true,"resolved":false}]}
SPEC

run_case core_binary_cannot_reach_rendering_transitively fail "evorch-core -> test-support [dev; resolved] -> egui" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch-core","to":"test-support","kind":"dev"},{"from":"test-support","to":"egui"}]}
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


# gui / evorch dependency ratchet: direct workspace dependencies are frozen by an
# allowlist. Only additions fail; the allowlisted ones may be optional, target-
# specific or any kind that the list names.
CASE_ALLOWLIST=$DEFAULT_ALLOWLIST
run_case ratchet_reports_allowlist_size ok "1 allowlisted gui/evorch dependencies" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

run_case ratchet_gui_new_normal_dependency fail "gui declares workspace dependency runtime [normal; declared] that is not in" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"runtime"}]}
SPEC

run_case ratchet_gui_new_optional_dependency fail "gui declares workspace dependency storage [normal; declared, optional]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"storage","optional":true,"resolved":false}]}
SPEC

run_case ratchet_gui_new_target_specific_dependency fail "gui declares workspace dependency config [normal; declared, target cfg(unix)]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"config","target":"cfg(unix)","resolved":false}]}
SPEC

run_case ratchet_gui_new_dev_dependency fail "gui declares workspace dependency runtime [dev; declared]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"runtime","kind":"dev"}]}
SPEC

run_case ratchet_gui_new_build_dependency fail "gui declares workspace dependency storage [build; declared]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"storage","kind":"build"}]}
SPEC

run_case ratchet_gui_new_optional_target_specific_dev_dependency fail "gui declares workspace dependency test-support [dev; declared, optional, target cfg(unix)]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"test-support","kind":"dev","optional":true,"target":"cfg(unix)","resolved":false}]}
SPEC

run_case ratchet_allowlisted_dependency_with_another_kind fail "gui declares workspace dependency workbench-host [dev; declared]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"workbench-host","kind":"dev"}]}
SPEC

run_case ratchet_evorch_new_normal_dependency fail "evorch declares workspace dependency runtime [normal; declared] that is not in" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch","to":"runtime"}]}
SPEC

run_case ratchet_evorch_new_optional_dependency fail "evorch declares workspace dependency storage [normal; declared, optional]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch","to":"storage","optional":true,"resolved":false}]}
SPEC

run_case ratchet_evorch_new_target_specific_dependency fail "evorch declares workspace dependency config [normal; declared, target cfg(unix)]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch","to":"config","target":"cfg(unix)","resolved":false}]}
SPEC

run_case ratchet_evorch_new_dev_dependency fail "evorch declares workspace dependency test-support [dev; declared]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch","to":"test-support","kind":"dev"}]}
SPEC

run_case ratchet_evorch_new_build_dependency fail "evorch declares workspace dependency storage [build; declared]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch","to":"storage","kind":"build"}]}
SPEC

run_case ratchet_client_and_protocol_need_no_entry ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"workbench-protocol"},{"from":"evorch","to":"workbench-client","kind":"dev"},{"from":"evorch","to":"workbench-protocol","optional":true,"resolved":false}]}
SPEC

run_case ratchet_core_instance_and_launcher_need_no_entry ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"core-launcher"},{"from":"gui","to":"core-instance","kind":"dev"},{"from":"evorch","to":"core-launcher"},{"from":"evorch","to":"core-instance","optional":true,"resolved":false},{"from":"core-launcher","to":"core-instance"},{"from":"core-launcher","to":"workbench-client"},{"from":"core-launcher","to":"workbench-protocol"}]}
SPEC

run_case ratchet_third_party_dependencies_are_not_counted ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch","to":"regex"},{"from":"gui","to":"egui"}]}
SPEC

CASE_ALLOWLIST="gui workbench-host normal 19b
gui runtime normal 19b
gui storage normal 19b
gui storage dev 19b
gui config normal 20
gui test-support dev unassigned
evorch runtime normal 17
evorch storage build 19b"
run_case ratchet_allows_listed_optional_target_dev_and_build_dependencies ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"runtime","optional":true,"resolved":false},{"from":"gui","to":"storage","target":"cfg(unix)","resolved":false},{"from":"gui","to":"storage","kind":"dev"},{"from":"gui","to":"config"},{"from":"gui","to":"test-support","kind":"dev"},{"from":"evorch","to":"runtime"},{"from":"evorch","to":"storage","kind":"build"}]}
SPEC

# Shrinking passes by default, with a stale-entry warning; strict mode fails.
run_case ratchet_removed_dependency_passes_with_a_stale_warning ok "warning: stale allowlist entry: gui runtime normal (removal PR 19b)" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_STALE=fail
run_case ratchet_stale_entry_fails_in_strict_mode fail "stale allowlist entry: gui runtime normal" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC
CASE_STALE=

CASE_ALLOWLIST="gui workbench-host normal 19b
gui workbench-host normal 19b"
run_case ratchet_duplicate_allowlist_entry fail "duplicate entry gui workbench-host normal" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ALLOWLIST="gui workbench-host normal soon"
run_case ratchet_allowlist_needs_a_removal_pr fail "removal PR soon must be a PR step" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ALLOWLIST="gui workbench-host normal"
run_case ratchet_allowlist_missing_removal_pr fail "expected \`<crate> <dependency> <kind> <removal-pr>\`, got 3 field(s)" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ALLOWLIST="gui workbench-host optional 19b"
run_case ratchet_allowlist_kind_is_not_optional fail "kind optional must be normal, dev or build" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ALLOWLIST="runtime storage normal 19b
gui workbench-host normal 19b"
run_case ratchet_allowlist_covers_only_gui_and_evorch fail "crate runtime is not covered by the ratchet" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ALLOWLIST=$DEFAULT_ALLOWLIST
CASE_ALLOWLIST_PATH="$work/no-such-allowlist.txt"
run_case ratchet_missing_allowlist_file fail "no-such-allowlist.txt not found" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC
CASE_ALLOWLIST_PATH=

run_case ratchet_gui_package_missing fail "workspace member 'gui' not found" <<SPEC
{"workspace": ["workbench-protocol","workbench-client","workbench-host","workbench-web","evorch-core","evorch"], $EXT, "deps": [{"from":"workbench-protocol","to":"serde"},{"from":"workbench-client","to":"workbench-protocol"}]}
SPEC

run_case ratchet_evorch_package_missing fail "workspace member 'evorch' not found" <<SPEC
{"workspace": ["workbench-protocol","workbench-client","workbench-host","workbench-web","evorch-core","gui"], $EXT, "deps": [{"from":"workbench-protocol","to":"serde"},{"from":"workbench-client","to":"workbench-protocol"}]}
SPEC


run_case ratchet_exempt_crates_absent_from_the_workspace_is_fine ok "boundary check passed" <<SPEC
{"workspace": ["workbench-protocol","workbench-client","workbench-host","workbench-web","evorch-core","runtime","storage","config","gui","evorch"], $EXT, "deps": [$GOOD]}
SPEC

run_case core_instance_with_a_workspace_dependency fail "core-instance must not depend on workspace crate runtime" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"core-instance","to":"runtime","optional":true,"resolved":false}]}
SPEC

run_case core_launcher_with_an_allowed_closure ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"core-launcher","to":"core-instance"},{"from":"core-launcher","to":"workbench-client"},{"from":"core-launcher","to":"workbench-protocol","kind":"dev"}]}
SPEC

run_case core_launcher_reaching_runtime_through_a_dependency fail "core-launcher must not depend on workspace crate runtime: core-launcher -> test-support [normal; resolved] -> runtime" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"core-launcher","to":"test-support"},{"from":"test-support","to":"runtime"}]}
SPEC

# Cases built from real Cargo manifests: `cargo metadata` output for small
# workspaces, so renames, `workspace = true` inheritance and [patch] sections go
# through Cargo's own resolver. The registry is a local directory source, so no
# network is used.
write_crate() { # <dir> <crate> <dependency tables>
  mkdir -p "$1/crates/$2/src"
  : > "$1/crates/$2/src/lib.rs"
  printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n%s\n' "$2" "$3" > "$1/crates/$2/Cargo.toml"
}
make_workspace() { # <dir> [extra workspace tables]
  local dir=$1
  mkdir -p "$dir/.cargo" "$dir/vendor/runtime/src"
  printf '[source.crates-io]\nreplace-with = "local"\n[source.local]\ndirectory = "vendor"\n' > "$dir/.cargo/config.toml"
  printf '[package]\nname = "runtime"\nversion = "0.1.0"\nedition = "2021"\n' > "$dir/vendor/runtime/Cargo.toml"
  : > "$dir/vendor/runtime/src/lib.rs"
  printf '{"files":{},"package":null}\n' > "$dir/vendor/runtime/.cargo-checksum.json"
  printf '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n%s\n' "${2:-}" > "$dir/Cargo.toml"
  write_crate "$dir" workbench-protocol ''
  write_crate "$dir" workbench-client 'workbench-protocol = { path = "../workbench-protocol" }'
  write_crate "$dir" workbench-host $'workbench-protocol = { path = "../workbench-protocol" }\nruntime = { path = "../runtime" }'
  write_crate "$dir" workbench-web 'workbench-protocol = { path = "../workbench-protocol" }'
  write_crate "$dir" runtime ''
  write_crate "$dir" evorch-core $'runtime = { path = "../runtime" }\nworkbench-host = { path = "../workbench-host" }'
  write_crate "$dir" gui 'workbench-host = { path = "../workbench-host" }'
  write_crate "$dir" evorch ''
  write_crate "$dir" core-instance ''
  write_crate "$dir" core-launcher $'core-instance = { path = "../core-instance" }\nworkbench-client = { path = "../workbench-client" }\nworkbench-protocol = { path = "../workbench-protocol" }'
}
add_to_crate() { # <dir> <crate> <toml appended to the manifest>
  printf '%s\n' "$3" >> "$1/crates/$2/Cargo.toml"
}
# run_real_case <name> <expect> <needle> [crate=toml-to-append ...]; extra workspace
# tables come from REAL_WORKSPACE_EXTRA.
run_real_case() {
  local name=$1 expect=$2 needle=$3
  shift 3
  local dir="$work/ws-$name"
  make_workspace "$dir" "${REAL_WORKSPACE_EXTRA:-}"
  local spec
  for spec in "$@"; do
    add_to_crate "$dir" "${spec%%=*}" "${spec#*=}"
  done
  (cd "$dir" && cargo metadata --format-version 1 --offline > "$work/$name.metadata.json" 2> "$work/$name.cargo.err") \
    || { fail=$((fail + 1)); echo "FAIL - $name (cargo metadata failed)"; sed 's/^/       | /' "$work/$name.cargo.err"; return; }
  CASE_METADATA="$work/$name.metadata.json" CASE_CORE_CRATES=runtime run_case "$name" "$expect" "$needle" < /dev/null
}

if command -v cargo > /dev/null; then
  CASE_ALLOWLIST='gui workbench-host normal 19b'

  run_real_case real_manifest_clean_workspace ok "boundary check passed"

  run_real_case real_gui_renamed_dependency fail "gui declares workspace dependency runtime [normal; declared]" \
    $'gui=rt = { path = "../runtime", package = "runtime" }'

  REAL_WORKSPACE_EXTRA=$'[workspace.dependencies]\nrt = { path = "crates/runtime", package = "runtime" }'
  run_real_case real_gui_workspace_inherited_dependency fail "gui declares workspace dependency runtime [normal; declared]" \
    $'gui=rt = { workspace = true }'
  run_real_case real_evorch_workspace_inherited_dev_dependency fail "evorch declares workspace dependency runtime [dev; declared]" \
    $'evorch=[dev-dependencies]\nrt = { workspace = true }'
  REAL_WORKSPACE_EXTRA=

  run_real_case real_gui_optional_target_dependency fail "gui declares workspace dependency runtime [normal; declared, optional, target cfg(unix)]" \
    $'gui=[target.\'cfg(unix)\'.dependencies]\nruntime = { path = "../runtime", optional = true }'

  run_real_case real_evorch_build_dependency fail "evorch declares workspace dependency runtime [build; declared]" \
    $'evorch=[build-dependencies]\nruntime = { path = "../runtime" }'

  run_real_case real_registry_crate_with_a_workspace_name_is_not_internal ok "boundary check passed" \
    $'gui=runtime = "0.1"'

  REAL_WORKSPACE_EXTRA=$'[patch.crates-io]\nruntime = { path = "crates/runtime" }'
  run_real_case real_gui_dependency_patched_to_a_workspace_crate fail "gui declares workspace dependency runtime [normal; resolved" \
    $'gui=runtime = "0.1"'
  run_real_case real_evorch_dev_dependency_patched_to_a_workspace_crate fail "evorch declares workspace dependency runtime [dev; resolved" \
    $'evorch=[dev-dependencies]\nruntime = "0.1"'
  run_real_case real_core_instance_dependency_patched_to_a_workspace_crate fail "core-instance must not depend on workspace crate runtime" \
    $'core-instance=runtime = "0.1"'
  REAL_WORKSPACE_EXTRA=

  run_real_case real_gui_depends_on_the_launcher ok "boundary check passed" \
    $'gui=core-launcher = { path = "../core-launcher" }' \
    $'evorch=core-instance = { path = "../core-instance" }'

  run_real_case real_gui_reaches_the_core_through_the_launcher fail "core-launcher must not depend on workspace crate runtime: core-launcher -> runtime" \
    $'gui=core-launcher = { path = "../core-launcher" }' \
    $'core-launcher=runtime = { path = "../runtime" }'

  run_real_case real_launcher_optional_target_dependency fail "core-launcher must not depend on workspace crate runtime: core-launcher -> runtime [normal; declared, optional, target cfg(unix)]" \
    $'core-launcher=[target.\'cfg(unix)\'.dependencies]\nruntime = { path = "../runtime", optional = true }'

  run_real_case real_launcher_depends_on_the_host fail "core-launcher must not depend on workspace crate workbench-host" \
    $'core-launcher=workbench-host = { path = "../workbench-host" }'

  run_real_case real_core_instance_depends_on_the_protocol fail "core-instance must not depend on workspace crate workbench-protocol" \
    $'core-instance=workbench-protocol = { path = "../workbench-protocol" }'

  run_real_case real_core_instance_dev_dependency fail "core-instance must not depend on workspace crate runtime: core-instance -> runtime [dev; resolved]" \
    $'core-instance=[dev-dependencies]\nruntime = { path = "../runtime" }'

  # The script's own `cargo metadata` call: it must resolve every feature, or an
  # optional dependency that a [patch] section redirects to a workspace crate
  # would have no resolved edge. run_case cannot be used because it passes a
  # metadata file; the script gets the fixture workspace through its manifest
  # override.
  run_real_script_case() { # <name> <expect> <needle> [crate=toml-to-append ...]
    local name=$1 expect=$2 needle=$3
    shift 3
    local dir="$work/ws-$name"
    make_workspace "$dir" "${REAL_WORKSPACE_EXTRA:-}"
    local spec
    for spec in "$@"; do
      add_to_crate "$dir" "${spec%%=*}" "${spec#*=}"
    done
    (cd "$dir" && cargo generate-lockfile --offline > /dev/null 2> "$work/$name.cargo.err") \
      || { fail=$((fail + 1)); echo "FAIL - $name (cargo generate-lockfile failed)"; sed 's/^/       | /' "$work/$name.cargo.err"; return; }
    printf '%s\n' "$CASE_ALLOWLIST" > "$work/$name.allow"
    local status=0
    WORKBENCH_BOUNDARY_MANIFEST_PATH="$dir/Cargo.toml" WORKBENCH_BOUNDARY_CORE_CRATES=runtime \
      WORKBENCH_BOUNDARY_ALLOWLIST="$work/$name.allow" \
      "$checker" > "$work/$name.out" 2>&1 || status=$?
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

  run_real_script_case real_script_runs_cargo_metadata_itself ok "boundary check passed"

  REAL_WORKSPACE_EXTRA=$'[patch.crates-io]\nruntime = { path = "crates/runtime" }'
  run_real_script_case real_gui_unselected_optional_dependency_patched_to_a_workspace_crate fail "gui declares workspace dependency runtime [normal; resolved" \
    $'gui=runtime = { version = "0.1", optional = true }'
  run_real_script_case real_evorch_unselected_optional_build_dependency_patched fail "evorch declares workspace dependency runtime [build; resolved" \
    $'evorch=[build-dependencies]\nruntime = { version = "0.1", optional = true }'
  run_real_script_case real_core_instance_unselected_optional_dependency_patched fail "core-instance must not depend on workspace crate runtime" \
    $'core-instance=runtime = { version = "0.1", optional = true }'
  run_real_script_case real_core_launcher_unselected_optional_target_dependency_patched fail "core-launcher must not depend on workspace crate runtime" \
    $'core-launcher=[target.\'cfg(unix)\'.dependencies]\nruntime = { version = "0.1", optional = true }'
  REAL_WORKSPACE_EXTRA=
else
  echo "skip - real-manifest cases (cargo not found)"
fi

echo "$pass passed, $fail failed"
[[ $fail -eq 0 ]]
