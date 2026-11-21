#!/usr/bin/env bash
# Management publication: one switch, one call site, one product hook per entrypoint.
#
#   1. Product code calls `enable_management` in exactly one place:
#      crates/evorch-core/src/admin_composition/capability.rs.
#   2. `MANAGEMENT_PUBLISHED` is declared once, in that file, as exactly
#      `const MANAGEMENT_PUBLISHED: bool = <expected_published>;`. Only the change
#      that publishes management edits `expected_published` together with the
#      constant; every other change that flips the constant fails here.
#   3. The `admin_composition` module is named only by its own tree, by lib.rs's
#      `pub mod admin_composition;` and by the two entrypoints (main.rs,
#      planned_entry.rs). An entrypoint calls `install` at most once, however it
#      is imported; aliasing the import is refused.
#   4. No environment variable names management publication.
#
# The check reads code, not text: comments are removed first, and the body of a
# `#[cfg(test)] mod name { ... }` is skipped by brace depth, so production code
# after it is still checked. Test code outside such a module (tests/ directories,
# tests.rs, *_tests.rs, shared-test-support/) is out of scope. A file that ends
# inside a comment, a string or a test module fails the check.
set -euo pipefail
root=${1:-$(cd "$(dirname "$0")/.." && pwd)}
cd "$root"
awk_bin=$(command -v gawk || command -v awk)

expected_published=false

capability=crates/evorch-core/src/admin_composition/capability.rs

