//! JSON a provider hands over, as a person reads it.
//!
//! Providers pass a call's arguments, many tools' results and some approval
//! details as JSON, and the app used to show that JSON as it came: a prompt
//! to a subagent was one line of `\n` escapes, and a command was quoted
//! twice, once for the shell and once for JSON. A reader wants the values.
//! An input that is only a command reads as the command; any other object
//! reads as its members, each value as the text it holds and each number as
//! written. Members that fit on their key's line come first, so a
//! call's settings are not pushed below a long prompt; each group keeps the
//! order the members arrived in. For Claude that is alphabetical, but not by
//! Claude: its adapter re-serializes the input through a `serde_json` map
//! that the daemon builds without `preserve_order`, so the model's order is
//! gone before this code sees it. Nothing is escaped: the lines are for
//! reading, selecting and copying, not parsing.
//!
//! ```text
//! description: Review the change
//! run_in_background: true
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
pub struct Readable {
    pub text: String,
    pub command: bool,
}

/// The input's readable form, or `None` when it is shown as it came: when
/// it is not one whole JSON object or array (a call still streaming, a
/// shortened input, a plain-text argument), or is too large to read here.
pub fn readable(input: &str) -> Option<Readable> {
    read(input, true)
}

/// A tool's result read as [`readable`] reads an input, except that a
/// result holding only a `command` is that member rather than a command the
/// call ran.
pub fn result(output: &str) -> Option<String> {
    read(output, false).map(|readable| readable.text)
}

fn read(input: &str, commands: bool) -> Option<Readable> {
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
    // Each level indents its lines further, so deep nesting would multiply
    // the input's size; arguments nested that deep are not read as lines.
    let (depth, numbers) = scan(trimmed);
    if depth > DEPTH {
        return None;
    }
    let numbers = std::cell::RefCell::new(numbers.into_iter());
    let mut deserializer = serde_json::Deserializer::from_str(trimmed);
    let node = serde::de::DeserializeSeed::deserialize(Read(&numbers), &mut deserializer).ok()?;
    deserializer.end().ok()?;
    if commands
        && let Node::Map(members) = &node
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

/// A value an adapter holds, such as an approval's details, as lines to
/// read; a value too large or deep to read stays JSON.
pub fn value(value: &serde_json::Value) -> String {
    let json = value.to_string();
    match readable(&json) {
        Some(readable) => readable.text,
        None if value.is_string() => value.as_str().unwrap_or_default().to_owned(),
        None => json,
    }
}

/// Whether a call was made without arguments, so its detail has no input to
/// show. This runs as the detail is drawn, so it reads no further than the
/// brackets.
pub fn is_empty(input: &str) -> bool {
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
            let (short, long): (Vec<_>, Vec<_>) = members
                .iter()
                .partition(|(_, value)| value.inline().is_some());
            for (key, value) in short.into_iter().chain(long) {
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

/// How deeply `json` nests its lists and objects, and the text of each
/// number in it, in the order written. A number is shown as written:
/// read as a float, `1.50` would read as `1.5` and an integer past `u64`
/// as an exponent. Run on valid JSON only; anything else fails to parse
/// after.
fn scan(json: &str) -> (usize, Vec<&str>) {
    let bytes = json.as_bytes();
    let (mut depth, mut deepest, mut numbers) = (0usize, 0, Vec::new());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        match byte {
            b'"' => {
                index += 1;
                while let Some(&byte) = bytes.get(index) {
                    index += match byte {
                        b'\\' => 2,
                        b'"' => break,
                        _ => 1,
                    };
                }
            }
            b'{' | b'[' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            b'-' | b'0'..=b'9' => {
                let start = index;
                while bytes.get(index + 1).is_some_and(|byte| {
                    matches!(byte, b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-')
                }) {
                    index += 1;
                }
                numbers.push(&json[start..=index]);
            }
            _ => {}
        }
        index += 1;
    }
    (deepest, numbers)
}

/// Reads a [`Node`], taking each number's text from what [`scan`] found:
/// the parser visits values in the order they are written.
#[derive(Clone, Copy)]
struct Read<'n, 's>(&'n std::cell::RefCell<std::vec::IntoIter<&'s str>>);

impl Read<'_, '_> {
    fn number<E: serde::de::Error>(self) -> Result<Node, E> {
        self.0
            .borrow_mut()
            .next()
            .map(|text| Node::Scalar(text.to_owned()))
            .ok_or_else(|| E::custom("a number the scan did not find"))
    }
}

impl<'de> serde::de::DeserializeSeed<'de> for Read<'_, '_> {
    type Value = Node;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Node, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> serde::de::Visitor<'de> for Read<'_, '_> {
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
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Node, E> {
        self.number()
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Node, E> {
        self.number()
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Node, E> {
        self.number()
    }
    fn visit_unit<E>(self) -> Result<Node, E> {
        Ok(Node::Scalar("null".into()))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Node, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(self)? {
            items.push(item);
        }
        Ok(Node::List(items))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Node, A::Error> {
        let mut members = Vec::new();
        while let Some(key) = map.next_key::<String>()? {
            members.push((key, map.next_value_seed(self)?));
        }
        Ok(Node::Map(members))
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
        // A result that names a command did not run one.
        assert_eq!(
            result(r#"{"command":"ls -la"}"#).as_deref(),
            Some("command: ls -la")
        );
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
    fn settings_come_before_long_values_and_multiline_text_reads_as_lines() {
        let input = r#"{"description":"Review","prompt":"Read the diff.\n\nReport what breaks.","subagent_type":"Explore","run_in_background":true}"#;
        assert_eq!(
            readable(input).unwrap().text,
            "description: Review\n\
             subagent_type: Explore\n\
             run_in_background: true\n\
             prompt:\n  Read the diff.\n\n  Report what breaks."
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
            "options: (none)\n\
             limit: null\n\
             todos:\n  - content: Fix it\n    status: pending\n  - content: Test it\n    status: done\n\
             paths:\n  - src/app.rs\n  - src/chat.rs"
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
    fn numbers_read_as_written() {
        let input = r#"{"price":1.50,"id":123456789012345678901234,"offset":-2e-3,"count":7,"label":"1.50 [x]","tags":["a\\\"]",0.10]}"#;
        assert_eq!(
            readable(input).unwrap().text,
            "price: 1.50\nid: 123456789012345678901234\noffset: -2e-3\ncount: 7\nlabel: 1.50 [x]\ntags:\n  - a\\\"]\n  - 0.10"
        );
    }

    #[test]
    fn what_is_not_one_whole_json_value_is_shown_as_it_came() {
        for input in [
            "",
            "plain text argument",
            r#"{"command":"still stre"#,
            "\"a JSON string\"",
            "42",
            "{} {}",
            "[1] [2]",
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
