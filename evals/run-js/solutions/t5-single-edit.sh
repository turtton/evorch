#!/usr/bin/env bash
set -euo pipefail
cd "${1:?usage}"
sed -i 's/^const MAX_LINES: usize = 200;/const MAX_LINES: usize = 150;/; 4s/最大 200 行/最大 150 行/' crates/tools/src/tools/grep.rs