# Print `file:line:code` for production code, with comments removed.
production_code() {
  find crates -name '*.rs' -not -path '*/tests/*' -not -path '*/shared-test-support/*' \
    -not -name 'tests.rs' -not -name '*_tests.rs' -print0 2>/dev/null |
    xargs -0 -r "$awk_bin" '
      function finish() {
        if (name != "" && (inblock > 0 || instr || intest > 0))
          print name ":0:@@UNPARSEABLE"
        inblock = 0; instr = 0; intest = 0; pend = 0
      }
      # Sets code (comments removed, strings kept) and shape (strings blanked).
      function scan(line,    i, n, c, d, j, k, prevc, before, h) {
        code = ""; shape = ""; n = length(line); i = 1
        while (i <= n) {
          c = substr(line, i, 1); d = substr(line, i, 2)
          if (inblock > 0) {
            if (d == "*/") { inblock--; i += 2 } else if (d == "/*") { inblock++; i += 2 } else i++
            continue
          }
          if (instr == 2) {
            if (c == "\"" && substr(line, i + 1, rawh) == hashes) {
              instr = 0; code = code substr(line, i, 1 + rawh); shape = shape substr(line, i, 1 + rawh); i += 1 + rawh
            } else { code = code c; shape = shape "x"; i++ }
            continue
          }
          if (instr) {
            if (c == "\\") { code = code substr(line, i, 2); shape = shape "xx"; i += 2; continue }
            if (c == "\"") { instr = 0; code = code c; shape = shape c } else { code = code c; shape = shape "x" }
            i++; continue
          }
          if (d == "//") break
          if (d == "/*") { inblock = 1; i += 2; continue }
          if (c == "r") {
            prevc = substr(code, length(code), 1); before = substr(code, length(code) - 1, 1)
            if (prevc !~ /[A-Za-z0-9_]/ || (prevc == "b" && before !~ /[A-Za-z0-9_]/)) {
              k = i + 1
              while (substr(line, k, 1) == "#") k++
              if (substr(line, k, 1) == "\"") {
                rawh = k - i - 1; hashes = substr(line, i + 1, rawh); instr = 2
                code = code substr(line, i, k - i + 1); shape = shape substr(line, i, k - i + 1); i = k + 1
                continue
              }
            }
          }
          if (c == "\"") { instr = 1; code = code c; shape = shape c; i++; continue }
          if (c == sq) {
            if (substr(line, i + 1, 1) == "\\") {
              j = index(substr(line, i + 3), sq)
              if (j > 0) { code = code "x"; shape = shape "x"; i += 3 + j; continue }
            } else if (substr(line, i + 2, 1) == sq) {
              code = code "x"; shape = shape "x"; i += 3; continue
            }
          }
          code = code c; shape = shape c; i++
        }
      }
      function count(s, ch,    t) { t = s; return gsub(ch, ch, t) }
      BEGIN { sq = "\047" }
      FNR == 1 { finish(); name = FILENAME }
      {
        scan($0)
        t = shape; sub(/^[[:space:]]+/, "", t); sub(/[[:space:]]+$/, "", t)
        out = code; sub(/^[[:space:]]+/, "", out); sub(/[[:space:]]+$/, "", out)
        if (intest > 0) {
          intest += count(shape, "{") - count(shape, "}")
          if (intest < 0) intest = 0
          next
        }
        if (t == "") next
        if (t ~ /^#\[cfg\(test\)\]$/) { pend = 1; next }
        if (pend && t ~ /^#\[/) next
        if (pend && t ~ /^(pub(\([a-z]+\))? )?mod [A-Za-z_0-9]+ *\{/) {
          pend = 0
          intest = count(shape, "{") - count(shape, "}")
          if (intest < 0) intest = 0
          next
        }
        pend = 0
        print name ":" FNR ":" out
      }
      END { finish() }'
}
sources=$(production_code)

violations=$(
  printf '%s\n' "$sources" | "$awk_bin" -v capability="$capability" -v expected="$expected_published" '
    {
      file = $0; sub(/:.*/, "", file); text = $0; sub(/^[^:]*:[0-9]+:/, "", text)
      norm = text; gsub(/[[:space:]]+/, " ", norm)
      if (text == "@@UNPARSEABLE") {
        print "VIOLATION: " file " ends inside a comment, string or test module; the check cannot read it"
        next
      }
      if (text ~ /enable_management/ && text !~ /fn enable_management/) {
        if (file == capability) enable_calls++
        else print "VIOLATION: enable_management called outside " capability ": " file ": " text
      }
      if (text ~ /MANAGEMENT_PUBLISHED([^A-Za-z0-9_]|$)/) {
        if (file != capability) print "VIOLATION: MANAGEMENT_PUBLISHED used outside " capability ": " file ": " text
        else if (text ~ /(const|static) +MANAGEMENT_PUBLISHED([^A-Za-z0-9_]|$)/) {
          declarations++
          want = "const MANAGEMENT_PUBLISHED: bool = " expected ";"
          if (norm != want && norm != "pub " want && norm !~ "^pub\\([a-z]+\\) " want "$")
            print "VIOLATION: the declaration must be exactly `" want "` (only the publishing change flips it, together with expected_published in this script): " text
        }
      }
      if (text ~ /(^|[^A-Za-z0-9_])admin_composition([^A-Za-z0-9_]|$)/) {
        inside = (file == "crates/evorch-core/src/admin_composition.rs" || index(file, "crates/evorch-core/src/admin_composition/") == 1)
        entry = (file == "crates/evorch-core/src/main.rs" || file == "crates/evorch-core/src/planned_entry.rs")
        if (file == "crates/evorch-core/src/lib.rs") {
          if (norm != "pub mod admin_composition;") print "VIOLATION: lib.rs may only declare `pub mod admin_composition;`: " text
        } else if (!inside && !entry) {
          print "VIOLATION: admin_composition named outside its tree and the two entrypoints: " file ": " text
        }
        if (entry && text ~ / as /) print "VIOLATION: an entrypoint must not alias admin_composition: " file ": " text
      }
      if (file == "crates/evorch-core/src/main.rs" || file == "crates/evorch-core/src/planned_entry.rs") {
        if (text ~ /(^|[^A-Za-z0-9_])install +as /) print "VIOLATION: an entrypoint must not alias install: " file ": " text
        if (text ~ /(^|[^A-Za-z0-9_])self +as /) print "VIOLATION: an entrypoint must not alias self: " file ": " text
        rest = text
        while (match(rest, /admin_composition::install([^A-Za-z0-9_]|$)/)) { installs[file]++; rest = substr(rest, RSTART + RLENGTH) }
        rest = text
        while (match(rest, /(^|[^A-Za-z0-9_:.])install[ ]*\(/)) { installs[file]++; rest = substr(rest, RSTART + RLENGTH) }
      }
      if (text ~ /env::var(_os)?\(/ && toupper(text) ~ /MANAGEMENT|SAFE_RESTART|ADMIN_PUBLISH/)
        print "VIOLATION: management publication must not read an environment variable: " file ": " text
    }
    END {
      if (enable_calls != 1) print "VIOLATION: expected one enable_management call in " capability ", found " enable_calls + 0
      if (declarations != 1) print "VIOLATION: expected one MANAGEMENT_PUBLISHED declaration, found " declarations + 0
      for (f in installs) if (installs[f] > 1) print "VIOLATION: " f " calls install " installs[f] " times"
    }' || true
)
if [[ -n $violations ]]; then printf '%s\n' "$violations"; exit 1; fi
echo 'Admin publication boundaries passed'
