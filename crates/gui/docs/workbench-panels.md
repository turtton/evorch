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

## Desktop notifications

The native GUI sends system notifications when an agent asks the user a
question or finishes a conversation turn. Notifications are sent only while
evorch is inactive, including when its window is minimized. A question shows
its title; completion shows the conversation title so the user can find the
relevant chat.

Internal questions and completions from child agents do not produce desktop
notifications. Waiting for an answer, stopping a run, and unfinished goals do
not count as completed work. Opening saved history does not resend old
notifications, and returning to an inactive window does not send notifications
for events already handled while evorch was active.

Notifications use the operating system's notification service and respect its
notification settings. A delivery failure is logged without interrupting the
agent. Headless runs do not send desktop notifications.

Delivery has been verified on Linux. Windows builds currently use the default
PowerShell notification identity, so their notification settings belong to
PowerShell rather than a separate evorch application entry.

## Terminal

The Terminal pane is an xterm-compatible emulator (alacritty_terminal) attached
to a real PTY. Each project gets its own shell, started in the project's repo
root the first time the pane shows it; with no project selected the shell starts
in the home directory. Switching projects keeps the other shells running, and
removing a project ends its shell. The shell is `$SHELL` (falling back to
`/bin/sh`) with `TERM=xterm-256color` and `COLORTERM=truecolor`. The grid
resizes with the pane and the PTY follows.

Click the grid to focus it. While focused, every key goes to the shell,
including Tab, arrows, Escape and Ctrl+letter. Workbench shortcuts that are not
Ctrl+letter (for example Ctrl+1/2/3 and Ctrl+Shift+R) still reach the
workbench.

- Drag to select; double-click selects a word and triple-click a line. Ctrl+C
  copies while text is selected and sends an interrupt otherwise;
  Ctrl+Shift+C always copies. Ctrl+V pastes, using bracketed paste when the
  application asks for it. The right-click menu has Copy and Paste.
- The mouse wheel scrolls the 10,000-line scrollback, as do Shift+PageUp and
  Shift+PageDown. Full-screen applications get wheel events as mouse reports,
  or as arrow keys when they did not enable mouse reporting.
- Hold Shift to select text in an application that captures the mouse.
- When the shell exits, the header shows its exit code. Press Enter or the
  restart button to start a new shell; the restart button also replaces a
  running shell.

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
