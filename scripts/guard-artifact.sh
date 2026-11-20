#!/usr/bin/env bash
# Immutable helper delivery. stdout contains NAME=value on prepare/seal.
# bind-callers consumes compiler/nextest executable paths, never guesses a
# target directory and never rebuilds an application.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
die() { echo "guard-artifact: $*" >&2; exit 1; }
hash() { sha256sum "$1" | cut -d' ' -f1; }
size() { wc -c < "$1" | tr -d ' '; }
not_target() { case "$(cd "$1" && pwd -P)/" in */target/*) die 'seal outside Cargo target';; esac; }
regular() { [[ -f "$1" && ! -L "$1" ]] || die "not a regular file: $1"; }
mode() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }
verify() {
  local manifest=$1 dir helper
  regular "$manifest"; dir=$(cd "$(dirname "$manifest")" && pwd -P)
  jq -e '.schema_version == 1 and .kind == "evorch-guard-bundle"
    and (.purpose == "test-helper" or .purpose == "development" or .purpose == "distribution")
    and .guard_abi == 1 and (.guard_compat_id | test("^[0-9a-f]{64}$"))
    and (.source_sha | test("^[0-9a-f]{40}$"))
    and .helper.package == "process-guard" and .helper.bin == "evorch-process-guard"
    and .helper.features == []
    and (.helper.file == "evorch-process-guard" or
      (.purpose == "development" and .helper.file == (".evorch-guard/" + .helper.sha256 + "/evorch-process-guard")))
    and (.purpose == "test-helper" or (.callers | length > 0))' "$manifest" >/dev/null || die 'guard manifest contract'
  helper="$dir/$(jq -r .helper.file "$manifest")"; regular "$helper"
  [[ "$(mode "$manifest")" == 444 && "$(mode "$helper")" == 555 ]] || die 'manifest/helper must be 0444/0555, including no owner-write bit'
  [[ "$(hash "$helper")" == "$(jq -r .helper.sha256 "$manifest")" && "$(size "$helper")" == "$(jq -r .helper.size_bytes "$manifest")" ]] || die 'helper hash/size mismatch'
  env -i "$helper" --verify-bundle "$manifest" || die 'helper rejected bundle ABI/compat/recipe'
  while IFS=$'\t' read -r file bytes sha; do
    [[ "$file" != */* && -n "$file" && "$file" != . && "$file" != .. ]] || die 'caller basename'
    regular "$dir/$file"
    [[ "$(hash "$dir/$file")" == "$sha" && "$(size "$dir/$file")" == "$bytes" ]] || die "caller mismatch: $file"
  done < <(jq -r '.callers[] | [.file,.size_bytes,.sha256] | @tsv' "$manifest")
}
seal() {
  local helper=$1 out=$2 purpose=$3 profile=$4 recipe=$5; shift 5
  regular "$helper"; mkdir -p "$out"; not_target "$out"
  local description tmp callers='[]' source entry role file
  description=$(env -i "$helper" --describe) || die 'helper --describe failed'
  jq -e '.features == []' <<<"$description" >/dev/null || die 'featureful helper'
  if [[ "$(realpath "$helper")" != "$(realpath -m "$out/evorch-process-guard")" ]]; then
    install -m 0555 "$helper" "$out/evorch-process-guard"
  fi
  chmod 0555 "$out/evorch-process-guard"
  for entry in "$@"; do
    role=${entry%%:*}; file=${entry#*:}; regular "$file"
    [[ "$(cd "$(dirname "$file")" && pwd -P)" == "$(cd "$out" && pwd -P)" ]] || die 'caller must already be beside bundle'
    callers=$(jq -c --arg role "$role" --arg file "$(basename "$file")" --argjson size "$(size "$file")" --arg sha "$(hash "$file")" '. + [{role:$role,file:$file,size_bytes:$size,sha256:$sha}]' <<<"$callers")
  done
  source=${GITHUB_SHA:-$(git -C "$repo" rev-parse HEAD)}
  tmp=$(mktemp "$out/.guard-bundle.XXXXXX")
  jq -n --arg purpose "$purpose" --arg profile "$profile" --argjson args "$recipe" --arg source "$source" --argjson desc "$description" --argjson callers "$callers" --argjson bytes "$(size "$out/evorch-process-guard")" --arg sha "$(hash "$out/evorch-process-guard")" '{schema_version:1,kind:"evorch-guard-bundle",purpose:$purpose,target:$desc.target,source_sha:$source,guard_abi:$desc.guard_abi,guard_compat_id:$desc.guard_compat_id,helper:{package:"process-guard",bin:"evorch-process-guard",file:"evorch-process-guard",size_bytes:$bytes,sha256:$sha,profile:$profile,features:[],build_args:$args},callers:$callers}' >"$tmp"
  chmod 0444 "$tmp"; mv -f "$tmp" "$out/guard-bundle.json"
  verify "$(cd "$out" && pwd -P)/guard-bundle.json"
  echo "EVORCH_PROCESS_GUARD_BUNDLE=$(cd "$out" && pwd -P)/guard-bundle.json"
}
prepare() {
  local profile=release out=''
  while [[ $# -gt 0 ]]; do
    case "$1" in --profile) profile=$2; shift 2;; --out) out=$2;shift 2;; *) die "unknown prepare argument: $1";; esac
  done
  [[ -n "$out" && ( "$profile" == release || "$profile" == dev ) ]] || die 'prepare --profile release|dev --out DIR'
  local listing helper recipe='["build","--locked","-p","process-guard","--bin","evorch-process-guard"]'
  local args=(build --locked -p process-guard --bin evorch-process-guard)
  if [[ "$profile" == release ]]; then
    args+=(--release); recipe='["build","--locked","--release","-p","process-guard","--bin","evorch-process-guard"]'
  fi
  listing=$(mktemp); trap 'rm -f "$listing"' RETURN
  (cd "$repo" && CARGO_BUILD_GC_ENABLED=false cargo "${args[@]}" -j 1 --message-format=json-render-diagnostics) >"$listing"
  helper=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "evorch-process-guard" and .executable != null) | .executable' "$listing" | tail -n1)
  [[ -n "$helper" ]] || die 'Cargo did not report helper executable'
  seal "$helper" "$out" test-helper "$profile" "$recipe"
}
bind() {
  local bundle='' metadata=''
  while [[ $# -gt 0 ]]; do case "$1" in --bundle) bundle=$2; shift 2;; --build-metadata) metadata=$2; shift 2;; *) die "unknown bind argument $1";; esac; done
  [[ -f "$bundle" ]] || bundle="$bundle/guard-bundle.json"
  verify "$bundle"; regular "$metadata"
  local dir sha helper paths p role dest tmp callers grouped name rel
  dir=$(cd "$(dirname "$bundle")" && pwd -P); sha=$(jq -r .helper.sha256 "$bundle"); helper="$dir/$(jq -r .helper.file "$bundle")"
  paths=$(mktemp); grouped=$(mktemp); trap 'rm -f "$paths" "$grouped"' RETURN
  jq -rs '[.[] | if has("rust-build-meta") then
      .["rust-build-meta"] as $meta | $meta["non-test-binaries"] | to_entries[] | .value[] | select(.kind == "bin-exe") | {name:.name,path:(if (.path|startswith("/")) then .path else $meta["target-directory"] + "/" + .path end)}
    else select(.reason == "compiler-artifact" and .executable != null and (.target.kind | index("bin")) and .profile.test == false) | {name:.target.name,path:.executable} end]
    | map(select(.name == "evorch" or .name == "evorch-core" or .name == "evorch-gui")) | unique_by(.path) | .[] | [.name,.path] | @tsv' "$metadata" >"$paths"
  while IFS=$'\t' read -r name p; do
    [[ -n "$p" ]] || continue
    p=$(realpath "$p"); regular "$p"; dest=$(dirname "$p"); rel=".evorch-guard/$sha/evorch-process-guard"
    mkdir -p "$dest/.evorch-guard/$sha"
    [[ ! -L "$dest/.evorch-guard" && ! -L "$dest/.evorch-guard/$sha" ]] || die 'symlink helper directory'
    if [[ ! -f "$dest/$rel" ]]; then install -m 0555 "$helper" "$dest/$rel"; fi
    [[ "$(hash "$dest/$rel")" == "$sha" ]] || die 'immutable development helper mismatch'
    case "$name" in evorch) role=cli;;evorch-core) role=core;;evorch-gui) role=gui;; esac
    jq -cn --arg dir "$dest" --arg role "$role" --arg file "$(basename "$p")" --arg sha "$(hash "$p")" --argjson size "$(size "$p")" '{dir:$dir,caller:{role:$role,file:$file,size_bytes:$size,sha256:$sha}}' >>"$grouped"
  done <"$paths"
  while IFS= read -r dest; do
    callers=$(jq -sc --arg dir "$dest" 'map(select(.dir==$dir)|.caller)' "$grouped")
    tmp=$(mktemp "$dest/.guard-bundle.XXXXXX")
    jq --arg rel ".evorch-guard/$sha/evorch-process-guard" --argjson callers "$callers" '.purpose="development" | .helper.file=$rel | .callers=$callers' "$bundle" >"$tmp"
    chmod 0444 "$tmp"; mv -f "$tmp" "$dest/guard-bundle.json"; verify "$dest/guard-bundle.json"
  done < <(jq -rs 'map(.dir)|unique[]' "$grouped")
}
case "${1:-}" in
 prepare) shift;prepare "$@";;
 seal) shift;seal "$@";;
 verify) verify "${2:?manifest path}";;
 smoke) verify "${2:?manifest path}"; b=$2; env -i "$(dirname "$b")/$(jq -r .helper.file "$b")" --smoke-bundle "$b";;
 bind-callers) shift;bind "$@";;
 *) die 'prepare --profile release --out DIR | seal HELPER DIR PURPOSE PROFILE BUILD_ARGS [ROLE:CALLER...] | verify MANIFEST | smoke MANIFEST | bind-callers --bundle MANIFEST --build-metadata FILE';;
esac
