#!/usr/bin/env bash
# Captures the size, DPI, panel and settings-screen evidence matrix.
#
# Usage: scripts/gui-evidence-matrix.sh [OUT]
# HEADLESS_CAPTURE_BIN  a built headless_capture binary. Unset: build it once with
#                       cargo and use it from ${CARGO_TARGET_DIR:-target}/debug.
# GUI_EVIDENCE_JOBS     captures to run at once (default: the CPU count, at most 4).
#                       Every capture writes its own PNG and uses its own temporary
#                       directories, so the order does not matter.
set -euo pipefail

OUT=${1:-target/gui-evidence}
mkdir -p "$OUT"

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
if [[ -z "${HEADLESS_CAPTURE_BIN:-}" ]]; then
    (cd "$repo_root" && cargo build -q -p gui --bin headless_capture)
    HEADLESS_CAPTURE_BIN=${CARGO_TARGET_DIR:-$repo_root/target}/debug/headless_capture
fi
[[ -x "$HEADLESS_CAPTURE_BIN" ]] || { echo "headless_capture is not executable: $HEADLESS_CAPTURE_BIN" >&2; exit 1; }
jobs_max=${GUI_EVIDENCE_JOBS:-$(( $(nproc) < 4 ? $(nproc) : 4 ))}

capture_failed=0
# Start one capture in the background, keeping at most $jobs_max running. A failed
# capture is remembered and fails the script once every capture has finished.
capture() {
    while (( $(jobs -rp | wc -l) >= jobs_max )); do
        wait -n || capture_failed=1
    done
    "$HEADLESS_CAPTURE_BIN" "$@" &
}

for theme in graphite tokyo-night high-contrast; do
    theme_out="$OUT/$theme"
    mkdir -p "$theme_out"
    for state in empty demo error-thread edit-profile theme-settings pending-approvals; do
        flags=(--theme "$theme")
        case "$state" in
            demo) flags+=(--demo) ;;
            error-thread) flags+=(--demo --error-thread) ;;
            edit-profile) flags+=(--demo --edit-profile) ;;
            theme-settings) flags+=(--open-theme-settings) ;;
            pending-approvals) flags+=(--demo --pending-approvals) ;;
        esac
        for s in 1280x720 1024x768 1920x720 960x600; do
            d=1.0
            path="$theme_out/$state-$s@$d.png"
            capture "${flags[@]}" --size "$s" --dpi "$d" --out "$path"
        done
    done

    # One focused capture per default dock panel. A missing tab fails in
    # headless_capture instead of silently dropping its evidence.
    for panel in sidebar-main agent-main subagents-home tasks-main notifications-main diff-main terminal-main; do
        path="$theme_out/panel-$panel-1280x720@1.0.png"
        capture --theme "$theme" --demo --activate "$panel" --out "$path"
    done

    for panel in memory-main; do
        path="$theme_out/panel-$panel-1280x720@1.0.png"
        capture --theme "$theme" --demo --with-memory --activate "$panel" --out "$path"
    done

    for modal in provider role routing sandbox; do
        case "$modal" in
            provider) flag=--open-settings ;;
            *) flag="--open-$modal-settings" ;;
        esac
        path="$theme_out/modal-$modal-1280x720@1.0.png"
        capture --theme "$theme" --demo "$flag" --out "$path"
    done

    for state in demo edit-profile; do
        flags=(--theme "$theme" --demo)
        if [[ "$state" == edit-profile ]]; then
            flags+=(--edit-profile)
        fi
        s=1280x720
        for d in 1.5 2.0; do
            path="$OUT/$theme/$state-$s@$d.png"
            capture "${flags[@]}" --size "$s" --dpi "$d" --out "$path"
        done
    done
done

while (( $(jobs -rp | wc -l) > 0 )); do
    wait -n || capture_failed=1
done
if (( capture_failed )); then
    echo 'At least one headless_capture run failed.' >&2
    exit 1
fi

# Ignored tests store their own evidence in subdirectories, outside this matrix.
shopt -s nullglob dotglob
for theme in graphite tokyo-night high-contrast; do
    pngs=("$OUT/$theme"/*.png)
    if [[ ${#pngs[@]} -ne 40 ]]; then
        printf 'Expected 40 matrix PNGs in %s, found %s\n' "$OUT/$theme" "${#pngs[@]}" >&2
        exit 1
    fi
    printf '%s\n' "${pngs[@]}"
done
