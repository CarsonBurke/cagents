//! Which of storage's rules a refused write broke, named without the write.
//!
//! A refused write fails on content cagents produced, and the account of the
//! failure names that content: card and conversation identifiers, and here and
//! there a path. That account can be logged but not kept in private
//! diagnostics or printed outside the window. The rule it broke can: it is
//! static text written into this crate, or a name from the SQLite schema.

use std::fmt;

use rusqlite::{ErrorCode, ffi};

use super::StoreError;

/// A write storage refuses because it breaks one of its own rules.
///
/// `check` names the rule and is static: the template `message` was formatted
/// from, or a description written for the rule. It names the rule, never the
/// write, which is what lets it be kept where `message` cannot be.
#[derive(Debug)]
pub struct Violation {
    check: &'static str,
    /// What `{kind}` in `check` stands for, when that is a static noun such
    /// as `card generation` rather than the write's content.
    kind: Option<&'static str>,
    message: String,
}

impl Violation {
    /// `check` must not contain `{` or `}` unless they mark where `message`
    /// holds a value, as in a format template.
    pub(crate) fn new(check: &'static str, message: impl Into<String>) -> Self {
        Self {
            check,
            kind: None,
            message: message.into(),
        }
    }

    /// A violation of a rule shared by several kinds of record, whose check
    /// names the kind: `{kind}` in `check` reads as `kind`, which is static
    /// text like the rest of the check.
    pub(crate) fn of_kind(
        check: &'static str,
        kind: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            check,
            kind: Some(kind),
            message: message.into(),
        }
    }

    pub fn check(&self) -> &'static str {
        self.check
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// The [`StoreError::InvalidState`] of a write that breaks a rule, with a
/// message formatted like `format!`.
///
/// The format string becomes the rule's check, which is why it has to be a
/// literal: what it interpolates is the write's content, and the template
/// alone is not.
macro_rules! invalid_state {
    ($check:literal $(,)?) => {
        $crate::persistence::StoreError::InvalidState($crate::persistence::Violation::new(
            $check,
            format!($check),
        ))
    };
    ($check:literal, $($argument:tt)+) => {
        $crate::persistence::StoreError::InvalidState($crate::persistence::Violation::new(
            $check,
            format!($check, $($argument)+),
        ))
    };
}
pub(crate) use invalid_state;

/// The rule a write storage refused by construction broke.
///
/// Its `Display` is bounded text that carries none of the write's content, so
/// it can be kept in private diagnostics and shown outside the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusedCheck<'a> {
    /// One of storage's own rules, by its static check and the static kind of
    /// record it concerns, when the check names one.
    Rule {
        check: &'static str,
        kind: Option<&'static str>,
    },
    /// The state could not be encoded as JSON. serde_json's category is no
    /// help here: it reports every encoding failure as a syntax error.
    Encoding,
    /// A SQLite constraint. `target` is the schema name SQLite reported for
    /// it - a table and column, or a constraint name - when that is nothing
    /// but identifiers.
    Constraint {
        kind: Option<&'static str>,
        target: Option<&'a str>,
    },
    /// A value larger than SQLite stores.
    TooBig,
    /// A value SQLite cannot store in its column's type.
    TypeMismatch,
}

impl StoreError {
    /// The rule this error says a write broke, when storage refused the write
    /// because of what it contains, so offering the same content again can
    /// only fail the same way.
    ///
    /// Everything else is a condition of storage itself - a lock, a full disk,
    /// a read-only or corrupt file, a base generation that fell behind - which
    /// a retry can outlast and which every other write would hit too.
    pub fn refused_check(&self) -> Option<RefusedCheck<'_>> {
        match self {
            Self::Card { error, .. } => error.refused_check(),
            Self::InvalidState(violation) => Some(RefusedCheck::Rule {
                check: violation.check,
                kind: violation.kind,
            }),
            Self::Serialization(_) => Some(RefusedCheck::Encoding),
            Self::Sqlite(rusqlite::Error::SqliteFailure(error, message)) => match error.code {
                ErrorCode::ConstraintViolation => Some(RefusedCheck::Constraint {
                    kind: constraint_kind(error.extended_code),
                    target: message.as_deref().and_then(constraint_target),
                }),
                ErrorCode::TooBig => Some(RefusedCheck::TooBig),
                ErrorCode::TypeMismatch => Some(RefusedCheck::TypeMismatch),
                _ => None,
            },
            // Decode/missing-row failures describe storage, not this write's
            // content. Quarantining the write would silently strand valid edits.
            Self::Sqlite(_) | Self::CorruptRow { .. } | Self::InvalidUuid { .. } => None,
            Self::Io(_)
            | Self::UnsupportedSchema { .. }
            | Self::StaleGeneration { .. }
            | Self::InvalidBusyTimeout { .. }
            | Self::MutexPoisoned
            | Self::Closed => None,
        }
    }
}

