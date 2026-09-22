pub const ORCHESTRATOR_TOOLS: &[&str] = &[
    "delegate",
    "send_message",
    "skill_load",
    "send",
    "wait_reply",
    "inbox",
    "wait",
    "run_output",
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
    "ask_user",
    "user_answers",
];

pub const EXPLORER_TOOLS: &[&str] = &[
    "read",
    "grep",
    "ledger_append",
    "ledger_read",
    "ask_user",
    "user_answers",
];

pub const WORKER_TOOLS: &[&str] = &[
    "read",
    "edit",
    "write",
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
    "ask_user",
    "user_answers",
];

pub const REVIEWER_TOOLS: &[&str] = &[
    "read",
    "grep",
    "git_diff",
    "submit_review",
    "ledger_append",
    "ledger_read",
    "ask_user",
    "user_answers",
];

pub const LIBRARIAN_TOOLS: &[&str] = &[
    "read",
    "grep",
    "web_search",
    "web_fetch",
    "ledger_append",
    "ledger_read",
    "ask_user",
    "user_answers",
];
