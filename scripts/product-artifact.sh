#!/usr/bin/env bash
# The sealed product artifact: the `evorch-core` binary as it is distributed,
# built apart from every test build, plus a manifest that describes it. On
# Linux the sealed product is a closure of four files: the core, the
# `evorch-process-guard` helper built by the same release build, the helper's
# runtime bundle (guard-bundle.json, scripts/guard-artifact.sh) and the
# product manifest (schema 2) that records all three. The reader (and the
# contract the manifest follows) is crates/test-support/src/product_artifact.rs.
#
# Usage:
#   scripts/product-artifact.sh build <outdir>                 build, then seal into <outdir>
#   scripts/product-artifact.sh seal <core> <helper> <outdir>  seal existing executables (Linux)
#   scripts/product-artifact.sh seal <core> <outdir>           seal an existing executable (not Linux)
#   scripts/product-artifact.sh install <srcdir> <dir>         verify a downloaded copy, reseal it into <dir>
#
# On Linux `build` runs `cargo build --locked --release -p evorch-core -p
# process-guard --bin evorch-core --bin evorch-process-guard`: the arguments of
# the distribution (flake.nix) and no feature. Elsewhere the helper is not
# part of the product and the build selects `evorch-core` alone.
# `seal` and `install` leave, in <outdir>, evorch-core (mode 0555),
# evorch-process-guard (0555), guard-bundle.json (0444) and
# product-manifest.json (0444); the helper and the bundle only on Linux.
# `build` and `install` print the variables the product tests read, as
# NAME=value lines on stdout (the format of $GITHUB_ENV;
# `export $(scripts/product-artifact.sh build dir)` locally); progress goes to
# stderr. `install` exists because upload-artifact drops file modes: it checks
# size and SHA-256 of every file against the manifests before it restores them.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)
guard_tool="$repo/scripts/guard-artifact.sh"

exe_name=evorch-core
guard_name=evorch-process-guard
bundle_name=guard-bundle.json
manifest_name=product-manifest.json
exe_env=EVORCH_PRODUCT_CORE_EXE
manifest_env=EVORCH_PRODUCT_MANIFEST
bundle_env=EVORCH_PROCESS_GUARD_BUNDLE
if [[ "$(uname -s)" == Linux ]]; then
  linux=true
  distribution_args='["-p","evorch-core","-p","process-guard","--bin","evorch-core","--bin","evorch-process-guard"]'
  build_args='["build","--locked","--release","-p","evorch-core","-p","process-guard","--bin","evorch-core","--bin","evorch-process-guard"]'
else
  linux=false
  distribution_args='["-p","evorch-core","--bin","evorch-core"]'
  build_args='["build","--locked","--release","-p","evorch-core","--bin","evorch-core"]'
fi

die() { echo "product-artifact: $*" >&2; exit 1; }

