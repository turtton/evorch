#!/usr/bin/env bash
set -euo pipefail
cd "${1:?usage}"
node -e '
const fs = require("fs");
const f = "crates/agents/src/role.rs";
let s = fs.readFileSync(f, "utf8");
const anchor = "impl Role {\n";
const add = `impl Role {
    /// 全ロール。enum の宣言順。
    pub const ALL: [Role; 9] = [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
        Role::WebResearcher,
        Role::Planner,
        Role::Advisor,
        Role::MultimodalLooker,
        Role::Evaluator,
    ];

`;
if (!s.includes(anchor)) throw new Error("anchor");
fs.writeFileSync(f, s.replace(anchor, add));
'
cat > crates/agents/tests/role_all.rs <<'RS'
use agents::Role;

#[test]
fn all_has_nine_roles() {
    assert_eq!(Role::ALL.len(), 9);
}

#[test]
fn all_round_trips_through_names() {
    for role in Role::ALL {
        assert_eq!(Role::from_name(role.name()).unwrap(), role);
    }
}
RS
cargo fmt -p agents
final=$(cargo test -p agents --offline 2>&1 | awk '/^test result: ok\./ { s += $4 } END { print s + 0 }')
printf '{"baseline_passed": 28, "final_passed": %s, "all_len": 9}\n' "$final" > answers/t4.json
