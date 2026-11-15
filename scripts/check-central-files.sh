#!/usr/bin/env bash
# Keep feature PRs out of the central files of the single-core migration.
#
# Usage:
#   scripts/check-central-files.sh --scope <scope> [--base <ref>] [--files-from <file>]
#   scripts/check-central-files.sh --check-data
#
# --scope S      the PR's scope in scripts/feature-scopes.txt. Every file the PR
#                changes (git diff --name-only <base>...HEAD, base defaults to
#                origin/main; or the names in --files-from) must be outside
#                scripts/central-files.txt and inside the scope.
#                `--scope integration` is an integration PR: it may change
#                anything, and the check only reports what it touched.
# --check-data   validate the two data files against the tracked files: every
#                central pattern matches a tracked file, every scope names
#                something, and no scope pattern is central.
set -euo pipefail
cd "$(dirname "$0")/.."

central_file=scripts/central-files.txt
scopes_file=scripts/feature-scopes.txt
scope=""
base=origin/main
files_from=""
check_data=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --scope) scope="${2:?--scope needs a name}"; shift 2 ;;
    --base) base="${2:?--base needs a ref}"; shift 2 ;;
    --files-from) files_from="${2:?--files-from needs a file}"; shift 2 ;;
    --check-data) check_data=1; shift ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
done
[[ "$check_data" == 1 || -n "$scope" ]] || { echo "expected --scope or --check-data" >&2; exit 2; }

# Lines of a data file without comments and blanks.
data_lines() { sed -e 's/#.*//' -e 's/[[:space:]]*$//' "$1" | grep -v '^$' || true; }

is_central() {
  local file=$1 verdict=0 line
  while IFS= read -r line; do
    if [[ "$line" == '!'* ]]; then
      # shellcheck disable=SC2254 # the pattern is a glob on purpose
      case "$file" in ${line#!}) verdict=0 ;; esac
    else
      # shellcheck disable=SC2254
      case "$file" in $line) verdict=1 ;; esac
    fi
  done < <(data_lines "$central_file")
  [[ "$verdict" == 1 ]]
}

# The patterns a scope allows, one per line (`area:` items expanded).
scope_patterns() {
  local name=$1 line item area found=0
  while IFS= read -r line; do
    [[ "${line%% *}" == "$name" ]] || continue
    found=1
    read -ra items <<< "${line#* }"
    for item in "${items[@]}"; do
      if [[ "$item" == area:* ]]; then
        area=${item#area:}
        echo "crates/workbench-protocol/src/namespaces/$area.rs"
        echo "crates/workbench-protocol/src/namespaces/$area/*"
        echo "crates/workbench-protocol/tests/common/areas/$area.rs"
        echo "crates/workbench-protocol/tests/common/areas/$area/*"
        echo "crates/workbench-protocol/tests/golden/areas/$area.json"
        echo "crates/workbench-host/src/ingress/namespaces/$area.rs"
        echo "crates/workbench-host/src/ingress/namespaces/$area/*"
        echo "crates/workbench-host/src/areas/$area.rs"
        echo "crates/workbench-host/src/areas/$area/*"
        echo "crates/gui/src/core_client/features/$area.rs"
        echo "crates/gui/src/core_client/features/$area/*"
        echo "crates/*/tests/$area*"
      else
        echo "$item"
      fi
    done
  done < <(data_lines "$scopes_file")
  [[ "$found" == 1 ]] || return 1
}

matches_any() { # file, patterns on stdin
  local file=$1 pattern
  while IFS= read -r pattern; do
    # shellcheck disable=SC2254
    case "$file" in $pattern) return 0 ;; esac
  done
  return 1
}

if [[ "$check_data" == 1 ]]; then
  failures=0
  mapfile -t tracked < <(git ls-files --cached --others --exclude-standard)
  while IFS= read -r line; do
    pattern=${line#!}
    hit=0
    for file in "${tracked[@]}"; do
      # shellcheck disable=SC2254
      case "$file" in $pattern) hit=1; break ;; esac
    done
    [[ "$hit" == 1 ]] || { echo "FAIL: central pattern matches no tracked file: $pattern" >&2; failures=$((failures + 1)); }
  done < <(data_lines "$central_file")
  while IFS= read -r name; do
    # An `area:` item that expands to nothing tracked would be a typo.
    while IFS= read -r item; do
      [[ "$item" == area:* ]] || continue
      area=${item#area:}
      [[ -f "crates/workbench-protocol/src/namespaces/$area.rs" ]] \
        || { echo "FAIL: scope $name names the unknown namespace $area" >&2; failures=$((failures + 1)); }
    done < <(data_lines "$scopes_file" | awk -v n="$name" '$1 == n { for (i = 2; i <= NF; i++) print $i }')
    while IFS= read -r pattern; do
      # A scope pattern that is itself a central file would make the check unsatisfiable.
      case "$pattern" in *'*'*) continue ;; esac
      if is_central "$pattern"; then
        echo "FAIL: scope $name allows the central file $pattern" >&2
        failures=$((failures + 1))
      fi
    done < <(scope_patterns "$name")
  done < <(data_lines "$scopes_file" | awk '{ print $1 }')
  [[ "$failures" == 0 ]] || exit 1
  echo "central file data is consistent"
  exit 0
fi

if [[ -n "$files_from" ]]; then
  [[ -r "$files_from" ]] || { echo "cannot read $files_from" >&2; exit 2; }
  mapfile -t changed < "$files_from"
else
  # A failed diff (an unknown base, a shallow clone without it) is an error, not
  # an empty change set: a process substitution would hide its exit status.
  git rev-parse --verify --quiet "$base^{commit}" > /dev/null \
    || { echo "base $base is not a commit here (fetch it, or pass --base)" >&2; exit 2; }
  diff_names=$(git diff --name-only "$base"...HEAD) \
    || { echo "git diff $base...HEAD failed" >&2; exit 2; }
  changed=()
  while IFS= read -r file; do
    [[ -z "$file" ]] || changed+=("$file")
  done <<< "$diff_names"
fi

if [[ "$scope" == integration ]]; then
  for file in "${changed[@]}"; do
    is_central "$file" && echo "central: $file"
  done
  echo "integration PR: central files may change"
  exit 0
fi

patterns=$(scope_patterns "$scope") || { echo "unknown scope: $scope (see $scopes_file)" >&2; exit 2; }
failures=0
for file in "${changed[@]}"; do
  [[ -n "$file" ]] || continue
  if is_central "$file"; then
    echo "FAIL: $file is a central file; only an integration PR may change it (scope $scope)" >&2
    failures=$((failures + 1))
  elif ! matches_any "$file" <<< "$patterns"; then
    echo "FAIL: $file is outside scope $scope" >&2
    failures=$((failures + 1))
  fi
done
[[ "$failures" == 0 ]] || exit 1
echo "scope $scope: ${#changed[@]} changed file(s), none central, all in scope"
