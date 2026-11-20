#!/usr/bin/env bash
# The sealed product artifact: the `evorch-core` binary as it is distributed,
# built apart from every test build, plus a manifest that describes it. The
# reader (and the contract the manifest follows) is
# crates/test-support/src/product_artifact.rs.
#
# Usage:
#   scripts/product-artifact.sh build <outdir>            build, then seal into <outdir>
#   scripts/product-artifact.sh seal <exe> <outdir>       seal an existing executable
#   scripts/product-artifact.sh install <srcdir> <dir>    verify a downloaded copy, reseal it into <dir>
#
# `build` runs `cargo build --locked --release -p evorch-core --bin evorch-core`:
# the arguments of the distribution (flake.nix `evorch-core`) and no feature.
# `seal` and `install` leave <outdir>/evorch-core (mode 0555) and
# <outdir>/product-manifest.json (mode 0444). `build` and `install` print the
# variables the product tests read, as NAME=value lines on stdout (the format
# of $GITHUB_ENV; `export $(scripts/product-artifact.sh build dir)` locally);
# progress goes to stderr. `install` exists because upload-artifact drops file
# modes: it checks size and SHA-256 against the manifest before it restores them.
set -euo pipefail
repo=$(cd "$(dirname "$0")/.." && pwd)

exe_name=evorch-core
manifest_name=product-manifest.json
exe_env=EVORCH_PRODUCT_CORE_EXE
manifest_env=EVORCH_PRODUCT_MANIFEST
distribution_args='["-p","evorch-core","--bin","evorch-core"]'
build_args='["build","--locked","--release","-p","evorch-core","--bin","evorch-core"]'

die() { echo "product-artifact: $*" >&2; exit 1; }

# A sealed copy must never sit where a test build writes.
refuse_target_dir() {
  case "$(cd "$1" && pwd -P)/" in
    */target/*) die "$1 is inside a target directory" ;;
  esac
}

sha256_of() { sha256sum "$1" | cut -d' ' -f1; }
size_of() { wc -c < "$1" | tr -d ' '; }

# Checks <dir>/{evorch-core,product-manifest.json} against the contract.
verify_dir() {
  local dir=$1 exe manifest
  exe="$dir/$exe_name"
  manifest="$dir/$manifest_name"
  [[ -f "$exe" && ! -L "$exe" ]] || die "$exe is not a regular file"
  [[ -f "$manifest" && ! -L "$manifest" ]] || die "$manifest is not a regular file"
  jq -e --argjson dist "$distribution_args" --argjson build "$build_args" \
    --arg file "$exe_name" --argjson size "$(size_of "$exe")" --arg sha "$(sha256_of "$exe")" '
      .schema_version == 1
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
}

seal_into() { # <exe> <outdir>
  local exe=$1 out=$2 host rustc_version
  [[ -f "$exe" ]] || die "no executable at $exe"
  mkdir -p "$out"
  refuse_target_dir "$out"
  rm -f "$out/$exe_name" "$out/$manifest_name"
  install -m 0555 "$exe" "$out/$exe_name"
  host=$(rustc -vV | sed -n 's/^host: //p')
  rustc_version=$(rustc -V)
  jq -n --argjson dist "$distribution_args" --argjson build "$build_args" \
    --arg target "$host" --arg rustc "$rustc_version" --arg rustflags "${RUSTFLAGS:-}" \
    --arg sha_src "${GITHUB_SHA:-$(git -C "$repo" rev-parse HEAD 2> /dev/null || echo unknown)}" \
    --arg file "$exe_name" --argjson size "$(size_of "$out/$exe_name")" \
    --arg sha "$(sha256_of "$out/$exe_name")" '{
      schema_version: 1, kind: "evorch-core-product",
      package: "evorch-core", bin: "evorch-core",
      distribution_args: $dist, build_args: $build,
      features: [], profile: "release",
      target: $target, rustc: $rustc, rustflags: $rustflags, source_sha: $sha_src,
      file: $file, size_bytes: $size, sha256: $sha}' > "$out/$manifest_name"
  chmod 0444 "$out/$manifest_name"
  verify_dir "$out"
}

print_env() { # <dir>
  local dir
  dir=$(cd "$1" && pwd -P)
  echo "$exe_env=$dir/$exe_name"
  echo "$manifest_env=$dir/$manifest_name"
}

build() { # <outdir>
  local out=$1 listing exe
  listing=$(mktemp)
  trap 'rm -f "$listing"' RETURN
  # The distribution's arguments, release profile, no feature. Diagnostics go
  # to stderr (json-render-diagnostics); stdout is the artifact listing.
  # shellcheck disable=SC2046 # the arguments are fixed words
  (cd "$repo" && cargo $(jq -r '.[]' <<< "$build_args") --message-format=json-render-diagnostics) > "$listing"
  exe=$(jq -r 'select(.reason == "compiler-artifact" and .target.name == "evorch-core"
      and (.target.kind | index("bin")) and .executable != null) | .executable' "$listing" | tail -n 1)
  [[ -n "$exe" ]] || die "cargo did not report the evorch-core executable"
  seal_into "$exe" "$out"
  echo "sealed $(size_of "$out/$exe_name") bytes, sha256 $(sha256_of "$out/$exe_name")" >&2
  print_env "$out"
}

install_from() { # <srcdir> <destdir>
  local src=$1 dest=$2
  [[ -d "$src" ]] || die "no downloaded artifact directory $src"
  # download-artifact keeps no modes: verify content first, then restore them.
  verify_dir "$src"
  mkdir -p "$dest"
  refuse_target_dir "$dest"
  rm -f "$dest/$exe_name" "$dest/$manifest_name"
  install -m 0555 "$src/$exe_name" "$dest/$exe_name"
  install -m 0444 "$src/$manifest_name" "$dest/$manifest_name"
  verify_dir "$dest"
  print_env "$dest"
}

command -v jq > /dev/null || die "jq is required"
case "${1:-}" in
  build) build "${2:?build needs <outdir>}" ;;
  seal) seal_into "${2:?seal needs <exe>}" "${3:?seal needs <outdir>}"; print_env "$3" ;;
  install) install_from "${2:?install needs <srcdir>}" "${3:?install needs <destdir>}" ;;
  *) echo "usage: $0 build <outdir> | seal <exe> <outdir> | install <srcdir> <destdir>" >&2; exit 2 ;;
esac
