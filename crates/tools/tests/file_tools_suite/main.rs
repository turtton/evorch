//! Tools contracts for file reading, editing, patching and post-edit checks.
//!
//! One integration-test binary for the former per-file targets below; each
//! test is now named `<old_file>::<test>`.

mod apply_patch_boundary;
mod apply_patch_post_edit;
mod apply_patch_reserved_name;
mod apply_patch_tool;
mod edit_aliases;
mod edit_tool;
mod grep_tool;
mod post_edit;
mod post_edit_executor;
mod read_aliases;
mod read_tool;
mod workspace_boundary;
mod write_tool;
