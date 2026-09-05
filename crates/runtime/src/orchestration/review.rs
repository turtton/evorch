//! reviewer の構造化出力を証跡へ変換する bounded review loop。

use event_bus::{CriterionCheck, CriterionStatus, GateEvidence, ReviewVerdict};
use serde::Deserialize;

const UNPARSABLE_FINDING: &str = "reviewer output unparsable";

/// reviewer final text から復元した構造化結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewResult {
    /// reviewer の判定。
    pub verdict: ReviewVerdict,
    /// acceptance criteria ごとの確認結果。
    pub criteria: Vec<CriterionCheck>,
}

/// reviewer 出力の parse 失敗。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    /// fenced `json` block が存在しない。
    #[error("reviewer output does not contain a fenced json block")]
    MissingJsonFence,
    /// JSON または列挙値が契約形状に合わない。
    #[error("reviewer json is invalid: {0}")]
    InvalidJson(String),
}

/// 1 review round から生成される criteria/review 証跡。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewEvidence {
    /// criteria 証跡。
    pub criteria: GateEvidence,
    /// review verdict 証跡。
    pub review: GateEvidence,
}

/// reviewer 完了後に supervisor が行う次の動作。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewOutcome {
    /// 現在 head を承認した。
    Approve {
        /// 完了した review round。
        round: u32,
        /// SHA-bound 証跡。
        evidence: ReviewEvidence,
    },
    /// repair worker へ指摘を渡す。
    Repair {
        /// 完了した review round。
        round: u32,
        /// repair 対象の指摘。
        findings: Vec<String>,
        /// SHA-bound 証跡。
        evidence: ReviewEvidence,
    },
    /// bounded loop を停止する。
    Blocked {
        /// 完了した review round。
        round: u32,
        /// block 理由。
        reason: String,
        /// SHA-bound 証跡。
        evidence: ReviewEvidence,
    },
}

impl ReviewOutcome {
    /// この結果から merge binding を発行できるかを返す。
    pub const fn can_issue_merge_binding(&self) -> bool {
        matches!(self, Self::Approve { .. })
    }

    /// round で記録する証跡を返す。
    pub const fn evidence(&self) -> &ReviewEvidence {
        match self {
            Self::Approve { evidence, .. }
            | Self::Repair { evidence, .. }
            | Self::Blocked { evidence, .. } => evidence,
        }
    }
}

/// review round 数と直前指摘を保持する bounded state machine。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewLoop {
    max_rounds: u32,
    rounds_used: u32,
    previous_findings: Option<Vec<String>>,
}

impl ReviewLoop {
    /// 最大 round 数を指定して初期化する。
    pub const fn new(max_rounds: u32) -> Self {
        Self {
            max_rounds,
            rounds_used: 0,
            previous_findings: None,
        }
    }

    /// 消費済み round 数を返す。
    pub const fn rounds_used(&self) -> u32 {
        self.rounds_used
    }

    /// reviewer final text を現在 head の証跡へ変換し、次状態を返す。
    pub fn on_reviewer_done(
        &mut self,
        result: &str,
        head_sha: &str,
        reviewer_run_id: &str,
    ) -> ReviewOutcome {
        self.rounds_used = self.rounds_used.saturating_add(1);
        let round = self.rounds_used;
        let parsed = parse_review_result(result).unwrap_or_else(|_| ReviewResult {
            verdict: ReviewVerdict::RequestUpdate {
                findings: vec![UNPARSABLE_FINDING.to_string()],
            },
            criteria: Vec::new(),
        });
        let unmet_ids = parsed
            .criteria
            .iter()
            .filter(|check| check.status != CriterionStatus::Met)
            .map(|check| check.id.clone())
            .collect::<Vec<_>>();
        let verdict = match parsed.verdict {
            ReviewVerdict::Approve if !unmet_ids.is_empty() => ReviewVerdict::RequestUpdate {
                findings: vec![format!(
                    "acceptance criteria not met: {}",
                    unmet_ids.join(", ")
                )],
            },
            verdict => verdict,
        };
        let evidence = ReviewEvidence {
            criteria: GateEvidence::Criteria {
                head_sha: head_sha.to_string(),
                reviewer_run_id: reviewer_run_id.to_string(),
                round,
                checklist: parsed.criteria,
            },
            review: GateEvidence::Review {
                head_sha: head_sha.to_string(),
                reviewer_run_id: reviewer_run_id.to_string(),
                round,
                verdict: verdict.clone(),
            },
        };

        match verdict {
            ReviewVerdict::Approve => ReviewOutcome::Approve { round, evidence },
            ReviewVerdict::RequestUpdate { findings } => {
                if self.previous_findings.as_ref() == Some(&findings) {
                    return ReviewOutcome::Blocked {
                        round,
                        reason: "review findings repeated".to_string(),
                        evidence,
                    };
                }
                self.previous_findings = Some(findings.clone());
                if round >= self.max_rounds {
                    ReviewOutcome::Blocked {
                        round,
                        reason: "review rounds exhausted".to_string(),
                        evidence,
                    }
                } else {
                    ReviewOutcome::Repair {
                        round,
                        findings,
                        evidence,
                    }
                }
            }
        }
    }
}

/// final text 内の最初の fenced `json` block を parse する。
///
/// # Errors
/// fence がない、または JSON が契約形状に合わない場合に失敗する。
pub fn parse_review_result(text: &str) -> Result<ReviewResult, ParseError> {
    let start = text.find("```json").ok_or(ParseError::MissingJsonFence)? + "```json".len();
    let remainder = &text[start..];
    let end = remainder.find("```").ok_or(ParseError::MissingJsonFence)?;
    let wire: ReviewResultWire = serde_json::from_str(remainder[..end].trim())
        .map_err(|error| ParseError::InvalidJson(error.to_string()))?;
    Ok(ReviewResult {
        verdict: match wire.verdict {
            VerdictWire::Approve => ReviewVerdict::Approve,
            VerdictWire::RequestUpdate => ReviewVerdict::RequestUpdate {
                findings: wire.findings,
            },
        },
        criteria: wire
            .criteria
            .into_iter()
            .map(|check| CriterionCheck {
                id: check.id,
                status: check.status.into(),
                note: check.note,
            })
            .collect(),
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewResultWire {
    verdict: VerdictWire,
    #[serde(default)]
    findings: Vec<String>,
    #[serde(default)]
    criteria: Vec<CriterionWire>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum VerdictWire {
    Approve,
    RequestUpdate,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CriterionWire {
    id: String,
    status: CriterionStatusWire,
    note: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CriterionStatusWire {
    Met,
    Unmet,
    Unknown,
}

impl From<CriterionStatusWire> for CriterionStatus {
    fn from(value: CriterionStatusWire) -> Self {
        match value {
            CriterionStatusWire::Met => Self::Met,
            CriterionStatusWire::Unmet => Self::Unmet,
            CriterionStatusWire::Unknown => Self::Unknown,
        }
    }
}
