use event_bus::{AgentRunPhase, EscalationMemoSummary, Event, EventKind, LifecycleEvent};
use workspace_ui::{PanelId, ThreadId, ThreadRecord};

use super::WorkbenchState;
use crate::model::{tasks::AgentRunSource, transcript::TranscriptEntry};

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn bind_escalation_thread(
        &mut self,
        source_run_id: &str,
        new_run_id: &str,
        summary: &EscalationMemoSummary,
    ) -> bool {
        let Some(parent) = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.run_ids.iter().any(|run| run == source_run_id))
            .cloned()
        else {
            return false;
        };
        let id = ThreadId::new(format!("escalation-{new_run_id}"));
        let created = !self.sidebar.threads.iter().any(|thread| thread.id == id);
        if created {
            let mut child = ThreadRecord::new(
                id.clone(),
                parent.project_id,
                format!("Orchestrator · {}", parent.title),
            );
            child.parent_thread_id = Some(parent.id.clone());
            child.escalation_source_run_id = Some(source_run_id.into());
            self.sidebar.threads.push(child);
        }
        let thread_id = id.to_string();
        let adopted = !self.transcripts.is_thread_root(new_run_id);
        let has_output = self
            .transcripts
            .run(new_run_id)
            .is_some_and(|run| !run.entries().is_empty());
        self.transcripts.adopt_thread_root(&thread_id, new_run_id);
        let bound = self.bind_thread_run(&thread_id, new_run_id);
        if adopted {
            self.transcripts.push_to_thread(
                &parent.id.to_string(),
                TranscriptEntry::Notice {
                    text: "Orchestrator thread を開始しました。会話上部の子 thread から開けます。"
                        .into(),
                },
            );
            // Do not insert a notice between chunks of an already-started stream.
            if !has_output {
                self.transcripts.push_to_thread(
                    &thread_id,
                    TranscriptEntry::Notice {
                        text: format!(
                            "{} からの escalation: {}\n{}",
                            parent.title, summary.escalation_reason, summary.original_request
                        ),
                    },
                );
            }
        }
        created || bound
    }

    pub(super) fn project_escalation_result(&mut self, event: &Event) {
        let EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) =
            &event.kind
        else {
            return;
        };
        let status = match to {
            AgentRunPhase::Done => "完了しました",
            AgentRunPhase::Error => "エラーで停止しました",
            _ => return,
        };
        if !self.transcripts.is_thread_root(run_id) {
            return;
        }
        let Some(thread) = self.sidebar.threads.iter().find(|thread| {
            thread.escalation_source_run_id.is_some() && thread.run_ids.contains(run_id)
        }) else {
            return;
        };
        if let Some(parent) = &thread.parent_thread_id {
            let result = (to == &AgentRunPhase::Done)
                .then(|| {
                    self.transcripts.run(run_id).and_then(|transcript| {
                        transcript
                            .entries()
                            .iter()
                            .rev()
                            .find_map(|entry| match entry {
                                TranscriptEntry::Message { text, .. }
                                    if !text.trim().is_empty() =>
                                {
                                    Some(text.trim())
                                }
                                _ => None,
                            })
                    })
                })
                .flatten()
                .map(|text| {
                    let preview: String = text.chars().take(500).collect();
                    if text.chars().count() > 500 {
                        format!("{preview}…")
                    } else {
                        preview
                    }
                });
            let mut text = format!("{} が{status}。", thread.title);
            if let Some(result) = result {
                text.push_str(&format!(" 結果: {result}"));
            }
            text.push_str(" 詳細の確認・継続依頼は子 thread を開いてください。");
            self.transcripts
                .push_to_thread(&parent.to_string(), TranscriptEntry::Notice { text });
        }
    }

    pub(super) fn prepare_escalation_thread(&mut self, source_run_id: &str, run_id: &str) {
        // Live handoff follows the source window's ownership. History replay
        // projects conversations without acquiring any execution authority.
        if let (Some(host), Some(parent), Some(child)) = (
            &self.ownership,
            self.thread_for_run(source_run_id),
            self.thread_for_run(run_id),
        ) && !self.readonly_threads.contains(&parent)
            && host.owned_permit(&parent).is_ok()
            && let Err(error) =
                crate::runtime_sink::finish_chat_start(host, &child, host.start(&child))
        {
            self.transcripts.push_to_thread(
                &child,
                TranscriptEntry::Notice {
                    text: format!("write mode を取得できません: {error}"),
                },
            );
        }
        let id = PanelId::new(format!("agent-{run_id}"));
        if let Some(path) = self.dock.find_tab(&id) {
            self.dock.remove_tab(path);
            self.panels.remove(&id);
            self.equalize_subagent_panes();
        }
    }
}
