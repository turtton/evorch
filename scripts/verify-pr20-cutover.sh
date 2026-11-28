#!/bin/sh
# PR 20 (ADR 0045 section 20) cutover check: an OLD build creates the data, a NEW
# build cold-starts on it, in the very same absolute paths.
#
# Manual / pre-merge verification only. NOT a CI job: it needs two real builds.
#
# usage:
#   scripts/verify-pr20-cutover.sh \
#     --old-core PATH --old-evorch PATH --old-sha SHA40 \
#     --new-core PATH --new-evorch PATH --new-sha SHA40 \
#     [--old-helper PATH] [--new-helper PATH] \
#     [--old-guard-profile dev|release] [--new-guard-profile dev|release] \
#     [--evidence DIR] [--base DIR] [--fixtures main,graph] [--approve] [--keep]
#
#   --old-*    the build immediately before PR C (for example the clean detached
#              checkout .worktrees/pr20-old-build: target/debug/{evorch-core,evorch}).
#   --new-*    the candidate build (the sealed release product or a dev build).
#   --*-helper evorch-process-guard of that build (default: the helper that the
#              guard-bundle.json beside the core names, else the file of that name
#              beside the core). The binaries are COPIED into a private work directory
#              and sealed together with scripts/guard-artifact.sh (a private bundle
#              that names BOTH the core and the CLI as callers); the originals are
#              never touched. The helper's profile and build recipe are never made
#              up: they are copied from the bundle beside the core when there is one,
#              else taken from --*-guard-profile (dev|release; the helper accepts
#              only these two canonical recipes), else inferred from a
#              target/release or target/debug helper path (recorded as such), else
#              the script stops. What was recorded is written to the evidence.
#   --base     must consist of [A-Za-z0-9._/-] only (it is also removed by cleanup).
#   --evidence where summary.md / summary.json / logs are written
#              (default: <base>/pr20-cutover-evidence-<new sha12>-<time>).
#   --base     a SHORT directory for the data roots (default /tmp). It must be short:
#              the core's sockets live below it (sun_path < 108 bytes).
#   --fixtures main,graph (default both). "main" = ordinary data (Running owner rows,
#              completed restart, owner progress, threads/lanes, retention binding,
#              attachment, ownership-only snapshots, owner-generation destinations,
#              two un-ACKed notifications, Stop destination, CLI run, scratch marker).
#              "graph" = an unfinished old restart graph.
#   --approve  the OLD core approves the project (workspace.set_project_trust) while it creates the
#              data. Default: the project keeps the trust the old core gave it (unapproved), and
#              the new core must keep serving the existing threads and a new thread of it.
#   --keep     keep the work roots (the evidence directory is always kept).
#
# How the check is done (nothing is edited by hand):
#   1. OLD core is started with --config-dir CONFIG in a fresh work root; a scripted
#      OpenAI-compatible mock provider and the old protocol/CLI create the data.
#   2. The OLD core is killed (SIGKILL at a defined point) and its exit and the
#      absence of surviving child processes are checked.
#   3. The NEW core is cold-started on the same UID and the same absolute
#      config/DB/sidebar/state-root/project paths, then observed.
#   Waits are for readiness lines, DB facts, ACKs and process exits. The only
#   deadlines are failure watchdogs (PR20_CUTOVER_WATCHDOG_MS, default 300000, and
#   PR20_CUTOVER_STEP_WATCHDOG_MS, default 120000); they never make a check succeed.
#
# Both phases must run inside ONE invocation of this script: the scratch and the
# attachment directories live under /var/tmp, which an isolated sandbox may drop
# between shell commands.
#
# Result of every check: pass | fail | unverified | todo. Only pass is success.
#   unverified  the OLD build did not produce that data (generated missing) or the
#               check cannot be driven from outside; never reported as success.
#   todo        depends on a new-build surface that is not implemented yet.
# Exit code: 0 all pass, 1 a check failed or a phase did not complete,
#            2 no failure but unverified/todo remain (NOT success), 64 usage error.
#
# Recording a run for the PR: the evidence directory holds summary.md (old/new
# SHA, binary sha256, environment, every check with its result), summary.json and
# the core / provider / CLI logs of each fixture. Paste summary.md into the PR and
# rerun the script on the FINAL candidate SHA whenever the candidate changes.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/.." && pwd)
driver="$here/verify-pr20-cutover/driver.mjs"
report="$here/verify-pr20-cutover/report.mjs"

