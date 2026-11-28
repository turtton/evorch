//! Samples of the operations the `workspace` area adds beyond those in the parent module.
use super::AreaSamples;
use workbench_protocol::commands::{Command, ThreadTarget};
use workbench_protocol::events::UiEvent;
use workbench_protocol::ids::{ProjectId, ThreadId};
use workbench_protocol::namespaces::workspace::*;
use workbench_protocol::namespaces::{WorkspaceEvent, WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::{PageRequest, Query, QueryReply};
use workbench_protocol::views::Page;
use workbench_protocol::wire::Revision;

fn project() -> ProjectId {
    ProjectId::new("proj-1").unwrap()
}

fn thread() -> ThreadId {
    ThreadId::new("thr-1").unwrap()
}

fn page_request() -> PageRequest {
    PageRequest {
        cursor: None,
        limit: 50,
    }
}

pub fn samples() -> AreaSamples {
    let ws = Command::Workspace;
    AreaSamples {
        name: "workspace",
        commands: vec![
            ws(WorkspaceCommand::RegisterProject(RegisterProject {
                name: "evorch".into(),
                path: "/work/evorch".into(),
            })),
            ws(WorkspaceCommand::RenameProject(RenameProject {
                project: project(),
                name: "evorch core".into(),
                expected_revision: Revision(7),
            })),
            ws(WorkspaceCommand::SetProjectTrust(SetProjectTrust {
                project: project(),
                trust: TrustLevel::Approved,
                expected_revision: Revision(7),
            })),
            ws(WorkspaceCommand::UpdateAllowedDirectory(
                UpdateAllowedDirectory {
                    project: project(),
                    action: AllowedDirectoryAction::Add {
                        path: "/work/shared".into(),
                        trust: TrustLevel::Approved,
                    },
                    expected_revision: Revision(7),
                },
            )),
            ws(WorkspaceCommand::SetThreadModelChoice(
                SetThreadModelChoice {
                    thread: thread(),
                    expected_revision: Revision(3),
                    choice: workbench_protocol::commands::ModelChoice::RoleDefault,
                },
            )),
            ws(WorkspaceCommand::RenameThread(RenameThread {
                thread: thread(),
                title: "Plan".into(),
                expected_revision: Revision(3),
            })),
            ws(WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
                thread: thread(),
                archived: true,
                expected_family: FamilyIdentity {
                    key: thread(),
                    membership_epoch: 1,
                },
                expected_revision: Revision(2),
            })),
            ws(WorkspaceCommand::SetFamilyPinned(SetFamilyPinned {
                thread: thread(),
                pinned: true,
                expected_family: FamilyIdentity {
                    key: thread(),
                    membership_epoch: 1,
                },
                expected_revision: Revision(0),
            })),
            ws(WorkspaceCommand::SetThreadExtraReadDirs(
                SetThreadExtraReadDirs {
                    thread: thread(),
                    dirs: vec!["/work/reference".into()],
                    expected_revision: Revision(3),
                },
            )),
        ],
        queries: vec![
            Query::Workspace(WorkspaceQuery::GetProjectAccess(ProjectTarget {
                project: project(),
            })),
            Query::Workspace(WorkspaceQuery::ListFamilies(page_request())),
            Query::Workspace(WorkspaceQuery::GetThreadDetail(ThreadTarget {
                thread: thread(),
            })),
            Query::Workspace(WorkspaceQuery::GetRetention(page_request())),
            Query::Workspace(WorkspaceQuery::ListSkills(ProjectTarget {
                project: project(),
            })),
            Query::Workspace(WorkspaceQuery::SearchThreads(SearchThreads {
                project: project(),
                query: "migration plan".into(),
                cursor: Some(ThreadSearchCursor::new("host:titles:20").unwrap()),
            })),
        ],
        ui_events: vec![UiEvent::Workspace(WorkspaceEvent::WorkspaceChanged {
            revision: Revision(11),
        })],
        query_replies: vec![
            QueryReply::Workspace(WorkspaceReply::ProjectAccess(Box::new(ProjectAccessView {
                project: project(),
                name: "evorch".into(),
                root: "/work/evorch".into(),
                trust: TrustLevel::Approved,
                allowed_directories: vec![AllowedDirectoryView {
                    path: "/work/shared".into(),
                    trust: TrustLevel::Approved,
                    live: false,
                }],
                revision: Revision(7),
            }))),
            QueryReply::Workspace(WorkspaceReply::Families(Page {
                items: vec![FamilyView {
                    key: thread(),
                    membership_epoch: 1,
                    members: 2,
                    archived: true,
                    archived_at: Some(1_700_000_000),
                    archive_revision: Revision(2),
                    pinned: false,
                    pin_revision: Revision(0),
                }],
                next_cursor: None,
                snapshot_revision: Revision(5),
            })),
            QueryReply::Workspace(WorkspaceReply::ThreadDetail(Box::new(ThreadDetail {
                thread: thread(),
                project: project(),
                title: "Plan".into(),
                family: Some(thread()),
                parent_thread: None,
                lineage: Some(LineageType::Fork),
                root_run: None,
                active_root: Some("/work/evorch".into()),
                model_choice: workbench_protocol::commands::ModelChoice::Explicit(
                    workbench_protocol::commands::ModelPreference {
                        profile: "default".into(),
                        model: None,
                        reasoning_effort: None,
                    },
                ),
                chat_role: Some("worker".into()),
                extra_read_dirs: vec![],
                archived: false,
                pinned: true,
                revision: Revision(3),
            }))),
            QueryReply::Workspace(WorkspaceReply::Retention(Page {
                items: vec![
                    RetentionEntry {
                        thread: thread(),
                        state: RetentionState::DueIn { days: 3 },
                    },
                    RetentionEntry {
                        thread: thread(),
                        state: RetentionState::Blocked {
                            block: RetentionBlock::Recovery {
                                reason: "member of a committed recovery manifest".into(),
                            },
                        },
                    },
                    RetentionEntry {
                        thread: thread(),
                        state: RetentionState::Blocked {
                            block: RetentionBlock::SafetyHold {
                                reason: "a runtime-use gate holds the thread".into(),
                            },
                        },
                    },
                ],
                next_cursor: None,
                snapshot_revision: Revision(5),
            })),
            QueryReply::Workspace(WorkspaceReply::Skills(vec![SkillSummary {
                name: "review".into(),
                description: "Review a change".into(),
            }])),
            QueryReply::Workspace(WorkspaceReply::ThreadSearchResults(ThreadSearchResults {
                hits: vec![ThreadSearchHit {
                    thread: thread(),
                    title: Some("Plan the migration".into()),
                    snippet_kind: Some("user".into()),
                    snippet: Some("...the migration plan needs a rollback...".into()),
                    archived: false,
                }],
                next_cursor: Some(ThreadSearchCursor::new("host:body-start").unwrap()),
                index: Some(ThreadSearchIndex {
                    enabled: Some(true),
                    unindexed: 2,
                    backfill_in_progress: true,
                }),
                title_scan_limited: false,
            })),
        ],
    }
}
