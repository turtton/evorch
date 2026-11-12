//! CLI continuity validation only: PR07/17 own a future trusted issuance path.
//! The evidence cannot be constructed/serialized by callers or read from a recipe.
use super::ResumePolicy;

/// Opaque proof from a trusted live CLI handoff. No public issuer exists in 06c.
/// Neither this type nor successful validation grants CoreOwned authority.
pub struct CliContinuityEvidence {
    invocation: String,
    handoff: String,
    expires_unix_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliContinuityProblem {
    MissingEvidence,
    UnsupportedPolicy,
    InvocationMismatch,
    HandoffMismatch,
    Expired,
}

/// Validate against the CURRENT invocation, not identity deserialized as authority.
/// Expiry is exclusive: at the exact deadline the evidence is no longer valid.
/// Core activation still rejects CLI policies even after a successful check.
pub fn validate_cli_continuity(
    policy: &ResumePolicy,
    evidence: Option<&CliContinuityEvidence>,
    current_invocation: &str,
    now_unix_ms: u64,
) -> Result<(), CliContinuityProblem> {
    let ResumePolicy::CliInvocation {
        invocation_id,
        planned_handoff,
    } = policy
    else {
        return Err(CliContinuityProblem::UnsupportedPolicy);
    };
    let proof = evidence.ok_or(CliContinuityProblem::MissingEvidence)?;
    if current_invocation.is_empty()
        || invocation_id != current_invocation
        || proof.invocation != current_invocation
    {
        return Err(CliContinuityProblem::InvocationMismatch);
    }
    if proof.handoff.is_empty() || planned_handoff.as_deref() != Some(proof.handoff.as_str()) {
        return Err(CliContinuityProblem::HandoffMismatch);
    }
    if now_unix_ms >= proof.expires_unix_ms {
        return Err(CliContinuityProblem::Expired);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cli_proof_is_not_a_recipe_and_must_match_current_invocation_handoff_and_expiry() {
        let policy = ResumePolicy::CliInvocation {
            invocation_id: "current".into(),
            planned_handoff: Some("handoff".into()),
        };
        let proof = CliContinuityEvidence {
            invocation: "current".into(),
            handoff: "handoff".into(),
            expires_unix_ms: 100,
        };
        assert_eq!(
            validate_cli_continuity(&policy, None, "current", 0),
            Err(CliContinuityProblem::MissingEvidence)
        );
        assert_eq!(
            validate_cli_continuity(&policy, Some(&proof), "different", 0),
            Err(CliContinuityProblem::InvocationMismatch)
        );
        assert_eq!(
            validate_cli_continuity(&policy, Some(&proof), "current", 99),
            Ok(())
        );
        assert_eq!(
            validate_cli_continuity(&policy, Some(&proof), "current", 100),
            Err(CliContinuityProblem::Expired)
        );
        assert_eq!(
            validate_cli_continuity(&policy, Some(&proof), "current", u64::MAX),
            Err(CliContinuityProblem::Expired)
        );
        let other = ResumePolicy::CliInvocation {
            planned_handoff: Some("other".into()),
            invocation_id: "current".into(),
        };
        assert_eq!(
            validate_cli_continuity(&other, Some(&proof), "current", 0),
            Err(CliContinuityProblem::HandoffMismatch)
        );
        let no_handoff = ResumePolicy::CliInvocation {
            planned_handoff: None,
            invocation_id: "current".into(),
        };
        assert_eq!(
            validate_cli_continuity(&no_handoff, Some(&proof), "current", 0),
            Err(CliContinuityProblem::HandoffMismatch)
        );
        assert_eq!(
            validate_cli_continuity(&ResumePolicy::CoreOwned, Some(&proof), "current", 0),
            Err(CliContinuityProblem::UnsupportedPolicy)
        );
    }
}
