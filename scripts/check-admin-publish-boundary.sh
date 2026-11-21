#!/usr/bin/env bash
# Management publication: one switch, one call site, one product hook per entrypoint.
#
# This check does not parse Rust. It matches raw lines against a short allowlist
# and treats everything outside the allowlist as a violation, so a form it does
# not recognise is refused, never skipped.
#
#   1. The switch file is pinned by content (`capability_sha256`) and must hold
#      exactly one `const MANAGEMENT_PUBLISHED: bool = <expected_published>;`.
#      The one product call of `enable_management` lives in that file. The
#      change that publishes management updates `expected_published` and the
#      hash together; any other edit to the file stops here and is reviewed.
#   2. Outside that file the word `enable_management` appears only in the single
#      definition line of crates/workbench-host/src/admin/service.rs. Tests use
#      `publish_for_test`, which exists only under test builds.
#   3. `MANAGEMENT_PUBLISHED` appears nowhere else.
#   4. `admin_composition` is named only inside its own tree, by lib.rs as
#      `pub mod admin_composition;`, and by main.rs and planned_entry.rs. An
#      entrypoint writes the fully qualified path (`use` and `as` are refused),
#      calls `admin_composition::install(` at most once, and may also build
#      `admin_composition::Inputs`.
#   5. No environment variable names management publication.
#
# Only a whole line that starts with `//` and holds no `"` or `*/` is exempt.
#
# What this does NOT guarantee: that the switch is off at run time (the tests
# that call the real `publish` pin that), that a macro does not build an
# identifier, or that unsafe code does not write the flag. It catches
# accidental and ordinary changes, not deliberate evasion.
set -euo pipefail
root=${1:-$(cd "$(dirname "$0")/.." && pwd)}
cd "$root"

expected_published=false
capability_sha256=acde91be78717b1223689922308844582fd2a4a7f442330d29288a5ccbffcdf3

capability=crates/evorch-core/src/admin_composition/capability.rs
service=crates/workbench-host/src/admin/service.rs
tree=crates/evorch-core/src/admin_composition
entrypoints="crates/evorch-core/src/main.rs crates/evorch-core/src/planned_entry.rs"

violations=()
violate() { violations+=("VIOLATION: $*"); }

# `file<TAB>line` for every .rs line under crates/ that matches $1, minus the
# one exempt comment form.
lines() {
  { grep -rnE --include='*.rs' -e "$1" crates 2>/dev/null || true; } | awk '
    {
      file = $0; sub(/:.*/, "", file)
      text = $0; sub(/^[^:]*:[0-9]+:/, "", text)
      sub(/^[[:space:]]+/, "", text); sub(/[[:space:]]+$/, "", text)
      if (text ~ /^\/\// && text !~ /\*\// && text !~ /"/) next
      print file "\t" text
    }'
}
occurrences() { { printf '%s\n' "$2" | grep -oE -e "$1" || true; } | wc -l; }
word='(^|[^A-Za-z0-9_])'
wordend='([^A-Za-z0-9_]|$)'

# 1. the switch file
if [[ ! -f $capability ]]; then
  violate "$capability is missing"
else
  actual=$(sha256sum "$capability" | cut -d' ' -f1)
  if [[ $actual != "$capability_sha256" ]]; then
    violate "$capability changed (sha256 $actual). Review it; only the publishing change may flip the switch, and it updates expected_published and capability_sha256 in this script together"
  fi
  declared=$(grep -cxF "const MANAGEMENT_PUBLISHED: bool = $expected_published;" "$capability" || true)
  [[ $declared == 1 ]] || violate "$capability must hold exactly one 'const MANAGEMENT_PUBLISHED: bool = $expected_published;' line, found $declared"
fi

# 2. enable_management
definitions=0
while IFS=$'\t' read -r file text; do
  [[ -n $file ]] || continue
  if [[ $file == "$capability" ]]; then
    continue
  elif [[ $file == "$service" && $text == 'pub fn enable_management(&self) {' ]]; then
    definitions=$((definitions + 1))
  else
    violate "enable_management outside $capability: $file: $text (tests call publish_for_test)"
  fi
done < <(lines "${word}enable_management${wordend}")
[[ $definitions == 1 ]] || violate "expected one 'pub fn enable_management(&self) {' line in $service, found $definitions"

# 3. MANAGEMENT_PUBLISHED
while IFS=$'\t' read -r file text; do
  [[ -n $file ]] || continue
  [[ $file == "$capability" ]] || violate "MANAGEMENT_PUBLISHED outside $capability: $file: $text"
done < <(lines 'MANAGEMENT_PUBLISHED')

# 4. admin_composition
declare -A installs=()
while IFS=$'\t' read -r file text; do
  [[ -n $file ]] || continue
  if [[ $file == "$tree.rs" || $file == "$tree/"* ]]; then
    continue
  elif [[ $file == crates/evorch-core/src/lib.rs ]]; then
    [[ $text == 'pub mod admin_composition;' ]] || violate "lib.rs may only declare 'pub mod admin_composition;': $text"
  elif [[ " $entrypoints " == *" $file "* ]]; then
    all=$(occurrences "${word}admin_composition${wordend}" "$text")
    called=$(occurrences "${word}admin_composition::install\\(" "$text")
    built=$(occurrences "${word}admin_composition::Inputs${wordend}" "$text")
    installs[$file]=$(( ${installs[$file]:-0} + called ))
    if [[ $all != $((called + built)) || $text =~ (^|[^A-Za-z0-9_])use[[:space:]] ]]; then
      violate "an entrypoint writes 'admin_composition::install(' or 'admin_composition::Inputs' fully qualified, with no use or alias: $file: $text"
    fi
  else
    violate "admin_composition named outside its tree and the two entrypoints: $file: $text"
  fi
done < <(lines "${word}admin_composition${wordend}")
for file in "${!installs[@]}"; do
  [[ ${installs[$file]} -le 1 ]] || violate "$file calls admin_composition::install ${installs[$file]} times"
done

# 5. environment
while IFS=$'\t' read -r file text; do
  [[ -n $file ]] || continue
  if [[ ${text^^} =~ MANAGEMENT|SAFE_RESTART|ADMIN_PUBLISH ]]; then
    violate "management publication must not read an environment variable: $file: $text"
  fi
done < <(lines 'env::var(_os)?\(')

if ((${#violations[@]})); then printf '%s\n' "${violations[@]}"; exit 1; fi
echo 'Admin publication boundaries passed'
