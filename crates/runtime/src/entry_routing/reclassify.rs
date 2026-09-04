//! 同一モデルによる entry 再分類 (issue #71)。
//!
//! ローカルキーワードルールで判定不能だった場合に、起動予定の Orchestrator と
//! 同じモデルへ Intent Gate 本文と ExecutionShape 報告形式の指示を渡して
//! 再分類する。応答の解析は Text ブロックのみを対象とし、一意なマーカーが
//! 取れない場合は呼び出し側の fail-safe (Coordinated 倒し) に委ねる。

use std::sync::{Arc, OnceLock};

use agents::Role;
use providers::{ChatResponse, ContentBlock, Message, Role as MessageRole};
use regex::Regex;

use crate::model::{AgentInvocationContext, AgentModel};
use crate::prompt::{EXECUTION_SHAPES, ExecutionShape, render_routing_gate_body};

/// 再分類応答で ExecutionShape を報告させる接頭辞。
pub(crate) const SHAPE_ANSWER_PREFIX: &str = "ExecutionShape:";

/// 再分類モデル呼び出しの相関用 run ID。
pub(crate) const ENTRY_INVOCATION_RUN_ID: &str = "entry-routing";

/// 応答本文から ExecutionShape マーカーを抽出する正規表現。
static SHAPE_ANSWER_RE: OnceLock<Regex> = OnceLock::new();

/// 再分類の結果。判定成功・一意マーカー無し・呼び出し失敗の 3 相を型で区別する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReclassifyOutcome {
    /// 応答から一意な ExecutionShape を判定できた。
    Classified(ExecutionShape),
    /// 応答に一意な ExecutionShape マーカーが無かった。
    NoUniqueMarker,
    /// モデル呼び出しが失敗した (理由は Display 文字列)。
    Error(String),
}

/// 再分類応答の報告形式指示文をレンダリングする。
///
/// `- {SHAPE_ANSWER_PREFIX} <名前>` の行は [`EXECUTION_SHAPES`] を走査して
/// 生成する (手書きの列挙はしない)。
pub(crate) fn render_shape_answer_instruction() -> String {
    let mut instruction =
        String::from("分類結果の報告形式:\n応答の最終行に次のいずれか 1 行のみを出力すること。\n");
    for shape in &EXECUTION_SHAPES {
        instruction.push_str("- ");
        instruction.push_str(SHAPE_ANSWER_PREFIX);
        instruction.push(' ');
        instruction.push_str(shape.name());
        instruction.push('\n');
    }
    instruction
}

fn shape_answer_regex() -> &'static Regex {
    SHAPE_ANSWER_RE.get_or_init(|| {
        Regex::new(r"(?i)ExecutionShape:\s*([A-Za-z]+)")
            .expect("shape answer regex is statically valid")
    })
}

/// モデル応答から ExecutionShape を解析する。
///
/// Text ブロックのみを連結して走査し (Reasoning / ToolUse / ToolResult は無視)、
/// 解析できた ExecutionShape の集合がちょうど 1 要素のときだけ Some を返す。
/// マーカーなし・両マーカー・未知の名前はいずれも None になる。
pub(crate) fn parse_shape_answer(response: &ChatResponse) -> Option<ExecutionShape> {
    let text = response
        .message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let mut distinct: Vec<ExecutionShape> = Vec::new();
    for capture in shape_answer_regex().captures_iter(&text) {
        let Some(shape) = capture
            .get(1)
            .and_then(|matched| ExecutionShape::from_name(matched.as_str()))
        else {
            continue;
        };
        if !distinct.contains(&shape) {
            distinct.push(shape);
        }
    }

    match distinct[..] {
        [shape] => Some(shape),
        _ => None,
    }
}

/// Orchestrator と同じモデルへ Intent Gate 本文 + 報告形式指示を渡して再分類する。
///
/// 呼び出し失敗時はエラー理由を [`ReclassifyOutcome::Error`] に載して返す
/// (fail-safe で Coordinated に倒す判断は呼び出し側が行う)。
pub(crate) async fn reclassify(model: &Arc<dyn AgentModel>, message: &str) -> ReclassifyOutcome {
    let system = Message {
        role: MessageRole::System,
        content: vec![ContentBlock::Text {
            text: render_routing_gate_body() + &render_shape_answer_instruction(),
        }],
    };
    let user = Message {
        role: MessageRole::User,
        content: vec![ContentBlock::Text {
            text: message.to_string(),
        }],
    };
    let invocation = AgentInvocationContext {
        run_id: ENTRY_INVOCATION_RUN_ID.to_string(),
    };

    match model
        .complete(&invocation, Role::Orchestrator, &[system, user], &[])
        .await
    {
        Ok(response) => match parse_shape_answer(&response) {
            Some(shape) => ReclassifyOutcome::Classified(shape),
            None => ReclassifyOutcome::NoUniqueMarker,
        },
        Err(error) => ReclassifyOutcome::Error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::{SHAPE_ANSWER_PREFIX, render_shape_answer_instruction};
    use crate::prompt::EXECUTION_SHAPES;

    // Given: 報告形式指示文
    // When: render_shape_answer_instruction を呼ぶ
    // Then: EXECUTION_SHAPES の全 shape の報告行を含む (手書き列挙でない単一ソース走査の証明)
    #[test]
    fn shape_answer_instruction_lists_every_execution_shape() {
        let instruction = render_shape_answer_instruction();

        assert!(instruction.starts_with("分類結果の報告形式:"));
        for shape in &EXECUTION_SHAPES {
            assert!(
                instruction.contains(&format!("- {SHAPE_ANSWER_PREFIX} {}", shape.name())),
                "報告形式に {} が無い",
                shape.name()
            );
        }
    }
}
