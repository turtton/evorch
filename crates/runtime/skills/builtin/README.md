# Builtin skills

Builtin skills are compiled into the runtime binary. They are the lowest-priority
fallback: repo `.evorch/skills` → repo `.agents/skills` → user skills → builtin.
A filesystem skill with the same name overrides its builtin counterpart.

## Available skills

- `git-best-practices`: consult before Git work or delegation. Prefer the target
  repository's commit-message and branch-naming rules over generic conventions;
  protect existing work, review staged changes, and verify the published commit.
  The skill content is repository- and agent-runtime-independent; integration
  instructions below are documentation for this runtime, not part of the skill.

Discovery exposes only the name and description. A model can request the body
with `skill_load({"name":"git-best-practices"})`; an Orchestrator can provide it
to a Worker with `delegate(..., load_skills=["git-best-practices"])`.
This is guidance, not a commit hook, mandatory automatic loading, or permission
to execute Git operations.

## Adding a skill

1. Add `<name>/SKILL.md` with valid Agent Skills frontmatter. The `name` must match
   the directory. Keep the description explicit about when to consult the skill.
2. Register it in `src/skill/builtin.rs` using `include_str!`. Metadata and the body
   are derived from this single source; do not duplicate the description in Rust.
3. Register any bundled UTF-8 resources using safe relative keys (`file` or
   `dir/file`) and `include_str!` in the resource table.
4. Add tests for discovery, body loading, filesystem overrides, and relevant
   runtime/cache contracts. Rebuild to include the new content.

Filesystem additions are re-discovered at run boundaries. Builtin changes require
an updated binary; they are not loaded from the source tree at runtime.