# A sealed copy must never sit where a test build writes.
refuse_target_dir() {
  case "$(cd "$1" && pwd -P)/" in
    */target/*) die "$1 is inside a target directory" ;;
  esac
}

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
size_of() { wc -c < "$1" | tr -d ' '; }
regular() { [[ -f "$1" && ! -L "$1" ]] || die "$1 is not a regular file"; }

# Checks the content of <dir>/{evorch-core,product-manifest.json} and, on Linux,
# of the helper and the bundle against the contract, whatever their modes are.
verify_content() {
  local dir=$1 exe manifest guard bundle
  exe="$dir/$exe_name"
  manifest="$dir/$manifest_name"
  regular "$exe"
  regular "$manifest"
  jq -e --argjson dist "$distribution_args" --argjson build "$build_args" \
    --arg file "$exe_name" --argjson size "$(size_of "$exe")" --arg sha "$(sha256_of "$exe")" '
      .schema_version == 2
      and .kind == "evorch-core-product"
      and .package == "evorch-core" and .bin == "evorch-core"
      and .profile == "release"
      and .features == []
      and .distribution_args == $dist and .build_args == $build
      and .file == $file and .size_bytes == $size and .sha256 == $sha
      and (.target | type == "string") and (.rustc | type == "string")
      and (.rustflags | type == "string") and (.source_sha | type == "string")' \
    "$manifest" > /dev/null \
    || die "$manifest does not describe $exe (contract, features, size or sha256 differ)"
  if [[ "$linux" != true ]]; then
    jq -e '.guard == null and .guard_bundle == null' "$manifest" > /dev/null \
      || die "$manifest names a guard helper, but this platform has none"
    return
  fi
  guard="$dir/$guard_name"
  bundle="$dir/$bundle_name"
  regular "$guard"
  regular "$bundle"
  jq -e --arg gfile "$guard_name" --argjson gsize "$(size_of "$guard")" --arg gsha "$(sha256_of "$guard")" \
    --arg bfile "$bundle_name" --argjson bsize "$(size_of "$bundle")" --arg bsha "$(sha256_of "$bundle")" \
    --argjson build "$build_args" '
      .guard.package == "process-guard" and .guard.bin == "evorch-process-guard"
      and .guard.file == $gfile and .guard.size_bytes == $gsize and .guard.sha256 == $gsha
      and .guard.profile == "release" and .guard.features == [] and .guard.build_args == $build
      and (.guard.guard_abi | type == "number")
      and (.guard.guard_compat_id | test("^[0-9a-f]{64}$"))
      and .guard_bundle == {file: $bfile, size_bytes: $bsize, sha256: $bsha}' \
    "$manifest" > /dev/null \
    || die "$manifest does not describe $guard and $bundle (helper or bundle differ)"
  # The nested receipt of the bundle must be the root receipt, field by field.
  jq -e --slurpfile root "$manifest" --arg file "$exe_name" \
    --argjson size "$(size_of "$exe")" --arg sha "$(sha256_of "$exe")" '
      ($root[0]) as $r
      | .purpose == "distribution"
      and .source_sha == $r.source_sha and .target == $r.target
      and .guard_abi == $r.guard.guard_abi and .guard_compat_id == $r.guard.guard_compat_id
      and .helper == ($r.guard | del(.guard_abi, .guard_compat_id))
      and .callers == [{role: "core", file: $file, size_bytes: $size, sha256: $sha}]' \
    "$bundle" > /dev/null \
    || die "$bundle does not agree with $manifest (receipt, caller or purpose differ)"
}

# Content, then modes and the helper's own check of the bundle (Linux).
verify_dir() {
  local dir=$1
  verify_content "$dir"
  [[ "$(stat -c %a "$dir/$exe_name")" == 555 ]] || die "$dir/$exe_name is not mode 0555"
  [[ "$(stat -c %a "$dir/$manifest_name")" == 444 ]] || die "$dir/$manifest_name is not mode 0444"
  if [[ "$linux" == true ]]; then
    "$guard_tool" verify "$dir/$bundle_name" || die "the guard bundle in $dir is refused"
  fi
}

seal_into() { # <core> <helper|""> <outdir>
  local exe=$1 helper=$2 out=$3 host rustc_version sha_src guard='null' bundle='null'
  [[ -f "$exe" ]] || die "no executable at $exe"
  [[ "$linux" != true || -f "$helper" ]] || die "no helper executable at $helper"
  mkdir -p "$out"
  refuse_target_dir "$out"
  rm -f "$out/$exe_name" "$out/$guard_name" "$out/$bundle_name" "$out/$manifest_name"
  install -m 0555 "$exe" "$out/$exe_name"
  sha_src=${GITHUB_SHA:-$(git -C "$repo" rev-parse HEAD 2> /dev/null || echo unknown)}
  if [[ "$linux" == true ]]; then
    # The bundle records the combined recipe the two binaries were built with.
    GITHUB_SHA=$sha_src "$guard_tool" seal "$helper" "$out" distribution release "$build_args" \
      "core:$out/$exe_name" > /dev/null
    guard=$(jq -c '.helper + {guard_abi: .guard_abi, guard_compat_id: .guard_compat_id}' "$out/$bundle_name")
    bundle=$(jq -cn --arg file "$bundle_name" --argjson size "$(size_of "$out/$bundle_name")" \
      --arg sha "$(sha256_of "$out/$bundle_name")" '{file: $file, size_bytes: $size, sha256: $sha}')
  fi
  host=$(rustc -vV | sed -n 's/^host: //p')
  rustc_version=$(rustc -V)
  jq -n --argjson dist "$distribution_args" --argjson build "$build_args" \
    --arg target "$host" --arg rustc "$rustc_version" --arg rustflags "${RUSTFLAGS:-}" \
    --arg sha_src "$sha_src" --argjson guard "$guard" --argjson bundle "$bundle" \
    --arg file "$exe_name" --argjson size "$(size_of "$out/$exe_name")" \
    --arg sha "$(sha256_of "$out/$exe_name")" '{
      schema_version: 2, kind: "evorch-core-product",
      package: "evorch-core", bin: "evorch-core",
      distribution_args: $dist, build_args: $build,
      features: [], profile: "release",
      target: $target, rustc: $rustc, rustflags: $rustflags, source_sha: $sha_src,
      file: $file, size_bytes: $size, sha256: $sha,
      guard: $guard, guard_bundle: $bundle}' > "$out/$manifest_name"
  chmod 0444 "$out/$manifest_name"
  verify_dir "$out"
}

print_env() { # <dir>
  local dir
  dir=$(cd "$1" && pwd -P)
  echo "$exe_env=$dir/$exe_name"
  echo "$manifest_env=$dir/$manifest_name"
  if [[ "$linux" == true ]]; then
    echo "$bundle_env=$dir/$bundle_name"
  fi
}

build() { # <outdir>
  local out=$1 listing exe helper=''
  listing=$(mktemp)
  trap 'rm -f "$listing"' RETURN
  # The distribution's arguments, release profile, no feature. Diagnostics go
  # to stderr (json-render-diagnostics); stdout is the artifact listing.
  if [[ "$linux" == true ]]; then "$guard_tool" check-dependencies; fi
  # shellcheck disable=SC2046 # the arguments are fixed words
  (cd "$repo" && CARGO_BUILD_GC_ENABLED=false cargo $(jq -r '.[]' <<< "$build_args") --message-format=json-render-diagnostics) > "$listing"
  exe=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "evorch-core"
      and (.target.kind | index("bin")) and .executable != null) | .executable' "$listing" | tail -n 1)
  [[ -n "$exe" ]] || die "cargo did not report the evorch-core executable"
  if [[ "$linux" == true ]]; then
    helper=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "evorch-process-guard"
        and (.target.kind | index("bin")) and .executable != null) | .executable' "$listing" | tail -n 1)
    [[ -n "$helper" ]] || die "cargo did not report the evorch-process-guard executable"
  fi
  seal_into "$exe" "$helper" "$out"
  echo "sealed $(size_of "$out/$exe_name") bytes, sha256 $(sha256_of "$out/$exe_name")" >&2
  print_env "$out"
}

install_from() { # <srcdir> <destdir>
  local src=$1 dest=$2
  [[ -d "$src" ]] || die "no downloaded artifact directory $src"
  # download-artifact keeps no modes: verify content first, then restore them.
  verify_content "$src"
  mkdir -p "$dest"
  refuse_target_dir "$dest"
  rm -f "$dest/$exe_name" "$dest/$guard_name" "$dest/$bundle_name" "$dest/$manifest_name"
  install -m 0555 "$src/$exe_name" "$dest/$exe_name"
  if [[ "$linux" == true ]]; then
    install -m 0555 "$src/$guard_name" "$dest/$guard_name"
    install -m 0444 "$src/$bundle_name" "$dest/$bundle_name"
  fi
  install -m 0444 "$src/$manifest_name" "$dest/$manifest_name"
  verify_dir "$dest"
  print_env "$dest"
}

command -v jq > /dev/null || die "jq is required"
case "${1:-}" in
  build) build "${2:?build needs <outdir>}" ;;
  seal)
    if [[ "$linux" == true ]]; then
      seal_into "${2:?seal needs <core>}" "${3:?seal needs <helper>}" "${4:?seal needs <outdir>}"
      print_env "$4"
    else
      seal_into "${2:?seal needs <core>}" "" "${3:?seal needs <outdir>}"
      print_env "$3"
    fi ;;
  install) install_from "${2:?install needs <srcdir>}" "${3:?install needs <destdir>}" ;;
  *) echo "usage: $0 build <outdir> | seal <core> <helper> <outdir> | install <srcdir> <destdir>" >&2; exit 2 ;;
esac
