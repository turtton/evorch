#!/usr/bin/env bash
# Tests for scripts/ci-main-only-gui.sh against a fixture gui package: a metadata
# document and a source tree that are written here. No cargo, no build.
#
# The registry names integration-test binaries only; the checks below also run
# against mutated copies, so a registry or a source that should be rejected is.
set -euo pipefail
cd "$(dirname "$0")/../.."
tool="$PWD/scripts/ci-main-only-gui.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# make_tree <dir>: a gui package with the registered binaries `attention`
# (tests/attention/main.rs) and `matrix` (tests/matrix.rs), a shared support
# module, an unregistered binary `newsys`, a lib, a bin and a test with a custom
# path. Writes <dir>/metadata.json and <dir>/registry.json.
make_tree() {
  local root=$1
  mkdir -p "$root/crates/gui/tests/attention" "$root/crates/gui/tests/matrix" "$root/crates/gui/tests/support" "$root/crates/gui/tests/newsys" "$root/crates/gui/src" "$root/crates/gui/elsewhere"
  printf 'mod panel;\n#[test]\nfn it_works() {}\n' > "$root/crates/gui/tests/attention/main.rs"
  printf '#[test]\nfn panel() {}\n' > "$root/crates/gui/tests/attention/panel.rs"
  printf '#[path = "support/mod.rs"]\nmod support;\nmod geometry;\n' > "$root/crates/gui/tests/matrix.rs"
  printf '#[test]\nfn geometry() {}\n' > "$root/crates/gui/tests/matrix/geometry.rs"
  printf 'pub fn helper() {}\n' > "$root/crates/gui/tests/support/mod.rs"
  printf 'mod core_client_page;\n' > "$root/crates/gui/tests/newsys/main.rs"
  printf 'use gui::core_client::Page;\n' > "$root/crates/gui/tests/newsys/core_client_page.rs"
  printf 'pub mod core_client;\n' > "$root/crates/gui/src/lib.rs"
  printf '#[test]\nfn custom() {}\n' > "$root/crates/gui/elsewhere/custom.rs"
  jq -n --arg r "$root/crates/gui" '{packages: [{name: "gui", manifest_path: ($r + "/Cargo.toml"), targets: [
      {name: "attention", kind: ["test"], src_path: ($r + "/tests/attention/main.rs")},
      {name: "matrix", kind: ["test"], src_path: ($r + "/tests/matrix.rs")},
      {name: "newsys", kind: ["test"], src_path: ($r + "/tests/newsys/main.rs")},
      {name: "custom", kind: ["test"], src_path: ($r + "/elsewhere/custom.rs")},
      {name: "gui", kind: ["lib"], src_path: ($r + "/src/lib.rs")},
      {name: "evorch-gui", kind: ["bin"], src_path: ($r + "/src/main.rs")}]}]}' > "$root/metadata.json"
  printf '%s\n' '{"package": "gui", "binaries": [
    {"target": "attention", "reason": "fixture"},
    {"target": "matrix", "reason": "fixture"}]}' > "$root/registry.json"
}

fails=0
run() { # run <root> <registry> <command>: output in $work/out
  local root=$1 registry=$2
  shift 2
  "$tool" --manifest "$registry" --metadata "$root/metadata.json" "$@" > "$work/out" 2>&1
}
expect_ok() { # expect_ok <name> <root> <registry> <command>
  local name=$1
  shift
  if ! run "$@"; then
    echo "FAIL ($name): expected success" >&2
    cat "$work/out" >&2
    fails=$((fails + 1))
  else
    echo "ok: $name"
  fi
}
expect_fail() { # expect_fail <name> <message regexp> <root> <registry> <command>
  local name=$1 pattern=$2
  shift 2
  if run "$@"; then
    echo "FAIL ($name): expected rejection" >&2
    cat "$work/out" >&2
    fails=$((fails + 1))
  elif ! grep -Eq "$pattern" "$work/out"; then
    echo "FAIL ($name): rejected, but not with /$pattern/:" >&2
    cat "$work/out" >&2
    fails=$((fails + 1))
  else
    echo "ok: $name"
  fi
}

t="$work/tree"
make_tree "$t"
reg="$t/registry.json"

