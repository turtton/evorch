//! Shared logical accounting for the conversation index (ADR 0041 M4).
//!
//! The writer job, request finalization, re-accounting after an
//! `accounting_version` change and the W6 measurement harness all charge index
//! inputs through these functions; nothing else may reimplement the formula.
//! The result is a LOGICAL budget charge compared with `max_bytes`. It is not a
//! physical growth estimate: physical admission uses `PlannedGrowth` instead.

use super::{
    AccountingError, AccountingPolicy, BlockLocator, ExtractedIndexInput, IndexAccountant,
    IndexAccounting, IndexBlockKind, IndexRole,
};

/// Version of [`DEFAULT_ACCOUNTING_POLICY`]. Any coefficient change must bump
/// it so stored charges are re-accounted (R2 "accounting_version のみ変更").
/// `0` is reserved for "not applied yet" in `thread_index_state`.
pub const ACCOUNTING_VERSION: u32 = 1;

/// Coefficients measured on a full copy of the real store (ADR 0041 「Phase B
/// 順序 2 の計測」): rounded-up per-block row/UNIQUE-index overhead, per-run
/// state row overhead and the FTS-to-text ratio of the trigram index.
pub const DEFAULT_ACCOUNTING_POLICY: AccountingPolicy = AccountingPolicy {
    version: ACCOUNTING_VERSION,
    per_block_bytes: 376,
    per_run_bytes: 325,
    fts_numerator: 2387,
    fts_denominator: 1000,
};

/// The single production [`IndexAccountant`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SharedIndexAccountant;

impl IndexAccountant for SharedIndexAccountant {
    fn account_index_input(
        &self,
        input: &ExtractedIndexInput,
        policy: AccountingPolicy,
    ) -> Result<IndexAccounting, AccountingError> {
        account_index_input(input, policy)
    }
}

fn validate(policy: AccountingPolicy) -> Result<(), AccountingError> {
    if policy.version == 0 || policy.fts_denominator == 0 {
        return Err(AccountingError::InvalidPolicy);
    }
    Ok(())
}

fn role_name(role: IndexRole) -> &'static str {
    match role {
        IndexRole::User => "user",
        IndexRole::Assistant => "assistant",
        IndexRole::WorkingNote => "working_note",
    }
}

fn kind_name(kind: IndexBlockKind) -> &'static str {
    match kind {
        IndexBlockKind::Text => "text",
        IndexBlockKind::WorkingNote => "working_note",
        IndexBlockKind::ToolUse => "tool_use",
        IndexBlockKind::ToolResult => "tool_result",
        IndexBlockKind::ToolError => "tool_error",
    }
}

fn add(left: u64, right: u64) -> Result<u64, AccountingError> {
    left.checked_add(right).ok_or(AccountingError::Overflow)
}

fn len(bytes: usize) -> Result<u64, AccountingError> {
    u64::try_from(bytes).map_err(|_| AccountingError::Overflow)
}

/// Attribute bytes as documented on [`IndexAccounting::attribute_bytes`]: the
/// run id once, then per block the compact serde JSON of its locator, the
/// role/kind names without quotes, the tool name when present and the hash.
pub fn index_attribute_bytes(input: &ExtractedIndexInput) -> Result<u64, AccountingError> {
    let mut total = len(input.run_id.len())?;
    for block in &input.blocks {
        let locator = serde_json::to_string::<BlockLocator>(&block.locator)
            .map_err(|_| AccountingError::Overflow)?;
        total = add(total, len(locator.len())?)?;
        total = add(total, len(role_name(block.role).len())?)?;
        total = add(total, len(kind_name(block.kind).len())?)?;
        total = add(total, len(block.tool_name.as_ref().map_or(0, String::len))?)?;
        total = add(total, len(block.hash.len())?)?;
    }
    Ok(total)
}

/// `text + attributes + per_block*count + per_run + ceil(text*fts_num/fts_den)`
/// with checked arithmetic and no intermediate overflow.
pub fn charge_index_bytes(
    text_bytes: u64,
    attribute_bytes: u64,
    block_count: u64,
    policy: AccountingPolicy,
) -> Result<u64, AccountingError> {
    validate(policy)?;
    let fts = (u128::from(text_bytes) * u128::from(policy.fts_numerator))
        .div_ceil(u128::from(policy.fts_denominator));
    let fts = u64::try_from(fts).map_err(|_| AccountingError::Overflow)?;
    let blocks = policy
        .per_block_bytes
        .checked_mul(block_count)
        .ok_or(AccountingError::Overflow)?;
    [attribute_bytes, blocks, policy.per_run_bytes, fts]
        .into_iter()
        .try_fold(text_bytes, add)
}

