#!/usr/bin/env bash
# Delivery tests use an already-built, sealed helper. Never build from a test.
set -euo pipefail
repo=$(cd "$(dirname "$0")/../.." && pwd)
bundle=${EVORCH_PROCESS_GUARD_BUNDLE:?prepare a guard bundle first}
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
fail() { echo "test_guard_artifact: $*" >&2; exit 1; }
must_reject() { if "$@" >"$work/rejected.log" 2>&1; then fail "unexpected success: $*"; fi; }
"$repo/scripts/guard-artifact.sh" verify "$bundle"
"$repo/scripts/guard-artifact.sh" smoke "$bundle"
helper="$(dirname "$bundle")/$(jq -r .helper.file "$bundle")"
mkdir -p "$work/fixture/scripts" "$work/fixture/bin" "$work/fixture/target/debug"
cp "$repo/scripts/guard-artifact.sh" "$repo/scripts/guard-cargo.sh" "$work/fixture/scripts/"
app="$work/fixture/target/debug/evorch-core"
cp "$helper" "$app"
printf '%s\n' cargo-owned-sentinel >"$work/fixture/target/debug/evorch-process-guard"
jq -n --arg target "$work/fixture/target" '{
  "rust-build-meta": {
    "target-directory": $target,
    "non-test-binaries": {
      "core-id": [{"name":"evorch-core","kind":"bin-exe","path":"debug/evorch-core"}]
    }
  }
}' >"$work/binaries.json"
export FIXTURE_WORK="$work"
cat >"$work/fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
[[ "${CARGO_BUILD_GC_ENABLED:-}" == false ]]
printf '%s\n' "$*" >>"$FIXTURE_WORK/cargo-calls"
case "$1 $2" in
  "tree --locked") printf '%s\n' "$EXPECTED_TREE" | tr ' ' '\n' | sed 's/@/ v/';;
  "metadata --locked") printf '{}\n';;
  "nextest list") cat "$FIXTURE_WORK/binaries.json";;
  "nextest run")
    [[ "$*" == *--cargo-metadata* && "$*" == *--binaries-metadata* ]]
    [[ -f "$FIXTURE_WORK/fixture/target/debug/guard-bundle.json" ]]
    ;;
  *) echo 'unexpected Cargo invocation' >&2;exit 1;;
esac
SH
chmod +x "$work/fixture/bin/cargo"
mkdir -p "$work/fixture/crates/process-guard"
cp "$repo/crates/process-guard/Cargo.toml" "$work/fixture/crates/process-guard/"
export EXPECTED_TREE
EXPECTED_TREE=$(sed -n 's/^dependency-receipt = "\(.*\)"$/\1/p' "$repo/crates/process-guard/Cargo.toml")
PATH="$work/fixture/bin:$PATH" "$work/fixture/scripts/guard-artifact.sh" check-dependencies
must_reject env PATH="$work/fixture/bin:$PATH" EXPECTED_TREE=libc@0.0.0 "$work/fixture/scripts/guard-artifact.sh" check-dependencies
rm "$work/cargo-calls"
PATH="$work/fixture/bin:$PATH" "$work/fixture/scripts/guard-cargo.sh" run \
  --locked -p evorch-core --test fixture --run-ignored only --test-threads 1 --no-tests fail
[[ "$(wc -l <"$work/cargo-calls")" == 3 ]] || fail 'metadata must be reused without a second build'
[[ "$(tail -n1 "$work/cargo-calls")" == nextest\ run* ]] || fail 'binding must precede execution'
[[ "$(cat "$work/fixture/target/debug/evorch-process-guard")" == cargo-owned-sentinel ]] || fail 'Cargo helper was overwritten'
manifest="$work/fixture/target/debug/guard-bundle.json"
"$repo/scripts/guard-artifact.sh" verify "$manifest"
jq -e '.purpose=="development" and .callers[0].role=="core"
  and .callers[0].file=="evorch-core" and (.helper.file|startswith(".evorch-guard/"))' "$manifest" >/dev/null
# Compiler-artifact JSON-lines bind the exact same executable path too.
jq -cn --arg app "$app" '{reason:"compiler-artifact",target:{name:"evorch-core",kind:["bin"]},profile:{test:false},executable:$app}' >"$work/compiler.json"
"$repo/scripts/guard-artifact.sh" bind-callers --bundle "$bundle" --build-metadata "$work/compiler.json"
"$repo/scripts/guard-artifact.sh" verify "$manifest"
chmod u+w "$app"
printf x >>"$app"
must_reject "$repo/scripts/guard-artifact.sh" verify "$manifest"
must_reject env -u EVORCH_PROCESS_GUARD_BUNDLE "$repo/scripts/guard-cargo.sh" run --locked -p tools
must_reject "$repo/scripts/guard-artifact.sh" verify "$work/absent.json"
# Keep the pre-existing ignored-selection requirement when recognizing our wrapper.
definition=$(sed -n '/^ci_selects_ignored()/,/^}/p' "$repo/scripts/tests/test_check_central_files.sh")
eval "$definition"
ci="$work/ci.yml"
printf '%s\n' 'scripts/guard-cargo.sh run -p evorch-core --test fixture --run-ignored only --no-tests fail' >"$ci"
ci_selects_ignored fixture || fail 'wrapper ignored selection was not recognized'
printf '%s\n' 'scripts/guard-cargo.sh run -p evorch-core --test fixture --no-tests fail' >"$ci"
must_reject ci_selects_ignored fixture
printf '%s\n' 'scripts/guard-cargo.sh run -p evorch-core --test fixture --run-ignored only' >"$ci"
must_reject ci_selects_ignored fixture
printf '%s\n' 'scripts/guard-cargo.sh run -p evorch-core --test other --run-ignored only --no-tests fail' >"$ci"
must_reject ci_selects_ignored fixture
echo 'guard delivery, receipt drift, metadata reuse and ignored-selection negatives: passed'