expect_ok "a valid registry" "$t" "$reg" validate
expect_ok "sources without core_client" "$t" "$reg" check-sources
run "$t" "$reg" filter
[[ "$(cat "$work/out")" == 'package(=gui) & kind(test) & (binary(=attention) | binary(=matrix))' ]] \
  || { echo "FAIL: unexpected filter: $(cat "$work/out")" >&2; fails=$((fails + 1)); }
run "$t" "$reg" binaries
[[ "$(tr '\n' ' ' < "$work/out")" == 'gui::attention gui::matrix ' ]] \
  || { echo "FAIL: unexpected binary ids: $(cat "$work/out")" >&2; fails=$((fails + 1)); }
echo "ok: filter and binary ids"

# --- the registry itself -------------------------------------------------------
variant() { # variant <name> <jq program>: a changed copy of the registry
  jq "$2" "$reg" > "$work/$1.json"
}
variant other-package '.package = "runtime"'
expect_fail "a package other than gui" "package must be gui" "$t" "$work/other-package.json" validate
variant empty '.binaries = []'
expect_fail "no binaries" "binaries must be a non-empty array" "$t" "$work/empty.json" validate
variant dup '.binaries += [.binaries[0]]'
expect_fail "a binary registered twice" "registered twice" "$t" "$work/dup.json" validate
variant selector '.binaries[0].tests = ["panel"]'
expect_fail "a test list next to a binary" "exactly the keys target and reason" "$t" "$work/selector.json" validate
variant filter-key '.binaries[0].filter = "test(panel)"'
expect_fail "a filter expression next to a binary" "exactly the keys target and reason" "$t" "$work/filter-key.json" validate
variant top-key '.selector = "x"'
expect_fail "an extra top-level key" "exactly the keys package and binaries" "$t" "$work/top-key.json" validate
variant no-reason 'del(.binaries[0].reason)'
expect_fail "a binary without a reason" "exactly the keys target and reason" "$t" "$work/no-reason.json" validate
variant empty-reason '.binaries[0].reason = ""'
expect_fail "an empty reason" "needs a reason" "$t" "$work/empty-reason.json" validate
variant module '.binaries[0].target = "attention::panel"'
expect_fail "a module path instead of a binary" "lowercase integration-test name" "$t" "$work/module.json" validate
variant unknown '.binaries += [{"target": "ghost", "reason": "x"}]'
expect_fail "a target the gui package does not have" "ghost is not a target of the gui package" "$t" "$work/unknown.json" validate
variant lib '.binaries += [{"target": "gui", "reason": "x"}]'
expect_fail "the gui library" "gui has kind 'lib'" "$t" "$work/lib.json" validate
variant bin '.binaries += [{"target": "evorch_gui", "reason": "x"}]'
expect_fail "a binary target under another name" "evorch_gui is not a target" "$t" "$work/bin.json" validate
variant custom '.binaries += [{"target": "custom", "reason": "x"}]'
expect_fail "a test outside tests/<name>.rs and tests/<name>/main.rs" "custom has source" "$t" "$work/custom.json" validate
printf 'not json\n' > "$work/garbage.json"
expect_fail "a registry that is not JSON" "is not a valid registry" "$t" "$work/garbage.json" validate
expect_fail "a missing registry" "does not exist" "$t" "$work/absent.json" validate

# A target of another package with a registered name is not enough.
jq '.packages[0].name = "other"' "$t/metadata.json" > "$work/metadata-other.json"
if "$tool" --manifest "$reg" --metadata "$work/metadata-other.json" validate > "$work/out" 2>&1; then
  echo "FAIL: metadata without the gui package was accepted" >&2
  fails=$((fails + 1))
else
  echo "ok: metadata without the gui package"
fi

# A kind other than test, whatever the name.
jq '.packages[0].targets |= map(if .name == "matrix" then .kind = ["bench"] else . end)' "$t/metadata.json" > "$work/metadata-bench.json"
if "$tool" --manifest "$reg" --metadata "$work/metadata-bench.json" validate > "$work/out" 2>&1; then
  echo "FAIL: a bench was accepted" >&2
  fails=$((fails + 1))
