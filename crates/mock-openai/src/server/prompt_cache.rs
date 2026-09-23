//! A deterministic prefix oracle, not a tokenizer or a billing simulator.
//!
//! Each UTF-8 byte of the canonical prompt is one synthetic token. This catches
//! prompt mutations without modeling provider minimum lengths, block sizes, TTL,
//! routing, generated-output caching, or real token counts and prices.

use std::collections::VecDeque;

use serde_json::Value;

const MAX_CACHED_PROMPTS: usize = 64;

struct Entry {
    model: Option<Value>,
    cache_key: Option<Value>,
    prompt: Vec<u8>,
}

#[derive(Default)]
pub(super) struct PromptCache {
    entries: VecDeque<Entry>,
}

impl PromptCache {
    /// Looks up the longest reusable prefix, then retains the current input.
    pub(super) fn observe(&mut self, request: &Value) -> (u64, u64) {
        let model = request.get("model").cloned();
        let cache_key = request.get("prompt_cache_key").cloned();
        let prompt = prompt_bytes(request);
        let cached = self
            .entries
            .iter()
            .filter(|entry| entry.model == model && entry.cache_key == cache_key)
            .map(|entry| {
                entry
                    .prompt
                    .iter()
                    .zip(&prompt)
                    .take_while(|(left, right)| left == right)
                    .count()
            })
            .max()
            .unwrap_or_default();
        let input_tokens = prompt.len() as u64;
        if self.entries.len() == MAX_CACHED_PROMPTS {
            self.entries.pop_front();
        }
        self.entries.push_back(Entry {
            model,
            cache_key,
            prompt,
        });
        (input_tokens, cached as u64)
    }
}

fn prompt_bytes(request: &Value) -> Vec<u8> {
    let mut settings = request.clone();
    if let Some(object) = settings.as_object_mut() {
        for field in ["messages", "stream", "stream_options"] {
            object.remove(field);
        }
    }
    let mut bytes = Vec::new();
    append_json(&settings, &mut bytes);
    bytes.push(b'\n');
    if let Some(messages) = request.get("messages").and_then(Value::as_array) {
        for message in messages {
            append_json(message, &mut bytes);
            bytes.push(b'\n');
        }
    }
    bytes
}

/// Sort object keys recursively so harmless JSON serialization order is ignored.
/// Arrays retain order; message order, tools, arguments, and metadata all matter.
fn append_json(value: &Value, bytes: &mut Vec<u8>) {
    match value {
        Value::Object(object) => {
            bytes.push(b'{');
            let mut fields: Vec<_> = object.iter().collect();
            fields.sort_unstable_by_key(|(key, _)| *key);
            for (index, (key, value)) in fields.into_iter().enumerate() {
                if index != 0 {
                    bytes.push(b',');
                }
                bytes.extend_from_slice(
                    serde_json::to_string(key)
                        .expect("JSON object key serializes")
                        .as_bytes(),
                );
                bytes.push(b':');
                append_json(value, bytes);
            }
            bytes.push(b'}');
        }
        Value::Array(values) => {
            bytes.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    bytes.push(b',');
                }
                append_json(value, bytes);
            }
            bytes.push(b']');
        }
        _ => bytes.extend_from_slice(
            serde_json::to_string(value)
                .expect("JSON scalar serializes")
                .as_bytes(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{MAX_CACHED_PROMPTS, PromptCache, prompt_bytes};

    #[test]
    fn canonical_prompt_keeps_message_boundaries_and_counts_utf8_bytes() {
        let request = json!({
            "model": "m", "temperature": 0, "stream": true,
            "stream_options": {"include_usage": true},
            "messages": [{"role": "user", "content": "こんにちは"}]
        });
        let expected = r#"{"model":"m","temperature":0}
{"content":"こんにちは","role":"user"}
"#;
        assert_eq!(prompt_bytes(&request), expected.as_bytes());
        let mut cache = PromptCache::default();
        assert_eq!(cache.observe(&request), (expected.len() as u64, 0));
    }

    #[test]
    fn retains_longest_prior_prefix_not_just_the_latest_prompt() {
        let mut cache = PromptCache::default();
        let original = json!({"model": "m", "messages": [{"content": "first"}]});
        let (input, _) = cache.observe(&original);
        cache.observe(&json!({"model": "m", "messages": [{"content": "different"}]}));
        assert_eq!(cache.observe(&original), (input, input));
    }

    #[test]
    fn bounds_cached_history_and_evicts_oldest_prompt() {
        let mut cache = PromptCache::default();
        let oldest = json!({"model": "oldest", "messages": []});
        cache.observe(&oldest);
        for index in 0..MAX_CACHED_PROMPTS {
            cache.observe(&json!({"model": format!("m{index}"), "messages": []}));
        }
        assert_eq!(cache.entries.len(), MAX_CACHED_PROMPTS);
        assert_eq!(cache.observe(&oldest).1, 0);
    }
}
