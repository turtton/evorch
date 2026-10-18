use serde_json::{Value, json};

use super::AgentError;

const NAMED: &[(&str, &str, i64, Option<&str>)] = &[
    ("Enter", "Enter", 13, Some("\r")),
    ("Tab", "Tab", 9, None),
    ("Escape", "Escape", 27, None),
    ("Backspace", "Backspace", 8, None),
    ("Delete", "Delete", 46, None),
    ("ArrowUp", "ArrowUp", 38, None),
    ("ArrowDown", "ArrowDown", 40, None),
    ("ArrowLeft", "ArrowLeft", 37, None),
    ("ArrowRight", "ArrowRight", 39, None),
    ("Home", "Home", 36, None),
    ("End", "End", 35, None),
    ("PageUp", "PageUp", 33, None),
    ("PageDown", "PageDown", 34, None),
    ("Space", "Space", 32, Some(" ")),
];

const MODIFIERS: &[(&str, i64)] = &[("Alt", 1), ("Control", 2), ("Meta", 4), ("Shift", 8)];

/// `Input.dispatchKeyEvent` params for one press of a key such as `Enter`,
/// `a` or `Control+a`: key down, then key up.
pub(super) fn key_events(combo: &str) -> Result<[Value; 2], AgentError> {
    let unknown = || AgentError::UnknownKey(combo.to_owned());
    let mut parts: Vec<&str> = combo.split('+').collect();
    let key = match parts.pop() {
        Some("") if combo.ends_with('+') => "+",
        Some(key) if !key.is_empty() => key,
        _ => return Err(unknown()),
    };
    let mut modifiers = 0;
    for part in parts.iter().filter(|part| !part.is_empty()) {
        let (_, bit) = MODIFIERS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(part))
            .ok_or_else(unknown)?;
        modifiers |= bit;
    }
    let (key, code, key_code, text) = match NAMED
        .iter()
        .find(|(name, ..)| name.eq_ignore_ascii_case(key))
    {
        Some(&(name, code, key_code, text)) => (
            if name == "Space" { " " } else { name }.to_owned(),
            code.to_owned(),
            key_code,
            text.map(str::to_owned),
        ),
        None => {
            let mut chars = key.chars();
            let (Some(character), None) = (chars.next(), chars.next()) else {
                return Err(unknown());
            };
            let upper = character.to_ascii_uppercase();
            let code = match upper {
                'A'..='Z' => format!("Key{upper}"),
                '0'..='9' => format!("Digit{upper}"),
                _ => String::new(),
            };
            let key_code = if upper.is_ascii_alphanumeric() {
                i64::from(u32::from(upper))
            } else {
                0
            };
            (key.to_owned(), code, key_code, Some(key.to_owned()))
        }
    };
    let text = text.filter(|_| modifiers & !8 == 0);
    let mut down = json!({
        "type": if text.is_some() { "keyDown" } else { "rawKeyDown" },
        "key": key,
        "code": code,
        "windowsVirtualKeyCode": key_code,
        "modifiers": modifiers,
    });
    if let Some(text) = text {
        down["text"] = json!(text);
    }
    let up = json!({
        "type": "keyUp",
        "key": key,
        "code": code,
        "windowsVirtualKeyCode": key_code,
        "modifiers": modifiers,
    });
    Ok([down, up])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_printable_and_modified_keys_map_to_cdp_events() {
        let [down, up] = key_events("Enter").unwrap();
        assert_eq!(down["type"], "keyDown");
        assert_eq!(down["text"], "\r");
        assert_eq!(up["type"], "keyUp");

        let [down, _] = key_events("control+a").unwrap();
        assert_eq!(down["type"], "rawKeyDown");
        assert_eq!(down["code"], "KeyA");
        assert_eq!(down["modifiers"], 2);
        assert!(down.get("text").is_none());

        let [down, _] = key_events("Shift+A").unwrap();
        assert_eq!(down["text"], "A");

        assert!(matches!(
            key_events("Hyper+a"),
            Err(AgentError::UnknownKey(_))
        ));
        assert!(matches!(
            key_events("NotAKey"),
            Err(AgentError::UnknownKey(_))
        ));
    }
}
