#!/usr/bin/env bash
# Fixture tests for scripts/check-workbench-boundaries.sh: one passing graph and
# one failing graph per rule. Fixtures are generated as `cargo metadata`-shaped
# JSON from a compact spec so no workspace build is needed.
set -euo pipefail
cd "$(dirname "$0")/../.."

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
checker=scripts/check-workbench-boundaries.sh

# Spec on stdin: {"workspace": [names], "external": [names], "deps": [{from, to, kind?, optional?, target?, resolved?, features?, rename?}],
#                  "pkg_features": {crate: {feature: [entries]}}}
# `resolved: false` models a declaration the resolved graph leaves out
# (an optional dependency, or one for another target). `features` and `rename` are
# what a declaration carries in `cargo metadata`; `pkg_features` is a package's
# [features] table.
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
        features: (($s.pkg_features // {})[$n] // {}),
        source: (if is_ws($n) then null else "registry+https://example.invalid/index" end),
        dependencies: [$s.deps[] | . as $d | select($d.from == $n) | {
          name: $d.to,
          source: (if is_ws($d.to) then null else "registry+https://example.invalid/index" end),
          kind: kind($d), optional: ($d.optional // false), target: ($d.target // null),
          features: ($d.features // []), rename: ($d.rename // null) }]
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

: > "$work/empty-root-manifest.toml"
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
  # The sources, the root manifest and the product tree are fixtures too: by default
  # an empty root manifest and no source tree (rule 11 and the product check skip).
  WORKBENCH_BOUNDARY_ROOT_MANIFEST="${CASE_ROOT_MANIFEST:-$work/empty-root-manifest.toml}" \
    WORKBENCH_BOUNDARY_SOURCE_ROOT="${CASE_SOURCE_ROOT:-$work/no-sources}" \
    WORKBENCH_BOUNDARY_PRODUCT_TREE="${CASE_PRODUCT_TREE:-}" \
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

WS='"workspace": ["workbench-protocol","workbench-client","workbench-host","workbench-web","evorch-core","runtime","storage","config","gui","evorch","core-instance","core-launcher","test-support","test-socket-dir","workbench-authority"]'
EXT='"external": ["serde","serde_json","thiserror","tokio","tokio-util","regex","egui","wgpu","axum","http","futures-core","http-body-util","hyper","hyper-util","rustix","uuid"]'
GOOD='{"from":"workbench-protocol","to":"serde"},{"from":"workbench-protocol","to":"serde_json"},{"from":"workbench-protocol","to":"thiserror"},
      {"from":"workbench-client","to":"workbench-protocol"},{"from":"workbench-client","to":"tokio"},{"from":"workbench-client","to":"thiserror"},
      {"from":"gui","to":"workbench-client"},
      {"from":"workbench-host","to":"workbench-protocol"},{"from":"workbench-host","to":"runtime"},{"from":"workbench-host","to":"tokio"},{"from":"gui","to":"workbench-host"},
      {"from":"workbench-authority","to":"workbench-protocol"},{"from":"workbench-authority","to":"tokio"},{"from":"workbench-authority","to":"rustix"},{"from":"workbench-authority","to":"uuid"},
      {"from":"workbench-host","to":"workbench-authority"},{"from":"workbench-web","to":"workbench-authority"},
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

run_case web_tracks_detached_work_with_tokio_util ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"tokio-util"}]}
SPEC

run_case web_utility_still_cannot_reach_rendering fail "workbench-web -> tokio-util@1.0.0 [normal; resolved] -> egui@1.0.0" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"tokio-util"},{"from":"tokio-util","to":"egui"}]}
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
{"workspace": ["workbench-protocol","workbench-client","workbench-host","workbench-web","workbench-authority","evorch-core","runtime","storage","config","gui","evorch"], $EXT, "deps": [$GOOD]}
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

run_case client_and_launcher_dev_depend_on_the_test_socket_leaf ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"test-socket-dir","kind":"dev"},{"from":"core-launcher","to":"core-instance"},{"from":"core-launcher","to":"test-socket-dir","kind":"dev"},{"from":"runtime","to":"test-socket-dir","kind":"dev"},{"from":"gui","to":"test-socket-dir","kind":"dev"}]}
SPEC

run_case test_socket_leaf_as_a_normal_dependency fail "runtime must depend on test-socket-dir only as a dev-dependency" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"test-socket-dir"}]}
SPEC

