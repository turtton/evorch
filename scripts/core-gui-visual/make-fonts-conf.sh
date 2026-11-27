#!/usr/bin/env bash
# Print the capture's fontconfig for one font file.
# Usage: make-fonts-conf.sh <font-file>
set -euo pipefail
font=${1:?font file}
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
dir=$(dirname "$font")
sed -e "s|@FONT_DIR@|$dir|" -e "s|@DIR_NAME@|$(basename "$dir")|" -e "s|@FILE_NAME@|$(basename "$font")|" \
  "$here/fonts.conf.in"
