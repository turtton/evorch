#!/usr/bin/env bash
# Regression tests of scripts/release-expiry.sh against a mock `cargo` (the nextest
# listing) and a mock scripts/guard-cargo.sh (the run). No cargo, no network, no
# product: what is checked is the partition of the ignored cells into the three
# groups, the arguments of a run, its evidence requirement and the CEF environment
# rule of the groups that are not CEF.
set -euo pipefail
cd "$(dirname "$0")/../.."
work=$(mktemp -d "${TMPDIR:-/tmp}/test-release-expiry.XXXXXXXX")
trap 'rm -rf -- "$work"' EXIT
fails=0
ok() { # <description> <command...>
  local what=$1
  shift
  if ! "$@" > "$work/out" 2>&1; then
    echo "FAIL: $what" >&2
    sed 's/^/  | /' "$work/out" >&2
    fails=$((fails + 1))
  fi
}
rejects() { # <description> <command...>
  local what=$1
  shift
  if "$@" > "$work/out" 2>&1; then
    echo "FAIL: $what was accepted" >&2
    fails=$((fails + 1))
  fi
}
rejects_with() { # <description> <message> <command...>
  local what=$1 msg=$2
  shift 2
  if "$@" > "$work/out" 2>&1; then
    echo "FAIL: $what was accepted" >&2
    fails=$((fails + 1))
  elif ! grep -qF -- "$msg" "$work/out"; then
    echo "FAIL: $what was rejected for another reason (wanted: $msg)" >&2
    sed 's/^/  | /' "$work/out" >&2
    fails=$((fails + 1))
  fi
}

# A mock tree: the script under test beside a mock guard-cargo, a mock cargo on PATH.
mkdir -p "$work/bin" "$work/tree/scripts" "$work/meta"
cp scripts/release-expiry.sh "$work/tree/scripts/"
echo '{}' > "$work/meta/cargo.json"
echo '{}' > "$work/meta/default-binaries.json"

# `cargo nextest list ... --run-ignored only [-E <filterset>] --message-format json`
# over the cells in FAKE_CELLS ("binary:test ..."). A filterset selects a test when
# it names the test's binary and either names no test or names this one.
cat > "$work/bin/cargo" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
[[ "$1 $2" == "nextest list" ]] || { echo "mock cargo: unexpected: $*" >&2; exit 64; }
expr=''
ignored_only=false
while [[ $# -gt 0 ]]; do
  case "$1" in
    -E) expr=$2; shift 2 ;;
    --run-ignored) [[ "$2" == only ]] && ignored_only=true; shift 2 ;;
    *) shift ;;
  esac