run_case client_depends_on_the_test_socket_leaf_normally fail "workbench-client must depend on test-socket-dir only as a dev-dependency" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"test-socket-dir"}]}
SPEC

run_case test_socket_leaf_with_a_workspace_dependency fail "test-socket-dir must not depend on workspace crate runtime" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"test-socket-dir","to":"runtime"}]}
SPEC

run_case client_dev_dependency_on_test_support_stays_rejected fail "workbench-client -> test-support [dev;" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"test-support","kind":"dev"},{"from":"workbench-client","to":"test-socket-dir","kind":"dev"}]}
SPEC

# 5a-I: the transport authority (rules 6, 9, 10 and 11). The clean graph above has the
# direction host, web -> authority -> protocol (the authority uses tokio, rustix and
# uuid) and an authority-free protocol, client and core.
run_case authority_closure_is_protocol_only_and_web_may_reach_it ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch-core","to":"workbench-authority"},{"from":"evorch-core","to":"workbench-authority","kind":"dev","features":["test-support"]},{"from":"workbench-host","to":"workbench-authority","kind":"dev","features":["test-support"]},{"from":"workbench-web","to":"workbench-authority","kind":"dev","features":["test-support"]},{"from":"workbench-authority","to":"serde_json","kind":"dev"}]}
SPEC

run_case authority_depends_on_workbench_host fail "workbench-authority must not depend on workspace crate workbench-host" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"workbench-host"}]}
SPEC

run_case authority_depends_on_workbench_web fail "workbench-authority must not depend on workspace crate workbench-web" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"workbench-web"}]}
SPEC

run_case authority_depends_on_runtime fail "workbench-authority must not depend on workspace crate runtime" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"runtime"}]}
SPEC

run_case authority_depends_on_storage fail "workbench-authority must not depend on workspace crate storage" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"storage"}]}
SPEC

run_case authority_depends_on_workbench_client fail "workbench-authority must not depend on workspace crate workbench-client" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"workbench-client"}]}
SPEC

run_case authority_depends_on_gui fail "workbench-authority must not depend on workspace crate gui" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"gui"}]}
SPEC

run_case authority_reaches_runtime_through_a_dependency fail "workbench-authority -> test-support [normal; resolved] -> runtime" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"test-support"},{"from":"test-support","to":"runtime"}]}
SPEC

run_case authority_dev_dependency_on_a_workspace_crate fail "workbench-authority -> test-support [dev;" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"test-support","kind":"dev"}]}
SPEC

run_case authority_optional_dependency_declared_only fail "workbench-authority -> storage [normal; declared, optional]" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"storage","optional":true,"resolved":false}]}
SPEC

run_case authority_third_party_outside_its_list fail "workbench-authority declares third-party dependency regex" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"regex"}]}
SPEC

run_case authority_build_dependency_outside_its_list fail "workbench-authority declares third-party dependency serde" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"serde","kind":"build"}]}
SPEC

run_case authority_tokio_with_an_extra_feature fail "workbench-authority enables feature(s) rt of tokio outside its list (sync, net, time)" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"tokio","features":["sync","rt"]}]}
SPEC

run_case authority_uuid_with_an_extra_feature fail "workbench-authority enables feature(s) v7 of uuid" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"uuid","features":["v4","v7"]}]}
SPEC

run_case authority_dev_tokio_features_are_free ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"tokio","kind":"dev","features":["macros","rt","test-util"]}]}
SPEC

run_case authority_tokio_features_inside_its_list ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"tokio","features":["sync","net","time"]},{"from":"workbench-authority","to":"rustix","features":["process"],"target":"cfg(unix)"},{"from":"workbench-authority","to":"uuid","features":["v4"]}]}
SPEC

run_case protocol_depends_on_authority fail "workbench-protocol must not depend on workspace crate workbench-authority" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-protocol","to":"workbench-authority"}]}
SPEC

run_case client_depends_on_authority fail "workbench-client must not depend on workspace crate workbench-authority" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-client","to":"workbench-authority"}]}
SPEC

run_case gui_depends_on_authority fail "gui must not depend on workbench-authority" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"workbench-authority"}]}
SPEC

run_case gui_dev_depends_on_authority fail "gui must not depend on workbench-authority" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"gui","to":"workbench-authority","kind":"dev"}]}
SPEC

run_case core_launcher_depends_on_authority fail "core-launcher must not depend on workspace crate workbench-authority" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"core-launcher","to":"workbench-authority"}]}
SPEC

