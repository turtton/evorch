#!/bin/sh
set -eu
printf '%s\000' "$0" "$@" > "$B3_CAPTURE/argv"
cat /proc/$$/environ > "$B3_CAPTURE/env"
pwd -P > "$B3_CAPTURE/cwd"
readlink /proc/$$/fd/0 > "$B3_CAPTURE/stdin"
readlink /proc/$$/fd/1 > "$B3_CAPTURE/stdout"
readlink /proc/$$/fd/2 > "$B3_CAPTURE/stderr-fd"
printf '%s\n' "$$" > "$B3_CAPTURE/pid"
IFS= read -r _ < "$B3_CAPTURE/start"
cat "$B3_CAPTURE/endpoint" >&2
if [ -f "$B3_CAPTURE/close-stderr" ]; then exec 2>&-; fi
IFS= read -r _ < "$B3_CAPTURE/hold"
