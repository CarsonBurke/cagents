//! A tool call's input as its detail shows it.
//!
//! Providers hand a call's arguments over as JSON, and the detail used to
//! show that JSON as it came: a prompt to a subagent was one line of `\n`
//! escapes, and a command was quoted twice, once for the shell and once for
//! JSON. A reader wants the values. An input that is only a command reads as
//! the command; any other object reads as its members, one per line in the
//! order the model wrote them, each value as the text it holds. Nothing is
//! escaped: the lines are for reading, selecting and copying, not parsing.
//!
//! ```text
//! description: Review the change
//! prompt:
//!   Read the diff.
//!   Report what breaks.
//! files:
//!   - src/app.rs
//!   - src/chat.rs
//! ```

use std::fmt;

/// Inputs above this are shown as they came. Reading one is linear, but it
/// happens on the event thread when an open step's call changes, and an
/// input this large is a file being written rather than arguments a reader
/// scans.
const LIMIT: usize = 256 * 1024;

/// Inputs nested deeper than this are shown as they came.
const DEPTH: usize = 16;

/// How an input is drawn: the lines of its readable form, and whether they
/// are a shell command to highlight as one.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Readable {
    pub(super) text: String,
    pub(super) command: bool,
}

/// The input's readable form, or `None` when it is shown as it came: when
/// it is not one whole JSON object or array (a call still streaming, a
/// shortened input, a plain-text argument), or is too large to read here.
pub(super) fn readable(input: &str) -> Option<Readable> {
    let trimmed = input.trim();
    // An input still streaming is an unclosed prefix, which is not read
    // through only to fail at its end on every delta.
    if trimmed.len() > LIMIT
        || !matches!(
            (trimmed.as_bytes().first(), trimmed.as_bytes().last()),
            (Some(b'{'), Some(b'}')) | (Some(b'['), Some(b']'))
        )
    {
        return None;
    }
    let node: Node = serde_json::from_str(trimmed).ok()?;
    // Each level indents its lines further, so deep nesting would multiply
    // the input's size; arguments nested that deep are not read as lines.
    if node.depth() > DEPTH {
        return None;
    }
    if let Node::Map(members) = &node
        && let [(key, Node::Text(command))] = members.as_slice()
        && matches!(key.as_str(), "command" | "cmd")
    {
        return Some(Readable {
            text: command.clone(),
            command: true,
        });
    }
    let mut text = String::new();
    // A call without arguments has nothing to read; members that are empty
    // still say so beside their key.
    if !matches!(&node, Node::Map(members) if members.is_empty())
        && !matches!(&node, Node::List(items) if items.is_empty())
    {
        block(&mut text, &node, 0);
    }
    Some(Readable {
        text,
        command: false,
    })
}

/// Whether a call was made without arguments, so its detail has no input to
/// show. This runs as the detail is drawn, so it reads no further than the
/// brackets.
pub(super) fn is_empty(input: &str) -> bool {
    let input = input.trim();
    input.is_empty()
        || [("{", "}"), ("[", "]")].into_iter().any(|(open, close)| {
            input
                .strip_prefix(open)
                .and_then(|inner| inner.strip_suffix(close))
                .is_some_and(|inner| inner.trim().is_empty())
        })
}

/// A JSON value as read, keeping an object's members in their written
/// order. `serde_json::Value` keeps it only while some crate in the graph
/// enables its `preserve_order` feature.
#[derive(Debug)]
enum Node {
    Text(String),
    /// A number, a boolean or null, as JSON writes it.
    Scalar(String),
    List(Vec<Node>),
    Map(Vec<(String, Node)>),
}

impl Node {
    /// How many lists and objects deep the value nests.
    fn depth(&self) -> usize {
        match self {
            Node::List(items) => 1 + items.iter().map(Node::depth).max().unwrap_or(0),
            Node::Map(members) => {
                1 + members
                    .iter()
                    .map(|(_, value)| value.depth())
                    .max()
                    .unwrap_or(0)
            }
            Node::Text(_) | Node::Scalar(_) => 0,
        }
    }

    /// The value on its key's line, when it fits there.
    fn inline(&self) -> Option<&str> {
        match self {
            Node::Text(text) if !text.contains('\n') => Some(text),
            Node::Scalar(text) => Some(text),
            Node::List(items) if items.is_empty() => Some("(none)"),
            Node::Map(members) if members.is_empty() => Some("(none)"),
            _ => None,
        }
    }
}

/// Starts a new line at `indent`, after any line already written.
fn line(out: &mut String, indent: usize) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.extend(std::iter::repeat_n(' ', indent));
}

/// `node` standing alone, its lines at `indent`.
fn block(out: &mut String, node: &Node, indent: usize) {
    match node {
        Node::Text(text) => {
            let text = text.strip_suffix('\n').unwrap_or(text);
            for (index, text) in text.split('\n').enumerate() {
                let text = text.strip_suffix('\r').unwrap_or(text);
                // A blank line within a prompt stays blank rather than
                // holding the indent as trailing spaces. The first line
                // always has one, which a list item's marker replaces.
                line(
                    out,
                    if index > 0 && text.is_empty() {
                        0
                    } else {
                        indent
                    },
                );
                out.push_str(text);
            }
        }
        Node::Map(members) if !members.is_empty() => {
            for (key, value) in members {
                member(out, key, value, indent);
            }
        }
        Node::List(items) if !items.is_empty() => {
            for item in items {
                // Each item is drawn as a block two further in, and its
                // first line then takes the marker in those two columns.
                let start = out.len() + usize::from(!out.is_empty()) + indent;
                block(out, item, indent + 2);
                out.replace_range(start..start + 2, "- ");
            }
        }
        node => {
            line(out, indent);
            out.push_str(node.inline().unwrap_or_default());
        }
    }
}

