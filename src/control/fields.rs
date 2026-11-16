//! Compact `--json` rows for listings, and `--fields`, which keeps only the
//! named fields of each row, as `gh --json FIELDS` does.
//!
//! A row states each fact once and only where it says something: a field
//! that would be null, empty, false or the listing's default is left out,
//! enumerations are snake_case words, and fractions are whole percentages.
//! What every row of a listing shares, such as an account's allowance, is
//! reported once beside the rows.
use serde::Serialize;
use serde_json::{Map, Value};

/// How `--fields` reads on each listing that takes it.
pub(super) const HELP: &str =
    "Print only these comma-separated fields of each row as JSON; implies --json";

/// One listing row, built a field at a time.
#[derive(Debug, Default)]
pub(super) struct Row(Map<String, Value>);

impl Row {
    pub(super) fn new() -> Self {
        Self::default()
    }

    /// `value` under `key`, unless it says nothing: null, false, or an
    /// empty string, list or object.
    pub(super) fn put(mut self, key: &str, value: impl Serialize) -> Self {
        let value = serde_json::to_value(value).unwrap_or(Value::Null);
        if !is_blank(&value) {
            self.0.insert(key.to_owned(), value);
        }
        self
    }

    /// A count under `key`, unless it is zero.
    pub(super) fn count(self, key: &str, count: impl Into<u64>) -> Self {
        match count.into() {
            0 => self,
            count => self.put(key, count),
        }
    }

    pub(super) fn into_value(self) -> Value {
        Value::Object(self.0)
    }
}

impl From<Row> for Value {
    fn from(row: Row) -> Self {
        row.into_value()
    }
}

fn is_blank(value: &Value) -> bool {
    match value {
        Value::Null | Value::Bool(false) => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.is_empty(),
        Value::Bool(true) | Value::Number(_) => false,
    }
}

/// A unit enumeration variant as a row names it: `InProgress` reads
/// `in_progress`. A value that is not a plain word is returned as is.
pub(super) fn word(value: impl Serialize) -> Value {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => Value::String(snake_case(&name)),
        Ok(other) => other,
        Err(_) => Value::Null,
    }
}

fn snake_case(name: &str) -> String {
    let mut snake = String::with_capacity(name.len() + 4);
    for (index, character) in name.char_indices() {
        if character.is_ascii_uppercase() {
            if index > 0 && !snake.ends_with('_') {
                snake.push('_');
            }
            snake.push(character.to_ascii_lowercase());
        } else {
            snake.push(character);
        }
    }
    snake
}

/// A fraction in 0..=1 as the whole percentage a listing shows, rounded
/// down, so the `0.74` a provider reports, which reads `0.7400000095367432`
/// once widened, is `74`.
pub(super) fn percent(fraction: f32) -> u32 {
    (fraction.clamp(0.0, 1.0) * 100.0).floor() as u32
}

/// `rows` with only the `fields` named, in the order named. A row without
/// a field leaves it out, as `--json` does.
pub(super) fn select(rows: Vec<Value>, fields: &[String]) -> Vec<Value> {
    rows.into_iter()
        .map(|row| {
            let Value::Object(mut row) = row else {
                return row;
            };
            Value::Object(
                fields
                    .iter()
                    .filter_map(|field| row.remove(field).map(|value| (field.clone(), value)))
                    .collect(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_row_leaves_out_what_says_nothing() {
        let row = Row::new()
            .put("id", "a")
            .put("title", Option::<String>::None)
            .put("pinned", false)
            .put("draft", true)
            .put("edited", Vec::<String>::new())
            .put("label", "")
            .put("left", 0)
            .count("ahead", 0u32)
            .count("behind", 2u32);
        assert_eq!(
            row.into_value(),
            json!({"id": "a", "draft": true, "left": 0, "behind": 2})
        );
    }

    #[test]
    fn words_are_snake_case_and_fractions_whole_percentages() {
        #[derive(Serialize)]
        enum Example {
            InProgress,
            NeedsLogin,
            Ok,
        }
        assert_eq!(word(Example::InProgress), json!("in_progress"));
        assert_eq!(word(Example::NeedsLogin), json!("needs_login"));
        assert_eq!(word(Example::Ok), json!("ok"));
        assert_eq!(percent(0.74), 74);
        assert_eq!(percent(0.26), 26);
        assert_eq!(percent(0.299), 29);
        assert_eq!(percent(1.5), 100);
        assert_eq!(percent(-0.1), 0);
    }

    #[test]
    fn selection_keeps_the_named_fields_in_order() {
        let rows = vec![
            json!({"id": "a", "state": "idle", "title": "x"}),
            json!({"id": "b", "title": "y"}),
        ];
        assert_eq!(
            select(rows, &["state".into(), "id".into()]),
            vec![json!({"state": "idle", "id": "a"}), json!({"id": "b"})]
        );
    }
}
