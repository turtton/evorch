//! Reading the structured answers of the simulated user and the judge.
//!
//! A model asked for a JSON object does not always return exactly that: it may put prose
//! around the object, split the answer over several text blocks, or return no text at all.
//! [`parse`] recovers the object when exactly one is present, and an answer that still
//! cannot be used is described by [`Unusable`]. Everything it keeps of the answer has been
//! redacted and bounded, because it reaches reports, logs and the next request.

use std::sync::OnceLock;

use providers::{ContentBlock, Message};
use secret_guard::SecretRedactor;
use serde::de::DeserializeOwned;

/// How many times a structured answer is requested before giving up.
pub(crate) const ATTEMPTS: usize = 3;
/// Characters of a rejection reason kept (serde errors can quote long field names or values).
const REASON_CHARS: usize = 400;
/// Characters of an unusable answer shown in its diagnostic.
const EXCERPT_CHARS: usize = 240;
/// Characters of an unusable answer shown to the model when it is asked again.
const FEEDBACK_ANSWER_CHARS: usize = 4_000;
/// Positions of `{` tried when the answer is not exactly one object.
const MAX_OBJECT_STARTS: usize = 64;

fn redactor() -> &'static SecretRedactor {
    static REDACTOR: OnceLock<SecretRedactor> = OnceLock::new();
    REDACTOR.get_or_init(SecretRedactor::from_env)
}

/// `text` redacted as a whole, then cut to `limit` characters, so a credential that
/// straddles the cut is masked before it can be split.
fn sanitize(redactor: &SecretRedactor, text: &str, limit: usize) -> String {
    let redacted = redactor.redact(text).text;
    match redacted.char_indices().nth(limit) {
        Some((end, _)) => format!("{}…", &redacted[..end]),
        None => redacted,
    }
}

/// An answer that could not be used, with what is needed to trace and correct it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unusable {
    reason: String,
    /// The redacted, bounded text of the answer; empty when it held none.
    answer: String,
    answer_chars: usize,
    /// The kinds of the non-text blocks the answer held; their content is never kept.
    other_blocks: Vec<&'static str>,
}

impl Unusable {
    /// An answer rejected for `reason`, such as a missing field.
    pub(crate) fn new(reason: &str, content: &[ContentBlock]) -> Self {
        Self::with(redactor(), reason, content)
    }

    fn with(redactor: &SecretRedactor, reason: &str, content: &[ContentBlock]) -> Self {
        let answer = text_of(content);
        Self {
            reason: sanitize(redactor, reason, REASON_CHARS),
            answer: sanitize(redactor, &answer, FEEDBACK_ANSWER_CHARS),
            answer_chars: answer.chars().count(),
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
                self.answer_chars
            )
        };
        format!("{}; {seen} (attempt {attempt} of {ATTEMPTS})", self.reason)
    }

    /// Append this answer (when it had text) and a request to answer again to
    /// `messages`, which the next attempt sends in full: what was already sent stays
    /// as it was, so the provider's cached prefix is kept.
    pub(crate) fn append_retry(&self, messages: &mut Vec<Message>) {
        if !self.answer.is_empty() {
            messages.push(Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: self.answer.clone(),
                }],
            });
        }
        let seen = if self.answer.is_empty() && !self.other_blocks.is_empty() {
            format!(" (it held no text, only {})", self.other_blocks.join(", "))
        } else {
            String::new()
        };
        messages.push(Message {
            role: providers::Role::User,
            content: vec![ContentBlock::Text {
                text: format!(
                    "That answer could not be used{seen}: {}. Answer again with only the JSON object that matches the required schema, without any other text or code fence.",
                    self.reason
                ),
            }],
        });
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

