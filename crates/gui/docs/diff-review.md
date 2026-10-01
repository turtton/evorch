# Diff review and thread status

## Diff tab

The Diff tab is a read-only review surface inspired by [Crit's code mode](https://crit.md/modes/code):

- Select **Working tree** (index versus working tree) or **Branch vs main**
  (the merge base with `main` versus `HEAD`); **Refresh** reloads the selected scope.
- **Unified** shows old/new line-number gutters together. **Split** aligns removed
  lines on the left and added lines on the right, with shared context on both sides.
- Each file has a collapsible path heading, change status, and addition/deletion counts.
- Additions have a subtle green background and `+` marker; deletions have a subtle
  red background and `−` marker. Hunk headers separate context ranges. Source text
  uses the existing syntax highlighter and remains selectable.
- Horizontal scrolling preserves indentation and long lines. Vertical rendering is
  virtualized, and parsing is cached until the snapshot changes.
- Binary files, renames, mode changes, no-newline markers, errors, empty states,
  and the existing 256 KiB truncation notice remain visible. Non-Git tool snapshots
  fall back to selectable monospace text rather than being discarded.

This does not implement Crit's commenting, round-to-round review, or context fetching.

## Thread status

The sidebar dot represents `ThreadRecord::state`, not selection or unread activity.
The existing aggregation priority is unchanged:

1. Operator pause → **Paused**.
2. Any stopped run → **Stopped (resumable)**.
3. Any failed run → **Error**.
4. Any pending or running run → **Running**.
5. Any waiting run → **Waiting**.
6. All known runs finished → **Done**.
7. Otherwise → **Active** (no resolved execution state).

Graphite colors: Active = accent blue, Paused = gray, Stopped = amber,
Running = cyan (`#22d3ee`), Waiting = blue (`#60a5fa`), Done = green, Error = red.
Tokyo Night uses the same semantic roles with its own palette; Running and Waiting
are distinct in both palettes.

The dot's hover tooltip and accessibility label retain the status name. Redundant
always-visible status text is omitted, including on narrow rows. An unanswered user
question is indicated independently with an amber **?** and an “Answer needed”
tooltip, even if the runtime continues working; it never overrides the state dot.

## Regression coverage

`diff_headless` covers colored full-width rows, both line numbers, split alignment,
file collapse, narrow toolbar layout, large-document virtualization, and legacy
loading/empty/error/truncated/plain-text states. Parser unit tests cover multiple
hunks, additions/deletions, renames, binary/mode changes, quoted UTF-8 paths, and
incomplete output. Sidebar and question tests cover lifecycle updates, accessible
status dots, dense row geometry, and independent question indicators.

The ignored `capture_review_diff_unified_and_split` test renders both views at
1280×720 and 800×600. CI's offscreen gate runs it with a required adapter and uploads
PNGs under `target/gui-evidence/diff-review/` as part of the `gui-evidence` artifact.
