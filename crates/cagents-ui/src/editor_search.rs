//! Shared bounded search rules for editor, diff and project search surfaces.
use crate::repository_widget::text_editor::{self, MatchBuilder};
pub use crate::repository_widget::text_editor::{MatchIndex, Matches, Position};
pub use crate::settings::EditorFindOptions as FindOptions;

const REGEX_SIZE_LIMIT: usize = 2 * 1024 * 1024;

/// Replacement admission, not a general editing limit: already larger buffers
/// may shrink or retain their byte length, but cannot grow through Replace.
pub(crate) fn replacement_budget(source_bytes: usize) -> usize {
    source_bytes.max(crate::editor::MAX_EDITABLE_FILE_BYTES as usize)
}

fn output_limit_error() -> String {
    "Replacement exceeds the result size limit (8 MiB, or the current buffer size if larger). Use a smaller replacement or replace fewer matches.".into()
}

pub(crate) fn checked_output_size(
    current: usize,
    additional: usize,
    limit: usize,
) -> Result<usize, String> {
    current
        .checked_add(additional)
        .filter(|size| *size <= limit)
        .ok_or_else(output_limit_error)
}

fn reserve_output(result: &mut String, additional: usize, limit: usize) -> Result<(), String> {
    let needed = checked_output_size(result.len(), additional, limit)?;
    if needed > result.capacity() {
        // Amortized growth without requesting capacity beyond the output budget.
        let capacity = needed.max(result.capacity().saturating_mul(2)).min(limit);
        result.try_reserve_exact(capacity - result.len()).map_err(|_| "Not enough memory to prepare this replacement. Use a smaller replacement or replace fewer matches.".to_owned())?;
    }
    Ok(())
}

pub(crate) fn append_output(result: &mut String, text: &str, limit: usize) -> Result<(), String> {
    reserve_output(result, text.len(), limit)?;
    result.push_str(text);
    Ok(())
}

// Mirror the pinned regex 1.13.1 Captures::expand interpolation semantics
// (regex-automata 0.4.18 util/interpolate.rs): longest ASCII unbraced name,
// arbitrary braced name, numeric index when parseable, missing groups empty,
// malformed references literal, and $$ escapes a dollar. Visit borrowed pieces
// to count EXACT bytes before invoking the library's expansion allocator.
fn expanded_size(
    captures: &regex::Captures<'_>,
    mut text: &str,
    limit: usize,
) -> Result<usize, String> {
    let mut size = 0;
    while let Some(dollar) = text.find('$') {
        size = checked_output_size(size, dollar, limit)?;
        text = &text[dollar..];
        if text.starts_with("$$") {
            size = checked_output_size(size, 1, limit)?;
            text = &text[2..];
            continue;
        }
        let reference = if text.starts_with("${") {
            text[2..].find('}').map(|end| (&text[2..2 + end], 3 + end))
        } else {
            let end = text.as_bytes()[1..]
                .iter()
                .take_while(|byte| byte.is_ascii_alphanumeric() || **byte == b'_')
                .count()
                + 1;
            (end > 1).then(|| (&text[1..end], end))
        };
        if let Some((name, end)) = reference {
            let captured = match name.parse::<usize>() {
                Ok(index) => captures.get(index),
                Err(_) => captures.name(name),
            };
            if let Some(captured) = captured {
                size = checked_output_size(size, captured.len(), limit)?;
            }
            text = &text[end..];
        } else {
            size = checked_output_size(size, 1, limit)?;
            text = &text[1..];
        }
    }
    checked_output_size(size, text.len(), limit)
}

fn converted_size(text: &str, case: ReplacementCase, limit: usize) -> Result<usize, String> {
    let mut size = 0;
    for (index, character) in text.chars().enumerate() {
        let bytes = match case {
            ReplacementCase::Upper => character.to_uppercase().map(char::len_utf8).sum(),
            ReplacementCase::Lower => character.to_lowercase().map(char::len_utf8).sum(),
            ReplacementCase::Title if index == 0 => {
                character.to_uppercase().map(char::len_utf8).sum()
            }
            ReplacementCase::Title => character.to_lowercase().map(char::len_utf8).sum(),
            ReplacementCase::Unchanged => character.len_utf8(),
        };
        // str::to_lowercase's contextual final Sigma has the same UTF-8 byte
        // length as ordinary sigma. Actual output still uses full str mapping.
        size = checked_output_size(size, bytes, limit)?;
    }
    Ok(size)
}

