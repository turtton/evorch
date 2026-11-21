#!/usr/bin/env bash
# Management publication: one switch, one call site, one product hook per entrypoint.
#
#   1. Product code calls `enable_management` in exactly one place:
#      crates/evorch-core/src/admin_composition/capability.rs.
#   2. `MANAGEMENT_PUBLISHED` is declared once, in that file, and holds the value
#      expected below. Only the change that publishes management edits
#      `expected_published` together with the constant; every other change that
#      flips the constant fails here.
#   3. `admin_composition::install` is referenced by product code only in the two
#      entrypoints (main.rs, planned_entry.rs), at most once each.
#   4. No environment variable or configuration key names management publication.
#
# Test code is out of scope: tests/ directories, tests.rs, *_tests.rs,
# shared-test-support/ and a trailing `#[cfg(test)] mod ... {` block.
set -euo pipefail
root=${1:-$(cd "$(dirname "$0")/.." && pwd)}
cd "$root"

expected_published=false

capability=crates/evorch-core/src/admin_composition/capability.rs
cat_sources() {
  find crates -name '*.rs' -not -path '*/tests/*' -not -path '*/shared-test-support/*' \
    -not -name 'tests.rs' -not -name '*_tests.rs' -print0 2>/dev/null |
    xargs -0 -r awk '
      FNR == 1 { intest = 0; prev = "" }
      {
        if (!intest && prev ~ /^[[:space:]]*#\[cfg\(test\)\][[:space:]]*$/ && $0 ~ /^[[:space:]]*(pub(\([a-z]+\))? )?mod [A-Za-z_0-9]+ *\{/) intest = 1
        prev = $0
        if (intest) next
        text = $0; sub(/^[[:space:]]+/, "", text)
        if (text ~ /^\/\//) next
        print FILENAME ":" FNR ":" text
      }'
}
sources=$(cat_sources)

violations=$(
  printf '%s\n' "$sources" | awk -v capability="$capability" -v expected="$expected_published" '
    {
      file = $0; sub(/:.*/, "", file); text = $0; sub(/^[^:]*:[0-9]+:/, "", text)
      if (text ~ /enable_management/ && text !~ /fn enable_management/) {
        if (file == capability) enable_calls++
        else print "VIOLATION: enable_management called outside " capability ": " file ": " text
      }
      if (text ~ /MANAGEMENT_PUBLISHED/) {
        if (file != capability) print "VIOLATION: MANAGEMENT_PUBLISHED used outside " capability ": " file ": " text
        else if (text ~ /const MANAGEMENT_PUBLISHED/) {
          declarations++
          want = "const MANAGEMENT_PUBLISHED: bool = " expected ";"
          if (index(text, want) == 0) print "VIOLATION: MANAGEMENT_PUBLISHED must be " expected " (only the publishing change flips it, together with expected_published in this script): " text
        }
      }
      if (text ~ /admin_composition::install/) {
        if (file == "crates/evorch-core/src/main.rs" || file == "crates/evorch-core/src/planned_entry.rs") installs[file]++
        else print "VIOLATION: admin_composition::install referenced outside the two entrypoints: " file ": " text
      }
      if (text ~ /env::var(_os)?\(/ && toupper(text) ~ /MANAGEMENT|SAFE_RESTART|ADMIN_PUBLISH/)
        print "VIOLATION: management publication must not read an environment variable: " file ": " text
    }
    END {
      if (enable_calls != 1) print "VIOLATION: expected one enable_management call in " capability ", found " enable_calls + 0
      if (declarations != 1) print "VIOLATION: expected one MANAGEMENT_PUBLISHED declaration, found " declarations + 0
      for (f in installs) if (installs[f] > 1) print "VIOLATION: " f " references admin_composition::install " installs[f] " times"
    }' || true
)
if [[ -n $violations ]]; then printf '%s\n' "$violations"; exit 1; fi
echo 'Admin publication boundaries passed'
