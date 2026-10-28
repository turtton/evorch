#!/usr/bin/env bash
# t5-single-edit-v3: 対照課題。旧課題 t5-single-edit と同じ検証（run_js を使わないことが望ましい）。
exec "$(dirname "$0")/t5-single-edit.sh" "$@"
