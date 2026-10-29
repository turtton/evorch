//! Reading the structured answers of the simulated user and the judge.
//!
//! A model asked for a JSON object does not always return exactly that: it may put prose
//! around the object, split the answer over several text blocks, or return no text at all.
//! [`parse`] recovers the object when one is present, and an answer that still cannot be
//! used is described by [`Unusable`] so the failure can be traced and the next attempt can
//! be told what was wrong.

use providers::{ContentBlock, Message};
use serde::de::DeserializeOwned;

/// How many times a structured answer is requested before giving up.
pub(crate) const ATTEMPTS: usize = 3;
/// Characters of an unusable answer kept in its diagnostic.
const EXCERPT_CHARS: usize = 240;
/// Characters of an unusable answer shown to the model when it is asked again.
const FEEDBACK_ANSWER_CHARS: usize = 4_000;
/// Positions of `{` tried when the answer does not start with the object.
const MAX_OBJECT_STARTS: usize = 32;

/// An answer that could not be used, with what is needed to trace and correct it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unusable {
    /// Why the answer was rejected.
    pub reason: String,
    /// The text blocks of the answer, empty when it held none.
    pub answer: String,
    /// The kinds of the non-text blocks the answer held.
    other_blocks: Vec<&'static str>,
}

impl Unusable {
    /// An answer rejected after it was parsed, such as a missing field.
    pub(crate) fn rejected(reason: impl Into<String>, content: &[ContentBlock]) -> Self {
        Self {
            reason: reason.into(),
            answer: text_of(content),
            other_blocks: other_blocks(content),
        }
    }

    /// The reason with what the answer looked like, for the report and the log.
    pub(crate) fn describe(&self, attempt: usize) -> String {
        let seen = if self.answer.is_empty() {
            if self.other_blocks.is_empty() {
                "the answer was empty".to_owned()
            } else {
                format!(
                    "the answer held no text, only {} block(s)",
                    self.other_blocks.join(", ")
                )
            }
        } else {
            let excerpt: String = self.answer.chars().take(EXCERPT_CHARS).collect();
            let more = if self.answer.chars().count() > EXCERPT_CHARS {
                "…"
            } else {
                ""
            };
            format!(
                "the answer was {} characters and began {excerpt:?}{more}",
                self.answer.chars().count()
            )
        };
        format!("{}; {seen} (attempt {attempt} of {ATTEMPTS})", self.reason)
    }

    /// `base` followed by this answer and a request to answer again, or `base` itself when
    /// there is no answer text to point at.
    pub(crate) fn retry_messages(&self, base: &[Message]) -> Vec<Message> {
        let mut messages = base.to_vec();
        if self.answer.is_empty() {
            return messages;
        }
        messages.push(Message {
            role: providers::Role::Assistant,
            content: vec![ContentBlock::Text {
                text: crate::report::clip(&self.answer, FEEDBACK_ANSWER_CHARS),
            }],
        });
        messages.push(Message {
            role: providers::Role::User,
            content: vec![ContentBlock::Text {
                text: format!(
                    "That answer could not be used: {}. Answer again with only the JSON object that matches the required schema, without any other text or code fence.",
                    self.reason
                ),
            }],
        });
        messages
    }
}

fn text_of(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn other_blocks(content: &[ContentBlock]) -> Vec<&'static str> {
    content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { .. } => None,
            ContentBlock::Image { .. } => Some("image"),
            ContentBlock::Reasoning { .. } | ContentBlock::ResponsesReasoning { .. } => {
                Some("reasoning")
            }
            ContentBlock::Anthropic { .. } => Some("anthropic"),
            ContentBlock::Compaction { .. } => Some("compaction"),
            ContentBlock::ToolUse { .. } => Some("tool_use"),
            ContentBlock::ToolResult { .. } => Some("tool_result"),
        })
        .collect()
}

