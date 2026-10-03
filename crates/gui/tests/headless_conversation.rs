//! Related GUI conversation contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_conversation/compaction_ledger.rs"]
mod compaction_ledger;
#[path = "headless_conversation/composer_tab.rs"]
mod composer_tab;
#[path = "headless_conversation/continuation_transcript_headless.rs"]
mod continuation_transcript_headless;
#[path = "headless_conversation/conversation_replay.rs"]
mod conversation_replay;
#[path = "headless_conversation/escalation_threads.rs"]
mod escalation_threads;
#[path = "headless_conversation/ledger.rs"]
mod ledger;
#[path = "headless_conversation/message_completion.rs"]
mod message_completion;
#[path = "headless_conversation/resume_ledger_routing.rs"]
mod resume_ledger_routing;
#[path = "headless_conversation/thinking_block.rs"]
mod thinking_block;
#[path = "headless_conversation/thread_archive.rs"]
mod thread_archive;
#[path = "headless_conversation/thread_drafts.rs"]
mod thread_drafts;
#[path = "headless_conversation/thread_history.rs"]
mod thread_history;
#[path = "headless_conversation/thread_role_lock.rs"]
mod thread_role_lock;
#[path = "headless_conversation/thread_role_restore.rs"]
mod thread_role_restore;
#[path = "headless_conversation/thread_tree.rs"]
mod thread_tree;
