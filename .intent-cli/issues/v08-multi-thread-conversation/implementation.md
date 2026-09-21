# v08-multi-thread-conversation Implementation Packet

## Goal

Let the user open any sidebar thread as its own conversation pane (Obsidian-style ctrl+click), bound permanently to that thread id, so multiple thread conversations can be viewed side by side in the dock. The main conversation pane keeps following the active thread; extra panes never retarget.

## Why

Thread badges (arc: subagent tab thread labels) made lineage visible, but inspection is still serialized: to watch two threads the user must switch the active thread back and forth, which also retargets the only conversation view. The transcript model layer already stores one `TranscriptModel` per thread (`TranscriptKey::Thread`), so the only missing piece is the view layer: per-pane thread binding instead of a single global `ConversationFocus`.

## Current baseline (verified in code)

- `WorkbenchState::transcript()` (`crates/gui/src/app/state.rs`) returns exactly one view: `ConversationFocus::Thread` (the sidebar `active_thread`) or `ConversationFocus::Agent(run_id)`.
- Routing is already per-thread (`crates/gui/src/model/transcript_registry.rs`); thread-root rebinding for continuations already lands in the owning thread's model.
- Dock panel persistence is serde-based `PanelKind` + optional `target` with validators (`workspace-ui/src/panels.rs`, `validate.rs`). Pre-v1 policy (AGENTS.md) permits adding new `PanelKind` variants without migration.

## Scope

- New `PanelKind::ThreadConversation` (or equivalent) with required `target = thread_id`; register + validate like `AgentTranscript`.
- Sidebar affordance: ctrl+click on a thread row opens (idempotently) its thread-bound pane in the dock; existing click behavior (switch active thread) is unchanged.
- Conversation renderer path that takes an explicit `thread_id` binding per pane instead of resolving through global `ConversationFocus`. The main pane path stays exactly as today.
- Extra thread panes are read-only in this slice (no composer). The composer stays bound to the main/active-thread conversation. This is the deliberate first-slice decision; revisit per-pane composers at closeout.
- Idempotent open, close-tab behavior, and live streaming into non-focused thread panes.
- Layout save/load: thread-bound panes persist with kind + target and come back after restart (rendering history from the persisted transcript; empty view acceptable if no events yet).

## Out of scope

- Per-pane composers / sending input from a non-active thread pane.
- Moving or cross-referencing messages between threads.
- Any change to `TranscriptRegistry` routing, thread-root rebinding, or the subagent dock region.
- Multi-window support; floating panes already work via standard egui_dock drag.

## Verification

- New headless tests in `crates/gui/tests/` (e.g. `thread_panes_headless.rs`): idempotent open, no-retarget on active-thread switch, live delta routed into the bound pane, archived-thread pane renders persisted transcript, layout roundtrip preserves binding.
- Existing suites must stay green: `cargo test -p gui`, `cargo test -p workspace-ui`, dock/layout canaries (`dock_roundtrip`, `layout_v02`, `terminal_default_layout`, `right_panes_headless`, `approvals_layout_headless`).
- `cargo build --workspace`, `cargo fmt --all --check`, `git diff --check`.

## Knowledge Maintenance (G461)

- Intent placement: `intents/evorch/features/gui-workbench/overview.md` (no new node).
- ADR candidate: none — view-layer binding is a local design choice, not an architecture decision.
- Diagram candidate: none.
- Docs update: `intents/evorch/features/gui-workbench/overview.md` gains the per-pane thread-binding contract and the ctrl+click affordance.
- Closeout writeback (required): record whether read-only extra panes suffice and any routing assumption that broke under concurrent rendering (see packet.yaml `closeout_learning`).
- Guide reachability (G645): route declared in packet.yaml — implementation-loop → thread-bound conversation panes affordance.
