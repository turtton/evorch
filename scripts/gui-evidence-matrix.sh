#!/usr/bin/env bash
set -euo pipefail

OUT=${1:-target/gui-evidence}
mkdir -p "$OUT"

for state in empty demo error-thread edit-profile; do
    flags=()
    case "$state" in
        demo) flags=(--demo) ;;
        error-thread) flags=(--demo --error-thread) ;;
        edit-profile) flags=(--demo --edit-profile) ;;
    esac
    for s in 1280x720 1024x768 1920x720 960x600; do
        d=1.0
        path="$OUT/$state-$s@$d.png"
        cargo run -q -p gui --bin headless_capture -- "${flags[@]}" --size "$s" --dpi "$d" --out "$path"
    done
done

for state in demo edit-profile; do
    flags=(--demo)
    if [[ "$state" == edit-profile ]]; then
        flags+=(--edit-profile)
    fi
    s=1280x720
    for d in 1.5 2.0; do
        path="$OUT/$state-$s@$d.png"
        cargo run -q -p gui --bin headless_capture -- "${flags[@]}" --size "$s" --dpi "$d" --out "$path"
    done
done

# Ignored tests store their own evidence in subdirectories, outside this matrix.
shopt -s nullglob dotglob
pngs=("$OUT"/*.png)
if [[ ${#pngs[@]} -ne 20 ]]; then
    printf 'Expected 20 matrix PNGs in %s, found %s\n' "$OUT" "${#pngs[@]}" >&2
    exit 1
fi
printf '%s\n' "${pngs[@]}"
