# Workbench panels (W-EF)

The right-hand workbench contains **Agents**, **Diff**, and **Terminal**.
Dedicated Goal and Merge panes have been removed.

## Goals and merge approval

Send `/goal <text>` from the conversation composer. In demo mode, send
`/goal DEMO-GOAL implement fixture unit` and look for `accepted: goal-1`.
Sending `/goal` without arguments shows usage without changing the selected tab.

Goal submission, pause/resume/cancel, and merge decisions remain available through
the existing state APIs. The runtime `DeliveryPort` is unchanged. A pending merge
request produces a conversation notice containing its PR number. This version
does not provide an approval button; the Diff-view approval surface is follow-up
work, not an automatic approval policy.

## Saved layouts

Workspace schema v3 migrates v1/v2 JSON layouts and embedded TOML settings before
typed deserialization. Panels with kind `goal` or `merge_approval` are removed by
kind, including customized IDs. Their tab references are removed from main,
floating, and extra windows; active indices are clamped, empty tabs disappear,
and one-sided splits collapse. Empty extra windows are dropped. A surviving
floating node can replace an empty root. A completely empty main window returns
a typed migration error instead of manufacturing an invalid empty tabs node.

Historical screenshot directories document earlier releases and may still show
the removed surfaces. Current capture command:

```sh
cargo run -p gui --bin headless_capture -- --demo --out /tmp/opencode/w-ef.png
```
