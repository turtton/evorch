//! supervisor が起動する orchestrator run 用プロンプト。

use std::fmt::Write;

use event_bus::{AgentMessage, GateRejection, GoalStage};

use super::ledger::GoalSnapshot;

/// idle continuation の構造化プロンプトを生成する。
pub fn render_continuation_prompt(
    snapshot: &GoalSnapshot,
    unmet: &[GateRejection],
    review_history: &[String],
    nudges: u32,
) -> String {
    let mut prompt = format!(
        "[evorch continuation epoch={} goal={}]\n\n## Goal\n{}\n\n## References\n",
        snapshot.epoch, snapshot.goal_id, snapshot.goal
    );
    append_lines(
        &mut prompt,
        snapshot
            .references
            .iter()
            .map(|reference| format!("- {}: {}", reference.kind, reference.value)),
    );
    prompt.push_str("\n## Constraints\n");
    append_lines(
        &mut prompt,
        snapshot
            .constraints
            .iter()
            .map(|value| format!("- {value}")),
    );
    let _ = write!(
        prompt,
        "\n## Deliverable branch\n{}\n\n## Unmet gate conditions\n",
        snapshot
            .deliverable_branch
            .as_deref()
            .unwrap_or("not bound")
    );
    append_lines(&mut prompt, unmet.iter().map(|item| format!("- {item:?}")));
    prompt.push_str("\n## Review findings so far\n");
    append_lines(
        &mut prompt,
        review_history.iter().map(|item| format!("- {item}")),
    );
    let next = if snapshot.deliverable_branch.is_none() {
        "delegate a worker with `workspace_mode: \"isolated\"`"
    } else if snapshot.stage == GoalStage::ReadyToFinish {
        "call `finish`"
    } else {
        "wait; evidence is being collected"
    };
    let _ = write!(
        prompt,
        "\n## Nudges sent\n{nudges}\n\n## Next action\n{next}\n"
    );
    prompt
}

/// recovery 用に直近 transcript を 32 KiB 以内で追記する。
pub fn render_recovery_prompt(snapshot: &GoalSnapshot, transcript: &[AgentMessage]) -> String {
    let mut prompt = render_continuation_prompt(snapshot, &snapshot.last_rejections, &[], 0);
    let rendered = transcript
        .iter()
        .map(|message| {
            format!(
                "{} -> {}: {}",
                message.sender_run_id, message.recipient_run_id, message.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let start = rendered.len().saturating_sub(32 * 1024);
    let boundary = rendered
        .char_indices()
        .find_map(|(index, _)| (index >= start).then_some(index))
        .unwrap_or(rendered.len());
    prompt.push_str("\n## Prior transcript\n");
    prompt.push_str(&rendered[boundary..]);
    prompt
}

fn append_lines(output: &mut String, lines: impl Iterator<Item = String>) {
    for line in lines {
        output.push_str(&line);
        output.push('\n');
    }
}
