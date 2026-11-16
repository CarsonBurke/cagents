//! What a tool call's input says so far, read as its fragments arrive.
//!
//! Claude streams a call's input as fragments of one JSON object. A row that
//! shows the call while it streams needs the members that have arrived, and
//! the string member still arriving as far as it goes. Reparsing the whole
//! prefix for every fragment costs time in proportion to everything received
//! so far, so a long command's input took quadratic time to stream. This
//! reads each fragment once and keeps its place between them.

use serde_json::{Map, Value};

/// A tool call's input, read as far as it has arrived.
///
/// [`value`](Self::value) holds every member whose value has arrived whole,
/// and a string member that is still arriving, as far as it has. A member
/// whose value is anything else is left out until it ends: a number or a
/// container cut off part-way would say something its whole value does not.
#[derive(Debug, Default)]
pub(super) struct StreamedInput {
    value: Value,
    at: At,
    /// The member being read, once its key has ended.
    key: String,
    /// A string being read: the key, or a string member's value.
    string: Unescape,
    /// A value other than a string, as written, until it ends.
    raw: Raw,
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
enum At {
    /// Before the opening brace.
    #[default]
    Start,
    /// Inside the object, before a member's key.
    BeforeKey,
    Key,
    /// After a key, before its colon.
    AfterKey,
    BeforeValue,
    StringValue,
    OtherValue,
    /// After a member's value, before a comma or the closing brace.
    AfterValue,
    /// After the closing brace.
    End,
    /// The input is not an object being written: nothing more is read.
    Unreadable,
}

/// A JSON string's characters, unescaped as they arrive.
#[derive(Debug, Default)]
struct Unescape {
    /// After a backslash whose escape has not ended.
    escape: Option<Escape>,
    /// A `\u` escape's high surrogate, waiting for its low half.
    high: Option<u16>,
}

#[derive(Debug, Clone, Copy)]
enum Escape {
    /// Just after the backslash.
    Begun,
    /// A `\u` escape's hex digits so far, and how many.
    Unicode(u16, u8),
}

/// What a string character contributes once unescaped.
enum Read {
    Char(char),
    /// Part of an escape that has not ended.
    Pending,
    /// The closing quote.
    Close,
    Invalid,
}

impl Unescape {
    fn read(&mut self, next: char) -> Read {
        match self.escape {
            None if next == '\\' => {
                self.escape = Some(Escape::Begun);
                Read::Pending
            }
            None if next == '"' && self.high.is_none() => Read::Close,
            None if self.high.is_some() => Read::Invalid,
            None if next < ' ' => Read::Invalid,
            None => Read::Char(next),
            Some(Escape::Begun) => {
                self.escape = None;
                let unescaped = match next {
                    'u' => {
                        self.escape = Some(Escape::Unicode(0, 0));
                        return Read::Pending;
                    }
                    _ if self.high.is_some() => return Read::Invalid,
                    '"' => '"',
                    '\\' => '\\',
                    '/' => '/',
                    'b' => '\u{8}',
                    'f' => '\u{c}',
                    'n' => '\n',
                    'r' => '\r',
                    't' => '\t',
                    _ => return Read::Invalid,
                };
                Read::Char(unescaped)
            }
            Some(Escape::Unicode(code, digits)) => {
                let Some(digit) = next.to_digit(16) else {
                    return Read::Invalid;
                };
                let code = code << 4 | digit as u16;
                if digits < 3 {
                    self.escape = Some(Escape::Unicode(code, digits + 1));
                    return Read::Pending;
                }
                self.escape = None;
                match (self.high.take(), code) {
                    (None, 0xD800..=0xDBFF) => {
                        self.high = Some(code);
                        Read::Pending
                    }
                    (Some(high), 0xDC00..=0xDFFF) => {
                        let scalar = 0x10000
                            + ((u32::from(high) - 0xD800) << 10)
                            + (u32::from(code) - 0xDC00);
                        char::from_u32(scalar).map_or(Read::Invalid, Read::Char)
                    }
                    (None, code) => char::from_u32(code.into()).map_or(Read::Invalid, Read::Char),
                    (Some(_), _) => Read::Invalid,
                }
            }
        }
    }
}

/// A value other than a string, kept as written until it ends at a comma
/// or brace outside any string or container it opened.
#[derive(Debug, Default)]
struct Raw {
    text: String,
    depth: usize,
    string: bool,
    escaped: bool,
}

impl Raw {
    /// Whether `next` ends the value rather than belonging to it.
    fn ends_at(&mut self, next: char) -> bool {
        if self.string {
            match (self.escaped, next) {
                (true, _) => self.escaped = false,
                (false, '\\') => self.escaped = true,
                (false, '"') => self.string = false,
                _ => {}
            }
        } else {
            match next {
                '"' => self.string = true,
                '{' | '[' => self.depth += 1,
                '}' | ']' if self.depth > 0 => self.depth -= 1,
                ',' | '}' if self.depth == 0 => return true,
                _ => {}
            }
        }
        self.text.push(next);
        false
    }
}

impl StreamedInput {
    /// The input as far as it has arrived: `Value::Null` before its object
    /// opens, or when what arrived is not an object.
    pub(super) fn value(&self) -> &Value {
        &self.value
    }

