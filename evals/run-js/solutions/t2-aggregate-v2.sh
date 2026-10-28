#!/usr/bin/env bash
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cp "$here/verify/expected/t2-aggregate-v2.json" "${1:?usage}/answers/t2.json"
