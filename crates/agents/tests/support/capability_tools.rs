pub const ORCHESTRATOR_TOOLS: &[&str] = &[
    "delegate",
    "delegate_background",
    "send_message",
    "skill_load",
    "send",
    "wait_reply",
    "inbox",
    "wait",
    "cancel",
    "list_agents",
    "inspect_agent",
    "read",
    "grep",
    "git_diff",
    "compact",
    "finish",
    "web_fetch",
    "ledger_append",
    "ledger_read",
];

pub const EXPLORER_TOOLS: &[&str] = &["read", "grep", "ledger_append", "ledger_read"];

pub const WORKER_TOOLS: &[&str] = &[
    "read",
    "edit",
    "grep",
    "shell",
    "skill_load",
    "git_diff",
    "send",
    "wait_reply",
    "inbox",
    "escalate",
    "ledger_append",
    "ledger_read",
];

pub const REVIEWER_TOOLS: &[&str] = &["read", "grep", "git_diff", "ledger_append", "ledger_read"];

pub const LIBRARIAN_TOOLS: &[&str] = &[
    "read",
    "grep",
    "web_search",
    "web_fetch",
    "ledger_append",
    "ledger_read",
];