    /// Set the member being read, once the object has opened.
    fn insert(&mut self, value: Value) {
        if let Value::Object(members) = &mut self.value {
            members.insert(std::mem::take(&mut self.key), value);
        }
    }

    /// Read the next fragment of the input.
    pub(super) fn push(&mut self, fragment: &str) {
        for next in fragment.chars() {
            self.at = self.step(next);
            if self.at == At::Unreadable {
                self.value = Value::Null;
                return;
            }
        }
    }

    fn step(&mut self, next: char) -> At {
        let space = matches!(next, ' ' | '\t' | '\n' | '\r');
        match self.at {
            At::Unreadable => At::Unreadable,
            At::Start
            | At::BeforeKey
            | At::AfterKey
            | At::BeforeValue
            | At::AfterValue
            | At::End
                if space =>
            {
                self.at
            }
            At::Start if next == '{' => {
                self.value = Value::Object(Map::new());
                At::BeforeKey
            }
            At::BeforeKey if next == '"' => {
                self.key.clear();
                At::Key
            }
            At::BeforeKey | At::AfterValue if next == '}' => At::End,
            At::Key => match self.string.read(next) {
                Read::Char(next) => {
                    self.key.push(next);
                    At::Key
                }
                Read::Pending => At::Key,
                Read::Close => At::AfterKey,
                Read::Invalid => At::Unreadable,
            },
            At::AfterKey if next == ':' => At::BeforeValue,
            At::BeforeValue if next == '"' => {
                // The key stays set while its value arrives, to extend it.
                if let Value::Object(members) = &mut self.value {
                    members.insert(self.key.clone(), Value::String(String::new()));
                }
                At::StringValue
            }
            At::BeforeValue => {
                self.raw = Raw::default();
                self.read_other(next)
            }
            At::StringValue => match self.string.read(next) {
                Read::Char(next) => {
                    if let Value::Object(members) = &mut self.value
                        && let Some(Value::String(value)) = members.get_mut(&self.key)
                    {
                        value.push(next);
                    }
                    At::StringValue
                }
                Read::Pending => At::StringValue,
                Read::Close => At::AfterValue,
                Read::Invalid => At::Unreadable,
            },
            At::OtherValue => self.read_other(next),
            At::AfterValue if next == ',' => At::BeforeKey,
            _ => At::Unreadable,
        }
    }

