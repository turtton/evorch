#!/usr/bin/env bash
# Tests for scripts/product-artifact.sh with a dummy executable; no cargo.
# The Rust side of the contract is tested in crates/test-support/src/product_artifact.rs.
set -euo pipefail
cd "$(dirname "$0")/../.."
tool="$PWD/scripts/product-artifact.sh"
work=$(mktemp -d)
trap 'chmod -R u+w "$work"; rm -rf "$work"' EXIT

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

printf 'dummy evorch-core bytes\n' > "$work/dummy"
chmod +x "$work/dummy"

expect ok "seal a dummy executable" "$tool" seal "$work/dummy" "$work/sealed"
mode_is() { [[ "$(stat -c %a "$1")" == "$2" ]]; }
expect ok "the sealed executable is read-only and executable" mode_is "$work/sealed/evorch-core" 555
expect ok "the sealed manifest is read-only" mode_is "$work/sealed/product-manifest.json" 444
expect ok "the manifest records no feature" jq -e '.features == [] and .profile == "release"' "$work/sealed/product-manifest.json"
expect ok "the manifest records the distribution arguments" \
  jq -e '.distribution_args == ["-p","evorch-core","--bin","evorch-core"]' "$work/sealed/product-manifest.json"
expect ok "the manifest records the build command" \
  jq -e '.build_args == ["build","--locked","--release","-p","evorch-core","--bin","evorch-core"]' "$work/sealed/product-manifest.json"
env_lines=$("$tool" seal "$work/dummy" "$work/sealed")
expect ok "seal prints both variables" bash -c "grep -q '^EVORCH_PRODUCT_CORE_EXE=.*/evorch-core\$' <<< '$env_lines' && grep -q '^EVORCH_PRODUCT_MANIFEST=.*/product-manifest.json\$' <<< '$env_lines'"

# A downloaded copy has lost its modes; install checks content, then reseals.
mkdir "$work/downloaded"
cp "$work/sealed/evorch-core" "$work/sealed/product-manifest.json" "$work/downloaded/"
chmod 0644 "$work/downloaded/evorch-core" "$work/downloaded/product-manifest.json"
expect ok "install accepts a downloaded copy" "$tool" install "$work/downloaded" "$work/installed"
expect ok "install restores the executable mode" mode_is "$work/installed/evorch-core" 555
expect ok "install restores the manifest mode" mode_is "$work/installed/product-manifest.json" 444
expect ok "an installed copy is byte-identical" cmp "$work/dummy" "$work/installed/evorch-core"

# Anything that differs from the manifest is refused.
tamper() { # <dir> <jq-filter>: edit the manifest of a downloaded copy
  chmod u+w "$1/product-manifest.json"
  jq "$2" "$1/product-manifest.json" > "$1/m.tmp" && mv "$1/m.tmp" "$1/product-manifest.json"
}
fresh() { rm -rf "$work/bad"; mkdir "$work/bad"; cp "$work/downloaded/"* "$work/bad/"; }
fresh; printf 'changed' >> "$work/bad/evorch-core"
expect fail "a changed executable is refused" "$tool" install "$work/bad" "$work/out1"
fresh; tamper "$work/bad" '.features = ["renderer-test-helper"]'
expect fail "a feature is refused" "$tool" install "$work/bad" "$work/out2"
fresh; tamper "$work/bad" '.build_args = ["build","-p","evorch-core","--bin","evorch-core"]'
expect fail "another build command is refused" "$tool" install "$work/bad" "$work/out3"
fresh; tamper "$work/bad" '.profile = "dev"'
expect fail "another profile is refused" "$tool" install "$work/bad" "$work/out4"
fresh; tamper "$work/bad" 'del(.source_sha)'
expect fail "a manifest without a field is refused" "$tool" install "$work/bad" "$work/out5"
fresh; rm "$work/bad/product-manifest.json"
expect fail "a missing manifest is refused" "$tool" install "$work/bad" "$work/out6"
fresh; rm "$work/bad/evorch-core"
expect fail "a missing executable is refused" "$tool" install "$work/bad" "$work/out7"
expect fail "a missing download directory is refused" "$tool" install "$work/none" "$work/out8"
mkdir -p "$work/target/release"
expect fail "a sealed copy inside a target directory is refused" "$tool" seal "$work/dummy" "$work/target/release"
expect fail "seal needs an executable" "$tool" seal "$work/none" "$work/out9"
expect fail "an unknown subcommand is refused" "$tool" frobnicate

if (( fails > 0 )); then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "product artifact sealing behaves as documented"