usage() { sed -n '2,/^set -eu/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'; }
die() { echo "verify-pr20-cutover: $*" >&2; exit 64; }

old_core= old_evorch= old_sha= old_helper= old_profile= new_core= new_evorch= new_sha= new_helper= new_profile=
evidence= base=/tmp fixtures=main,graph keep=0 approve=no
while [ $# -gt 0 ]; do
  case "$1" in
    --old-core) old_core=$2; shift 2 ;;
    --old-evorch) old_evorch=$2; shift 2 ;;
    --old-sha) old_sha=$2; shift 2 ;;
    --old-helper) old_helper=$2; shift 2 ;;
    --old-guard-profile) old_profile=$2; shift 2 ;;
    --new-core) new_core=$2; shift 2 ;;
    --new-evorch) new_evorch=$2; shift 2 ;;
    --new-sha) new_sha=$2; shift 2 ;;
    --new-helper) new_helper=$2; shift 2 ;;
    --new-guard-profile) new_profile=$2; shift 2 ;;
    --evidence) evidence=$2; shift 2 ;;
    --base) base=$2; shift 2 ;;
    --fixtures) fixtures=$2; shift 2 ;;
    --keep) keep=1; shift ;;
    --approve) approve=yes; shift ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; die "unknown argument $1" ;;
  esac
done
for v in old_core old_evorch old_sha new_core new_evorch new_sha; do
  eval "val=\$$v"
  [ -n "$val" ] || { usage >&2; die "missing --$(echo "$v" | tr _ -)"; }
done
for sha in "$old_sha" "$new_sha"; do
  printf '%s' "$sha" | grep -Eq '^[0-9a-f]{40}$' || die "SHA must be 40 lowercase hex digits: $sha"
done
for pr in "$old_profile" "$new_profile"; do
  case "$pr" in ""|dev|release) ;; *) die "--*-guard-profile must be dev or release" ;; esac
done
for f in "$old_core" "$old_evorch" "$new_core" "$new_evorch"; do
  [ -f "$f" ] && [ -x "$f" ] || die "not an executable file: $f"
done
command -v node >/dev/null || die "node (>= 22.5, node:sqlite) is required"
node -e 'const [a,b]=process.versions.node.split(".").map(Number); process.exit(a>22||(a===22&&b>=5)?0:1)' || die "node >= 22.5 is required"
command -v jq >/dev/null || die "jq is required (scripts/guard-artifact.sh)"
case "$fixtures" in main|graph|main,graph|graph,main) ;; *) die "--fixtures must be main, graph or main,graph" ;; esac

case "$base" in *[!A-Za-z0-9._/-]*|"") die "--base may only contain [A-Za-z0-9._/-]: $base" ;; esac
mkdir -p "$base"
base=$(cd "$base" && pwd -P)
case "$base" in *[!A-Za-z0-9._/-]*) die "resolved --base may only contain [A-Za-z0-9._/-]: $base" ;; esac
stamp=$(date -u +%Y%m%dT%H%M%SZ)
[ -n "$evidence" ] || evidence="$base/pr20-cutover-evidence-$(printf '%s' "$new_sha" | cut -c1-12)-$stamp"
mkdir -p "$evidence"
evidence=$(cd "$evidence" && pwd -P)
work=$(mktemp -d "$base/pr20c-run-XXXXXX")