run_case runtime_depends_on_authority fail "core crate runtime must not depend on workbench-authority" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"workbench-authority"}]}
SPEC

run_case runtime_dev_depends_on_authority fail "runtime must not depend on workbench-authority" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"runtime","to":"workbench-authority","kind":"dev"}]}
SPEC

run_case web_depends_on_host_even_with_the_authority fail "workbench-web must not depend on workspace crate workbench-host" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"workbench-host"}]}
SPEC

run_case web_reaches_the_runtime_through_the_authority fail "workbench-web must not depend on workspace crate runtime" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-authority","to":"runtime"}]}
SPEC

run_case authority_package_missing fail "workspace member 'workbench-authority' not found" <<SPEC
{"workspace": ["workbench-protocol","workbench-client","workbench-host","workbench-web","evorch-core","runtime","storage","config","gui","evorch","core-instance","core-launcher","test-support","test-socket-dir"], $EXT,
 "deps": [{"from":"workbench-protocol","to":"serde"},{"from":"workbench-client","to":"workbench-protocol"}]}
SPEC

# Rule 10: the fake verifier is enabled by a dev-dependency declaration only.
run_case fake_through_a_normal_dependency fail "workbench-web enables workbench-authority/test-support through a normal dependency" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"workbench-authority","features":["test-support"]}]}
SPEC

run_case fake_through_a_build_dependency fail "workbench-host enables workbench-authority/test-support through a build dependency" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-host","to":"workbench-authority","kind":"build","features":["test-support"]}]}
SPEC

run_case fake_through_an_optional_target_dependency fail "evorch-core enables workbench-authority/test-support through a normal dependency" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch-core","to":"workbench-authority","optional":true,"target":"cfg(unix)","features":["test-support"],"resolved":false}]}
SPEC

run_case fake_forwarded_by_the_web_feature fail "feature test-support of workbench-web forwards to workbench-authority/test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD],
 "pkg_features": {"workbench-web": {"test-support": ["workbench-authority/test-support"]}}}
SPEC

run_case fake_forwarded_by_the_core_bootstrap_feature fail "feature bootstrap-test-support of evorch-core forwards to workbench-authority/test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD],
 "pkg_features": {"evorch-core": {"bootstrap-test-support": ["workbench-authority/test-support"]}}}
SPEC

run_case fake_forwarded_weakly fail "feature test-support of workbench-host forwards to workbench-authority?/test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD],
 "pkg_features": {"workbench-host": {"test-support": ["workbench-authority?/test-support"]}}}
SPEC

run_case fake_forwarded_through_a_renamed_dependency fail "feature test-support of workbench-web forwards to auth/test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"workbench-web","to":"workbench-authority","rename":"auth"}],
 "pkg_features": {"workbench-web": {"test-support": ["auth/test-support"]}}}
SPEC

run_case fake_forwarded_weakly_through_a_renamed_dependency fail "feature bootstrap-test-support of evorch-core forwards to auth?/test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD, {"from":"evorch-core","to":"workbench-authority","rename":"auth"}],
 "pkg_features": {"evorch-core": {"bootstrap-test-support": ["auth?/test-support"]}}}
SPEC

run_case other_test_support_forwarding_is_fine ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD],
 "pkg_features": {"evorch-core": {"bootstrap-test-support": ["storage/test-support","workbench-web/test-support"]}, "workbench-web": {"test-support": []}}}
SPEC

run_case another_authority_feature_forwarding_is_fine ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD],
 "pkg_features": {"workbench-host": {"x": ["workbench-authority/other"]}}}
SPEC

run_case authority_default_enables_the_fake fail "the default features of workbench-authority reach test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD],
 "pkg_features": {"workbench-authority": {"default": ["test-support"], "test-support": []}}}
SPEC

run_case authority_default_reaches_the_fake_transitively fail "the default features of workbench-authority reach test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD],
 "pkg_features": {"workbench-authority": {"default": ["a"], "a": ["b"], "b": ["test-support"], "test-support": []}}}
SPEC

run_case authority_fake_feature_off_by_default_is_fine ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD],
 "pkg_features": {"workbench-authority": {"default": [], "test-support": []}}}
SPEC