/// The `T` an answer holds: the whole text, or else the one JSON object in it that reads
/// as a `T` (the answer may be fenced, or have prose before or after the object). An
/// answer holding several such objects, such as an example and then the real answer, is
/// ambiguous and rejected.
pub(crate) fn parse<T: DeserializeOwned>(content: &[ContentBlock]) -> Result<T, Unusable> {
    let answer = text_of(content);
    if answer.is_empty() {
        return Err(Unusable::new("the answer has no text", content));
    }
    let unfenced = unfence(&answer);
    let whole = match serde_json::from_str::<T>(unfenced) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    let mut found = Vec::new();
    let mut resume = 0;
    for (start, _) in unfenced.match_indices('{').take(MAX_OBJECT_STARTS) {
        if start < resume {
            continue;
        }
        let mut values = serde_json::Deserializer::from_str(&unfenced[start..]).into_iter::<T>();
        if let Some(Ok(value)) = values.next() {
            resume = start + values.byte_offset();
            found.push(value);
        }
    }
    match found.len() {
        0 => Err(Unusable::new(&whole.to_string(), content)),
        1 => Ok(found.remove(0)),
        count => Err(Unusable::new(
            &format!("the answer holds {count} JSON objects that fit the required schema"),
            content,
        )),
    }
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

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "snake_case")]
    enum Pick {
        Reply,
    }

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Choice {
        #[allow(dead_code)]
        pick: Pick,
    }

    fn text(value: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: value.into() }]
    }

    fn answer(value: &str) -> Answer {
        Answer {
            value: value.into(),
        }
    }

    const OBJECT: &str = r#"{"value":"x"}"#;

    /// A credential shape the shared redactor recognises, 43 characters long.
    fn credential() -> String {
        format!("sk-{}", "a1B2c3D4".repeat(5))
    }

    fn appended(unusable: &Unusable) -> String {
        let mut messages = Vec::new();
        unusable.append_retry(&mut messages);
        serde_json::to_string(&messages).unwrap()
    }

    #[test]
    fn an_object_is_read_from_plain_fenced_and_surrounded_text() {
        // Given: the object bare, in fences of either case, split over blocks, and with prose
        // before it, after it, or both.
        let split = vec![
            ContentBlock::Text {
                text: "Here is the analysis:".into(),
            },
            ContentBlock::Text {
                text: OBJECT.into(),
            },
        ];
        let forms = [
            OBJECT.to_owned(),
            format!("```json\n{OBJECT}\n```"),
            format!("```JSON\n{OBJECT}\n```"),
            format!("Sure! {{not json}} then {OBJECT} Done."),
            format!("{OBJECT}\nThat is my answer."),
            format!("```json\n{OBJECT}\n```\nHope that helps."),
        ];

        // When / Then: every form yields the object.
        for form in forms {
            assert_eq!(
                parse::<Answer>(&text(&form)).unwrap(),
                answer("x"),
                "{form}"
            );
        }
        assert_eq!(parse::<Answer>(&split).unwrap(), answer("x"));
    }

    #[test]
    fn several_objects_that_fit_are_ambiguous() {
        // Given: an example followed by the real answer, the same object twice, and an
        // object followed by a different one that does not fit.
        let two = format!("Example: {OBJECT}\nMine: {{\"value\":\"y\"}}");
        let repeated = format!("{OBJECT}\n{OBJECT}");
        let other_shape = format!("{OBJECT}\n{{\"other\":1}}");

        // When / Then: the first two are rejected and say why; the last has one candidate.
        let error = parse::<Answer>(&text(&two)).unwrap_err();
        assert!(error.describe(1).contains("2 JSON objects"));
        assert!(parse::<Answer>(&text(&repeated)).is_err());
        assert_eq!(parse::<Answer>(&text(&other_shape)).unwrap(), answer("x"));
    }

    #[test]
    fn an_object_nested_in_a_fitting_one_is_not_a_second_candidate() {
        // Given: a type whose object holds another object of the same type.
        #[derive(Debug, Deserialize)]
        struct Tree {
            #[allow(dead_code)]
            child: Option<Box<Tree>>,
        }
        let nested = text(r#"Here: {"child":{"child":null}} Bye"#);

        // When / Then: the outer object is the only candidate.
        assert!(parse::<Tree>(&nested).is_ok());
    }

    #[test]
    fn unusable_answers_say_what_they_were() {
        // Given: prose only, an object of another shape, an empty answer and a tool call.
        let tool = vec![ContentBlock::ToolUse {
            id: "1".into(),
            name: "shell".into(),
            input: serde_json::json!({}),
        }];

        // When: they are parsed.
        let prose = parse::<Answer>(&text("I could not decide.")).unwrap_err();
        let shape = parse::<Answer>(&text(r#"{"other":1}"#)).unwrap_err();
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
    fn a_retry_appends_the_answer_only_when_it_had_text_and_always_asks_for_json_only() {
        // Given: a base history and three rejected answers: prose, nothing and a tool call.
        let base = Message {
            role: providers::Role::User,
            content: text("question"),
        };
        let prose = parse::<Answer>(&text("no json here")).unwrap_err();
        let empty = parse::<Answer>(&text("")).unwrap_err();
        let tool = parse::<Answer>(&[ContentBlock::ToolUse {
            id: "1".into(),
            name: "shell".into(),
            input: serde_json::json!({"command": "ls"}),
        }])
        .unwrap_err();

        // When: each is appended to the history.
        let mut after_prose = vec![base.clone()];
        prose.append_retry(&mut after_prose);
        let mut after_empty = vec![base.clone()];
        empty.append_retry(&mut after_empty);
        let mut after_tool = vec![base.clone()];
        tool.append_retry(&mut after_tool);

        // Then: the base is untouched; prose is shown to the model, and every rejection ends
        // with the request to answer with JSON only; a tool call is named by kind only.
        assert_eq!(after_prose.len(), 3);
        assert_eq!(after_prose[0], base);
        assert_eq!(after_prose[1].role, providers::Role::Assistant);
        assert_eq!(after_prose[1].content, text("no json here"));
        assert_eq!((after_empty.len(), after_tool.len()), (2, 2));
        for history in [&after_prose, &after_empty, &after_tool] {
            let last = serde_json::to_string(history.last().unwrap()).unwrap();
            assert!(last.contains("only the JSON object"), "{last}");
        }
        let sent = serde_json::to_string(&after_tool).unwrap();
        assert!(sent.contains("only tool_use"), "{sent}");
        assert!(
            !sent.contains("\"ls\"") && !sent.contains("shell"),
            "{sent}"
        );
    }

    #[test]
    fn credentials_never_reach_the_diagnostic_or_the_retry() {
        // Given: answers carrying a credential at the start, across the excerpt limit and
        // across the retry limit, and an unknown field named like a credential.
        let secret = credential();
        let at_start = format!("{secret} is my key");
        let across_excerpt = format!("{} {secret}", "x".repeat(EXCERPT_CHARS - 10));
        let across_feedback = format!("{} {secret}", "x".repeat(FEEDBACK_ANSWER_CHARS - 10));
        let as_field = format!("{{\"{secret}\": 1}}");

        for raw in [at_start, across_excerpt, across_feedback, as_field] {
            // When: the answer is rejected.
            let unusable = parse::<Answer>(&text(&raw)).unwrap_err();

            // Then: neither the diagnostic nor the retry holds any of the credential.
            for shown in [unusable.describe(1), appended(&unusable)] {
                assert!(!shown.contains(&secret[..12]), "{shown}");
                assert!(!shown.contains(&secret[secret.len() - 12..]), "{shown}");
            }
        }
    }

    #[test]
    fn a_known_credential_across_the_cut_is_masked_before_cutting() {
        // Given: a configured credential value that straddles each limit.
        let secret = "plain-configured-credential-value";
        let redactor = SecretRedactor::with_known_values([secret.to_owned()]);
        for limit in [EXCERPT_CHARS, FEEDBACK_ANSWER_CHARS] {
            let raw = format!("{}{secret}", "x".repeat(limit - 8));

            // When: the answer is rejected.
            let unusable = Unusable::with(&redactor, "bad", &text(&raw));

            // Then: no part of it is shown.
            for shown in [unusable.describe(1), appended(&unusable)] {
                assert!(!shown.contains("plain-conf"), "{shown}");
                assert!(!shown.contains("configured"), "{shown}");
            }
        }
    }

    #[test]
    fn long_reasons_and_answers_are_bounded() {
        // Given: an unknown field and an enum value of 100k characters, and a huge answer.
        let field = "f".repeat(100_000);
        let unknown_field = parse::<Answer>(&text(&format!("{{\"{field}\": 1}}"))).unwrap_err();
        let variant = "v".repeat(100_000);
        let unknown_variant =
            parse::<Choice>(&text(&format!("{{\"pick\": \"{variant}\"}}"))).unwrap_err();
        let huge = parse::<Answer>(&text(&"y".repeat(1_000_000))).unwrap_err();

        // When / Then: the diagnostic and the retry stay within a few thousand characters.
        for unusable in [&unknown_field, &unknown_variant, &huge] {
            assert!(unusable.describe(1).chars().count() < REASON_CHARS + EXCERPT_CHARS + 200);
            assert!(
                appended(unusable).chars().count() < REASON_CHARS + FEEDBACK_ANSWER_CHARS + 400
            );
        }
    }
}