pub fn pattern(query: &str, options: FindOptions) -> Result<regex::Regex, String> {
    let expression = if options.whole_word {
        format!(r"\b{{start-half}}(?:{query})\b{{end-half}}")
    } else {
        query.to_owned()
    };
    regex::RegexBuilder::new(&expression)
        .case_insensitive(!options.case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(2 * REGEX_SIZE_LIMIT)
        .build()
        .map_err(|error| {
            let message = error.to_string();
            let line = message
                .lines()
                .last()
                .unwrap_or("Invalid regular expression")
                .trim();
            line.strip_prefix("error: ")
                .unwrap_or(line)
                .chars()
                .take(240)
                .collect()
        })
}

fn word(character: char) -> bool {
    static WORD: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let mut encoded = [0; 4];
    WORD.get_or_init(|| regex::Regex::new(r"^\w$").expect("constant word pattern"))
        .is_match(character.encode_utf8(&mut encoded))
}

fn whole_word(text: &str, start: usize, end: usize) -> bool {
    !text[..start].chars().next_back().is_some_and(word)
        && !text[end..].chars().next().is_some_and(word)
}

/// Positions address source UTF-8 bytes. Cancellation never returns partial results.
pub fn find_in_lines<T: AsRef<str>>(
    lines: impl Iterator<Item = T>,
    query: &str,
    options: FindOptions,
    cancelled: impl Fn() -> bool,
) -> Result<Option<MatchIndex>, String> {
    if query.is_empty() {
        return Ok(Some(MatchIndex::default()));
    }
    let mut matches = MatchBuilder::default();
    if !options.case_sensitive && !options.regex {
        let complete =
            text_editor::visit_matches(lines, query, cancelled, |line, text, start, end| {
                if !options.whole_word || whole_word(text, start, end) {
                    matches.push(
                        Position {
                            line,
                            column: start,
                        },
                        Position { line, column: end },
                    );
                }
            });
        return Ok(complete.then(|| matches.finish()));
    }
    let regex = options.regex.then(|| pattern(query, options)).transpose()?;
    for (line, text) in lines.enumerate() {
        if cancelled() {
            return Ok(None);
        }
        let text = text.as_ref();
        let spans: Box<dyn Iterator<Item = (usize, usize)> + '_> = if let Some(regex) = &regex {
            Box::new(
                regex
                    .find_iter(text)
                    .map(|found| (found.start(), found.end())),
            )
        } else {
            Box::new(
                text.match_indices(query)
                    .map(|(start, _)| (start, start + query.len())),
            )
        };
        for (start, end) in spans {
            if cancelled() {
                return Ok(None);
            }
            if options.whole_word && !options.regex && !whole_word(text, start, end) {
                continue;
            }
            matches.push(
                Position {
                    line,
                    column: start,
                },
                Position { line, column: end },
            );
        }
    }
    Ok(Some(matches.finish()))
}

/// Plain replacements preserve the matched word's all-upper, all-lower or title case.
pub fn preserve_case(matched: &str, replacement: &str) -> String {
    convert_case(replacement, matched_case(matched))
}

#[derive(Clone, Copy)]
enum ReplacementCase {
    Upper,
    Lower,
    Title,
    Unchanged,
}

fn matched_case(matched: &str) -> ReplacementCase {
    let mut letters = matched
        .chars()
        .filter(|character| character.is_alphabetic());
    let Some(first) = letters.next() else {
        return ReplacementCase::Unchanged;
    };
    let (mut upper, mut lower, mut title) = (
        first.is_uppercase(),
        first.is_lowercase(),
        first.is_uppercase(),
    );
    for letter in letters {
        upper &= letter.is_uppercase();
        lower &= letter.is_lowercase();
        title &= letter.is_lowercase();
    }
    if upper {
        ReplacementCase::Upper
    } else if lower {
        ReplacementCase::Lower
    } else if title {
        ReplacementCase::Title
    } else {
        ReplacementCase::Unchanged
    }
}

fn convert_case(replacement: &str, case: ReplacementCase) -> String {
    match case {
        ReplacementCase::Upper => replacement.to_uppercase(),
        // str conversion preserves contextual Unicode mappings, including
        // final Sigma before case-ignorable combining marks.
        ReplacementCase::Lower => replacement.to_lowercase(),
        ReplacementCase::Title => {
            // Lower the whole string before replacing its initial mapping:
            // the tail needs the original prefix for contextual final Sigma.
            let mut result = replacement.to_lowercase();
            if let Some(first) = replacement.chars().next() {
                // Contextual Sigma changes neither the initial mapping's
                // scalar count nor its UTF-8 length. Include every byte when
                // the initial lowercase mapping expands (for example İ).
                let prefix_bytes: usize = first.to_lowercase().map(char::len_utf8).sum();
                result.replace_range(..prefix_bytes, &first.to_uppercase().to_string());
            }
            result
        }
        ReplacementCase::Unchanged => replacement.into(),
    }
}

