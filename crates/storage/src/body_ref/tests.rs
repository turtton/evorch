use std::collections::HashMap;

use serde_json::{Value, json};

use super::*;

/// A deterministic generator: the tests are properties over many values, not timing.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const ALPHABET: [char; 8] = ['a', 'Z', '0', ' ', '"', '\\', 'é', '漢'];

fn random_string(rng: &mut Rng) -> String {
    let len = match rng.below(8) {
        0 => 2_047,
        1 => 2_048,
        2 => 2_049,
        3 => rng.below(4_200) as usize,
        _ => rng.below(40) as usize,
    };
    let mut text = String::new();
    match rng.below(6) {
        0 => text.push(ESCAPE),
        1 => text.push_str(&BodyToken::of("some body").to_value_string()),
        2 => text.push_str("\u{e000}evorch-body:v2:00:1"),
        _ => {}
    }
    while text.len() < len {
        text.push(ALPHABET[rng.below(ALPHABET.len() as u64) as usize]);
    }
    text
}

fn random_value(rng: &mut Rng, depth: u32) -> Value {
    match rng.below(if depth == 0 { 4 } else { 7 }) {
        0 => Value::Null,
        1 => json!(rng.below(1_000)),
        2 => Value::Bool(rng.below(2) == 0),
        3 => Value::String(random_string(rng)),
        4 | 5 => Value::Array(
            (0..rng.below(4))
                .map(|_| random_value(rng, depth - 1))
                .collect(),
        ),
        _ => Value::Object(
            (0..rng.below(4))
                .map(|_| (random_string(rng), random_value(rng, depth - 1)))
                .collect(),
        ),
    }
}

fn tokenize(value: &mut Value, bodies: &mut HashMap<BodyToken, String>) {
    tokenize_in_place(value, &mut |text| {
        let token = BodyToken::of(text);
        bodies.insert(token, text.to_owned());
        Ok(token)
    })
    .unwrap();
}

fn resolve(value: &mut Value, bodies: &HashMap<BodyToken, String>) -> Result<(), StorageError> {
    resolve_in_place(value, "test", &mut |token| {
        bodies
            .get(token)
            .cloned()
            .ok_or(StorageError::BodyUnresolved {
                owner: "test".into(),
                hash: Some(token.hash_hex()),
                reason: BodyUnresolvedReason::BlobMissing,
            })
    })
}

#[test]
fn tokenize_then_resolve_restores_the_exact_serialization() {
    // Given: many random values around the threshold, with escape-looking strings.
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for _ in 0..400 {
        let original = random_value(&mut rng, 4);
        let bytes = serde_json::to_vec(&original).unwrap();
        let mut value = original.clone();
        let mut bodies = HashMap::new();

        // When: it is tokenized and resolved again.
        tokenize(&mut value, &mut bodies);
        let stored = serde_json::to_string(&value).unwrap();
        resolve(&mut value, &bodies).unwrap();

        // Then: the value and its serialized bytes are the same, and the stored form
        // holds only well-formed tokens in token positions.
        assert_eq!(value, original);
        assert_eq!(serde_json::to_vec(&value).unwrap(), bytes);
        let reparsed: Value = serde_json::from_str(&stored).unwrap();
        assert!(collect_tokens(&reparsed, "test").is_ok());
    }
}

#[test]
fn only_strings_at_the_threshold_or_starting_with_the_escape_are_tokenized() {
    // Given: strings of 2,047 / 2,048 / 2,049 bytes, multi-byte text straddling the
    // threshold, and escape-prefixed strings.
    let cases = [
        ("a".repeat(2_047), false),
        ("a".repeat(2_048), true),
        ("a".repeat(2_049), true),
        // 1,024 two-byte characters are 2,048 bytes, though only 1,024 chars.
        ("é".repeat(1_024), true),
        (format!("{}é", "a".repeat(2_046)), true),
        (format!("{}é", "a".repeat(2_045)), false),
        ("\u{e000}".to_owned(), true),
        ("\u{e000}x".to_owned(), true),
        ("x\u{e000}".to_owned(), false),
    ];
    for (text, expected) in cases {
        assert_eq!(needs_token(&text), expected, "{} bytes", text.len());
    }

    // When: a long key is tokenized together with its short value.
    let mut value = json!({ "k".repeat(5_000): "short" });
    let mut bodies = HashMap::new();
    tokenize(&mut value, &mut bodies);

    // Then: keys are never tokenized.
    assert!(bodies.is_empty());
    assert_eq!(value, json!({ "k".repeat(5_000): "short" }));
}

