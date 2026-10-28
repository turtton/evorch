#!/usr/bin/env bash
set -euo pipefail
cd "${1:?usage}"
git grep -lwE 'UpdateFileChunk|is_end_of_file' -- crates | xargs sed -i -E 's/\bUpdateFileChunk\b/UpdateChunk/g; s/\bis_end_of_file\b/at_eof/g'