# [workspace.dependencies] in the root manifest (cargo metadata cannot show an entry no member inherits).
CASE_ROOT_MANIFEST="$work/workspace_dependency_enables_the_fake_inline.root.toml"
cat > "$CASE_ROOT_MANIFEST" <<'TOML'
[workspace]
members = ["crates/*"]
[workspace.dependencies]
tokio = "1"
workbench-authority = { path = "crates/workbench-authority", features = ["test-support"] }
TOML
run_case workspace_dependency_enables_the_fake_inline fail "[workspace.dependencies] of" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ROOT_MANIFEST="$work/workspace_dependency_enables_the_fake_multiline.root.toml"
cat > "$CASE_ROOT_MANIFEST" <<'TOML'
[workspace.dependencies]
workbench-authority = { path = "crates/workbench-authority",
  features = [
    "test-support",
  ] }
serde = "1"
TOML
run_case workspace_dependency_enables_the_fake_multiline fail "enables the fake verifier" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ROOT_MANIFEST="$work/workspace_dependency_enables_the_fake_as_a_table.root.toml"
cat > "$CASE_ROOT_MANIFEST" <<'TOML'
[workspace.dependencies.workbench-authority]
path = "crates/workbench-authority"
features = ["test-support"]
TOML
run_case workspace_dependency_enables_the_fake_as_a_table fail "enables the fake verifier" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ROOT_MANIFEST="$work/workspace_dependency_enables_the_fake_through_an_alias.root.toml"
cat > "$CASE_ROOT_MANIFEST" <<'TOML'
[workspace.dependencies]
auth = { path = "crates/workbench-authority", package = "workbench-authority", features = ["test-support"] }
TOML
run_case workspace_dependency_enables_the_fake_through_an_alias fail "enables the fake verifier" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ROOT_MANIFEST="$work/workspace_dependency_enables_the_fake_with_a_dotted_key.root.toml"
cat > "$CASE_ROOT_MANIFEST" <<'TOML'
[workspace.dependencies]
workbench-authority.path = "crates/workbench-authority"
workbench-authority.features = ["test-support"]
TOML
run_case workspace_dependency_enables_the_fake_with_a_dotted_key fail "enables the fake verifier" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ROOT_MANIFEST="$work/workspace_dependency_without_the_fake_is_fine.root.toml"
cat > "$CASE_ROOT_MANIFEST" <<'TOML'
[workspace.dependencies]
workbench-authority = { path = "crates/workbench-authority" }
storage = { path = "crates/storage", features = ["test-support"] }
[workspace.dependencies.runtime]
path = "crates/runtime"
features = ["test-support"]
TOML
run_case workspace_dependency_without_the_fake_is_fine ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ROOT_MANIFEST="$work/workspace_dependency_fake_in_a_comment_is_fine.root.toml"
cat > "$CASE_ROOT_MANIFEST" <<'TOML'
[workspace.dependencies]
# features = ["test-support"] stays dev-only
workbench-authority = { path = "crates/workbench-authority" } # no "test-support"
TOML
run_case workspace_dependency_fake_in_a_comment_is_fine ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

CASE_ROOT_MANIFEST=

# The product build: what `cargo tree -e normal,build -f '{p} {f}'` lists for the dependents.
cat > "$work/product_tree_enabling_the_fake.tree" <<'TREE'
evorch-core v0.1.0 (/w/crates/evorch-core) default
workbench-authority v0.1.0 (/w/crates/workbench-authority) test-support
tokio v1.0.0 sync,net,time
TREE
CASE_PRODUCT_TREE="$work/product_tree_enabling_the_fake.tree" run_case product_tree_enabling_the_fake fail "the product (normal and build) dependency graph enables workbench-authority/test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

cat > "$work/product_tree_enabling_the_fake_beside_others.tree" <<'TREE'
workbench-authority v0.1.0 (/w/crates/workbench-authority) default,test-support (*)
TREE
CASE_PRODUCT_TREE="$work/product_tree_enabling_the_fake_beside_others.tree" run_case product_tree_enabling_the_fake_beside_others fail "enables workbench-authority/test-support" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

cat > "$work/product_tree_without_the_fake.tree" <<'TREE'
evorch-core v0.1.0 (/w/crates/evorch-core) default
workbench-authority v0.1.0 (/w/crates/workbench-authority) default
workbench-authority v0.1.0 (/w/crates/workbench-authority) (*)
workbench-web v0.1.0 (/w/crates/workbench-web) test-support
TREE
CASE_PRODUCT_TREE="$work/product_tree_without_the_fake.tree" run_case product_tree_without_the_fake ok "boundary check passed" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC

# Rule 11: the one production call of verify_unix_peer.
make_sources() { # <name> [path=content ...]: a source tree under $work/src-<name>; prints its path
  local dir="$work/src-$1" spec
  shift
  mkdir -p "$dir/crates"
  for spec in "$@"; do
    mkdir -p "$dir/$(dirname "${spec%%=*}")"
    printf '%s\n' "${spec#*=}" > "$dir/${spec%%=*}"
  done
  printf '%s' "$dir"
}
UNIX=crates/evorch-core/src/unix.rs
CALL='let peer = workbench_authority::verify_unix_peer(&stream)?;'
DEF='pub fn verify_unix_peer(stream: &UnixStream) -> io::Result<()> { Ok(()) }'
source_case() { # <name> <ok|fail> <needle> [path=content ...]
  local name=$1 expect=$2 needle=$3
  shift 3
  CASE_SOURCE_ROOT=$(make_sources "$name" "$@") run_case "$name" "$expect" "$needle" <<SPEC
{$WS, $EXT, "deps": [$GOOD]}
SPEC
}
source_case peer_call_only_in_the_core_unix_accept ok "boundary check passed" \
  "$UNIX=use workbench_authority::verify_unix_peer;
$CALL" "crates/workbench-authority/src/lib.rs=$DEF" "crates/workbench-authority/src/tests.rs=verify_unix_peer(&s);" \
  "crates/evorch-core/tests/public_restart_contracts.rs=verify_unix_peer(&s);" "crates/evorch-core/src/unix_tests.rs=verify_unix_peer(&s);" \
  "crates/workbench-web/src/api.rs=// verify_unix_peer is not called here"
source_case peer_call_missing_from_the_core_unix fail "crates/evorch-core/src/unix.rs must call verify_unix_peer exactly once in production code, found 0" \
  "$UNIX=fn accept() {}" "crates/workbench-authority/src/lib.rs=$DEF"
source_case peer_call_twice_in_the_core_unix fail "must call verify_unix_peer exactly once in production code, found 2" \
  "$UNIX=$CALL
$CALL" "crates/workbench-authority/src/lib.rs=$DEF"
source_case peer_call_in_another_core_file fail "crates/evorch-core/src/lifecycle.rs uses verify_unix_peer" \
  "$UNIX=$CALL" "crates/evorch-core/src/lifecycle.rs=let p = workbench_authority::verify_unix_peer(&s);"
source_case peer_call_in_the_web_adapter fail "crates/workbench-web/src/server.rs uses verify_unix_peer" \
  "$UNIX=$CALL" "crates/workbench-web/src/server.rs=let p = verify_unix_peer(&s);"
source_case peer_import_outside_the_core_unix fail "crates/workbench-host/src/ingress.rs uses verify_unix_peer" \
  "$UNIX=$CALL" "crates/workbench-host/src/ingress.rs=use workbench_authority::verify_unix_peer;"
source_case peer_call_in_a_src_directory_named_tests_is_not_a_test fail "crates/gui/src/testsupport.rs uses verify_unix_peer" \
  "$UNIX=$CALL" "crates/gui/src/testsupport.rs=verify_unix_peer(&s);"

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
  write_crate "$dir" workbench-host $'workbench-protocol = { path = "../workbench-protocol" }\nworkbench-authority = { path = "../workbench-authority" }\nruntime = { path = "../runtime" }'
  write_crate "$dir" workbench-web $'workbench-protocol = { path = "../workbench-protocol" }\nworkbench-authority = { path = "../workbench-authority" }'
  write_crate "$dir" workbench-authority 'workbench-protocol = { path = "../workbench-protocol" }'
  add_to_crate "$dir" workbench-authority $'[features]\ndefault = []\ntest-support = []'
  write_crate "$dir" runtime ''
  write_crate "$dir" evorch-core $'runtime = { path = "../runtime" }\nworkbench-host = { path = "../workbench-host" }\nworkbench-authority = { path = "../workbench-authority" }'
  # The one production call site of the local-proof function (rule 11).
  mkdir -p "$dir/crates/evorch-core/src"
  printf 'pub fn accept() { let _ = workbench_authority::verify_unix_peer(&stream); }\n' > "$dir/crates/evorch-core/src/unix.rs"
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
  CASE_METADATA="$work/$name.metadata.json" CASE_CORE_CRATES=runtime CASE_SOURCE_ROOT="$dir" \
    CASE_ROOT_MANIFEST="$dir/Cargo.toml" run_case "$name" "$expect" "$needle" < /dev/null
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
    WORKBENCH_BOUNDARY_SOURCE_ROOT="$dir" \
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
