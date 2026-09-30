//! `self_improvement` セクションの安全な書き戻し。

use std::path::Path;

use crate::{ConfigError, SelfImprovementConfig};

/// 自己改善ドラフト設定だけを置き換え、他セクションとコメントを保持する。
///
/// strict 検証後に原子的に保存する。同一パスへの並行保存は呼び出し側で直列化する。
///
/// # Errors
/// 入力・既存設定の不正や入出力失敗を返す。検証失敗時は既存ファイルを変更しない。
pub fn save_self_improvement(
    path: &Path,
    self_improvement: &SelfImprovementConfig,
) -> Result<(), ConfigError> {
    self_improvement.validate()?;
    let mut doc = crate::save::read_document(path)?;
    let mut section = toml_edit::Table::new();
    section.insert("enabled", toml_edit::value(self_improvement.enabled));
    if let Some(draft_dir) = &self_improvement.draft_dir {
        section.insert("draft_dir", toml_edit::value(draft_dir.as_str()));
    }
    section.insert(
        "max_candidates",
        toml_edit::value(i64::from(self_improvement.max_candidates)),
    );
    section.insert(
        "evidence_max_bytes",
        toml_edit::value(i64::from(self_improvement.evidence_max_bytes)),
    );
    section.insert(
        "daily_limit",
        toml_edit::value(i64::from(self_improvement.daily_limit)),
    );
    // validate() 済みなので、上限 31_536_000 は TOML 整数の範囲内に収まる。
    section.insert(
        "duplicate_cooldown_secs",
        toml_edit::value(self_improvement.duplicate_cooldown_secs as i64),
    );
    section.insert(
        "collect_diagnostics",
        toml_edit::value(self_improvement.collect_diagnostics),
    );
    section.insert(
        "collect_lessons",
        toml_edit::value(self_improvement.collect_lessons),
    );
    doc.insert("self_improvement", toml_edit::Item::Table(section));
    crate::save::write_document(path, &doc)
}