    fn read_other(&mut self, next: char) -> At {
        if !self.raw.ends_at(next) {
            return At::OtherValue;
        }
        let Ok(value) = serde_json::from_str(&self.raw.text) else {
            return At::Unreadable;
        };
        self.insert(value);
        if next == '}' { At::End } else { At::BeforeKey }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn read(prefix: &str) -> Value {
        let mut input = StreamedInput::default();
        input.push(prefix);
        input.value().clone()
    }

    #[test]
    fn a_prefix_reads_as_the_members_that_have_arrived() {
        for (prefix, value) in [
            ("", Value::Null),
            ("{", json!({})),
            ("{\"comm", json!({})),
            ("{\"command\"", json!({})),
            ("{\"command\":", json!({})),
            ("{\"command\":\"", json!({"command": ""})),
            ("{\"command\":\"ls -", json!({"command": "ls -"})),
            ("{\"command\":\"ls\",", json!({"command": "ls"})),
            // A number cut off part-way is not yet the number it will be.
            (
                "{\"command\":\"ls\",\"timeout\":12",
                json!({"command": "ls"}),
            ),
            (
                "{\"command\":\"ls\",\"timeout\":120000,",
                json!({"command": "ls", "timeout": 120000}),
            ),
            (
                "{\"command\":\"ls\",\"run_in_background\":tr",
                json!({"command": "ls"}),
            ),
            (
                "{\"command\":\"ls\",\"run_in_background\":true}",
                json!({"command": "ls", "run_in_background": true}),
            ),
            // An escape cut off part-way keeps what preceded it.
            ("{\"command\":\"echo \\", json!({"command": "echo "})),
            ("{\"command\":\"echo \\u00", json!({"command": "echo "})),
            (
                "{\"command\":\"echo \\u00e9",
                json!({"command": "echo \u{e9}"}),
            ),
            ("{\"command\":\"echo \\ud83d", json!({"command": "echo "})),
            (
                "{\"command\":\"echo \\ud83d\\ude00",
                json!({"command": "echo \u{1f600}"}),
            ),
            (
                "{\"command\":\"echo \\\"hi\\\" ",
                json!({"command": "echo \"hi\" "}),
            ),
            ("{\"command\":\"a, {b}\"", json!({"command": "a, {b}"})),
            ("{\"edits\":[{\"old\":\"a,}\"},{\"ol", json!({})),
            (
                "{\"edits\":[{\"old\":\"a,}\"}],\"path\":\"x",
                json!({"edits": [{"old": "a,}"}], "path": "x"}),
            ),
            (" \n{ \"a\" : \"b\" } ", json!({"a": "b"})),
            ("[\"not an object\"", Value::Null),
            ("{\"a\":\"b\"}x", Value::Null),
            ("{\"a\":\"b\" \"c\"", Value::Null),
            ("{\"a\":\"\\q\"", Value::Null),
            ("{\"a\":nul}", Value::Null),
        ] {
            assert_eq!(read(prefix), value, "{prefix}");
        }
    }

    /// Where the fragments break makes no difference, and a whole input
    /// reads as its JSON parses.
    #[test]
    fn fragments_read_the_same_however_they_break() {
        let whole = json!({
            "command": "printf 'caf\u{e9} \u{1f600}\\n' | tee \"out, {1}\"\n",
            "description": "Write \"quoted\" text",
            "timeout": 120000,
            "run_in_background": false,
            "nested": {"a": [1, {"b": "}],"}]},
        })
        .to_string();
        let expected = read(&whole);
        assert_eq!(expected, serde_json::from_str::<Value>(&whole).unwrap());
        let boundaries: Vec<usize> = whole.char_indices().map(|(at, _)| at).collect();
        for &cut in &boundaries {
            let mut input = StreamedInput::default();
            input.push(&whole[..cut]);
            assert_eq!(input.value(), &read(&whole[..cut]), "{cut}");
            input.push(&whole[cut..]);
            assert_eq!(input.value(), &expected, "{cut}");
        }
        let mut input = StreamedInput::default();
        for (at, next) in whole.char_indices() {
            input.push(&whole[at..at + next.len_utf8()]);
        }
        assert_eq!(input.value(), &expected);
    }
}
