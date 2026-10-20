use std::{fs, io::Write, path::Path};

use storage::improvement::ImprovementCandidate;

use super::{ImprovementCollector, SelfImprovementError, bound_text, new_id};

pub(super) const DRAFT_MARKER: &str = "<!-- DRAFT — NOT FILED. Posting anywhere requires explicit operator confirmation (ADR 0011). -->";

/// Deterministic local review material, never an issue submission.
pub fn render_issue_draft(c: &ImprovementCandidate) -> String {
    // Indented blocks keep untrusted evidence (including markdown fences) inert.
    format!(
        "{DRAFT_MARKER}\n\n# Title\n\n{}\n\n## Summary\n\nPassive improvement candidate; not a validated defect.\n\nCode: {}\nSource: {}\nSeverity: {}\n\n{}\n## Evidence\n\n{}\n## Environment\n\nRecorded by: {}\nRecheck against the current build at filing time; crash evidence names the crashed build.\n\n## Suggested next steps\n\n- Review and reproduce the evidence; rule out configuration or external causes.\n- Define acceptance criteria and a minimal regression test.\n- Obtain explicit operator confirmation before posting anywhere.\n",
        indent(&super::title(&c.title)),
        indent(&c.code),
        c.source.as_str(),
        c.severity.as_str(),
        summary_fields(c),
        indent(&pretty_evidence(&c.evidence)),
        super::build_info(),
    )
}

/// Comment-marked mirror of implementation_issue_packet/acceptance_criteria from
/// the repository packet shape. This is Markdown review material, NOT packet.yaml.
pub fn render_packet_draft(c: &ImprovementCandidate) -> String {
    // JSON strings are valid YAML quoted scalars; indent every evidence line so
    // candidate content cannot inject fields or a canonical-packet claim.
    let scalar = |text: &str| serde_json::Value::String(text.to_owned()).to_string();
    format!(
        "{DRAFT_MARKER}\n<!-- DRAFT for review only — not a canonical packet; do not place under .intent-cli -->\n\n# Packet draft\n\n    # DRAFT for review only — not a canonical packet; do not place under .intent-cli\n    implementation_issue_packet:\n      id: {}\n      status: draft\n      issue_title: {}\n      issue_kind: improvement\n      problem: {}\n      code: {}\n      evidence: |\n{}      acceptance_criteria:\n{}",
        scalar(&format!("draft-si-{}", c.id)),
        scalar(&super::title(&c.title)),
        scalar(&super::title(&c.title)),
        scalar(&c.code),
        pretty_evidence(&c.evidence)
            .lines()
            .map(|line| format!("        {line}\n"))
            .collect::<String>(),
        acceptance_criteria(&c.code)
            .iter()
            .map(|criterion| format!("        - {}\n", scalar(criterion)))
            .collect::<String>(),
    )
}

fn acceptance_criteria(code: &str) -> &'static [&'static str] {
    match code {
        "NoProgress" => &[
            "Reproduce the no-progress loop using evidence.fields trigger, tool, path, count and input_sha256.",
            "Add a regression test that detects repeated failures at the configured threshold while allowing genuine progress.",
            "Verify the diagnostic preserves structured evidence and the run terminates or escalates without losing its checkpoint.",
        ],
        "IdenticalToolCalls" => &[
            "Reproduce identical tool calls with the recorded tool and input; verify they stop at the configured threshold.",
            "Add a regression test proving distinct tool inputs and successful progress are not incorrectly stopped.",
        ],
        "BudgetExhausted" => &[
            "Reproduce budget exhaustion with the recorded limit and usage; distinguish configured limits from harness defects.",
            "Add boundary tests proving the budget is enforced and the terminal reason and checkpoint are preserved.",
        ],
        code if code.starts_with("RunError:") => &[
            "Reproduce the run failure using evidence.reason and normalized text; rule out cancellation and provider failures.",
            "Add a regression test for the classified runtime failure, including recovery and preservation of durable run state.",
            "Verify repeated equivalent failures deduplicate and the run-error daily limit remains enforced.",
        ],
        _ => &[
            "Reproduce the observation using evidence.fields and the original evidence; distinguish harness faults from external causes.",
            "Add a minimal regression test for the verified cause and confirm normal execution remains unchanged.",
        ],
    }
}

fn summary_fields(c: &ImprovementCandidate) -> String {
    let mut table = String::from("| Field | Value |\n| --- | --- |\n");
    table.push_str(&format!("| occurrences | {} |\n", c.occurrences));
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&c.evidence)
        && let Some(fields) = value.get("fields").and_then(serde_json::Value::as_object)
    {
        for (key, value) in fields {
            // Escape Markdown/HTML instead of letting evidence create links or table rows.
            let value = value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string());
            table.push_str(&format!(
                "| {} | {} |\n",
                table_cell(key),
                table_cell(&value)
            ));
        }
    }
    table.push_str("\nCounts are a draft-generation intake snapshot, not the authoritative cumulative total; consult the database-backed GUI for current occurrences.\n");
    table
}

fn table_cell(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '|' => "&#124;".into(),
            '`' => "&#96;".into(),
            '\\' => "&#92;".into(),
            '[' => "&#91;".into(),
            ']' => "&#93;".into(),
            '*' => "&#42;".into(),
            '_' => "&#95;".into(),
            ch if ch.is_control() => " ".into(),
            ch => ch.to_string(),
        })
        .collect()
}

fn pretty_evidence(evidence: &str) -> String {
    let bounded = bound_text(evidence, 65_536);
    let pretty = serde_json::from_str::<serde_json::Value>(&bounded)
        .ok()
        .and_then(|value| serde_json::to_string_pretty(&value).ok())
        .unwrap_or(bounded);
    bound_text(&pretty, 65_536)
}

fn indent(text: &str) -> String {
    text.lines().map(|line| format!("    {line}\n")).collect()
}

impl ImprovementCollector {
    pub(super) fn write_drafts(
        &self,
        candidate_id: &str,
        issue: &str,
        packet: &str,
    ) -> Result<(String, String), SelfImprovementError> {
        let dir = self
            .settings
            .policy
            .draft_dir
            .as_deref()
            .ok_or(SelfImprovementError::UnresolvedDirectory)?;
        fs::create_dir_all(dir)?;
        let id: String = candidate_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let issue_name = format!("{id}.issue.md");
        let packet_name = format!("{id}.packet.md");
        atomic_write(&dir.join(&issue_name), issue.as_bytes())?;
        atomic_write(&dir.join(&packet_name), packet.as_bytes())?;
        Ok((issue_name, packet_name))
    }
}

/// Exclusive temporary file in the same directory + rename (no partially visible draft).
/// Also used by the panic hook: no unwraps, logging, locks or panicking callbacks.
pub(super) fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), SelfImprovementError> {
    let temp = path.with_extension(new_id("tmp")?);
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temp)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result.map_err(SelfImprovementError::from)
}
