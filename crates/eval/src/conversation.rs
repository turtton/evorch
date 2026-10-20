//! What the user saw of a replay, in order: the input sent to the root run, the
//! root's assistant text, and its user-visible questions with their answers.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Question {
        id: String,
        title: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        options: Vec<String>,
    },
    Answer {
        question_id: String,
        title: String,
        text: String,
    },
}