done
$ignored_only || { echo "mock cargo: the listing must use --run-ignored only" >&2; exit 64; }
out='{"rust-suites":{}}'
for cell in $FAKE_CELLS; do
  bin=${cell%%:*}
  name=${cell#*:}
  status=matches
  if [[ -n "$expr" ]]; then
    if [[ "$expr" != *"binary(=$bin)"* ]]; then
      status=mismatch
    elif [[ "$expr" == *"test("* && "$expr" != *"test(=$name)"* ]]; then
      status=mismatch
    fi
  fi
  out=$(jq -c --arg bin "$bin" --arg name "$name" --arg status "$status" '
    .["rust-suites"]["evorch-core::" + $bin] |= (
      (. // {"package-name": "evorch-core", "binary-id": ("evorch-core::" + $bin), "testcases": {}})
      | .testcases[$name] = {ignored: true, "filter-match": {status: $status}})' <<< "$out")
done
echo "$out"
EOF
chmod +x "$work/bin/cargo"

# The mock run: records its arguments and the evidence directory, writes the death
# evidence of FAKE_EVIDENCE cells, exits with FAKE_RC.
cat > "$work/guard" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
{ printf '%s\n' "$@"; echo "evidence=$EVORCH_B3_EVIDENCE_DIR"; } > "$FAKE_RECORD"
for cell in ${FAKE_EVIDENCE:-}; do
  echo '{}' > "$EVORCH_B3_EVIDENCE_DIR/expiry-$cell-123-death.json"
done
exit "${FAKE_RC:-0}"
EOF
chmod +x "$work/guard"

all_cells='core_expiry:expiry_capture_first core_expiry:expiry_signal_first core_browser_expiry:expiry_chromium_t core_browser_expiry:expiry_chromium_stopped_t core_browser_expiry:expiry_cef_t core_browser_expiry:expiry_cef_stopped_t'
tree_script="$work/tree/scripts/release-expiry.sh"

ok "the six cells partition into the three groups" env FAKE_CELLS="$all_cells" PATH="$work/bin:$PATH" "$tree_script" check-inventory "$work/meta"
[[ "$(wc -l < "$work/meta/cells/core" | tr -d ' ')" == 2 ]] || { echo "FAIL: core selects two cells" >&2; fails=$((fails + 1)); }
[[ "$(cat "$work/meta/cells/chromium" "$work/meta/cells/cef" | LC_ALL=C sort | tr '\n' ' ')" == 'expiry_cef_stopped_t expiry_cef_t expiry_chromium_stopped_t expiry_chromium_t ' ]] \
  || { echo "FAIL: the Chromium and CEF groups select the four browser cells" >&2; fails=$((fails + 1)); }
rejects_with "a browser cell no group selects" "do not cover the ignored cells exactly" \
  env FAKE_CELLS="$all_cells core_browser_expiry:expiry_new_t" PATH="$work/bin:$PATH" "$tree_script" check-inventory "$work/meta"
rejects_with "a binary without any cell" "no ignored cell" \
  env FAKE_CELLS="" PATH="$work/bin:$PATH" "$tree_script" check-inventory "$work/meta"
rejects_with "a lost CEF cell pair" "group cef selects no cell" \
  env FAKE_CELLS='core_expiry:expiry_capture_first core_browser_expiry:expiry_chromium_t' PATH="$work/bin:$PATH" "$tree_script" check-inventory "$work/meta"
rejects_with "a lost worker cell pair" "group core selects no cell" \
  env FAKE_CELLS='core_browser_expiry:expiry_chromium_t core_browser_expiry:expiry_cef_t' PATH="$work/bin:$PATH" "$tree_script" check-inventory "$work/meta"
mkdir -p "$work/overlap/scripts"
sed "s/test(=expiry_cef_t) |/test(=expiry_cef_t) | test(=expiry_chromium_t) |/" scripts/release-expiry.sh > "$work/overlap/scripts/release-expiry.sh"
chmod +x "$work/overlap/scripts/release-expiry.sh"
rejects_with "a cell selected by two groups" "the groups overlap" \
  env FAKE_CELLS="$all_cells" PATH="$work/bin:$PATH" "$work/overlap/scripts/release-expiry.sh" check-inventory "$work/meta"
rejects_with "no prepared metadata" "no prepared metadata" \
  env FAKE_CELLS="$all_cells" PATH="$work/bin:$PATH" "$tree_script" check-inventory "$work/nowhere"

# run: the arguments, the evidence requirement and the CEF rule.
FAKE_CELLS="$all_cells" PATH="$work/bin:$PATH" "$tree_script" check-inventory "$work/meta" > /dev/null
run() { # <group> <env assignments...>
  local group=$1
  shift
  rm -f "$work/record"
  env -u LD_LIBRARY_PATH -u CEF_PATH GITHUB_ACTIONS= "$@" FAKE_RECORD="$work/record" EVORCH_RELEASE_EXPIRY_GUARD_CARGO="$work/guard" "$tree_script" run "$work/meta" "$group" "$work/evidence"
}
ok "a group runs with its filter and flags" run chromium FAKE_EVIDENCE='expiry_chromium_t expiry_chromium_stopped_t'
for want in 'run' '--cargo-metadata' "$work/meta/cargo.json" '--binaries-metadata' "$work/meta/default-binaries.json" \
  '-E' 'binary(=core_browser_expiry) & (test(=expiry_chromium_t) | test(=expiry_chromium_stopped_t))' \
  '--run-ignored' 'only' '--test-threads' '1' '--success-output' 'immediate' '--no-tests' 'fail' "evidence=$work/evidence/chromium"; do
  grep -qxF -- "$want" "$work/record" || { echo "FAIL: the run lacks the argument: $want" >&2; fails=$((fails + 1)); }
done
ok "the worker group passes with its evidence" run core FAKE_EVIDENCE='expiry_capture_first expiry_signal_first'
ok "the CEF group passes with the CEF environment" run cef GITHUB_ACTIONS=true CEF_PATH=/r/cef-dist LD_LIBRARY_PATH=/r/cef-dist FAKE_EVIDENCE='expiry_cef_t expiry_cef_stopped_t'
rejects_with "a cell without evidence" "without its death evidence" run cef FAKE_EVIDENCE='expiry_cef_t'
rejects_with "no evidence at all" "without its death evidence" run core
rejects "a failing run" run core FAKE_RC=1 FAKE_EVIDENCE='expiry_capture_first expiry_signal_first'
rejects_with "the CEF loader path on the worker cells" "must not see the CEF distribution" \
  run core GITHUB_ACTIONS=true LD_LIBRARY_PATH=/r/cef-dist FAKE_EVIDENCE='expiry_capture_first expiry_signal_first'
rejects_with "CEF_PATH on the Chromium cells" "must not see the CEF distribution" \
  run chromium GITHUB_ACTIONS=true CEF_PATH=/r/cef-dist FAKE_EVIDENCE='expiry_chromium_t expiry_chromium_stopped_t'
[[ ! -e "$work/record" ]] || { echo "FAIL: a run with a CEF environment still started" >&2; fails=$((fails + 1)); }
ok "outside CI the CEF environment is not refused (dev shells carry it)" run core LD_LIBRARY_PATH=/r/cef-dist FAKE_EVIDENCE='expiry_capture_first expiry_signal_first'
rejects_with "an unknown group" "unknown group" run nowhere
rejects_with "a run before the inventory check" "run check-inventory first" \
  env FAKE_RECORD="$work/record" EVORCH_RELEASE_EXPIRY_GUARD_CARGO="$work/guard" "$tree_script" run "$work/nowhere" core "$work/evidence"
rejects "an unknown mode" "$tree_script" nonsense

if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "release-expiry: passed"
