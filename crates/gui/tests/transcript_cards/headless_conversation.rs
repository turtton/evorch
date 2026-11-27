//! Related GUI conversation contracts share a test binary to reduce repeated linking.
//! Each module keeps its own fixtures; nextest still runs each test in a separate process.

#[path = "headless_conversation/compaction_ledger.rs"]
mod compaction_ledger;
#[path = "headless_conversation/message_completion.rs"]
mod message_completion;
#[path = "headless_conversation/resume_ledger_routing.rs"]
mod resume_ledger_routing;
#[path = "headless_conversation/thinking_block.rs"]
mod thinking_block;
