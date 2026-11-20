#!/usr/bin/env bash
# Tests for scripts/product-artifact.sh with dummy executables; no cargo.
# The helper is a shell stand-in that answers `--describe` and
# `--verify-bundle` the way scripts/guard-artifact.sh asks; the real helper is
# covered by scripts/tests/test_guard_artifact.sh. The Rust side of the
# contract is tested in crates/test-support/src/product_artifact.rs.
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
mode_is() { [[ "$(stat -c %a "$1")" == "$2" ]]; }

printf 'dummy evorch-core bytes\n' > "$work/dummy"
chmod +x "$work/dummy"

if [[ "$(uname -s)" != Linux ]]; then
  # No helper outside Linux: schema 2 with null guard fields and the core-only recipe.
  expect ok "seal a dummy executable" "$tool" seal "$work/dummy" "$work/sealed"
  expect ok "the manifest is schema 2 without a guard" \
    jq -e '.schema_version == 2 and .guard == null and .guard_bundle == null
      and .distribution_args == ["-p","evorch-core","--bin","evorch-core"]' "$work/sealed/product-manifest.json"
  [[ ! -e "$work/sealed/guard-bundle.json" ]] || { echo "FAIL: a bundle was sealed without a helper" >&2; fails=$((fails + 1)); }
  if (( fails > 0 )); then echo "$fails check(s) failed" >&2; exit 1; fi
  echo "product artifact sealing behaves as documented (no helper platform)"
  exit 0
fi

cat > "$work/helper" <<'HELPER'
#!/bin/sh
case "$1" in
  --describe) echo '{"target":"x86_64-unknown-linux-gnu","guard_abi":1,"guard_compat_id":"0000000000000000000000000000000000000000000000000000000000000001","features":[]}' ;;
  --verify-bundle)
    while read -r line; do case $line in *refuse-me*) exit 1 ;; esac; done < "$2"
    exit 0 ;;
  *) exit 2 ;;
esac
HELPER
chmod +x "$work/helper"

combined_build='["build","--locked","--release","-p","evorch-core","-p","process-guard","--bin","evorch-core","--bin","evorch-process-guard"]'
combined_dist='["-p","evorch-core","-p","process-guard","--bin","evorch-core","--bin","evorch-process-guard"]'

expect ok "seal the four-file closure" "$tool" seal "$work/dummy" "$work/helper" "$work/sealed"
expect ok "the sealed executable is read-only and executable" mode_is "$work/sealed/evorch-core" 555
expect ok "the sealed helper is read-only and executable" mode_is "$work/sealed/evorch-process-guard" 555
expect ok "the sealed bundle is read-only" mode_is "$work/sealed/guard-bundle.json" 444
expect ok "the sealed manifest is read-only" mode_is "$work/sealed/product-manifest.json" 444
expect ok "the manifest is schema 2 and records no feature" \
  jq -e '.schema_version == 2 and .features == [] and .profile == "release"' "$work/sealed/product-manifest.json"
expect ok "the manifest records the combined distribution arguments" \
  jq -e --argjson want "$combined_dist" '.distribution_args == $want' "$work/sealed/product-manifest.json"
expect ok "the manifest records the combined build command" \
  jq -e --argjson want "$combined_build" '.build_args == $want' "$work/sealed/product-manifest.json"
expect ok "the manifest records the helper and the bundle" \
  jq -e '.guard.file == "evorch-process-guard" and .guard.guard_abi == 1 and (.guard.guard_compat_id | length == 64)
    and .guard_bundle.file == "guard-bundle.json"' "$work/sealed/product-manifest.json"
expect ok "the bundle carries the combined recipe, not the standalone helper build" \
  jq -e --argjson want "$combined_build" '.helper.build_args == $want and .purpose == "distribution"
    and (.callers | map(.role)) == ["core"]' "$work/sealed/guard-bundle.json"
