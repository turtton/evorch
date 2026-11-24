//! What a composer does with typed text before anything is sent: slash
//! commands, `@` mentions of files and folders, and accepting a completion.
//! Pure and egui-free, so the embedded workbench (`model::composer`) and the
//! core client share one implementation; neither the runtime nor the storage
//! is reached from here.

pub mod mentions;
pub mod slash;

pub use mentions::{CompletionItem, CompletionKind, MentionIndex, MentionQuery, mention_at};

/// The `@token` under `cursor`, a char index into `input`; without a cursor
/// (or one past the end) the end of the input.
pub fn mention_query(input: &str, cursor: Option<usize>) -> Option<MentionQuery<'_>> {
    let cursor = cursor
        .and_then(|chars| input.char_indices().nth(chars).map(|(byte, _)| byte))
        .unwrap_or(input.len());
    mention_at(input, cursor)
}

/// Replaces `item.range` of `input` with its replacement and returns the char
/// index just after it, where the cursor goes; `None` when the range no longer
/// fits the input, which is then left as it was.
pub fn apply_completion(input: &mut String, item: &CompletionItem) -> Option<usize> {
    let range = input.get(item.range.clone()).map(|_| item.range.clone())?;
    input.replace_range(range.clone(), &item.replacement);
    Some(
        input[..range.start + item.replacement.len()]
            .chars()
            .count(),
    )
}

/// The completion state of one composer editor, kept between frames.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompletionState {
    /// Highlighted row, clamped to the visible candidates.
    pub selected: usize,
    /// Esc hid the candidates of exactly this input.
    pub dismissed_for: Option<String>,
    /// Text cursor as a char index, mirrored from the editor each frame.
    pub cursor: Option<usize>,
    /// Char index the editor moves its cursor to on the next frame.
    pub pending_cursor: Option<usize>,
    pub focus_requested: bool,
    /// Whether the editor held keyboard focus at the end of the last frame.
    pub focused: bool,
    /// Tab presses (`true` with Shift) captured before egui focus navigation.
    pub tab_presses: Vec<bool>,
}

impl CompletionState {
    /// Slash commands among `offered` for a leading `/word`, otherwise the
    /// `@` mentions `index` completes at the cursor.
    pub fn items(
        &self,
        input: &str,
        offered: &[&str],
        index: &MentionIndex,
    ) -> Vec<CompletionItem> {
        if self.dismissed_for.as_deref() == Some(input) {
            return Vec::new();
        }
        let commands: Vec<_> = slash::completions_among(input, Some(offered))
            .into_iter()
            .map(|spec| slash::slash_item(spec, 0..input.len()))
            .collect();
        if !commands.is_empty() {
            return commands;
        }
        mention_query(input, self.cursor)
            .map(|mention| index.complete(&mention))
            .unwrap_or_default()
    }

    /// Applies an accepted `item` to `input`, moving the cursor after it and
    /// keeping the editor focused.
    pub fn accept(&mut self, input: &mut String, item: &CompletionItem) -> bool {
        let Some(cursor) = apply_completion(input, item) else {
            return false;
        };
        self.cursor = Some(cursor);
        self.pending_cursor = Some(cursor);
        self.selected = 0;
        self.focus_requested = true;
        true
    }

    pub fn dismiss(&mut self, input: &str) {
        self.dismissed_for = Some(input.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(files: &[&str]) -> MentionIndex {
        let files: Vec<_> = files.iter().map(|file| (*file).to_owned()).collect();
        MentionIndex::from_parts(None, &files, [])
    }

    #[test]
    fn slash_commands_win_over_mentions_and_esc_hides_them_for_that_input_only() {
        let mut state = CompletionState::default();
        let offered = ["goal", "new"];
        let labels = |state: &CompletionState, input: &str| -> Vec<String> {
            state
                .items(input, &offered, &index(&["README.md"]))
                .into_iter()
                .map(|item| item.label)
                .collect()
        };
        assert_eq!(labels(&state, "/"), ["/goal <text>", "/new"]);
        assert_eq!(labels(&state, "see @R"), ["@README.md"]);
        state.dismiss("/");
        assert!(labels(&state, "/").is_empty());
        assert_eq!(labels(&state, "/g"), ["/goal <text>"]);
    }

    #[test]
    fn accepting_replaces_the_token_and_moves_the_cursor_after_it() {
        let mut state = CompletionState {
            cursor: Some(6),
            selected: 1,
            ..CompletionState::default()
        };
        let mut input = "日本 @sr x".to_owned();
        let item = state
            .items(&input, &[], &index(&["src/lib.rs"]))
            .into_iter()
            .find(|item| item.kind == CompletionKind::Dir)
            .expect("the folder");
        assert!(state.accept(&mut input, &item));
        assert_eq!(input, "日本 @src/ x");
        assert_eq!(state.cursor, Some(8));
        assert_eq!(state.pending_cursor, Some(8));
        assert_eq!(state.selected, 0);
        assert!(state.focus_requested);
        let mut short = "x".to_owned();
        assert!(!state.accept(&mut short, &item));
        assert_eq!(short, "x");
    }

    #[test]
    fn index_within_lists_files_and_folders_without_skills() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/lib.rs"), "").unwrap();
        std::fs::write(root.path().join("README.md"), "").unwrap();
        let index = MentionIndex::index_within(root.path(), &[], [], false, 100);
        let entries: Vec<_> = index
            .entries
            .iter()
            .map(|entry| (entry.kind, entry.value.as_str()))
            .collect();
        assert_eq!(
            entries,
            [
                (CompletionKind::Dir, "src"),
                (CompletionKind::File, "README.md"),
                (CompletionKind::File, "src/lib.rs"),
            ]
        );
        assert!(index.is_built_for(root.path(), &[], false));
    }
}
