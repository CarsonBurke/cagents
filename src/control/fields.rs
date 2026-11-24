//! Compact `--json` rows for listings, and `--fields`, which keeps only the
//! named fields of each row, as `gh --json FIELDS` does.
//!
//! A row states each fact once and only where it says something: a field
//! that would be null, empty, false or the listing's default is left out,
//! enumerations are snake_case words, fractions are whole percentages and
//! times are Unix seconds. What every row of a listing shares, such as an
//! account's allowance, is reported once beside the rows.
//!
//! `--fields` prints rows without that context, so a selected row states
//! every field named, in full: `null`, `false` or `0` where it has nothing,
//! and the value even where it repeats the listing's.
use serde::Serialize;
use serde_json::{Map, Value};

/// How `--fields` reads on each listing that takes it.
pub(super) const HELP: &str =
    "Print only these comma-separated fields of each row as JSON; implies --json";

/// How much a row states.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Detail {
    /// Only what says something beside the listing, as `--json` prints it.
    #[default]
    Sparse,
    /// Every field, as `--fields` selects from.
    Complete,
}

impl Detail {
    /// The detail rows need when `selected` fields are printed alone.
    pub(super) fn of(selected: &[String]) -> Self {
        if selected.is_empty() {
            Self::Sparse
        } else {
            Self::Complete
        }
    }
}

/// One listing row, built a field at a time.
#[derive(Debug, Default)]
pub(super) struct Row {
    fields: Map<String, Value>,
    detail: Detail,
}

impl Row {
    pub(super) fn new(detail: Detail) -> Self {
        Self {
            fields: Map::new(),
            detail,
        }
    }

    /// `value` under `key`; a sparse row leaves it out where it says
    /// nothing: null, false, or an empty string, list or object.
    pub(super) fn put(self, key: &str, value: impl Serialize) -> Self {
        self.put_unless(key, value, false)
    }

    /// `value` under `key`, which a sparse row also leaves out where it is
    /// `redundant`: what the listing or the row already states.
    pub(super) fn put_unless(mut self, key: &str, value: impl Serialize, redundant: bool) -> Self {
        let value = serde_json::to_value(value).unwrap_or(Value::Null);
        if self.detail == Detail::Complete || !(redundant || is_blank(&value)) {
            self.fields.insert(key.to_owned(), value);
        }
        self
    }

    /// A count under `key`, which a sparse row leaves out where it is zero.
    pub(super) fn count(self, key: &str, count: impl Into<u64>) -> Self {
        let count = count.into();
        self.put_unless(key, count, count == 0)
    }

    pub(super) fn into_value(self) -> Value {
        Value::Object(self.fields)
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

/// An enumeration variant as a row names it: `InProgress` reads
/// `in_progress`, and `{"Project": path}` reads `{"project": path}`.
/// Anything else is returned as is.
pub(super) fn word(value: impl Serialize) -> Value {
    match serde_json::to_value(value) {
        Ok(Value::String(name)) => Value::String(snake_case(&name)),
        Ok(Value::Object(variant)) if variant.len() == 1 => Value::Object(
            variant
                .into_iter()
                .map(|(name, value)| (snake_case(&name), value))
                .collect(),
        ),
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

/// Unix milliseconds as the Unix seconds a row reports, rounded down.
pub(super) fn seconds(milliseconds: u64) -> u64 {
    milliseconds / 1000
}

/// A fraction in 0..=1 as the whole percentage a listing shows, rounded
/// down, so the `0.74` a provider reports, which reads `0.7400000095367432`
/// once widened, is `74`.
pub(super) fn percent(fraction: f32) -> u32 {
    (fraction.clamp(0.0, 1.0) * 100.0).floor() as u32
}

/// Complete `rows` with only the `fields` named, each stated: a field a
/// row's kind does not carry reads `null`.
pub(super) fn select(rows: Vec<Value>, fields: &[String]) -> Vec<Value> {
    rows.into_iter()
        .map(|row| {
            let Value::Object(mut row) = row else {
                return row;
            };
            Value::Object(
                fields
                    .iter()
                    .map(|field| (field.clone(), row.remove(field).unwrap_or(Value::Null)))
                    .collect(),
            )
        })
        .collect()
}

#[cfg(all(test, not(any(feature = "host", feature = "ui"))))]
mod tests {
    use super::*;
    use serde_json::json;

    fn example(detail: Detail) -> Value {
        Row::new(detail)
            .put("id", "a")
            .put("title", Option::<String>::None)
            .put("pinned", false)
            .put("draft", true)
            .put("edited", Vec::<String>::new())
            .put("label", "")
            .put("left", 0)
            .put_unless("branch", "a", true)
            .count("ahead", 0u32)
            .count("behind", 2u32)
            .into_value()
    }

    #[test]
    fn a_sparse_row_leaves_out_what_says_nothing() {
        assert_eq!(
            example(Detail::Sparse),
            json!({"id": "a", "draft": true, "left": 0, "behind": 2})
        );
    }

    #[test]
    fn a_complete_row_states_every_field() {
        assert_eq!(
            example(Detail::Complete),
            json!({
                "id": "a", "title": null, "pinned": false, "draft": true,
                "edited": [], "label": "", "left": 0, "branch": "a",
                "ahead": 0, "behind": 2,
            })
        );
        assert_eq!(Detail::of(&[]), Detail::Sparse);
        assert_eq!(Detail::of(&["id".into()]), Detail::Complete);
    }

    #[test]
    fn words_are_snake_case_times_seconds_and_fractions_whole_percentages() {
        #[derive(Serialize)]
        enum Example {
            InProgress,
            NeedsLogin,
            Ok,
            Project(&'static str),
        }
        assert_eq!(word(Example::InProgress), json!("in_progress"));
        assert_eq!(word(Example::NeedsLogin), json!("needs_login"));
        assert_eq!(word(Example::Ok), json!("ok"));
        assert_eq!(word(Example::Project("/r")), json!({"project": "/r"}));
        assert_eq!(seconds(1_794_865_909_660), 1_794_865_909);
        assert_eq!(percent(0.74), 74);
        assert_eq!(percent(0.26), 26);
        assert_eq!(percent(0.299), 29);
        assert_eq!(percent(1.5), 100);
        assert_eq!(percent(-0.1), 0);
    }

    #[test]
    fn selection_states_each_named_field() {
        let rows = vec![
            json!({"id": "a", "state": "idle", "title": "x"}),
            json!({"id": "b", "title": "y"}),
        ];
        assert_eq!(
            select(rows, &["state".into(), "id".into()]),
            vec![
                json!({"state": "idle", "id": "a"}),
                json!({"state": null, "id": "b"})
            ]
        );
    }
}
