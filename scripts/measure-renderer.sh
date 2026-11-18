#!/usr/bin/env bash
# Reproducible measurement of the out-of-process renderer helper (ADR 0045,
# plan stage 16a). Results: docs/renderer-measurement.md.
#
#   scripts/measure-renderer.sh [--out DIR] [--quick] [--check] [SCENARIO...]
#
#   --check   fmt, clippy and unit tests of the tool only (no measurement)
#
# SCENARIO: env startup tabs stream transfer ipc  (default: all)
#
# Needs the dev shell environment (CEF_PATH, LD_LIBRARY_PATH, LIBCLANG_PATH):
# run it inside `nix develop`, or through
# `.agents/skills/sandbox-build/scripts/with-dev-env.sh scripts/measure-renderer.sh`.
# No display is needed or used. CARGO_TARGET_DIR is not set; the real helper
# is built with the checkout's own cargo, and the measurement tool (a
# standalone package outside the workspace) is built beside it.
#
# JSON lines go to $OUT/<scenario>.jsonl, the environment to $OUT/environment.json,
# the build log to $OUT/build.log.
set -euo pipefail

root="$(git rev-parse --show-toplevel)"
cd "$root"

out=""
quick=0
check=0
scenarios=()
while (($#)); do
  case "$1" in
    --out) out="$2"; shift 2 ;;
    --quick) quick=1; shift ;;
    --check) check=1; shift ;;
    -h | --help) sed -n '2,18p' "$0"; exit 0 ;;
    *) scenarios+=("$1"); shift ;;
  esac
done
((${#scenarios[@]})) || scenarios=(env startup tabs stream transfer ipc)
if ((check)); then
  # What lefthook and CI do not cover: the tool is a standalone package.
  m=scripts/measure-renderer/Cargo.toml
  cargo fmt --check --manifest-path "$m"
  cargo clippy --offline --release --manifest-path "$m" --all-targets -- -D warnings
  cargo test --offline --release --manifest-path "$m"
  exit 0
fi
out="${out:-${TMPDIR:-/tmp}/measure-renderer-$(date +%Y%m%d-%H%M%S)}"
mkdir -p "$out"
out="$(cd "$out" && pwd)"

echo "building (log: $out/build.log)" >&2
cargo build --offline -p evorch-renderer -p browser-cef --features browser-cef/cef --bins \
  >"$out/build.log" 2>&1 || { tail -n 30 "$out/build.log" >&2; exit 1; }
cargo build --offline --release --manifest-path scripts/measure-renderer/Cargo.toml \
  >>"$out/build.log" 2>&1 || { tail -n 30 "$out/build.log" >&2; exit 1; }

# Where cargo put things: the checkout's target/ holds final binaries.
renderer="$root/target/debug/evorch-renderer"
tool="$root/scripts/measure-renderer/target/release/measure-renderer"
[[ -x "$renderer" && -x "$tool" ]] || { echo "binaries missing: $renderer $tool" >&2; exit 1; }

# The helper's TMPDIR is /proc/self/cwd (Chromium's singleton socket path must
# fit in sun_path), so the run directories can live anywhere writable.
work="$out/work"
mkdir -p "$work"

secs=20
runs=8
((quick)) && { secs=6; runs=3; }

run() {
  local name="$1"
  shift
  echo "== $name $*" >&2
  "$tool" "$renderer" "$work" "$@" >>"$out/$name.jsonl"
}

for scenario in "${scenarios[@]}"; do
  case "$scenario" in
    env) "$tool" "$renderer" "$work" env >"$out/environment.json" ;;
    startup)
      # Timing runs take no reading between spawn and the first frame; the
      # memory runs read at every stage and are a separate set of trials.
      run startup startup runs="$runs"
      run startup startup runs=3 kill=1
      run startup startup runs=3 mem=1
      ;;
    tabs)
      run tabs tabs w=1280 h=720
      run tabs tabs w=3840 h=2160
      ;;
    stream)
      for fps in 1 10 30 60; do
        run stream stream fps="$fps" secs="$secs" tabs=1 w=1280 h=720
      done
      run stream stream fps=30 secs="$secs" tabs=1 w=3840 h=2160
      run stream stream fps=10 secs="$secs" tabs=4 w=1280 h=720
      run stream stream fps=30 secs="$secs" tabs=4 w=1280 h=720
      run stream stream fps=30 secs="$secs" tabs=4 w=3840 h=2160
      ;;
    transfer)
      # The real helper's frame channel through the pass-through proxy.
      for fps in 10 30 60; do
        run transfer transfer fps="$fps" secs="$secs" tabs=1 w=1280 h=720
      done
      run transfer transfer fps=30 secs="$secs" tabs=1 w=320 h=180
      run transfer transfer fps=30 secs="$secs" tabs=1 w=3840 h=2160
      run transfer transfer fps=30 secs="$secs" tabs=4 w=1280 h=720
      run transfer transfer fps=30 secs="$secs" tabs=4 w=3840 h=2160
      ;;
    ipc)
      for fps in 1 10 30 60 0; do
        run ipc ipc fps="$fps" secs="$secs" w=1280 h=720
      done
      for fps in 30 0; do
        run ipc ipc fps="$fps" secs="$secs" w=3840 h=2160
      done
      run ipc ipc fps=30 secs="$secs" tabs=4 w=1280 h=720
      run ipc ipc fps=30 secs="$secs" tabs=4 w=3840 h=2160
      run ipc ipc fps=60 secs="$secs" w=1280 h=720 hold_ms=200
      run ipc ipc fps=60 secs="$secs" w=3840 h=2160 hold_ms=200
      ;;
    *) echo "unknown scenario $scenario" >&2; exit 2 ;;
  esac
done
echo "results: $out" >&2