pub struct Replacement<'a> {
    regex: Option<regex::Regex>,
    text: &'a str,
    preserve: bool,
    uppercase: std::sync::OnceLock<String>,
    lowercase: std::sync::OnceLock<String>,
    titlecase: std::sync::OnceLock<String>,
}

impl<'a> Replacement<'a> {
    pub fn new(
        query: &str,
        text: &'a str,
        options: FindOptions,
        preserve: bool,
    ) -> Result<Self, String> {
        Ok(Self {
            regex: options.regex.then(|| pattern(query, options)).transpose()?,
            text,
            preserve,
            uppercase: std::sync::OnceLock::new(),
            lowercase: std::sync::OnceLock::new(),
            titlecase: std::sync::OnceLock::new(),
        })
    }

    pub fn expand(&self, line: &str, start: usize, end: usize) -> Result<String, String> {
        self.expand_bounded(line, start, end, replacement_budget(line.len()))
    }

    pub(crate) fn expand_bounded(
        &self,
        line: &str,
        start: usize,
        end: usize,
        limit: usize,
    ) -> Result<String, String> {
        let mut result = String::new();
        self.append_bounded(line, start, end, &mut result, limit)?;
        Ok(result)
    }

    /// Replace All streams directly into one output; it does not allocate a
    /// temporary replacement String or letter Vec for every occurrence.
    pub fn append(
        &self,
        line: &str,
        start: usize,
        end: usize,
        result: &mut String,
    ) -> Result<(), String> {
        self.append_bounded(line, start, end, result, replacement_budget(line.len()))
    }

    pub(crate) fn append_bounded(
        &self,
        line: &str,
        start: usize,
        end: usize,
        result: &mut String,
        limit: usize,
    ) -> Result<(), String> {
        if let Some(regex) = &self.regex {
            let captures = regex
                .captures_at(line, start)
                .filter(|captures| {
                    captures
                        .get(0)
                        .is_some_and(|found| found.start() == start && found.end() == end)
                })
                .ok_or_else(|| {
                    "The selected match changed; search again before replacing.".to_owned()
                })?;
            let available = limit
                .checked_sub(result.len())
                .ok_or_else(output_limit_error)?;
            let bytes = expanded_size(&captures, self.text, available)?;
            reserve_output(result, bytes, limit)?;
            captures.expand(self.text, result);
        } else {
            let matched = line
                .get(start..end)
                .ok_or_else(|| "The selected match is outside the current line.".to_owned())?;
            if self.preserve {
                // Each full-string case variant is computed at most once per
                // replacement operation, preserving Unicode context without
                // allocating an intermediate String for every match.
                let case = matched_case(matched);
                let available = limit
                    .checked_sub(result.len())
                    .ok_or_else(output_limit_error)?;
                let cached = match case {
                    ReplacementCase::Upper => self.uppercase.get(),
                    ReplacementCase::Lower => self.lowercase.get(),
                    ReplacementCase::Title => self.titlecase.get(),
                    ReplacementCase::Unchanged => None,
                };
                if let Some(cached) = cached {
                    checked_output_size(0, cached.len(), available)?;
                } else {
                    converted_size(self.text, case, available)?;
                }
                let replacement = match case {
                    ReplacementCase::Upper => self
                        .uppercase
                        .get_or_init(|| convert_case(self.text, case))
                        .as_str(),
                    ReplacementCase::Lower => self
                        .lowercase
                        .get_or_init(|| convert_case(self.text, case))
                        .as_str(),
                    ReplacementCase::Title => self
                        .titlecase
                        .get_or_init(|| convert_case(self.text, case))
                        .as_str(),
                    ReplacementCase::Unchanged => self.text,
                };
                append_output(result, replacement, limit)?;
            } else {
                append_output(result, self.text, limit)?;
            }
        }
        Ok(())
    }
}

