#!/usr/bin/env bash
# The capture's fontconfig must list the one pinned font file and nothing else, even when
# the same directory holds other fonts.
#
# Needs FONT_FILE (a readable font) and fc-list (on PATH, or in VISUAL_PATH).
set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
font=${FONT_FILE:?FONT_FILE}
[[ -r "$font" ]] || { echo "font not readable: $font" >&2; exit 2; }
if [[ -n "${VISUAL_PATH:-}" ]]; then PATH="$VISUAL_PATH:$PATH"; fi
command -v fc-list >/dev/null || { echo "fc-list is not available" >&2; exit 2; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/fonts" "$work/xdg"
cp "$font" "$work/fonts/pinned.ttf"
cp "$font" "$work/fonts/extra.ttf"
"$here/../make-fonts-conf.sh" "$work/fonts/pinned.ttf" > "$work/fonts.conf"

listed=$(env -i PATH="$PATH" HOME="$work" XDG_CACHE_HOME="$work/xdg" FONTCONFIG_FILE="$work/fonts.conf" \
  fc-list --format '%{file}\n' | sort -u)
if [[ "$listed" != "$work/fonts/pinned.ttf" ]]; then
  echo "fontconfig lists more or less than the pinned file:" >&2
  echo "$listed" >&2
  exit 1
fi
echo OK
