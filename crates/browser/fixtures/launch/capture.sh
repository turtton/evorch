#!/bin/sh
set -eu
# Snapshot stdio before recording it: dash temporarily redirects the shell's
# own stdout for readlink, so observing fd 1 would capture the output file.
exec 3<&0 4>&1 5>&2
printf '%s\000' "$0" "$@" > "$B3_CAPTURE/argv"
cat /proc/$$/environ > "$B3_CAPTURE/env"
pwd -P > "$B3_CAPTURE/cwd"
readlink /proc/$$/fd/3 > "$B3_CAPTURE/stdin"
readlink /proc/$$/fd/4 > "$B3_CAPTURE/stdout"
readlink /proc/$$/fd/5 > "$B3_CAPTURE/stderr-fd"
# In particular, do not keep an extra stderr writer across the EOF barrier.
exec 3<&- 4>&- 5>&-
printf '%s\n' "$$" > "$B3_CAPTURE/pid"
IFS= read -r _ < "$B3_CAPTURE/start"
cat "$B3_CAPTURE/endpoint" >&2
if [ -f "$B3_CAPTURE/close-stderr" ]; then exec 2>&-; fi
IFS= read -r _ < "$B3_CAPTURE/hold"