pub fn replacement(
    line: &str,
    start: usize,
    end: usize,
    query: &str,
    text: &str,
    options: FindOptions,
    preserve: bool,
) -> Result<String, String> {
    Replacement::new(query, text, options, preserve)?.expand(line, start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn count(text: &str, query: &str, options: FindOptions) -> usize {
        find_in_lines(text.lines(), query, options, || false)
            .unwrap()
            .unwrap()
            .len()
    }
    #[test]
    fn bounded_capture_expansion_matches_pinned_regex_exactly() {
        let regex = regex::Regex::new(r"(?P<名字>λ)(a)?(b)(c)(d)(e)(f)(g)(h)(i)").unwrap();
        let line = "λbcdefghi";
        let captures = regex.captures(line).unwrap();
        let options = FindOptions {
            regex: true,
            ..Default::default()
        };
        for text in [
            "$0:$1:$2:$10",
            "$10x:${10}x:$1_suffix:${1}_suffix",
            "${名字}:$名字:${missing}:$99999999999999999999999999999999999",
            "$$:${}:$:$-:${unclosed",
            "$2$2$2",
            "λ${0}🦀$$",
        ] {
            let mut expected = String::new();
            captures.expand(text, &mut expected);
            let replacement = Replacement::new(regex.as_str(), text, options, false).unwrap();
            assert_eq!(
                expanded_size(&captures, text, expected.len()).unwrap(),
                expected.len(),
                "{text}"
            );
            assert_eq!(
                replacement
                    .expand_bounded(line, 0, line.len(), expected.len())
                    .unwrap(),
                expected,
                "{text}"
            );
            if !expected.is_empty() {
                assert!(
                    replacement
                        .expand_bounded(line, 0, line.len(), expected.len() - 1)
                        .unwrap_err()
                        .contains("result size limit"),
                    "{text}"
                );
            }
        }
        // Missing groups do not justify rejecting a long but empty expansion.
        assert_eq!(
            Replacement::new("(a)", &"$99".repeat(200), options, false)
                .unwrap()
                .expand_bounded("a", 0, 1, 0)
                .unwrap(),
            ""
        );
        assert!(checked_output_size(usize::MAX, 1, usize::MAX).is_err());
    }

    #[test]
    fn bounded_preserve_case_counts_unicode_before_caching_or_appending() {
        for (matched, text) in [
            ("foo", "ΟΣ\u{301}"),
            ("foo", "Ⱥ"),
            ("FOO", "ΐ"),
            ("FOO", "ßﬃ"),
            ("Foo", "ßΟΣ"),
            ("fOo", "λ🦀"),
        ] {
            let replacement = Replacement::new(matched, text, Default::default(), true).unwrap();
            let expected = preserve_case(matched, text);
            assert!(
                replacement
                    .expand_bounded(matched, 0, matched.len(), expected.len() - 1)
                    .is_err()
            );
            assert!(
                replacement.uppercase.get().is_none()
                    && replacement.lowercase.get().is_none()
                    && replacement.titlecase.get().is_none()
            );
            assert_eq!(
                replacement
                    .expand_bounded(matched, 0, matched.len(), expected.len())
                    .unwrap(),
                expected
            );
        }
        let replacement = Replacement::new("a", "λλ", Default::default(), false).unwrap();
        let mut result = "prefix".to_owned();
        let before = result.clone();
        assert!(
            replacement
                .append_bounded("a", 0, 1, &mut result, 9)
                .is_err()
        );
        assert_eq!(result, before);
    }

    #[test]
    fn options_preserve_unicode_literal_search_and_apply_case_regex_and_word_rules() {
        let mut options = FindOptions::default();
        assert_eq!(count("Straße STRASSE", "strasse", options), 2);
        options.case_sensitive = true;
        assert_eq!(count("Value value values", "value", options), 2);
        options.whole_word = true;
        assert_eq!(count("Value value values value_", "value", options), 1);
        options.regex = true;
        assert_eq!(count("value value2", r"value\d?", options), 2);
        assert_eq!(
            count("foo() foo()suffix prefixfoo()", r"foo\(\)", options),
            1
        );
        assert_eq!(count("foobar", "foo|foobar", options), 1);
        assert!(find_in_lines(["a"].into_iter(), "[", options, || false).is_err());
    }
    #[test]
    fn find_matches_all_case_regex_and_word_option_combinations() {
        for case_sensitive in [false, true] {
            for regex in [false, true] {
                for whole_word in [false, true] {
                    let options = FindOptions {
                        case_sensitive,
                        regex,
                        whole_word,
                    };
                    let query = if regex { "f[o]{2}" } else { "foo" };
                    let expected = match (case_sensitive, whole_word) {
                        (false, false) => 4,
                        (false, true) => 3,
                        (true, false) => 2,
                        (true, true) => 1,
                    };
                    assert_eq!(
                        count("foo Foo food FOO", query, options),
                        expected,
                        "{options:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn compact_dense_enumeration_cancellation_never_returns_a_partial_count() {
        let text = "a".repeat(200_000);
        for options in [
            FindOptions::default(),
            FindOptions {
                regex: true,
                ..Default::default()
            },
        ] {
            let calls = std::cell::Cell::new(0);
            let result = find_in_lines(std::iter::once(text.as_str()), "a", options, || {
                calls.set(calls.get() + 1);
                calls.get() == 3
            })
            .unwrap();
            assert!(result.is_none());
            assert_eq!(calls.get(), 3);
        }
    }

    #[test]
    fn preserved_lowercase_keeps_contextual_sigma_and_reuses_each_variant() {
        for (text, expected) in [("ΟΣ", "ος"), ("ΟΣ\u{301}", "ος\u{301}")] {
            assert_eq!(preserve_case("foo", text), expected);
            let replacement = Replacement::new("foo", text, FindOptions::default(), true).unwrap();
            let mut output = String::new();
            replacement.append("foo foo", 0, 3, &mut output).unwrap();
            let allocation = replacement.lowercase.get().unwrap().as_ptr();
            output.push(' ');
            replacement.append("foo foo", 4, 7, &mut output).unwrap();
            assert_eq!(output, format!("{expected} {expected}"));
            assert_eq!(replacement.lowercase.get().unwrap().as_ptr(), allocation);
            assert!(replacement.uppercase.get().is_none());
            assert!(replacement.titlecase.get().is_none());
        }
        assert_eq!(preserve_case("FOO", "ος"), "ΟΣ");
        assert_eq!(preserve_case("fOo", "ΟΣ"), "ΟΣ");
    }

    #[test]
    fn preserved_titlecase_keeps_contextual_sigma_and_reuses_its_variant() {
        for (text, expected) in [
            ("ΟΣ", "Ος"),
            ("ΟΣ\u{301}", "Ος\u{301}"),
            ("İΣ", "İς"),
            ("ßΟΣ", "SSος"),
            ("", ""),
        ] {
            assert_eq!(preserve_case("Foo", text), expected);
            let replacement = Replacement::new("foo", text, FindOptions::default(), true).unwrap();
            let mut output = String::new();
            replacement.append("Foo Foo", 0, 3, &mut output).unwrap();
            let allocation = replacement.titlecase.get().unwrap().as_ptr();
            output.push(' ');
            replacement.append("Foo Foo", 4, 7, &mut output).unwrap();
            assert_eq!(output, format!("{expected} {expected}"));
            assert_eq!(replacement.titlecase.get().unwrap().as_ptr(), allocation);
            assert!(replacement.uppercase.get().is_none());
            assert!(replacement.lowercase.get().is_none());
        }
    }

    #[test]
    fn contextual_titlecase_checks_exact_budget_before_caching_or_appending() {
        for (text, expected) in [
            ("ΟΣ", "Ος"),
            ("ΟΣ\u{301}", "Ος\u{301}"),
            ("İΣ", "İς"),
            ("ßΟΣ", "SSος"),
        ] {
            let replacement = Replacement::new("foo", text, FindOptions::default(), true).unwrap();
            let mut output = "prefix".to_owned();
            let limit = output.len() + expected.len();
            assert!(
                replacement
                    .append_bounded("Foo", 0, 3, &mut output, limit - 1)
                    .unwrap_err()
                    .contains("result size limit")
            );
            assert_eq!(output, "prefix");
            assert!(replacement.titlecase.get().is_none());
            replacement
                .append_bounded("Foo", 0, 3, &mut output, limit)
                .unwrap();
            assert_eq!(output, format!("prefix{expected}"));
            let allocation = replacement.titlecase.get().unwrap().as_ptr();
            let before = output.clone();
            assert!(
                replacement
                    .append_bounded("Foo", 0, 3, &mut output, limit)
                    .is_err()
            );
            assert_eq!(output, before);
            assert_eq!(replacement.titlecase.get().unwrap().as_ptr(), allocation);
        }
    }

    #[test]
    fn replacements_expand_captures_and_preserve_case() {
        let options = FindOptions {
            regex: true,
            ..FindOptions::default()
        };
        assert_eq!(
            replacement("item42", 0, 6, r"item(\d+)", "value$1", options, false).unwrap(),
            "value42"
        );
        for (source, expected) in [
            ("WORD", "VALUE"),
            ("word", "value"),
            ("Word", "Value"),
            ("wOrd", "value"),
        ] {
            assert_eq!(preserve_case(source, "value"), expected);
        }
    }
}
