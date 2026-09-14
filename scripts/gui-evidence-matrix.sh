#!/usr/bin/env bash
set -euo pipefail

OUT=${1:-target/gui-evidence}
mkdir -p "$OUT"

for theme in graphite tokyo-night; do
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
            cargo run -q -p gui --bin headless_capture -- "${flags[@]}" --size "$s" --dpi "$d" --out "$path"
        done
    done

    for state in demo edit-profile; do
        flags=(--theme "$theme" --demo)
        if [[ "$state" == edit-profile ]]; then
            flags+=(--edit-profile)
        fi
        s=1280x720
        for d in 1.5 2.0; do
            path="$OUT/$theme/$state-$s@$d.png"
            cargo run -q -p gui --bin headless_capture -- "${flags[@]}" --size "$s" --dpi "$d" --out "$path"
        done
    done
done

# Ignored tests store their own evidence in subdirectories, outside this matrix.
shopt -s nullglob dotglob
for theme in graphite tokyo-night; do
    pngs=("$OUT/$theme"/*.png)
    if [[ ${#pngs[@]} -ne 28 ]]; then
        printf 'Expected 28 matrix PNGs in %s, found %s\n' "$OUT/$theme" "${#pngs[@]}" >&2
        exit 1
    fi
    printf '%s\n' "${pngs[@]}"
done