fn constraint_kind(extended_code: std::ffi::c_int) -> Option<&'static str> {
    Some(match extended_code {
        ffi::SQLITE_CONSTRAINT_CHECK => "CHECK",
        ffi::SQLITE_CONSTRAINT_FOREIGNKEY => "FOREIGN KEY",
        ffi::SQLITE_CONSTRAINT_NOTNULL => "NOT NULL",
        ffi::SQLITE_CONSTRAINT_PRIMARYKEY => "PRIMARY KEY",
        ffi::SQLITE_CONSTRAINT_ROWID => "rowid",
        ffi::SQLITE_CONSTRAINT_TRIGGER => "trigger",
        ffi::SQLITE_CONSTRAINT_UNIQUE => "UNIQUE",
        ffi::SQLITE_CONSTRAINT_DATATYPE => "datatype",
        _ => return None,
    })
}

/// The longest schema name a constraint refusal reports.
const CONSTRAINT_TARGET_BYTES: usize = 96;

/// The schema names in a SQLite constraint message such as `UNIQUE constraint
/// failed: cards.terminal_id`, when they are nothing but identifiers.
///
/// A trigger's `RAISE` message or an unnamed `CHECK` expression could say
/// anything the schema does, so only a list of dotted identifiers is kept.
fn constraint_target(message: &str) -> Option<&str> {
    let (_, target) = message.split_once(" constraint failed: ")?;
    let identifier = |part: &str| {
        part.split('.').all(|name| {
            name.starts_with(|first: char| first.is_ascii_alphabetic() || first == '_')
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        })
    };
    (target.len() <= CONSTRAINT_TARGET_BYTES && target.split(", ").all(identifier))
        .then_some(target)
}

impl fmt::Display for RefusedCheck<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Rule { check, kind } => write_template(formatter, check, kind),
            Self::Encoding => formatter.write_str("the state could not be encoded as JSON"),
            Self::Constraint { kind, target } => {
                formatter.write_str("a SQLite ")?;
                if let Some(kind) = kind {
                    write!(formatter, "{kind} ")?;
                }
                formatter.write_str("constraint")?;
                if let Some(target) = target {
                    write!(formatter, " on {target}")?;
                }
                Ok(())
            }
            Self::TooBig => formatter.write_str("a value exceeds SQLite's size limit"),
            Self::TypeMismatch => formatter.write_str("a value does not fit its SQLite column"),
        }
    }
}

impl From<RefusedCheck<'_>> for crate::diagnostics::Check {
    fn from(check: RefusedCheck<'_>) -> Self {
        Self::new(&check.to_string())
    }
}