cleanup() {
  # Only processes started from the sealed copies below are ever signalled.
  for p in /proc/[0-9]*; do
    pid=${p#/proc/}
    exe=$(readlink "$p/exe" 2>/dev/null || true)
    case "$exe" in "$work"/art-*/evorch-core|"$work"/art-*/evorch) kill -9 "$pid" 2>/dev/null || true ;; esac
  done
  if [ "$keep" = 0 ]; then
    if [ -f "$work/roots" ]; then
      while IFS= read -r r; do
        case "$r" in "$base"/pr20c-*) rm -rf -- "$r" ;; esac
      done < "$work/roots"
    fi
    rm -rf -- "$work"
  fi
}
trap cleanup EXIT INT TERM
: > "$work/roots"

sha() { sha256sum "$1" | cut -d' ' -f1; }

# The source delivery (product-manifest.json, guard-bundle.json, binaries) is only ever READ and
# copied. Its hashes are taken before sealing and again after the run so that the summary can
# show it was not changed.
record_sources() { # label core evorch phase
  rs_dir=$(dirname "$2")
  {
    for n in product-manifest.json guard-bundle.json evorch-core evorch-process-guard; do
      [ -f "$rs_dir/$n" ] && printf '%s\t%s\n' "$rs_dir/$n" "$(sha "$rs_dir/$n")"
    done
    printf '%s\t%s\n' "$3" "$(sha "$3")"
  } | jq -Rn '[inputs | split("\t") | {(.[0]): .[1]}] | add' > "$work/src-$1.$4.json"
}
record_sources old "$old_core" "$old_evorch" before
record_sources new "$new_core" "$new_evorch" before