env_lines=$("$tool" seal "$work/dummy" "$work/helper" "$work/sealed")
expect ok "seal prints the three variables" bash -c "grep -q '^EVORCH_PRODUCT_CORE_EXE=.*/evorch-core\$' <<< '$env_lines' \
  && grep -q '^EVORCH_PRODUCT_MANIFEST=.*/product-manifest.json\$' <<< '$env_lines' \
  && grep -q '^EVORCH_PROCESS_GUARD_BUNDLE=.*/guard-bundle.json\$' <<< '$env_lines'"
expect fail "seal without a helper is refused" "$tool" seal "$work/dummy" "$work/none" "$work/out0"
expect fail "the old two-argument seal is refused" "$tool" seal "$work/dummy" "$work/out0b"

# A downloaded copy has lost its modes; install checks content, then reseals.
download() { # <dir>: a downloaded copy of the sealed closure
  rm -rf "$1"
  mkdir "$1"
  cp "$work/sealed/"* "$1/"
  chmod 0644 "$1"/*
}
download "$work/downloaded"
expect ok "install accepts a downloaded copy" "$tool" install "$work/downloaded" "$work/installed"
expect ok "install restores the executable mode" mode_is "$work/installed/evorch-core" 555
expect ok "install restores the helper mode" mode_is "$work/installed/evorch-process-guard" 555
expect ok "install restores the bundle mode" mode_is "$work/installed/guard-bundle.json" 444
expect ok "install restores the manifest mode" mode_is "$work/installed/product-manifest.json" 444
expect ok "an installed core is byte-identical" cmp "$work/dummy" "$work/installed/evorch-core"
expect ok "an installed helper is byte-identical" cmp "$work/helper" "$work/installed/evorch-process-guard"

# Anything that differs from the manifests is refused.
tamper() { # <dir> <file> <jq-filter>: edit one JSON file of a downloaded copy
  jq "$3" "$1/$2" > "$1/m.tmp" && mv "$1/m.tmp" "$1/$2"
}
tamper_bundle() { # <dir> <jq-filter>: edit the bundle and keep the manifest receipt of it current
  jq "$2" "$1/guard-bundle.json" > "$1/b.tmp" && mv "$1/b.tmp" "$1/guard-bundle.json"
  jq --argjson size "$(wc -c < "$1/guard-bundle.json" | tr -d " ")" --arg sha "$(sha256sum "$1/guard-bundle.json" | cut -d" " -f1)" '.guard_bundle.size_bytes = $size | .guard_bundle.sha256 = $sha' "$1/product-manifest.json" > "$1/m.tmp" && mv "$1/m.tmp" "$1/product-manifest.json"
}
fresh() { download "$work/bad"; }
fresh; printf 'changed' >> "$work/bad/evorch-core"
expect fail "a changed core is refused" "$tool" install "$work/bad" "$work/out1"
fresh; printf 'changed' >> "$work/bad/evorch-process-guard"
expect fail "a changed helper is refused" "$tool" install "$work/bad" "$work/out1h"
fresh; printf '\n' >> "$work/bad/guard-bundle.json"
expect fail "a changed bundle is refused" "$tool" install "$work/bad" "$work/out1b"
fresh; : > "$work/bad/evorch-process-guard"
expect fail "an emptied helper is refused" "$tool" install "$work/bad" "$work/out1e"
fresh; rm "$work/bad/evorch-process-guard"
expect fail "a missing helper is refused" "$tool" install "$work/bad" "$work/out1m"
fresh; rm "$work/bad/guard-bundle.json"
expect fail "a missing bundle is refused" "$tool" install "$work/bad" "$work/out1n"
fresh; tamper "$work/bad" product-manifest.json '.schema_version = 1'
expect fail "schema 1 is refused" "$tool" install "$work/bad" "$work/out1s"
fresh; tamper "$work/bad" product-manifest.json '.guard = null | .guard_bundle = null'
expect fail "a Linux manifest without a helper is refused" "$tool" install "$work/bad" "$work/out1g"
fresh; tamper "$work/bad" product-manifest.json '.features = ["renderer-test-helper"]'
expect fail "a feature is refused" "$tool" install "$work/bad" "$work/out2"
fresh; tamper "$work/bad" product-manifest.json '.build_args = ["build","--locked","--release","-p","evorch-core","--bin","evorch-core"]'
expect fail "the core-only build command is refused on Linux" "$tool" install "$work/bad" "$work/out3"
fresh; tamper "$work/bad" product-manifest.json '.profile = "dev"'
expect fail "another profile is refused" "$tool" install "$work/bad" "$work/out4"
fresh; tamper "$work/bad" product-manifest.json 'del(.source_sha)'
expect fail "a manifest without a field is refused" "$tool" install "$work/bad" "$work/out5"
fresh; tamper "$work/bad" product-manifest.json '.guard.guard_compat_id = "ff"'
expect fail "a malformed compat id is refused" "$tool" install "$work/bad" "$work/out5a"
fresh; tamper "$work/bad" product-manifest.json '.guard.guard_abi = 2'
expect fail "a helper ABI that differs from the bundle is refused" "$tool" install "$work/bad" "$work/out5b"
fresh; tamper "$work/bad" product-manifest.json '.guard.sha256 = ("0" * 64)'
expect fail "a helper hash that differs from the file is refused" "$tool" install "$work/bad" "$work/out5c"
fresh; tamper "$work/bad" product-manifest.json '.guard.build_args = ["build","--locked","--release","-p","process-guard","--bin","evorch-process-guard"]'
expect fail "the standalone helper recipe in the root receipt is refused" "$tool" install "$work/bad" "$work/out5d"
fresh; tamper_bundle "$work/bad" '.helper.build_args = ["build","--locked","--release","-p","process-guard","--bin","evorch-process-guard"]'
expect fail "the standalone helper recipe in the bundle is refused" "$tool" install "$work/bad" "$work/out5e"
fresh; tamper_bundle "$work/bad" '.note = "refuse-me"'
expect fail "a bundle that the helper refuses is refused" "$tool" install "$work/bad" "$work/out5i"
fresh; tamper "$work/bad" product-manifest.json '.guard.build_args = ["build","--locked","--release","-p","process-guard","--bin","evorch-process-guard"]'
tamper_bundle "$work/bad" '.helper.build_args = ["build","--locked","--release","-p","process-guard","--bin","evorch-process-guard"]'
expect fail "the standalone helper recipe in both receipts is refused" "$tool" install "$work/bad" "$work/out5j"
fresh; tamper_bundle "$work/bad" '.callers[0].sha256 = ("0" * 64)'
expect fail "a bundle bound to another core is refused" "$tool" install "$work/bad" "$work/out5f"
fresh; tamper_bundle "$work/bad" '.callers[0].role = "cli"'
expect fail "a bundle that binds the core under another role is refused" "$tool" install "$work/bad" "$work/out5k"
fresh; tamper_bundle "$work/bad" '.callers += [.callers[0]]'
expect fail "a bundle with a second caller is refused" "$tool" install "$work/bad" "$work/out5l"
fresh; tamper_bundle "$work/bad" '.purpose = "test-helper"'
expect fail "a bundle with another purpose is refused" "$tool" install "$work/bad" "$work/out5g"
fresh; tamper_bundle "$work/bad" '.source_sha = ("1" * 40)'
expect fail "a bundle from another source is refused" "$tool" install "$work/bad" "$work/out5h"
fresh; rm "$work/bad/product-manifest.json"
expect fail "a missing manifest is refused" "$tool" install "$work/bad" "$work/out6"
fresh; rm "$work/bad/evorch-core"
expect fail "a missing executable is refused" "$tool" install "$work/bad" "$work/out7"
expect fail "a missing download directory is refused" "$tool" install "$work/none" "$work/out8"
mkdir -p "$work/target/release"
expect fail "a sealed copy inside a target directory is refused" "$tool" seal "$work/dummy" "$work/helper" "$work/target/release"
expect fail "seal needs an executable" "$tool" seal "$work/none" "$work/helper" "$work/out9"
expect fail "an unknown subcommand is refused" "$tool" frobnicate

if (( fails > 0 )); then
  echo "$fails check(s) failed" >&2
  exit 1
fi
echo "product artifact sealing behaves as documented"
