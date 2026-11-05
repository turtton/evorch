//! A digest of a whole chat message, to tell a resend from a different message.

use sha2::{Digest, Sha256};

use crate::commands::{ChatInput, ComposerRole};

/// Bump when the encoding changes, so digests of the old encoding never match.
const VERSION: &str = "chat-v1";

fn field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn optional(hasher: &mut Sha256, value: Option<&[u8]>) {
    match value {
        Some(bytes) => {
            hasher.update([1]);
            field(hasher, bytes);
        }
        None => hasher.update([0]),
    }
}

/// The digest of every field of `chat` that makes it this message: where it goes
/// (project root, thread, composer role), what it says (text, skills, images) and
/// how it runs (model preference, fork seed), plus its submission id.
///
/// Fields are length-prefixed, so no two different messages share an encoding.
pub fn chat_fingerprint(chat: &ChatInput) -> String {
    let mut hasher = Sha256::new();
    field(&mut hasher, VERSION.as_bytes());
    field(&mut hasher, chat.submission_id.as_bytes());
    optional(
        &mut hasher,
        chat.project_root
            .as_ref()
            .map(|root| root.as_os_str().as_encoded_bytes()),
    );
    field(&mut hasher, chat.thread_id.as_bytes());
    field(
        &mut hasher,
        match chat.composer_role {
            ComposerRole::Worker => b"worker".as_slice(),
            ComposerRole::Orchestrator => b"orchestrator".as_slice(),
        },
    );
    field(&mut hasher, chat.text.as_bytes());
    hasher.update((chat.skills.len() as u64).to_be_bytes());
    for skill in &chat.skills {
        field(&mut hasher, skill.as_bytes());
    }
    hasher.update((chat.images.len() as u64).to_be_bytes());
    for image in &chat.images {
        field(&mut hasher, image.media_type.as_bytes());
        field(&mut hasher, image.data.as_bytes());
    }
    match &chat.model_preference {
        Some(preference) => {
            hasher.update([1]);
            field(&mut hasher, preference.profile.as_bytes());
            optional(&mut hasher, preference.model.as_deref().map(str::as_bytes));
            optional(
                &mut hasher,
                preference.reasoning_effort.as_deref().map(str::as_bytes),
            );
        }
        None => hasher.update([0]),
    }
    match &chat.fork_seed {
        Some(seed) => {
            hasher.update([1]);
            field(&mut hasher, seed.source_run_id.as_bytes());
            hasher.update(seed.context_len.to_be_bytes());
        }
        None => hasher.update([0]),
    }
    let digest = hasher.finalize();
    let mut text = String::with_capacity(VERSION.len() + 1 + digest.len() * 2);
    text.push_str(VERSION);
    text.push(':');
    for byte in digest {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn chat() -> ChatInput {
        ChatInput {
            submission_id: "sub-1".into(),
            project_root: Some(PathBuf::from("/work/a")),
            composer_role: ComposerRole::Worker,
            images: Vec::new(),
            thread_id: "thread-1".into(),
            text: "hello".into(),
            skills: vec!["review".into()],
            model_preference: None,
            fork_seed: None,
        }
    }

    #[test]
    fn the_same_message_has_the_same_fingerprint() {
        assert_eq!(chat_fingerprint(&chat()), chat_fingerprint(&chat()));
    }

    #[test]
    fn every_field_of_the_message_changes_the_fingerprint() {
        let base = chat_fingerprint(&chat());
        type Edit = fn(&mut ChatInput);
        let edits: [(&str, Edit); 10] = [
            ("submission", |c| c.submission_id = "sub-2".into()),
            ("project", |c| {
                c.project_root = Some(PathBuf::from("/work/b"))
            }),
            ("no project", |c| c.project_root = None),
            ("thread", |c| c.thread_id = "thread-2".into()),
            ("role", |c| c.composer_role = ComposerRole::Orchestrator),
            ("text", |c| c.text = "bye".into()),
            ("skills", |c| c.skills.clear()),
            ("image", |c| {
                c.images.push(runtime::DelegateImage {
                    media_type: "image/png".into(),
                    data: "AA==".into(),
                });
            }),
            ("model", |c| {
                c.model_preference = Some(runtime::ModelPreference {
                    profile: "fast".into(),
                    model: None,
                    reasoning_effort: None,
                });
            }),
            ("fork", |c| {
                c.fork_seed = Some(runtime::ChatForkSeed {
                    source_run_id: "run-1".into(),
                    context_len: 1,
                });
            }),
        ];
        for (field, edit) in edits {
            let mut changed = chat();
            edit(&mut changed);
            assert_ne!(chat_fingerprint(&changed), base, "{field}");
        }
    }

    #[test]
    fn moving_text_between_neighbouring_fields_changes_the_fingerprint() {
        let mut a = chat();
        a.skills = vec!["ab".into(), "c".into()];
        let mut b = chat();
        b.skills = vec!["a".into(), "bc".into()];
        let mut c = chat();
        c.thread_id = "thread-".into();
        c.text = "1hello".into();
        assert_ne!(chat_fingerprint(&a), chat_fingerprint(&b));
        assert_ne!(chat_fingerprint(&c), chat_fingerprint(&chat()));
    }
}