else
  grep -q "has kind 'bench'" "$work/out" && echo "ok: a bench target" || { echo "FAIL: wrong message for a bench" >&2; cat "$work/out" >&2; fails=$((fails + 1)); }
fi

# --- the sources ----------------------------------------------------------------
fresh() { rm -rf "$t"; make_tree "$t"; }

fresh; printf 'use gui::core_client::Page;\n' >> "$t/crates/gui/tests/attention/main.rs"
expect_fail "core_client in the root file" "attention.*main.rs names core_client" "$t" "$reg" check-sources
fresh; printf 'fn f() { crate::core_client::x(); }\n' >> "$t/crates/gui/tests/attention/panel.rs"
expect_fail "core_client in a child module" "panel.rs names core_client" "$t" "$reg" check-sources
fresh; printf 'fn f() { core_client::x(); }\n' >> "$t/crates/gui/tests/matrix/geometry.rs"
expect_fail "core_client in the module directory of a file-root binary" "geometry.rs names core_client" "$t" "$reg" check-sources
fresh; printf 'pub use gui::core_client;\n' >> "$t/crates/gui/tests/support/mod.rs"
expect_fail "core_client in a shared support file reached by #[path]" "support/mod.rs names core_client" "$t" "$reg" check-sources
fresh; printf 'include!("../shared.rs");\n' >> "$t/crates/gui/tests/matrix/geometry.rs"; printf 'use gui::core_client::P;\n' > "$t/crates/gui/tests/shared.rs"
expect_fail "core_client in a file reached by include!" "shared.rs names core_client" "$t" "$reg" check-sources
fresh; printf 'use r#core_client::P;\n' >> "$t/crates/gui/tests/attention/panel.rs"
expect_fail "a raw identifier" "names core_client" "$t" "$reg" check-sources
fresh; printf '#[path = "../missing.rs"]\nmod missing;\n' >> "$t/crates/gui/tests/attention/main.rs"
expect_fail "a #[path] to a file that does not exist" "missing.rs, which does not exist" "$t" "$reg" check-sources
fresh; printf 'include!("../absent.rs");\n' >> "$t/crates/gui/tests/attention/main.rs"
expect_fail "an include! of a file that does not exist" "absent.rs, which does not exist" "$t" "$reg" check-sources
fresh; printf 'include!(concat!(env!("OUT_DIR"), "/x.rs"));\n' >> "$t/crates/gui/tests/attention/main.rs"
expect_fail "an include! that is not a literal" "include! that is not a string literal" "$t" "$reg" check-sources
fresh; printf '#[path = concat!("a", "b")]\nmod x;\n' >> "$t/crates/gui/tests/attention/main.rs"
expect_fail "a #[path] that is not a literal" "#\\[path\\] that is not a string literal" "$t" "$reg" check-sources

fresh; printf '// core_client is not used here\n/* nor core_client in\n   a block */\nfn f() {}\n' >> "$t/crates/gui/tests/attention/panel.rs"
expect_ok "core_client only in comments" "$t" "$reg" check-sources
fresh; printf 'const SRC: &str = include_str!("../../src/lib.rs");\n' >> "$t/crates/gui/tests/attention/panel.rs"
expect_ok "include_str! of a file that names core_client is not followed" "$t" "$reg" check-sources
fresh; printf '#[test]\nfn added_later() {}\n' >> "$t/crates/gui/tests/attention/panel.rs"; printf '#[test]\nfn renamed() {}\n' >> "$t/crates/gui/tests/matrix/geometry.rs"
expect_ok "a test added to a registered binary needs no registry change" "$t" "$reg" check-sources
fresh; printf 'fn core_client_like() {}\nconst CORE_CLIENT: u8 = 0;\nuse gui::core_client_e2e::X;\n' >> "$t/crates/gui/tests/attention/panel.rs"
expect_ok "identifiers that only contain the word are not a reference" "$t" "$reg" check-sources
fresh
expect_ok "an unregistered binary may use core_client" "$t" "$reg" check-sources
variant with-newsys '.binaries += [{"target": "newsys", "reason": "x"}]'
expect_fail "registering that binary is caught" "core_client" "$t" "$work/with-newsys.json" check-sources

if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "main-only GUI registry tests passed"
