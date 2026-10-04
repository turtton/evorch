//! Turn forks and non-destructive rewinds. A rewind is a fork that replaces its
//! source as the visible version; every earlier version stays restorable.

use workspace_ui::{
    ForkPoint, LineageKind, ThreadError, ThreadId, ThreadLineage, ThreadRecord, ThreadRunPhase,
};

use super::{WorkbenchError, WorkbenchState};
use crate::model::tasks::AgentRunSource;
use crate::model::transcript::TranscriptModel;

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Why the thread cannot be rewound now, if it cannot.
    pub fn rewind_block(&self, thread: &ThreadRecord) -> Option<&'static str> {
        for run in &thread.run_ids {
            match self.phases.get(run) {
                Some(ThreadRunPhase::Running | ThreadRunPhase::Pending) => {
                    return Some("実行中は巻き戻せません");
                }
                // Mid-turn waits (subagents, replies) are not completed turns.
                Some(ThreadRunPhase::Waiting) if !self.idle_turns.contains(run) => {
                    return Some("ターンの途中では巻き戻せません");
                }
                _ => {}
            }
        }
        let thread_id = thread.id.to_string();
        if self.user_questions.values().any(|question| {
            question.answer.is_none()
                && super::questions::belongs_to_thread(question, &thread_id, &thread.run_ids)
        }) {
            return Some("未回答の質問があるため巻き戻せません");
        }
        None
    }

    /// Fork a new child thread holding history up to a completed turn.
    pub fn fork_at_turn(
        &mut self,
        source: ThreadId,
        entry_id: usize,
    ) -> Result<ThreadId, WorkbenchError> {
        let point = self
            .thread_transcript(&source)
            .and_then(|model| model.fork_point(entry_id))
            .ok_or(WorkbenchError::Branch("分岐できるターンが見つかりません"))?;
        self.branch_thread(&source, Some(point), LineageKind::Fork)
    }

    /// Show the conversation as it was after a completed turn. The current
    /// version is kept and can be shown again.
    pub fn rewind_to_turn(
        &mut self,
        thread: ThreadId,
        entry_id: usize,
    ) -> Result<ThreadId, WorkbenchError> {
        self.ensure_rewindable(&thread)?;
        let point = self
            .thread_transcript(&thread)
            .and_then(|model| model.fork_point(entry_id))
            .ok_or(WorkbenchError::Branch("巻き戻せるターンが見つかりません"))?;
        self.branch_thread(&thread, Some(point), LineageKind::Rewind)
    }

    /// Rewind to just before a turn's first message and return it to the composer.
    pub fn edit_from_message(
        &mut self,
        thread: ThreadId,
        entry_id: usize,
    ) -> Result<ThreadId, WorkbenchError> {
        self.ensure_rewindable(&thread)?;
        let (point, text) = self
            .thread_transcript(&thread)
            .and_then(|model| Some((model.edit_point(entry_id)?, model.user_text(entry_id)?)))
            .map(|(point, text)| (point, text.to_owned()))
            .ok_or(WorkbenchError::Branch("このメッセージからは編集できません"))?;
        let id = self.branch_thread(&thread, point, LineageKind::Rewind)?;
        self.composer.input = text;
        Ok(id)
    }

    /// Show another version of the active conversation.
    pub fn switch_version(&mut self, target: ThreadId) -> Result<(), WorkbenchError> {
        let thread = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id == target)
            .ok_or(ThreadError::UnknownThread)?;
        let carried = (thread.pinned, thread.archived);
        let current = workspace_ui::visible_version(&self.sidebar.threads, &target);
        let (pinned, archived) = current
            .as_ref()
            .and_then(|id| self.sidebar.threads.iter().find(|thread| &thread.id == id))
            .map_or(carried, |thread| (thread.pinned, thread.archived));
        workspace_ui::show_version(&mut self.sidebar.threads, &target);
        if let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id == target)
        {
            // The sidebar row keeps its state across versions.
            thread.pinned = pinned;
            thread.archived = archived;
        }
        self.switch_thread(target)?;
        self.save_sidebar();
        Ok(())
    }

    /// The forked thread's seed until it saves history of its own.
    pub(super) fn fork_seed(&self, thread_id: &ThreadId) -> Option<runtime::ChatForkSeed> {
        let point = self
            .sidebar
            .threads
            .iter()
            .find(|thread| &thread.id == thread_id)?
            .lineage
            .as_ref()?
            .point
            .as_ref()?;
        Some(runtime::ChatForkSeed {
            source_run_id: point.run_id.clone(),
            context_len: point.context_len,
        })
    }

    /// Replay: install inherited history for branches made at `point` once the
    /// owning conversation has reached it. `None` seeds branches made before any turn.
    pub(super) fn seed_branches(&mut self, point: Option<&ForkPoint>) {
        let owner = match point {
            Some(point) => match self.thread_for_run(&point.run_id) {
                Some(owner) => Some(owner),
                None => return,
            },
            None => None,
        };
        let branches: Vec<_> = self
            .sidebar
            .threads
            .iter()
            .filter_map(|thread| {
                let lineage = thread.lineage.as_ref()?;
                (lineage.point.as_ref() == point).then(|| {
                    (
                        thread.id.to_string(),
                        lineage.kind,
                        thread.parent_thread_id.as_ref().map(ToString::to_string),
                    )
                })
            })
            .collect();
        let empty = TranscriptModel::new();
        for (thread, kind, parent) in branches {
            let source = owner
                .as_deref()
                .and_then(|owner| self.transcripts.thread_model(owner))
                .unwrap_or(&empty);
            if let Some(model) =
                source.branch_at(point, kind, parent.as_deref().unwrap_or_default())
            {
                self.transcripts.insert_thread(&thread, model);
            }
        }
    }

    pub(super) fn thread_transcript(&self, thread: &ThreadId) -> Option<&TranscriptModel> {
        self.transcripts.thread_model(&thread.to_string())
    }

    fn ensure_rewindable(&self, thread: &ThreadId) -> Result<(), WorkbenchError> {
        let record = self
            .sidebar
            .threads
            .iter()
            .find(|candidate| &candidate.id == thread)
            .ok_or(ThreadError::UnknownThread)?;
        match self.rewind_block(record) {
            Some(reason) => Err(WorkbenchError::Branch(reason)),
            None => Ok(()),
        }
    }

    pub(super) fn branch_thread(
        &mut self,
        source: &ThreadId,
        point: Option<ForkPoint>,
        kind: LineageKind,
    ) -> Result<ThreadId, WorkbenchError> {
        let original = self
            .sidebar
            .threads
            .iter()
            .find(|thread| &thread.id == source)
            .cloned()
            .ok_or(ThreadError::UnknownThread)?;
        let empty = TranscriptModel::new();
        let transcript = self
            .thread_transcript(source)
            .unwrap_or(&empty)
            .branch_at(point.as_ref(), kind, &source.to_string())
            .ok_or(WorkbenchError::Branch("分岐点が履歴に見つかりません"))?;
        let id = self.branch_id(source, kind);
        let mut record = ThreadRecord {
            archived: original.archived,
            pinned: original.pinned,
            model_preference: original.model_preference.clone(),
            chat_role: original.chat_role,
            ..ThreadRecord::new(
                id.clone(),
                original.project_id.clone(),
                original.title.clone(),
            )
        };
        record.parent_thread_id = Some(source.clone());
        record.lineage = Some(ThreadLineage { kind, point });
        match kind {
            LineageKind::Fork => record.title = format!("{} (fork)", original.title),
            LineageKind::Rewind => {
                // A version replaces its source in place, including its sidebar position.
                record.created_at = original.created_at;
                if self.manually_titled.contains(source) {
                    self.manually_titled.insert(id.clone());
                }
            }
        }
        self.sidebar.threads.push(record);
        self.transcripts.insert_thread(&id.to_string(), transcript);
        if kind == LineageKind::Rewind {
            workspace_ui::show_version(&mut self.sidebar.threads, &id);
        }
        self.switch_thread(id.clone())?;
        self.save_sidebar();
        Ok(id)
    }

    fn branch_id(&self, source: &ThreadId, kind: LineageKind) -> ThreadId {
        let (base, label) = match kind {
            LineageKind::Fork => (source.clone(), "fork-"),
            LineageKind::Rewind => (
                workspace_ui::version_root(&self.sidebar.threads, source),
                "v",
            ),
        };
        (self.sidebar.threads.len() + 1..)
            .map(|n| ThreadId::new(format!("{base}-{label}{n}")))
            .find(|id| !self.sidebar.threads.iter().any(|thread| &thread.id == id))
            .expect("unbounded id search")
    }
}
