# Pre-multi-target lane_send tool fixture

`predecessor-lane-send-tools.json` holds the `lane_send` ToolSpec exactly as `meta::specs::tool_spec("lane_send")` returned it at `origin/main` 028a5331 (before `lane_send` took a `targets` array). It was dumped from that code, not written by hand. The successor changes only this tool's description and input schema; the test asserts that no other tool or tool order differs.

The pre-change call stored in the history (flat `lane_id` + revisions) is rejected by the current handler. That fixture proves the sent arguments and the error result stay byte-for-byte in the prefix; it does not prove old handler execution compatibility.
