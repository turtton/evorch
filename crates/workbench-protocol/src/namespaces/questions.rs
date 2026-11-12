//! Questions namespace: answering and listing questions agents asked.
use crate::{
    commands::AnswerUserQuestion,
    handshake::{Capability, WireLimits},
    limits::MAX_TEXT_BYTES,
    queries::PendingQuery,
    validation::{ValidationError, check_text},
    views::{Page, QuestionView},
};
use serde::{Deserialize, Serialize};

pub const COMMAND_OPERATIONS: &[&str] = &["answer_user_question"];
pub const QUERY_OPERATIONS: &[&str] = &["list_questions"];
pub const EVENT_TYPES: &[&str] = &["question_changed"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuestionsCommand {
    AnswerUserQuestion(AnswerUserQuestion),
}

impl QuestionsCommand {
    pub fn required_capability(&self) -> Capability {
        Capability::Questions
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::AnswerUserQuestion(c) => check_text("answer", &c.answer, MAX_TEXT_BYTES, false),
        }
    }
}

no_results!(QuestionsResult);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuestionsQuery {
    ListQuestions(PendingQuery),
}

impl QuestionsQuery {
    pub fn required_capability(&self) -> Capability {
        Capability::Questions
    }

    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::ListQuestions(q) => q.page.validate(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuestionsReply {
    Questions(Page<QuestionView>),
}

impl QuestionsReply {
    pub fn validate(&self, _limits: &WireLimits) -> Result<(), ValidationError> {
        match self {
            Self::Questions(page) => page.validate_with(QuestionView::validate),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    content = "payload",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum QuestionsEvent {
    QuestionChanged { question: QuestionView },
}

impl QuestionsEvent {
    pub fn validate(&self) -> Result<(), ValidationError> {
        match self {
            Self::QuestionChanged { question } => question.validate(),
        }
    }
}