/// Re-accounts stored totals with a new policy without re-extracting text.
pub fn reaccount_index(
    text_bytes: u64,
    attribute_bytes: u64,
    block_count: u64,
    policy: AccountingPolicy,
) -> Result<IndexAccounting, AccountingError> {
    Ok(IndexAccounting {
        accounting_version: policy.version,
        text_bytes,
        attribute_bytes,
        block_count,
        charged_bytes: charge_index_bytes(text_bytes, attribute_bytes, block_count, policy)?,
    })
}

/// Charges one extracted input under `policy`.
pub fn account_index_input(
    input: &ExtractedIndexInput,
    policy: AccountingPolicy,
) -> Result<IndexAccounting, AccountingError> {
    validate(policy)?;
    reaccount_index(
        input.text_bytes,
        index_attribute_bytes(input)?,
        len(input.blocks.len())?,
        policy,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_index::IndexBlock;

    fn block(
        locator: BlockLocator,
        kind: IndexBlockKind,
        tool: Option<&str>,
        text: &str,
    ) -> IndexBlock {
        IndexBlock {
            locator,
            role: IndexRole::Assistant,
            kind,
            tool_name: tool.map(str::to_owned),
            text: text.to_owned(),
            truncated: false,
            hash: "ab".repeat(32),
        }
    }

    fn input() -> ExtractedIndexInput {
        let blocks = vec![
            block(
                BlockLocator::Message {
                    ordinal: 3,
                    block_index: 1,
                },
                IndexBlockKind::ToolUse,
                Some("shell"),
                "ls -la",
            ),
            block(
                BlockLocator::WorkingNote { note_seq: 7 },
                IndexBlockKind::Text,
                None,
                "note",
            ),
        ];
        ExtractedIndexInput {
            run_id: "run-1".into(),
            snapshot_revision: 1,
            source_revision: 2,
            extractor_version: crate::thread_index::EXTRACTOR_VERSION,
            text_bytes: 10,
            blocks,
        }
    }

    #[test]
    fn attribute_bytes_follow_the_documented_fields() {
        let message_locator = r#"{"source_kind":"message","ordinal":3,"block_index":1}"#.len();
        let note_locator = r#"{"source_kind":"working_note","note_seq":7}"#.len();
        let expected = "run-1".len()
            + message_locator
            + "assistant".len()
            + "tool_use".len()
            + "shell".len()
            + 64
            + note_locator
            + "assistant".len()
            + "text".len()
            + 64;
        assert_eq!(index_attribute_bytes(&input()), Ok(expected as u64));
    }

    #[test]
    fn charge_matches_the_formula_and_rounds_fts_up() {
        let accounting = account_index_input(&input(), DEFAULT_ACCOUNTING_POLICY).unwrap();
        let attributes = index_attribute_bytes(&input()).unwrap();
        // ceil(10 * 2387 / 1000) = ceil(23.87) = 24.
        assert_eq!(
            accounting.charged_bytes,
            10 + attributes + 2 * 376 + 325 + 24
        );
        assert_eq!(accounting.accounting_version, ACCOUNTING_VERSION);
        assert_eq!(accounting.block_count, 2);
        assert_eq!(accounting.text_bytes, 10);
        let again = reaccount_index(10, attributes, 2, DEFAULT_ACCOUNTING_POLICY).unwrap();
        assert_eq!(again, accounting);
        assert_eq!(
            SharedIndexAccountant.account_index_input(&input(), DEFAULT_ACCOUNTING_POLICY),
            Ok(accounting)
        );
    }

    #[test]
    fn invalid_policies_and_overflow_are_rejected() {
        let zero_version = AccountingPolicy {
            version: 0,
            ..DEFAULT_ACCOUNTING_POLICY
        };
        let zero_denominator = AccountingPolicy {
            fts_denominator: 0,
            ..DEFAULT_ACCOUNTING_POLICY
        };
        assert_eq!(
            charge_index_bytes(1, 1, 1, zero_version),
            Err(AccountingError::InvalidPolicy)
        );
        assert_eq!(
            account_index_input(&input(), zero_denominator),
            Err(AccountingError::InvalidPolicy)
        );
        assert_eq!(
            charge_index_bytes(u64::MAX, 0, 0, DEFAULT_ACCOUNTING_POLICY),
            Err(AccountingError::Overflow)
        );
        assert_eq!(
            charge_index_bytes(0, 0, u64::MAX, DEFAULT_ACCOUNTING_POLICY),
            Err(AccountingError::Overflow)
        );
        // u128 intermediate: text * numerator does not overflow before division.
        let large = AccountingPolicy {
            fts_numerator: 1,
            fts_denominator: 1_000_000,
            ..DEFAULT_ACCOUNTING_POLICY
        };
        let text = u64::MAX / 4;
        assert!(charge_index_bytes(text, 0, 0, large).is_ok());
    }
}
