use workspace_ui::{LineageKind, ProjectId, SidebarState, ThreadId, UiSettings};

use super::WorkbenchState;
use crate::fixture::DemoSource;
use crate::model::commands::{PullRequestDerivation, RecordingSink, WorkbenchCommand};

fn state(directory: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", directory)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("parent"), project, "Parent")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("parent")).unwrap();
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_command_sink(Box::new(RecordingSink::default()))
}

fn inherits(state: &WorkbenchState<DemoSource>) -> Vec<(String, String, PullRequestDerivation)> {
    state
        .issued()
        .iter()
        .filter_map(|command| match command {
            WorkbenchCommand::InheritPullRequest {
                parent_thread_id,
                child_thread_id,
                kind,
            } => Some((parent_thread_id.clone(), child_thread_id.clone(), *kind)),
            _ => None,
        })
        .collect()
}

// Given: a thread that never ran / When: it is forked and rewound / Then: each derived thread is
// announced to the pull request service with how it was derived, where it is created
#[test]
fn a_fork_and_a_rewind_announce_their_inheritance_before_any_run() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = state(directory.path());

    let fork = state.fork_thread(ThreadId::new("parent")).unwrap();
    let rewind = state
        .branch_thread(&ThreadId::new("parent"), None, LineageKind::Rewind)
        .unwrap();

    assert_eq!(
        inherits(&state),
        [
            (
                "parent".to_owned(),
                fork.to_string(),
                PullRequestDerivation::Fork
            ),
            (
                "parent".to_owned(),
                rewind.to_string(),
                PullRequestDerivation::Rewind
            ),
        ]
    );
}

// Given: a conversation another process branched / When: this window imports it / Then: the
// import is display only and announces nothing
#[test]
fn an_imported_branch_announces_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = state(directory.path());
    let fork = state.fork_thread(ThreadId::new("parent")).unwrap();
    let before = inherits(&state).len();

    state.seed_imported_branch(&fork.to_string());

    assert_eq!(inherits(&state).len(), before);
}

// Given: a thread this window attached read-only / When: any pull request operation is asked for,
// Ignore included / Then: none reaches the sink, and the person is told why
#[test]
fn a_thread_attached_read_only_refuses_every_operation() {
    use crate::model::pull_requests::PullRequestAction;
    let directory = tempfile::tempdir().unwrap();
    let mut state = state(directory.path());
    state.readonly_threads.insert("parent".to_owned());

    for action in [
        PullRequestAction::Refresh {
            thread_id: "parent".into(),
        },
        PullRequestAction::Confirm {
            thread_id: "parent".into(),
            candidate_id: 1,
        },
        PullRequestAction::Dismiss {
            thread_id: "parent".into(),
            candidate_id: 1,
        },
    ] {
        state.apply_pull_request_action(action);
    }

    assert!(
        state.issued().is_empty(),
        "an operation got past the read-only attach: {:?}",
        state.issued()
    );
    let refused = state
        .transcript()
        .entries()
        .iter()
        .filter(|entry| {
            matches!(entry, crate::model::transcript::TranscriptEntry::Notice { text }
                if text == "Read-only attach: explicitly Start or Claim before mutating.")
        })
        .count();
    assert_eq!(refused, 3);
}
