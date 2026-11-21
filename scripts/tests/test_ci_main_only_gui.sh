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
  printf '#[test]\nfn geometry() {}\n' > "$root/crates/gui/tests/geometry.rs"
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

# --- the sources -------------------------------------------------------------
# check-depinfo reads the dependency-info file rustc writes for each registered
# binary. The fixtures below are compiled by the real rustc (dependency-info only,
# so an unresolved `gui` crate is an error rustc reports but still lists the files
# it read), so module resolution is the compiler's, not this script's.
command -v rustc > /dev/null || { echo "FAIL: rustc is required to produce dependency-info" >&2; exit 1; }
depinfo() { # compile the dependency-info of every fixture binary under $t, write $t/binaries.json
  local deps="$t/target/debug/deps" name src
  rm -rf "$t/target"
  mkdir -p "$deps"
  for pair in attention:tests/attention/main.rs matrix:tests/matrix.rs newsys:tests/newsys/main.rs; do
    name=${pair%%:*}
    src=crates/gui/${pair#*:}
    (cd "$t" && rustc --edition 2021 --test --crate-name "$name" -C extra-filename=-0123abcd \
      --emit=dep-info --out-dir target/debug/deps "$src") > /dev/null 2>&1 || true
    : > "$deps/$name-0123abcd"
  done
  jq -n --arg d "$deps" '{"rust-build-meta": {"target-directory": ($d | sub("/debug/deps$"; ""))},
    "rust-binaries": ([["attention"], ["matrix"], ["newsys"]] | map({key: ("gui::" + .[0]),
      value: {"binary-path": ($d + "/" + .[0] + "-0123abcd")}}) | from_entries)}' > "$t/binaries.json"
}
fresh() { rm -rf "$t"; make_tree "$t"; }
gui_dir="$t/crates/gui/tests"

fresh; depinfo
expect_ok "sources without core_client" "$t" "$reg" --root "$t" check-depinfo "$t/binaries.json"
grep -q "2 binaries" "$work/out" || { echo "FAIL: expected 2 registered binaries read" >&2; cat "$work/out" >&2; fails=$((fails + 1)); }

source_case() { # source_case <name> <regexp> : the tree is as the caller left it
  depinfo
  expect_fail "$1" "$2" "$t" "$reg" --root "$t" check-depinfo "$t/binaries.json"
}
shared() { # a registered binary that reaches shared sources outside its directory
  printf '#[path = "../shared/mod.rs"]\nmod shared;\n' >> "$gui_dir/attention/main.rs"
  mkdir -p "$gui_dir/shared/nested" "$gui_dir/shared/child"
}

fresh; printf 'use gui::core_client::Page;\n' >> "$gui_dir/attention/main.rs"
source_case "core_client in the root file" "attention/main.rs names core_client"
fresh; printf 'fn f() { crate::core_client::x(); }\n' >> "$gui_dir/attention/panel.rs"
source_case "core_client in a child module" "panel.rs names core_client"
fresh; printf 'fn f() { core_client::x(); }\n' >> "$gui_dir/geometry.rs"
source_case "core_client in a module of a file-root binary" "geometry.rs names core_client"
fresh; printf 'pub use gui::core_client;\n' >> "$gui_dir/support/mod.rs"
source_case "core_client in a shared file reached by #[path]" "support/mod.rs names core_client"
fresh; printf 'include!("../shared.rs");\n' >> "$gui_dir/attention/panel.rs"; printf 'use gui::core_client::P;\n' > "$gui_dir/shared.rs"
source_case "core_client in a file reached by include!" "shared.rs names core_client"
fresh; printf 'use r#core_client::P;\n' >> "$gui_dir/attention/panel.rs"
source_case "a raw identifier" "names core_client"

