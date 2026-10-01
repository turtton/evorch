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

## Continuing a conversation

Send `/continue` (no arguments) to resume the active conversation after an error
or an operator Stop, without adding a user-message bubble. It retains saved
history and uses current ownership and model settings; it also works after a
restart when a recoverable snapshot exists. It does not submit composer image
attachments, start an empty conversation, or blindly replay interrupted tools
or old shell jobs. A missing/invalid snapshot is reported rather than silently
starting over. Wait for an in-progress stop to settle before continuing; read-only
sessions must explicitly Start or Claim write mode first.

The command appears in completion suggestions and `/help`. Demo mode reports
that no resumable conversation is available.

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
