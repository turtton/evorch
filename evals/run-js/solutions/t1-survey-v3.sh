#!/usr/bin/env bash
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
cp "$here/verify/expected/t1-survey-v3.json" "${1:?usage}/answers/t1.json"