fn member(out: &mut String, key: &str, value: &Node, indent: usize) {
    line(out, indent);
    out.push_str(key);
    out.push(':');
    match value.inline() {
        Some(text) => {
            out.push(' ');
            out.push_str(text);
        }
        None => block(out, value, indent + 2),
    }
}

impl<'de> serde::Deserialize<'de> for Node {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{MapAccess, SeqAccess, Visitor};
        struct Read;
        impl<'de> Visitor<'de> for Read {
            type Value = Node;
            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("a JSON value")
            }
            fn visit_str<E>(self, value: &str) -> Result<Node, E> {
                Ok(Node::Text(value.to_owned()))
            }
            fn visit_string<E>(self, value: String) -> Result<Node, E> {
                Ok(Node::Text(value))
            }
            fn visit_bool<E>(self, value: bool) -> Result<Node, E> {
                Ok(Node::Scalar(value.to_string()))
            }
            fn visit_i64<E>(self, value: i64) -> Result<Node, E> {
                Ok(Node::Scalar(value.to_string()))
            }
            fn visit_u64<E>(self, value: u64) -> Result<Node, E> {
                Ok(Node::Scalar(value.to_string()))
            }
            fn visit_f64<E>(self, value: f64) -> Result<Node, E> {
                Ok(Node::Scalar(
                    serde_json::Number::from_f64(value)
                        .map_or_else(|| value.to_string(), |number| number.to_string()),
                ))
            }
            fn visit_unit<E>(self) -> Result<Node, E> {
                Ok(Node::Scalar("null".into()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Node::List(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
                let mut members = Vec::new();
                while let Some(member) = map.next_entry()? {
                    members.push(member);
                }
                Ok(Node::Map(members))
            }
        }
        deserializer.deserialize_any(Read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shown(input: serde_json::Value) -> Readable {
        readable(&input.to_string()).expect("readable")
    }

    #[test]
    fn a_command_reads_as_the_command_without_json_quoting() {
        let input = json!({"command": "reply 67218a1b \"lead: good design\""});
        assert_eq!(
            shown(input),
            Readable {
                text: "reply 67218a1b \"lead: good design\"".into(),
                command: true,
            }
        );
        assert!(shown(json!({"cmd": "ls -la"})).command);
        // A command among other arguments is one member of several.
        let input = json!({"command": "cargo test", "timeout": 600000});
        assert_eq!(
            shown(input),
            Readable {
                text: "command: cargo test\ntimeout: 600000".into(),
                command: false,
            }
        );
    }

    #[test]
    fn members_keep_their_order_and_multiline_text_reads_as_lines() {
        let input = r#"{"description":"Review","prompt":"Read the diff.\n\nReport what breaks.","subagent_type":"Explore","run_in_background":true}"#;
        assert_eq!(
            readable(input).unwrap().text,
            "description: Review\n\
             prompt:\n  Read the diff.\n\n  Report what breaks.\n\
             subagent_type: Explore\n\
             run_in_background: true"
        );
    }

    #[test]
    fn nested_values_are_indented_lists_and_members() {
        let input = r#"{
            "todos": [
                {"content": "Fix it", "status": "pending"},
                {"content": "Test it", "status": "done"}
            ],
            "paths": ["src/app.rs", "src/chat.rs"],
            "options": {},
            "limit": null
        }"#;
        assert_eq!(
            readable(input).unwrap().text,
            "todos:\n  - content: Fix it\n    status: pending\n  - content: Test it\n    status: done\n\
             paths:\n  - src/app.rs\n  - src/chat.rs\n\
             options: (none)\n\
             limit: null"
        );
    }

    #[test]
    fn empty_and_blank_list_items_keep_their_marker() {
        let input = r#"{"lines":["","\nafter a blank","last\n"]}"#;
        assert_eq!(
            readable(input).unwrap().text,
            "lines:\n  - \n  - \n    after a blank\n  - last"
        );
        assert_eq!(readable(r#"["a",["b"]]"#).unwrap().text, "- a\n- - b");
    }

    #[test]
    fn a_call_without_arguments_has_no_input() {
        for input in ["", " ", "{}", "{ }", "[]", "\n[\n]\n"] {
            assert!(is_empty(input), "{input:?}");
            assert!(readable(input).is_none_or(|shown| shown.text.is_empty()));
        }
        for input in [r#"{"a":1}"#, "[1]", "plain"] {
            assert!(!is_empty(input), "{input}");
        }
    }

    #[test]
    fn what_is_not_one_whole_json_value_is_shown_as_it_came() {
        for input in [
            "",
            "plain text argument",
            r#"{"command":"still stre"#,
            "\"a JSON string\"",
            "42",
        ] {
            assert_eq!(readable(input), None, "{input}");
        }
        let large = json!({ "content": "x".repeat(LIMIT) }).to_string();
        assert_eq!(readable(&large), None);
        let nested = |depth| format!("{}\"a\\nb\"{}", "[".repeat(depth), "]".repeat(depth));
        assert!(readable(&nested(DEPTH)).is_some());
        assert_eq!(readable(&nested(DEPTH + 1)), None);
    }
}
