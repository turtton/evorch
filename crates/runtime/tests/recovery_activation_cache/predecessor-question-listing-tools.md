# Pre-cancelled-question tool fixture

`predecessor-question-listing-tools.json` holds the `user_answers` and `subagent_questions` ToolSpecs as `meta::specs::tool_spec` returned them at `origin/main` 1d4a1f8f (before the offline `cleanup-archive-blockers` change). Only the two descriptions differ from the successor; the input schemas are identical and are asserted equal by the test. The description strings are copied verbatim from that commit's `crates/runtime/src/meta/specs.rs`.
