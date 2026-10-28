//! The `run_js` tool description: a fixed body plus the generated declarations.

use serde_json::Value;

use super::typings;

/// Description used before a run generates its own (and in direct tool tests).
pub const BASE_DESCRIPTION: &str = "Run JavaScript (ES2023, QuickJS) that calls your tools, and return only what you print or return.";

/// Added to `shell` when `run_js` is offered.
pub const SHELL_HINT: &str = " Before writing a `python3 - <<EOF` or `node -e` script, a `for ... done` loop or `xargs sed -i` to aggregate output, read many files or change many places, use run_js instead.";

/// Added to `edit` when `run_js` is offered.
pub const EDIT_HINT: &str = " To apply the same edit in many files, use run_js with tools.edit instead of looping `sed -i` in shell.";

/// The examples in the description. Each is the body of a program, and the tests run them.
pub const EXAMPLES: [&str; 2] = [
    "const paths = [\"a.rs\", \"b.rs\", \"c.rs\"];\n\
const texts = await Promise.all(paths.map((p) => readText(p)));\n\
return paths.filter((p, i) => texts[i].includes(\"unsafe\")).join(\"\\n\");",
    "const out = [];\n\
for (const p of [\"a.rs\", \"b.rs\", \"c.rs\"]) {\n\
  const t = await readText(p);\n\
  if (!t.includes(\"OldName\")) continue;\n\
  const r = await tools.edit({ path: p, old_string: \"OldName\", new_string: \"NewName\", replace_all: true });\n\
  out.push(p + \": \" + r.ok);\n\
}\n\
return out.join(\"\\n\");",
];

/// Builds the description for a run from its callable tools and the limits.
pub fn render(tools: &[(String, Value)], limits: &config::RunJsConfig) -> String {
    let seconds = limits.default_timeout_ms.div_ceil(1000);
    format!(
        "{BASE_DESCRIPTION}\n\
Use it instead of a python/node heredoc, a `for` loop or `xargs sed -i` in shell when you need to aggregate or transform output, \
read or write several files, or apply the same edit in many places. \
Do not use it for a single tool call, and do not make it a thin wrapper around shell: \
if the program would only return one long shell command's output, call shell directly.\n\
- Tool results stay out of your context unless you print or return them, so print only the essentials.\n\
- `tools.<name>(args)` returns Promise<{{ok, text, truncated, next, detail}}>; run independent calls with Promise.all. \
A failed call resolves with ok=false and the message in text: check `.ok`. `readText(path)` returns a whole file.\n\
- Edits go through tools.edit / tools.write so they show as diffs. The text of edit and write is a one-line summary (`edited <path> (+N −M)`): \
check `.ok` instead of printing it. The full diff is in `detail.diff`; git_diff's text is a per-file summary and its diff is in `detail.diff` too.\n\
- Output up to 1 MiB comes back whole. With truncated=true you have only part of it: follow `next` (`readText(next.path)`) before counting or concluding.\n\
- ES2023 plus queueMicrotask, performance, atob/btoa, WeakRef, Set methods and Iterator helpers. Not available: fs, require, import, \
fetch, timers, TextEncoder, URL, structuredClone, Intl (localeCompare orders by code point).\n\
- A returned string is shown as is, anything else as JSON.\n\
- Calls needing approval or non-isolated shell fail here; make them as normal tool calls.\n\
- Run builds and long tests as normal shell jobs, not here.\n\
- Limits: {seconds} s (timeout_ms up to {max}), {memory} MiB, {calls} tool calls.\n\
Available tools:\n\
{declarations}\n\
Example (read several files, return only a summary):\n\
{first}\n\
Example (the same edit in several files):\n\
{second}",
        max = limits.max_timeout_ms,
        memory = limits.memory_limit_mib,
        calls = limits.max_tool_calls,
        declarations = typings::declarations(tools),
        first = EXAMPLES[0],
        second = EXAMPLES[1],
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
    fn description_steers_shell_scripts_here_and_shows_both_examples() {
        let text = render(&tools(), &config::RunJsConfig::default());
        assert!(text.contains("instead of a python/node heredoc"));
        assert!(text.contains("thin wrapper around shell"));
        assert!(text.contains("check `.ok` instead of printing it"));
        assert!(text.contains(EXAMPLES[0]));
        assert!(text.contains(EXAMPLES[1]));
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
