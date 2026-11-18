# Pre-output-attachments tool fixture

`predecessor-output-attachment-tools.json` is the actual `delegate` and `run_output` ToolSpecs returned by `meta::specs::tool_spec` at commit `6d8f021d88be352b2d8faacd4ef800f879eac999`. It was exported before changing either spec using a temporary unit test serializing `[tool_spec("delegate"), tool_spec("run_output")]` via `serde_json::to_vec_pretty`. No fields were reconstructed from the successor schema.