# --- seal a private copy of one build: binaries + a guard bundle naming core and CLI
seal_build() { # label core evorch helper profile sha
  label=$1 core=$2 evo=$3 helper=$4 profile=$5 sha_of_build=$6
  out="$work/art-$label"
  mkdir -p "$out"
  src=$(dirname "$core")
  source_bundle=
  [ -f "$src/guard-bundle.json" ] && source_bundle="$src/guard-bundle.json"
  how=
  if [ -n "$source_bundle" ] && [ "$(dirname "$evo")" = "$src" ] && \
     jq -e '[.callers[].file] | (index("evorch-core") != null and index("evorch") != null)' "$source_bundle" >/dev/null 2>&1; then
    # already a sealed delivery that names both binaries: copy it whole, bundle included
    src_profile=$(jq -r .helper.profile "$source_bundle")
    [ -z "$profile" ] || [ "$profile" = "$src_profile" ] || die "--$label-guard-profile $profile contradicts the profile $src_profile recorded in $source_bundle"
    cp -p "$source_bundle" "$out/"
    [ -d "$src/.evorch-guard" ] && cp -pR "$src/.evorch-guard" "$out/"
    [ -f "$src/evorch-process-guard" ] && cp -p "$src/evorch-process-guard" "$out/"
    cp -p "$core" "$out/evorch-core"; cp -p "$evo" "$out/evorch"
    "$repo/scripts/guard-artifact.sh" verify "$out/guard-bundle.json" >/dev/null || die "sealed copy of the $label build does not verify"
    how="copied-sealed-delivery"
  else
    recipe=
    if [ -n "$source_bundle" ]; then
      [ -n "$helper" ] || helper="$src/$(jq -r .helper.file "$source_bundle")"
      src_profile=$(jq -r .helper.profile "$source_bundle")
      recipe=$(jq -c .helper.build_args "$source_bundle")
      [ -z "$profile" ] || [ "$profile" = "$src_profile" ] || die "--$label-guard-profile $profile contradicts the profile $src_profile recorded in $source_bundle"
      profile=$src_profile
      how="source-bundle"
    fi
    [ -n "$helper" ] || helper="$src/evorch-process-guard"
    [ -f "$helper" ] || die "no evorch-process-guard for the $label build: pass --$label-helper (build it with: cargo build -p process-guard --bin evorch-process-guard)"
    if [ -z "$profile" ]; then
      case "$helper" in
        */target/release/*) profile=release; how="path-heuristic" ;;
        */target/debug/*) profile=dev; how="path-heuristic" ;;
        *) die "cannot tell the profile of the $label helper $helper: pass --$label-guard-profile dev|release" ;;
      esac
    elif [ -z "$how" ]; then
      how="option"
    fi
    if [ -z "$recipe" ]; then
      if [ "$profile" = release ]; then
        recipe='["build","--locked","--release","-p","process-guard","--bin","evorch-process-guard"]'
      else
        recipe='["build","--locked","-p","process-guard","--bin","evorch-process-guard"]'
      fi
    fi
    cp -p "$core" "$out/evorch-core"; cp -p "$evo" "$out/evorch"
    recipe_note="recipe as decided by how_decided (source bundle, or the canonical recipe of the profile)"
    if ! GITHUB_SHA=$sha_of_build "$repo/scripts/guard-artifact.sh" seal "$helper" "$out" development "$profile" "$recipe" \
      "core:$out/evorch-core" "cli:$out/evorch" >/dev/null; then
      [ "$how" = source-bundle ] || die "could not seal the $label build (profile $profile, $how)"
      # The helper accepts a private (development) bundle only with the helper-only recipe; the combined
      # distribution recipe of a product bundle is valid only for purpose=distribution with the shipped
      # caller set. The real recipe stays recorded below (source_*); nothing is hidden.
      rm -f "$out/guard-bundle.json"
      if [ "$profile" = release ]; then
        recipe='["build","--locked","--release","-p","process-guard","--bin","evorch-process-guard"]'
      else
        recipe='["build","--locked","-p","process-guard","--bin","evorch-process-guard"]'
      fi
      GITHUB_SHA=$sha_of_build "$repo/scripts/guard-artifact.sh" seal "$helper" "$out" development "$profile" "$recipe" \
        "core:$out/evorch-core" "cli:$out/evorch" >/dev/null || die "could not seal the $label build (profile $profile, $how)"
      recipe_note="the PRIVATE bundle (work dir copy, evidence guard-bundle-*.private.json) carries the helper-only recipe because the helper accepts nothing else for a private core+cli bundle; the source product-manifest.json and guard-bundle.json are not modified and keep the real combined recipe (source_build_args; hashes in sourceFiles)"
    fi
  fi
  src_purpose=null src_args=null
  if [ -n "$source_bundle" ]; then
    src_purpose=$(jq -c .purpose "$source_bundle"); src_args=$(jq -c .helper.build_args "$source_bundle")
  fi
  jq -c --arg how "$how" --arg note "${recipe_note:-n/a}" --argjson sp "$src_purpose" --argjson sa "$src_args" --arg helper_path "$helper" \
    '{purpose, how_decided: $how, recipe_note: $note, profile: .helper.profile, build_args: .helper.build_args, source_purpose: $sp, source_build_args: $sa, helper_sha256: .helper.sha256, source_sha, callers: [.callers[].file]}' "$out/guard-bundle.json" > "$work/guard-$label.json"
  echo "guard bundle of the $label build: $(cat "$work/guard-$label.json")"
}
seal_build old "$old_core" "$old_evorch" "$old_helper" "$old_profile" "$old_sha"
seal_build new "$new_core" "$new_evorch" "$new_helper" "$new_profile" "$new_sha"

old_tree_sha=$(git -C "$(dirname "$old_core")" rev-parse HEAD 2>/dev/null || echo unknown)
new_tree_sha=$(git -C "$(dirname "$new_core")" rev-parse HEAD 2>/dev/null || echo unknown)
echo "old build: $old_sha (checkout HEAD of the binary: $old_tree_sha)"
echo "new build: $new_sha (checkout HEAD of the binary: $new_tree_sha)"

cat > "$work/identity-new.json" <<EOF
{"sha":"$new_sha","coreSha256":"$(sha "$work/art-new/evorch-core")","evorchSha256":"$(sha "$work/art-new/evorch")"}
EOF

fixture_json=
run_fixture() { # fixture
  fx=$1 attempts=0
  while :; do
    attempts=$((attempts + 1))
    root=$(mktemp -d "$base/pr20c-$fx-XXXXXX")
    echo "$root" >> "$work/roots"
    for d in config cache home tmp project run state; do mkdir -p "$root/$d"; done
    chmod 700 "$root/run" "$root/state"
    echo "== [$fx] attempt $attempts: OLD build creates the data in $root"
    set +e
    node "$driver" --phase gen --fixture "$fx" --root "$root" --artifact "$work/art-old" --approve "$approve"
    rc=$?
    set -e
    if [ "$rc" = 3 ] && [ "$attempts" -lt 5 ]; then
      echo "   the race of this fixture was lost (completed first); retrying with a fresh root"
      continue
    fi
    break
  done
  if [ "$rc" = 0 ]; then
    echo "== [$fx] NEW build cold-starts on the same paths"
    set +e
    node "$driver" --phase verify --fixture "$fx" --root "$root" --artifact "$work/art-new" --identity "$work/identity-new.json"
    set -e
  else
    echo "== [$fx] generation did not complete (exit $rc): the verification phase is not run" >&2
  fi
  mkdir -p "$evidence/$fx"
  cp -R "$root/driver/." "$evidence/$fx/" 2>/dev/null || true
  [ -d "$root/state/evorch/logs" ] && cp -R "$root/state/evorch/logs" "$evidence/$fx/core-logs" 2>/dev/null || true
  entry="{\"fixture\":\"$fx\",\"root\":\"$root\",\"genAttempts\":$attempts}"
  if [ -z "$fixture_json" ]; then fixture_json=$entry; else fixture_json="$fixture_json,$entry"; fi
}

for fx in $(echo "$fixtures" | tr ',' ' '); do run_fixture "$fx"; done

cat > "$work/meta.json" <<EOF
{
  "old": {"sha":"$old_sha","coreSha256":"$(sha "$work/art-old/evorch-core")","evorchSha256":"$(sha "$work/art-old/evorch")"},
  "new": {"sha":"$new_sha","coreSha256":"$(sha "$work/art-new/evorch-core")","evorchSha256":"$(sha "$work/art-new/evorch")"},
  "guard": {"old": $(cat "$work/guard-old.json"), "new": $(cat "$work/guard-new.json")},
  "approveProject": "$approve",
  "environment": {"uid":$(id -u),"uname":"$(uname -srm)","node":"$(node -v)","utc":"$stamp","base":"$base"},
  "fixtures": [$fixture_json]
}
EOF
record_sources old "$old_core" "$old_evorch" after
record_sources new "$new_core" "$new_evorch" after
for label in old new; do
  jq -n --slurpfile b "$work/src-$label.before.json" --slurpfile a "$work/src-$label.after.json" \
    '{before: $b[0], after: $a[0], unchanged: ($b[0] == $a[0])}' > "$work/src-$label.json"
done
tmp_meta=$(mktemp "$work/meta.XXXXXX")
jq --slurpfile o "$work/src-old.json" --slurpfile n "$work/src-new.json" '. + {sourceFiles: {old: $o[0], new: $n[0]}}' "$work/meta.json" > "$tmp_meta" && mv "$tmp_meta" "$work/meta.json"
cp "$work/meta.json" "$evidence/meta.json"
src_dir_new=$(dirname "$new_core")
[ -f "$src_dir_new/product-manifest.json" ] && cp "$src_dir_new/product-manifest.json" "$evidence/product-manifest-source.json"
[ -f "$src_dir_new/guard-bundle.json" ] && cp "$src_dir_new/guard-bundle.json" "$evidence/guard-bundle-source-new.json"
cp "$work/guard-new.json" "$evidence/guard-new.receipt.json"
cp "$work/guard-old.json" "$evidence/guard-old.receipt.json"
cp "$work/art-old/guard-bundle.json" "$evidence/guard-bundle-old.private.json"
cp "$work/art-new/guard-bundle.json" "$evidence/guard-bundle-new.private.json"
set +e
node "$report" --meta "$work/meta.json" --out "$evidence"
rc=$?
set -e
echo "evidence: $evidence"
exit "$rc"
