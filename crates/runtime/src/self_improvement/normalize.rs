//! Stable grouping text without regex dependencies. Evidence keeps the original values.

/// Remove volatile identifiers, absolute paths and decimal runs from display/grouping text.
pub fn normalize_variable_parts(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        let rest = &text[offset..];
        let boundary = offset == 0 || !identifier_byte(text.as_bytes()[offset - 1]);
        let replacement = if rest.starts_with('/') {
            Some((
                rest.find(char::is_whitespace).unwrap_or(rest.len()),
                "<path>",
            ))
        } else if boundary {
            identifier_replacement(rest)
        } else {
            None
        };
        if let Some((length, replacement)) = replacement {
            normalized.push_str(replacement);
            offset += length;
        } else if rest.as_bytes()[0].is_ascii_digit() {
            offset += rest.bytes().take_while(u8::is_ascii_digit).count();
            normalized.push('N');
        } else {
            // `offset` always advances by a complete character or an ASCII token.
            let ch = rest.chars().next().expect("nonempty remainder");
            normalized.push(ch);
            offset += ch.len_utf8();
        }
    }
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn ends_token(text: &str, length: usize) -> bool {
    text.as_bytes()
        .get(length)
        .is_none_or(|byte| !identifier_byte(*byte))
}

fn identifier_replacement(text: &str) -> Option<(usize, &'static str)> {
    for (prefix, replacement) in [("run-", "run-N"), ("thread-", "thread-N")] {
        if let Some(id) = text.strip_prefix(prefix) {
            let length = id.bytes().take_while(u8::is_ascii_alphanumeric).count();
            let numeric = length > 0 && id.as_bytes()[..length].iter().all(u8::is_ascii_digit);
            if (numeric || (prefix == "run-" && length == 26)) && ends_token(id, length) {
                return Some((prefix.len() + length, replacement));
            }
        }
    }
    for prefix in ["toolu_", "call_"] {
        if let Some(id) = text.strip_prefix(prefix) {
            let length = id
                .bytes()
                .take_while(|byte| identifier_byte(*byte) || *byte == b'-')
                .count();
            if length > 0 {
                return Some((prefix.len() + length, "<id>"));
            }
        }
    }
    let bytes = text.as_bytes();
    if bytes.len() >= 36
        && bytes[..36].iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
        && ends_token(text, 36)
    {
        return Some((36, "<id>"));
    }
    if bytes.len() >= 32 && bytes[..32].iter().all(u8::is_ascii_hexdigit) && ends_token(text, 32) {
        return Some((32, "<id>"));
    }
    None
}
