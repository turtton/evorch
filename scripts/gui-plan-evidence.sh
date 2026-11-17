#!/usr/bin/env bash
# Captures the conversation plan evidence: the composer progress band and the
# plan panel in every theme, for a conversation without a plan, for a plan in
# mixed progress, and for a selected task with long text and paged history.
# Kept apart from scripts/gui-evidence-matrix.sh so its 117 PNGs stay as they are.
#
# Usage: scripts/gui-plan-evidence.sh [OUT]
# HEADLESS_CAPTURE_BIN  a built headless_capture binary. Unset: build it once with
#                       cargo and use it from ${CARGO_TARGET_DIR:-target}/debug.
set -euo pipefail

OUT=${1:-target/gui-evidence/conversation-plan}
mkdir -p "$OUT"

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [[ -z "${HEADLESS_CAPTURE_BIN:-}" ]]; then
    (cd "$repo_root" && cargo build -q -p gui --bin headless_capture)
    HEADLESS_CAPTURE_BIN=${CARGO_TARGET_DIR:-$repo_root/target}/debug/headless_capture
fi
[[ -x "$HEADLESS_CAPTURE_BIN" ]] || { echo "headless_capture is not executable: $HEADLESS_CAPTURE_BIN" >&2; exit 1; }

themes=(graphite tokyo-night high-contrast)
scenarios=(unplanned mixed detailed)
for theme in "${themes[@]}"; do
    for scenario in "${scenarios[@]}"; do
        "$HEADLESS_CAPTURE_BIN" --theme "$theme" --demo --plan "$scenario" \
            --size 1280x720 --dpi 1.0 --out "$OUT/$theme-$scenario-1280x720@1.0.png"
    done
done

shopt -s nullglob
pngs=("$OUT"/*.png)
expected=$(( ${#themes[@]} * ${#scenarios[@]} ))
if [[ ${#pngs[@]} -ne $expected ]]; then
    printf 'Expected %s conversation plan PNGs in %s, found %s\n' "$expected" "$OUT" "${#pngs[@]}" >&2
    exit 1
fi
printf '%s\n' "${pngs[@]}"
