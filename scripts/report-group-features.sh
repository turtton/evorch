#!/usr/bin/env bash
# Report how dependency feature unification differs between the whole-workspace
# build and each CI test group's `-p` selection (scripts/ci-test-groups.json).
#
# Cargo unifies features over the selected packages only, so a group build can
# enable fewer features than `--workspace`. This prints, per group, every
# package (workspace members flagged "member") whose enabled feature set is
# smaller than the workspace's. Informational: it never fails on a difference.
# Uses `cargo tree` (no compilation), so it is cheap enough to run in CI.
#
# Usage: scripts/report-group-features.sh [group...]   (default: every group)
set -euo pipefail
cd "$(dirname "$0")/.."
groups=("$@")
if [[ ${#groups[@]} == 0 ]]; then
  mapfile -t groups < <(jq -r '.groups | keys[]' scripts/ci-test-groups.json)
fi
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# One line per package: `name<TAB>version<TAB>member|dep<TAB>feature,feature`.
tree() {
  cargo tree "$@" --locked -e normal,build,dev --prefix none -f '{p}|{f}' 2>/dev/null \
    | awk -F'|' '{
        split($1, w, " "); name = w[1]; ver = w[2]
        kind = ($1 ~ /\(\//) ? "member" : "dep"
        sub(/ \(\*\)$/, "", $2)
        print name "\t" ver "\t" kind "\t" $2
      }' | LC_ALL=C sort -u
}
tree --workspace > "$work/ws.tsv"
for g in "${groups[@]}"; do
  args=()
  while read -r p; do args+=(-p "$p"); done < <(jq -r --arg g "$g" '.groups[$g].packages[]' scripts/ci-test-groups.json)
  while read -r f; do args+=(-F "$f"); done < <(jq -r --arg g "$g" '.groups[$g].features[]' scripts/ci-test-groups.json)
  tree "${args[@]}" > "$work/$g.tsv"
  echo "== $g: packages with fewer enabled features than --workspace"
  # Join on name+version; a package can appear once per feature set, so union per key.
  awk -F'\t' '
    FNR == NR { k = $1 " " $2; kind[k] = $3; n = split($4, f, ","); for (i = 1; i <= n; i++) if (f[i] != "") ws[k SUBSEP f[i]] = 1; seen[k] = 1; next }
    { k = $1 " " $2; n = split($4, f, ","); for (i = 1; i <= n; i++) if (f[i] != "") gr[k SUBSEP f[i]] = 1; ing[k] = 1 }
    END {
      for (key in ws) { split(key, a, SUBSEP); k = a[1]; if ((k in ing) && !(key in gr)) miss[k] = miss[k] " " a[2] }
      for (k in miss) print kind[k] "\t" k "\t-" miss[k]
      for (k in seen) if (!(k in ing)) absent++
      print "(" absent + 0 " workspace packages not built by this group)" > "/dev/stderr"
    }' "$work/ws.tsv" "$work/$g.tsv" | LC_ALL=C sort
done