/// The `T` an answer holds: the whole text, or else the first JSON object in it that
/// reads as a `T` (the answer may be fenced, or surrounded by prose).
pub(crate) fn parse<T: DeserializeOwned>(content: &[ContentBlock]) -> Result<T, Unusable> {
    let answer = text_of(content);
    let unusable = |reason: String| Unusable {
        reason,
        answer: answer.clone(),
        other_blocks: other_blocks(content),
    };
    if answer.is_empty() {
        return Err(unusable("the answer has no text".into()));
    }
    let unfenced = unfence(&answer);
    let whole = match serde_json::from_str::<T>(unfenced) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    for (start, _) in unfenced
        .match_indices('{')
        .take(MAX_OBJECT_STARTS)
        .filter(|(start, _)| *start > 0)
    {
        if let Some(Ok(value)) = serde_json::Deserializer::from_str(&unfenced[start..])
            .into_iter::<T>()
            .next()
        {
            return Ok(value);
        }
    }
    Err(unusable(whole.to_string()))
}

/// The text without a surrounding Markdown code fence.
fn unfence(text: &str) -> &str {
    let text = text.trim();
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.strip_suffix("```").unwrap_or(rest);
    // Drop the language tag (`json`, `JSON`, ...) that follows the opening fence.
    match rest.split_once('\n') {
        Some((tag, body)) if !tag.trim().contains(['{', '[', '"']) => body.trim(),
        _ => rest.trim(),
    }
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, PartialEq, Eq, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        value: String,
    }

    fn text(value: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: value.into() }]
    }

    fn answer(value: &str) -> Answer {
        Answer {
            value: value.into(),
        }
    }

    #[test]
    fn an_object_is_read_from_plain_fenced_and_surrounded_text() {
        // Given: the object bare, in fences of either case, after prose and split over blocks.
        let object = r#"{"value":"x"}"#;
        let split = vec![
            ContentBlock::Text {
                text: "Here is the analysis:".into(),
            },
            ContentBlock::Text {
                text: object.into(),
            },
        ];

        // When / Then: every form yields the object.
        assert_eq!(parse::<Answer>(&text(object)).unwrap(), answer("x"));
        assert_eq!(
            parse::<Answer>(&text(&format!("```json\n{object}\n```"))).unwrap(),
            answer("x")
        );
        assert_eq!(
            parse::<Answer>(&text(&format!("```JSON\n{object}\n```"))).unwrap(),
            answer("x")
        );
        assert_eq!(
            parse::<Answer>(&text(&format!("Sure! {{not json}} then {object} Done."))).unwrap(),
            answer("x")
        );
        assert_eq!(parse::<Answer>(&split).unwrap(), answer("x"));
    }

    #[test]
    fn unusable_answers_say_what_they_were() {
        // Given: prose only, an object of another shape, an empty answer and a tool call.
        let prose = text("I could not decide.");
        let shape = text(r#"{"other":1}"#);
        let tool = vec![ContentBlock::ToolUse {
            id: "1".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }];

        // When: they are parsed.
        let prose = parse::<Answer>(&prose).unwrap_err();
        let shape = parse::<Answer>(&shape).unwrap_err();
        let empty = parse::<Answer>(&text("  ")).unwrap_err();
        let tool = parse::<Answer>(&tool).unwrap_err();

        // Then: each diagnostic names the cause and shows what was returned.
        assert!(prose.describe(1).contains("I could not decide."));
        assert!(shape.describe(2).contains("other"));
        assert!(shape.describe(2).contains("attempt 2 of 3"));
        assert!(empty.describe(3).contains("empty"));
        assert!(tool.describe(1).contains("tool_use"));
    }

    #[test]
    fn a_retry_shows_the_rejected_answer_only_when_there_was_one() {
        // Given: the original messages and two rejected answers.
        let base = [Message {
            role: providers::Role::User,
            content: text("question"),
        }];
        let prose = parse::<Answer>(&text("no json here")).unwrap_err();
        let empty = parse::<Answer>(&text("")).unwrap_err();

        // When / Then: the retry repeats the base, then the answer and the correction request;
        // an empty answer is retried unchanged.
        let retried = prose.retry_messages(&base);
        assert_eq!(retried.len(), 3);
        assert_eq!(retried[0], base[0]);
        assert_eq!(retried[1].role, providers::Role::Assistant);
        assert_eq!(retried[1].content, text("no json here"));
        assert_eq!(retried[2].role, providers::Role::User);
        assert_eq!(empty.retry_messages(&base), base);
    }
}
