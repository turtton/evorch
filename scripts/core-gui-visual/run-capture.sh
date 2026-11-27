#!/usr/bin/env bash
# One capture of the new GUI's screens in a process that has nothing of the runner.
#
# Usage: run-capture.sh <core_client_capture> <out-dir> <work-dir> <font-file>
#
# The process starts from an empty environment (`env -i`) with the allowlist below:
# a private HOME, XDG directories, TMPDIR and working directory under <work-dir>, a
# fixed locale and time zone, a fontconfig that knows only <font-file>, and the few
# variables the loader and the software renderer need. The job token and every other
# variable of the runner stay out. The tool checks the allowlist again.
#
# From the caller's environment only these are read: LD_LIBRARY_PATH (libcef.so and
# the Vulkan loader), VK_ICD_FILENAMES / VK_DRIVER_FILES (a renderer outside the
# default places) and VISUAL_PATH (where fc-match is; default /usr/bin:/bin).
set -euo pipefail

bin=$(realpath "${1:?core_client_capture binary}")
out=$(realpath -m "${2:?output directory}")
work=$(realpath -m "${3:?work directory}")
font=${4:?font file}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

[[ -x "$bin" ]] || { echo "not executable: $bin" >&2; exit 2; }
[[ -r "$font" ]] || { echo "font not readable: $font" >&2; exit 2; }
mkdir -p "$out"
rm -rf "$work"
mkdir -p "$work"/{home/.config/evorch,home/.cache,home/.local/share,home/.local/state,tmp,cwd,run}
chmod 700 "$work/run"

sentinel="sentinel-$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
# What the screens must never read or show: a settings file and a credential-looking
# value in the private home. Nothing opens them; a leak would be a drawn text.
printf 'secret = "%s"\n' "$sentinel" > "$work/home/.config/evorch/settings.toml"

"$here/make-fonts-conf.sh" "$font" > "$work/fonts.conf"

env_args=(
  "HOME=$work/home"
  "TMPDIR=$work/tmp"
  "XDG_CONFIG_HOME=$work/home/.config"
  "XDG_CACHE_HOME=$work/home/.cache"
  "XDG_DATA_HOME=$work/home/.local/share"
  "XDG_STATE_HOME=$work/home/.local/state"
  "XDG_RUNTIME_DIR=$work/run"
  "PATH=${VISUAL_PATH:-/usr/bin:/bin}"
  "FONTCONFIG_FILE=$work/fonts.conf"
  "LANG=C.UTF-8"
  "LC_ALL=C.UTF-8"
  "TZ=UTC"
  "WGPU_BACKEND=vulkan"
  "EVORCH_VISUAL_SENTINEL=$sentinel"
)
[[ -n "${LD_LIBRARY_PATH:-}" ]] && env_args+=("LD_LIBRARY_PATH=$LD_LIBRARY_PATH")
[[ -n "${VK_ICD_FILENAMES:-}" ]] && env_args+=("VK_ICD_FILENAMES=$VK_ICD_FILENAMES")
[[ -n "${VK_DRIVER_FILES:-}" ]] && env_args+=("VK_DRIVER_FILES=$VK_DRIVER_FILES")

cd "$work/cwd"
exec env -i "${env_args[@]}" "$bin" capture --out "$out" --font-file "$font"
