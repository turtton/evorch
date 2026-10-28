//! The `run_js` tool description: a fixed body plus the generated declarations.

use serde_json::Value;

use super::typings;

/// Description used before a run generates its own (and in direct tool tests).
pub const BASE_DESCRIPTION: &str = "Run JavaScript (ES2023, QuickJS) that calls your tools, and return only what you print or return.";

/// The sentence added to `shell` when `run_js` is offered.
pub const SHELL_HINT: &str = " For data processing or the same edit across many places, use run_js instead of a script inside shell.";

/// Builds the description for a run from its callable tools and the limits.
pub fn render(tools: &[(String, Value)], limits: &config::RunJsConfig) -> String {
    let seconds = limits.default_timeout_ms.div_ceil(1000);
    format!(
        "{BASE_DESCRIPTION}\n\
Use it to batch several reads/searches, filter or aggregate their output, or apply the same edit \
across many places. Do not use it for a single tool call.\n\
- Tool results stay out of your context unless you print or return them, so print only the essentials.\n\
- `tools.<name>(args)` returns Promise<{{ok, text, truncated, next, detail}}>; run independent calls with Promise.all. \
`readText(path)` returns a whole file.\n\
- Edits go through tools.edit / tools.write so they show as diffs.\n\
- A result with truncated=true holds only part of the output: follow `next` (or use readText) before counting or concluding.\n\
- ES2023 plus queueMicrotask, performance, atob/btoa, WeakRef, Set methods and Iterator helpers. Not available: fs, require, import, \
fetch, timers, TextEncoder, URL, structuredClone, Intl (localeCompare orders by code point).\n\
- A returned string is shown as is, anything else as JSON.\n\
- Calls needing approval or non-isolated shell fail here; make them as normal tool calls.\n\
- Run builds and long tests as normal shell jobs, not here.\n\
- Limits: {seconds} s (timeout_ms up to {max}), {memory} MiB, {calls} tool calls.\n\
Available tools:\n\
{declarations}\n\
Example: const hits = (await tools.grep({{pattern: \"TODO\", path: \"crates\"}})).text.split(\"\\n\"); ...",
        max = limits.max_timeout_ms,
        memory = limits.memory_limit_mib,
        calls = limits.max_tool_calls,
        declarations = typings::declarations(tools),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;

    fn tools() -> Vec<(String, Value)> {
        vec![
            ("read".to_owned(), crate::tools::Read.schema()),
            ("grep".to_owned(), crate::tools::Grep.schema()),
        ]
    }

    #[test]
    fn description_states_the_limits_and_the_declarations() {
        let text = render(&tools(), &config::RunJsConfig::default());
        assert!(text.starts_with(BASE_DESCRIPTION));
        assert!(text.contains("Limits: 30 s (timeout_ms up to 300000), 64 MiB, 200 tool calls."));
        assert!(text.contains("grep(args: { path: string; pattern: string })"));
        assert!(text.contains("Run builds and long tests as normal shell jobs"));
        assert!(text.contains("follow `next`"));
        assert!(text.contains("structuredClone"));
    }

    #[test]
    fn description_is_deterministic_and_follows_the_settings() {
        let settings = config::RunJsConfig {
            default_timeout_ms: 45_000,
            max_tool_calls: 10,
            ..config::RunJsConfig::default()
        };
        assert_eq!(render(&tools(), &settings), render(&tools(), &settings));
        assert!(render(&tools(), &settings).contains("45 s"));
        assert!(render(&tools(), &settings).contains("10 tool calls"));
    }
}