/// Writes a format template with an ellipsis where each value would go:
/// `chat card {card} changed source identity` reads `chat card … changed
/// source identity`. `{kind}` reads as `kind` when there is one.
fn write_template(
    formatter: &mut fmt::Formatter<'_>,
    template: &str,
    kind: Option<&str>,
) -> fmt::Result {
    use fmt::Write as _;

    let mut characters = template.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.next_if_eq(&'{').is_some() => formatter.write_char('{')?,
            '}' if characters.next_if_eq(&'}').is_some() => formatter.write_char('}')?,
            '{' => {
                let placeholder = characters
                    .by_ref()
                    .take_while(|&skipped| skipped != '}')
                    .collect::<String>();
                match kind {
                    Some(kind) if placeholder == "kind" => formatter.write_str(kind)?,
                    _ => formatter.write_char('…')?,
                }
            }
            character => formatter.write_char(character)?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constraint(extended_code: std::ffi::c_int, message: &str) -> StoreError {
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            ffi::Error::new(extended_code),
            Some(message.to_owned()),
        ))
    }

    #[test]
    fn a_rule_is_named_by_its_template_without_the_values_it_formats() {
        let card = crate::model::CardId::new();
        let path = "/home/someone/private project";
        let error = invalid_state!("chat card {card} changed source identity at {}", path);

        assert_eq!(
            error.to_string(),
            format!("invalid state: chat card {card} changed source identity at {path}")
        );
        let check = error.refused_check().unwrap().to_string();
        assert_eq!(check, "chat card … changed source identity at …");
        assert!(!check.contains(&card.to_string()) && !check.contains(path));
    }

    #[test]
    fn a_rule_within_a_snapshot_is_named_by_the_rule_it_broke() {
        let error = StoreError::Card {
            card: crate::model::CardId::new(),
            error: Box::new(invalid_state!("invalid card ordinal")),
        };

        assert_eq!(
            error.refused_check(),
            Some(RefusedCheck::Rule {
                check: "invalid card ordinal",
                kind: None,
            })
        );
    }

    #[test]
    fn a_rule_shared_by_kinds_of_record_names_the_kind() {
        let error = StoreError::InvalidState(Violation::of_kind(
            "{kind} {id} exhausted its generation counter",
            "card",
            "card 7 exhausted its generation counter",
        ));

        assert_eq!(
            error.refused_check().unwrap().to_string(),
            "card … exhausted its generation counter"
        );
        // Without a kind, `{kind}` is a value like any other.
        assert_eq!(
            invalid_state!("{kind} exhausted", kind = "secret")
                .refused_check()
                .unwrap()
                .to_string(),
            "… exhausted"
        );
    }

    #[test]
    fn escaped_braces_and_format_specifications_render_as_written_and_elided() {
        let error = invalid_state!("{{literal}} {value:?} and {0:>4}", 7, value = "secret");

        assert_eq!(
            error.refused_check().unwrap().to_string(),
            "{literal} … and …"
        );
    }

    #[test]
    fn a_constraint_is_named_by_its_kind_and_schema_identifiers_only() {
        let unique = constraint(
            ffi::SQLITE_CONSTRAINT_UNIQUE,
            "UNIQUE constraint failed: cards.terminal_id, cards.workspace_id",
        );
        assert_eq!(
            unique.refused_check().unwrap().to_string(),
            "a SQLite UNIQUE constraint on cards.terminal_id, cards.workspace_id"
        );

        // An unnamed CHECK reports its expression, and a trigger's RAISE says
        // whatever the schema wrote; neither is kept.
        let expression = constraint(
            ffi::SQLITE_CONSTRAINT_CHECK,
            "CHECK constraint failed: width_fraction > 0 AND title <> 'secret'",
        );
        assert_eq!(
            expression.refused_check().unwrap().to_string(),
            "a SQLite CHECK constraint"
        );
        let raised = constraint(ffi::SQLITE_CONSTRAINT_TRIGGER, "refused card secret title");
        assert_eq!(
            raised.refused_check().unwrap().to_string(),
            "a SQLite trigger constraint"
        );
        let foreign = constraint(
            ffi::SQLITE_CONSTRAINT_FOREIGNKEY,
            "FOREIGN KEY constraint failed",
        );
        assert_eq!(
            foreign.refused_check().unwrap().to_string(),
            "a SQLite FOREIGN KEY constraint"
        );
    }

    #[test]
    fn only_refusals_of_content_name_a_rule() {
        assert_eq!(
            constraint(ffi::SQLITE_BUSY, "database is locked").refused_check(),
            None
        );
        assert_eq!(
            StoreError::StaleGeneration {
                entity: "card".into(),
                expected: 1,
                actual: 2,
            }
            .refused_check(),
            None
        );
        let encoding = StoreError::Serialization(
            serde_json::to_string(&std::collections::HashMap::from([((1, 2), 3)])).unwrap_err(),
        );
        assert_eq!(
            encoding.refused_check().unwrap().to_string(),
            "the state could not be encoded as JSON"
        );
    }
}
