use std::collections::{BTreeSet, HashMap};

use serde_json::Value;

pub const MAX_SNAPSHOT_LINES: usize = 800;

const TRANSPARENT_ROLES: &[&str] = &[
    "none",
    "generic",
    "InlineTextBox",
    "LineBreak",
    "presentation",
    "LabelText",
];
const STATES: &[&str] = &[
    "checked", "selected", "expanded", "disabled", "focused", "modal", "required", "readonly",
];

/// An outline of the accessibility tree. `refs` holds the backend DOM node
/// ids that the text exposes as `ref=e<id>`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub text: String,
    pub refs: BTreeSet<i64>,
}

/// Renders `Accessibility.getFullAXTree` nodes.
pub fn render(nodes: &[Value]) -> Snapshot {
    let by_id: HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|node| Some((node.get("nodeId")?.as_str()?, node)))
        .collect();
    let mut out = Outline::default();
    for root in nodes.iter().filter(|node| node.get("parentId").is_none()) {
        walk(root, &by_id, 0, "", &mut out);
    }
    let mut text =
        out.lines
            .iter()
            .take(MAX_SNAPSHOT_LINES)
            .fold(String::new(), |mut text, line| {
                text.push_str(line);
                text.push('\n');
                text
            });
    if out.lines.len() > MAX_SNAPSHOT_LINES {
        text.push_str(&format!(
            "[snapshot truncated: {} more lines; scroll or narrow the page]\n",
            out.lines.len() - MAX_SNAPSHOT_LINES
        ));
    }
    Snapshot {
        text,
        refs: out.refs,
    }
}

#[derive(Default)]
struct Outline {
    lines: Vec<String>,
    refs: BTreeSet<i64>,
}

fn walk(
    node: &Value,
    by_id: &HashMap<&str, &Value>,
    depth: usize,
    parent_name: &str,
    out: &mut Outline,
) {
    let role = ax_string(node.get("role"));
    let name = ax_string(node.get("name"));
    let ignored = node.get("ignored").and_then(Value::as_bool) == Some(true);
    let transparent = ignored || TRANSPARENT_ROLES.contains(&role.as_str());
    let redundant_text = role == "StaticText" && (name.trim().is_empty() || parent_name == name);
    let emitted = !transparent && !redundant_text;
    if emitted {
        let backend = node.get("backendDOMNodeId").and_then(Value::as_i64);
        out.lines.push(format!(
            "{}{}",
            "  ".repeat(depth),
            describe(node, &role, &name, backend)
        ));
        if role != "StaticText"
            && let Some(backend) = backend
        {
            out.refs.insert(backend);
        }
    }
    let (depth, parent_name) = if emitted {
        (depth + 1, name.as_str())
    } else {
        (depth, parent_name)
    };
    for child in node
        .get("childIds")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Some(child) = by_id.get(child) {
            walk(child, by_id, depth, parent_name, out);
        }
    }
}

fn describe(node: &Value, role: &str, name: &str, backend: Option<i64>) -> String {
    let mut line = match (role, name.is_empty()) {
        ("StaticText", _) => format!("- text {name:?}"),
        (_, true) => format!("- {role}"),
        (_, false) => format!("- {role} {name:?}"),
    };
    let value = ax_string(node.get("value"));
    if !value.is_empty() {
        line.push_str(&format!(" value={value:?}"));
    }
    for property in node
        .get("properties")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(label) = property
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| STATES.contains(name))
        else {
            continue;
        };
        let state = ax_string(property.get("value"));
        if !state.is_empty() && state != "false" {
            line.push_str(&format!(" [{label}={state}]"));
        }
    }
    if role != "StaticText"
        && let Some(backend) = backend
    {
        line.push_str(&format!(" [ref=e{backend}]"));
    }
    line
}

/// Whether an element the outline can show has `text` in its name or value.
pub fn mentions(nodes: &[Value], text: &str) -> bool {
    nodes
        .iter()
        .filter(|node| node.get("ignored").and_then(Value::as_bool) != Some(true))
        .any(|node| {
            ax_string(node.get("name")).contains(text)
                || ax_string(node.get("value")).contains(text)
        })
}

fn ax_string(value: Option<&Value>) -> String {
    match value.and_then(|value| value.get("value")) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn node(id: &str, parent: Option<&str>, role: &str, name: &str, children: &[&str]) -> Value {
        let mut node = json!({
            "nodeId": id,
            "ignored": false,
            "role": {"type": "role", "value": role},
            "name": {"type": "computedString", "value": name},
            "childIds": children,
            "backendDOMNodeId": id.parse::<i64>().unwrap() + 100,
        });
        if let Some(parent) = parent {
            node["parentId"] = json!(parent);
        }
        node
    }

    #[test]
    fn outline_collapses_wrappers_and_exposes_refs_with_state() {
        let mut nodes = vec![
            node("1", None, "RootWebArea", "Shop", &["2", "6"]),
            node("2", Some("1"), "generic", "", &["3", "4"]),
            node("3", Some("2"), "button", "Buy", &["5"]),
            node("4", Some("2"), "textbox", "Email", &[]),
            node("5", Some("3"), "StaticText", "Buy", &[]),
            node("6", Some("1"), "StaticText", "Total 3", &[]),
        ];
        nodes[2]["properties"] = json!([
            {"name": "disabled", "value": {"type": "boolean", "value": true}},
            {"name": "focusable", "value": {"type": "boolean", "value": true}},
        ]);
        nodes[3]["value"] = json!({"type": "string", "value": "a@b.test"});
        nodes[1]["ignored"] = json!(true);

        let snapshot = render(&nodes);

        assert_eq!(
            snapshot.text,
            "- RootWebArea \"Shop\" [ref=e101]\n  - button \"Buy\" [disabled=true] [ref=e103]\n  - textbox \"Email\" value=\"a@b.test\" [ref=e104]\n  - text \"Total 3\"\n"
        );
        assert_eq!(snapshot.refs, BTreeSet::from([101, 103, 104]));
    }

    #[test]
    fn long_pages_are_truncated_with_a_note() {
        let children: Vec<String> = (2..MAX_SNAPSHOT_LINES as i64 + 12)
            .map(|id| id.to_string())
            .collect();
        let child_refs: Vec<&str> = children.iter().map(String::as_str).collect();
        let mut nodes = vec![node("1", None, "list", "Items", &child_refs)];
        nodes.extend(
            child_refs
                .iter()
                .map(|id| node(id, Some("1"), "listitem", id, &[])),
        );

        let snapshot = render(&nodes);

        assert_eq!(snapshot.text.lines().count(), MAX_SNAPSHOT_LINES + 1);
        assert!(
            snapshot
                .text
                .ends_with("[snapshot truncated: 11 more lines; scroll or narrow the page]\n")
        );
    }
}