# The cases module resolution gets wrong when it is re-implemented: a shared tree
# reached by #[path] that continues with ordinary `mod` declarations.
fresh; shared
printf 'mod child;\n' > "$gui_dir/shared/mod.rs"; printf 'mod grand;\n' > "$gui_dir/shared/child.rs"
printf 'use gui::core_client::P;\n' > "$gui_dir/shared/child/grand.rs"
source_case "core_client two levels into a shared tree (child.rs and child/)" "shared/child/grand.rs names core_client"
fresh; shared
printf 'mod child;\npub mod nested { mod child; }\n' > "$gui_dir/shared/mod.rs"
printf 'pub fn a() {}\n' > "$gui_dir/shared/child.rs"; printf 'use gui::core_client;\n' > "$gui_dir/shared/nested/child.rs"
source_case "a child module with the same name as one outside the inline module" "shared/nested/child.rs names core_client"
fresh; shared
printf 'pub mod a { mod child; }\npub mod b { mod child; }\n' > "$gui_dir/shared/mod.rs"
mkdir -p "$gui_dir/shared/a" "$gui_dir/shared/b"
printf 'pub fn a() {}\n' > "$gui_dir/shared/a/child.rs"; printf 'use gui::core_client;\n' > "$gui_dir/shared/b/child.rs"
source_case "sibling inline modules that both declare child" "shared/b/child.rs names core_client"
fresh; shared
printf 'mod child;\npub mod nested { mod child; }\n' > "$gui_dir/shared/mod.rs"
printf 'pub fn a() {}\n' > "$gui_dir/shared/child.rs"; printf 'pub fn b() {}\n' > "$gui_dir/shared/nested/child.rs"
depinfo
expect_ok "the same shapes without core_client" "$t" "$reg" --root "$t" check-depinfo "$t/binaries.json"
grep -q "shared/nested/child.rs" "$t/target/debug/deps/attention-0123abcd.d" \
  || { echo "FAIL: the compiler's list does not include the nested child module" >&2; fails=$((fails + 1)); }

fresh; printf '// core_client is only named here\nfn f() {}\n' >> "$gui_dir/attention/panel.rs"
source_case "core_client only in a line comment (nothing is parsed, so it is rejected)" "panel.rs names core_client"
fresh; printf '/* nor core_client in\n   a block */\nfn f() {}\n' >> "$gui_dir/attention/panel.rs"
source_case "core_client only in a block comment" "panel.rs names core_client"
fresh; printf 'const GLOB: &str = r#"{"glob": "/*"}"#;\nmod core_client { pub fn check() {} }\nfn g() { core_client::check(); }\n' >> "$gui_dir/attention/panel.rs"
# the fixture is real Rust: it compiles, and a comment stripper would drop its last line
rustc --edition 2021 --crate-type lib --emit=metadata --out-dir "$work/rawstr" "$gui_dir/attention/panel.rs" > "$work/rawstr.log" 2>&1 \
  || { echo "FAIL: the raw-string fixture is not compilable Rust" >&2; cat "$work/rawstr.log" >&2; fails=$((fails + 1)); }
source_case "core_client after a raw string that contains /* (compilable)" "panel.rs names core_client"
fresh; printf 'fn core_client_like() {}\n' >> "$gui_dir/attention/panel.rs"
source_case "a longer identifier containing the word is rejected too" "panel.rs names core_client"
fresh; printf 'const CORE_CLIENT: u8 = 0;\nfn other_client() {}\n' >> "$gui_dir/attention/panel.rs"; depinfo
expect_ok "other spellings are not the text core_client" "$t" "$reg" --root "$t" check-depinfo "$t/binaries.json"
fresh; depinfo
expect_ok "an unregistered binary may use core_client" "$t" "$reg" --root "$t" check-depinfo "$t/binaries.json"
variant with-newsys '.binaries += [{"target": "newsys", "reason": "x"}]'
expect_fail "registering that binary is caught" "newsys/main.rs names core_client" "$t" "$work/with-newsys.json" --root "$t" check-depinfo "$t/binaries.json"

# Fail closed: nothing that cannot be read is a pass.
fresh; depinfo; rm "$t/target/debug/deps/attention-0123abcd.d"
expect_fail "a missing dependency-info file" "no dependency-info file" "$t" "$reg" --root "$t" check-depinfo "$t/binaries.json"
fresh; depinfo; printf 'not a dependency file\n' > "$t/target/debug/deps/matrix-0123abcd.d"
expect_fail "a dependency-info file that lists nothing" "lists no file" "$t" "$reg" --root "$t" check-depinfo "$t/binaries.json"
fresh; depinfo; jq 'del(.["rust-binaries"]["gui::matrix"])' "$t/binaries.json" > "$work/b.json"
expect_fail "a registered binary the build metadata does not know" "has no binary gui::matrix" "$t" "$reg" --root "$t" check-depinfo "$work/b.json"
fresh; depinfo; rm "$gui_dir/attention/panel.rs"
expect_fail "a listed source that no longer exists" "panel.rs, which does not exist" "$t" "$reg" --root "$t" check-depinfo "$t/binaries.json"
expect_fail "no metadata file" "does not exist" "$t" "$reg" --root "$t" check-depinfo "$work/absent.json"

if ((fails > 0)); then
  echo "$fails failure(s)" >&2
  exit 1
fi
echo "main-only GUI registry tests passed"
