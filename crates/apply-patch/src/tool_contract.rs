//! The model-facing definition of the `apply_patch` tool.
//!
//! [`NAME`], [`FREEFORM_DESCRIPTION`] and [`LARK_GRAMMAR`] match Codex byte for
//! byte. [`JSON_DESCRIPTION`] and [`json_schema`] describe the same tool as a
//! JSON function whose `input` argument holds the patch. Tool definitions are
//! part of the prompt cache prefix, so these values must stay constant.

use serde_json::Value;
use serde_json::json;

pub const NAME: &str = "apply_patch";

/// The description Codex sends with the freeform (grammar) variant.
pub const FREEFORM_DESCRIPTION: &str = "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON.";

/// The description for the JSON function variant. The text after `Example:`
/// must stay a valid patch; a test parses it.
pub const JSON_DESCRIPTION: &str = r#"Edit files by applying a patch. Put the entire patch, from `*** Begin Patch` to `*** End Patch`, in `input`.

Between the `*** Begin Patch` and `*** End Patch` lines, the patch lists file operations:
- `*** Add File: <path>` creates a file. Every line of the new file follows, each starting with `+`.
- `*** Delete File: <path>` deletes a file. No lines follow it.
- `*** Update File: <path>` edits a file. An optional `*** Move to: <new path>` line right after it renames the file. One or more hunks follow.

A hunk starts with `@@`, or with `@@ <context>` where <context> is a line of the file, such as a class or function header, that locates the change. The first hunk of a file may omit the `@@` line. Every other hunk line starts with one character:
- a space for an unchanged context line,
- `-` for a removed line,
- `+` for an added line.
Show about 3 unchanged lines before and after each change, and enough context to make the location unique. Put `*** End of File` after a hunk that must match the end of the file.

Paths may be relative to the working directory or absolute. They must not contain `..`.

Example:
*** Begin Patch
*** Add File: docs/hello.txt
+Hello, world!
*** Update File: src/app.py
@@ def greet():
-    print("Hi")
+    print("Hello")
*** Update File: src/old_name.py
*** Move to: src/new_name.py
@@
-OLD = 1
+NEW = 1
*** Delete File: obsolete.txt
*** End Patch"#;

/// The Lark grammar Codex sends with the freeform variant, copied verbatim from
/// `codex-rs/core/assets/tools/apply_patch.lark`.
pub const LARK_GRAMMAR: &str = include_str!("../assets/apply_patch.lark");

/// The parameters of the JSON function variant.
pub fn json_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "input": {
                "type": "string",
                "description": "The entire patch, from '*** Begin Patch' to '*** End Patch'.",
            },
        },
        "required": ["input"],
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Hunk;
    use crate::parse_patch;

    #[test]
    fn name_is_apply_patch() {
        assert_eq!(NAME, "apply_patch");
    }

    #[test]
    fn freeform_description_matches_codex() {
        assert_eq!(
            FREEFORM_DESCRIPTION,
            "The `apply_patch` tool can be used to edit files. This is a FREEFORM tool, so do not wrap the patch in JSON."
        );
    }

    #[test]
    fn lark_grammar_matches_codex() {
        let expected = concat!(
            "start: begin_patch hunk+ end_patch\n",
            "begin_patch: \"*** Begin Patch\" LF\n",
            "end_patch: \"*** End Patch\" LF?\n",
            "\n",
            "hunk: add_hunk | delete_hunk | update_hunk\n",
            "add_hunk: \"*** Add File: \" filename LF add_line+\n",
            "delete_hunk: \"*** Delete File: \" filename LF\n",
            "update_hunk: \"*** Update File: \" filename LF change_move? change?\n",
            "\n",
            "filename: /(.+)/\n",
            "add_line: \"+\" /(.*)/ LF -> line\n",
            "\n",
            "change_move: \"*** Move to: \" filename LF\n",
            "change: (change_context | change_line)+ eof_line?\n",
            "change_context: (\"@@\" | \"@@ \" /(.+)/) LF\n",
            "change_line: (\"+\" | \"-\" | \" \") /(.*)/ LF\n",
            "eof_line: \"*** End of File\" LF\n",
            "\n",
            "%import common.LF\n",
        );
        assert_eq!(LARK_GRAMMAR, expected);
    }

    #[test]
    fn json_schema_requires_only_the_patch_input() {
        let expected: Value = serde_json::from_str(
            r#"{"type":"object","properties":{"input":{"type":"string","description":"The entire patch, from '*** Begin Patch' to '*** End Patch'."}},"required":["input"],"additionalProperties":false}"#,
        )
        .unwrap();
        assert_eq!(json_schema(), expected);
    }

    #[test]
    fn json_description_covers_the_grammar() {
        for needle in [
            "`input`",
            "*** Begin Patch",
            "*** End Patch",
            "*** Add File: ",
            "*** Delete File: ",
            "*** Update File: ",
            "*** Move to: ",
            "@@",
            "*** End of File",
            "`+`",
            "`-`",
            "space",
        ] {
            assert!(
                JSON_DESCRIPTION.contains(needle),
                "JSON_DESCRIPTION should mention {needle:?}"
            );
        }
    }

    #[test]
    fn json_description_example_is_a_valid_patch() {
        let (_, example) = JSON_DESCRIPTION
            .split_once("Example:\n")
            .expect("JSON_DESCRIPTION should end with an example patch");
        let hunks = parse_patch(example).unwrap().hunks;
        assert_eq!(
            hunks
                .iter()
                .map(|hunk| match hunk {
                    Hunk::AddFile { .. } => "add",
                    Hunk::DeleteFile { .. } => "delete",
                    Hunk::UpdateFile {
                        move_path: Some(_), ..
                    } => "move-update",
                    Hunk::UpdateFile {
                        move_path: None, ..
                    } => "update",
                })
                .collect::<Vec<_>>(),
            vec!["add", "update", "move-update", "delete"]
        );
    }
}
