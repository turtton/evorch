use std::sync::Arc;

use event_bus::{Event, EventKind, SnapshotEvent, WorkspaceLockHolder};
use secret_guard::SecretRedactor;
use serde_json::Value;

use super::LoopState;
use crate::snapshot::WorkspaceSnapshotGuard;

impl LoopState {
    pub(super) async fn snapshot_before_tool(
        &self,
        name: &str,
        call_id: &str,
        input: &Value,
    ) -> Result<Option<WorkspaceSnapshotGuard>, String> {
        if !self
            .shared
            .executor
            .tool_permissions(name)
            .is_some_and(|permissions| permissions.fs_write)
        {
            return Ok(None);
        }
        let Some(shared) = self.shared.runtime.upgrade() else {
            return Ok(None);
        };
        let Some(service) = shared.snapshots.get() else {
            return Ok(None);
        };
        let root = shared
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.task.run_id)
            .and_then(|workspace| workspace.worktree_path.clone());
        let mut workspace = service
            .lock_observed(
                root.as_deref(),
                WorkspaceLockHolder {
                    run_id: self.task.run_id.to_string(),
                    call_id: call_id.to_owned(),
                    tool_name: name.to_owned(),
                    command: command_preview(name, input),
                },
                Arc::clone(&self.shared.bus),
            )
            .await
            .map_err(|error| error.to_string())?;
        let owner = self
            .task
            .config
            .name
            .clone()
            .unwrap_or_else(|| self.task.run_id.to_string());
        let (workspace, snapshot) = tokio::task::spawn_blocking(move || {
            let snapshot = workspace.checkpoint(&owner);
            (workspace, snapshot)
        })
        .await
        .map_err(|error| error.to_string())?;
        let snapshot = snapshot.map_err(|error| error.to_string())?;
        self.shared
            .bus
            .emit(Event::new(EventKind::Snapshot(SnapshotEvent {
                run_id: self.task.run_id.to_string(),
                call_id: call_id.to_owned(),
                snapshot_id: snapshot.as_str().to_owned(),
                workspace_root: workspace.root().to_path_buf(),
            })));
        Ok(Some(workspace))
    }
}

const COMMAND_PREVIEW_CHARS: usize = 160;

fn command_preview(name: &str, input: &Value) -> Option<String> {
    if name != "shell" {
        return None;
    }
    let mut command = input.get("command")?.as_str()?.to_owned();
    // Match the shell's deprecated args fragments, which are appended verbatim.
    if let Some(args) = input.get("args").and_then(Value::as_array) {
        for arg in args.iter().filter_map(Value::as_str) {
            command.push(' ');
            command.push_str(arg);
        }
    }
    // Redact the full original first so a credential that crosses the preview
    // boundary cannot leave a partial secret in the UI or persisted events.
    let safe = SecretRedactor::from_env().redact(&command).text;
    let normalized = safe.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut chars = normalized.chars();
    let mut preview: String = chars.by_ref().take(COMMAND_PREVIEW_CHARS).collect();
    if chars.next().is_some() {
        preview.pop();
        preview.push('…');
    }
    Some(preview)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn command_preview_includes_shell_argument_fragments() {
        for (args, expected) in [
            (json!(["merge", "123"]), "gh pr merge 123"),
            (json!(["checks", "--watch"]), "gh pr checks --watch"),
        ] {
            assert_eq!(
                command_preview("shell", &json!({"command": "gh pr", "args": args})),
                Some(expected.to_owned()),
            );
        }
    }

    #[test]
    fn command_preview_is_bounded_single_line_and_redacts_before_truncation() {
        let prefix = format!("{}\n\tcurl -H", "あ".repeat(125));
        let auth = "'Authorization: sk-test-evorch-9f8e7d6c5b4a3f2e1d'";
        for input in [
            json!({"command": format!("{prefix} {auth}")}),
            json!({"command": prefix, "args": [auth]}),
        ] {
            let preview = command_preview("shell", &input).unwrap();
            assert!(preview.chars().count() <= COMMAND_PREVIEW_CHARS);
            assert!(!preview.contains(['\n', '\r', '\t']));
            assert!(!preview.contains("sk-test"));
            assert!(preview.ends_with('…'));
            assert!(command_preview("write", &input).is_none());
        }
    }
}
