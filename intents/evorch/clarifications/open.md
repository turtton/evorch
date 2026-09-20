# Open Clarifications

- none

## クローズ済み記録

- `v08-verification-matrix`: queue-state に残存していた stale な `blocked_by` clarification
  ("goal not yet recorded in the packet") は packet.yaml に goal/受け入れ基準が既に記録済みのため
  2026-09-21 に queue projection 側から除去。clarification 自体は存在しなかった。