#[test]
fn text_that_looks_like_a_token_is_stored_and_restored_unchanged() {
    // Given: an original string equal to a real token, one of an unknown version, and
    // ones that only start like a token.
    let originals = [
        BodyToken::of("something").to_value_string(),
        "\u{e000}evorch-body:v2:abc:1".to_owned(),
        "\u{e000}evorch-body:v1:".to_owned(),
        "\u{e000}".to_owned(),
    ];
    for original in originals {
        let mut value = json!({ "text": original.clone(), "list": [original.clone()] });
        let mut bodies = HashMap::new();

        // When: tokenized and resolved.
        tokenize(&mut value, &mut bodies);

        // Then: the stored form holds tokens only, and the original comes back once:
        // the resolved text is not scanned again.
        assert_eq!(bodies.len(), 1);
        for stored in [&value["text"], &value["list"][0]] {
            assert!(BodyToken::parse(stored.as_str().unwrap()).is_ok());
        }
        resolve(&mut value, &bodies).unwrap();
        assert_eq!(
            value,
            json!({ "text": original.clone(), "list": [original] })
        );
    }
}

#[test]
fn deep_nesting_does_not_use_the_call_stack() {
    // Given: a 5,000-level array holding one long string at the bottom.
    let mut value = Value::String("x".repeat(3_000));
    for _ in 0..5_000 {
        value = Value::Array(vec![value]);
    }
    let mut bodies = HashMap::new();

    // When: tokenizing and resolving it.
    tokenize(&mut value, &mut bodies);
    resolve(&mut value, &bodies).unwrap();

    // Then: the string is back at the bottom.
    assert_eq!(bodies.len(), 1);
    let mut cursor = &value;
    while let Value::Array(items) = cursor {
        cursor = &items[0];
    }
    assert_eq!(cursor.as_str().map(str::len), Some(3_000));
    // Dropping a value this deep recurses; that is serde_json's, not the code under test.
    std::mem::forget(value);
}

#[test]
fn token_grammar_accepts_only_the_exact_form() {
    let token = BodyToken::of("body");
    let text = token.to_value_string();
    assert_eq!(BodyToken::parse(&text), Ok(token));
    let hex = token.hash_hex();
    let head = "\u{e000}evorch-body:v1:";
    let bad = [
        String::new(),
        "plain".into(),
        format!("{text}x"),
        format!("{text} "),
        format!("{head}{}:4", hex.to_uppercase()),
        format!("{head}{hex}:04"),
        format!("{head}{hex}:+4"),
        format!("{head}{hex}:"),
        format!("{head}{hex}"),
        format!("{head}{hex}:4:5"),
        format!("{head}{hex}:99999999999999999999999"),
        format!("{head}{hex}:9223372036854775808"),
        format!("{head}{}:4", &hex[..62]),
        format!("{head}zz{}:4", &hex[2..]),
        format!("x{text}"),
    ];
    for candidate in bad {
        assert_eq!(
            BodyToken::parse(&candidate),
            Err(TokenError::Malformed),
            "{candidate:?}"
        );
    }
    assert_eq!(
        BodyToken::parse(&format!("{head}{hex}:0")).map(|t| t.byte_len),
        Ok(0)
    );
    assert_eq!(
        BodyToken::parse(&format!("{head}{hex}:9223372036854775807")).map(|t| t.byte_len),
        Ok(i64::MAX as u64)
    );
    for unknown in ["\u{e000}evorch-body:v2:x", "\u{e000}evorch-body:v10:abc:1"] {
        assert_eq!(BodyToken::parse(unknown), Err(TokenError::UnknownVersion));
    }
}

#[test]
fn an_unresolvable_token_is_a_hard_error_naming_the_owner() {
    // Given: a malformed token position, an unknown version, and a well-formed token
    // whose body is not available.
    let missing = BodyToken::of("nobody has this").to_value_string();
    let cases = [
        ("\u{e000}garbage", BodyUnresolvedReason::InvalidToken, false),
        (
            "\u{e000}evorch-body:v9:x",
            BodyUnresolvedReason::UnknownTokenVersion,
            false,
        ),
        (missing.as_str(), BodyUnresolvedReason::BlobMissing, true),
    ];
    for (text, reason, has_hash) in cases {
        let mut value = json!({ "a": [text] });

        // When: it is resolved.
        let error = resolve(&mut value, &HashMap::new()).unwrap_err();

        // Then: the error is BodyUnresolved with the reason; the hash is known only
        // for a well-formed token, and the message carries no body text.
        let StorageError::BodyUnresolved {
            owner,
            hash,
            reason: found,
        } = &error
        else {
            panic!("{error:?}");
        };
        assert_eq!((owner.as_str(), found), ("test", &reason));
        assert_eq!(hash.is_some(), has_hash);
        assert!(!error.to_string().contains("garbage"));
    }
}

#[test]
fn collect_tokens_lists_tokens_in_document_order_with_repeats() {
    let (a, b) = (BodyToken::of("a body"), BodyToken::of("b body"));
    let value = json!({
        "x": [a.to_value_string(), "plain", { "y": b.to_value_string() }],
        "z": a.to_value_string(),
    });
    assert_eq!(collect_tokens(&value, "t").unwrap(), vec![a, b, a]);
}
