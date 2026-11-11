#!/usr/bin/env bash
# scripts/gui-evidence-matrix.sh must fail when any capture fails, also when the
# capture has already exited by the time the script reaches it, and must reject a
# bad GUI_EVIDENCE_JOBS. A fake headless_capture stands in for the real one.
set -euo pipefail
cd "$(dirname "$0")/../.."
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

cat > "$work/fake-capture" <<'EOF'
#!/usr/bin/env bash
out=""
while [[ $# -gt 0 ]]; do
  if [[ "$1" == --out ]]; then out="$2"; fi
  shift
done
if [[ -n "${FAKE_FAIL_PATTERN:-}" && "$out" == *"$FAKE_FAIL_PATTERN"* ]]; then
  echo "fake capture failure: $out" >&2
  exit 42
fi
echo png > "$out"
EOF
chmod +x "$work/fake-capture"
export HEADLESS_CAPTURE_BIN="$work/fake-capture"

run() { # run <out-dir>; stdout and stderr go to $work/log
  scripts/gui-evidence-matrix.sh "$1" > "$work/log" 2>&1
}

for jobs in 1 4; do
  rm -rf "$work/ok"
  GUI_EVIDENCE_JOBS=$jobs run "$work/ok" || { cat "$work/log" >&2; echo "matrix failed with $jobs job(s)" >&2; exit 1; }
  [[ "$(find "$work/ok" -name '*.png' | wc -l)" -eq 120 ]] \
    || { echo "expected 120 PNGs with $jobs job(s)" >&2; exit 1; }
done

# Captures that fail at once, so they are finished when the script reaches them:
# the first ones started, a middle one and the last ones.
for pattern in 'empty-1280x720@1.0' 'panel-tasks-main' 'modal-sandbox'; do
  for jobs in 1 4; do
    rm -rf "$work/bad"
    if FAKE_FAIL_PATTERN=$pattern GUI_EVIDENCE_JOBS=$jobs run "$work/bad"; then
      echo "matrix succeeded although captures matching '$pattern' fail ($jobs job(s))" >&2
      exit 1
    fi
    grep -q 'failed with status 42' "$work/log" \
      || { cat "$work/log" >&2; echo "the failing status of '$pattern' was not reported" >&2; exit 1; }
  done
done

for jobs in 0 -1 abc; do
  rm -rf "$work/none"
  status=0
  GUI_EVIDENCE_JOBS=$jobs run "$work/none" || status=$?
  [[ "$status" -eq 2 ]] || { echo "GUI_EVIDENCE_JOBS='$jobs' exited $status, expected 2" >&2; exit 1; }
done
echo "gui evidence matrix tests passed"
