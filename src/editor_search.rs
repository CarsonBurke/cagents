//! Shared bounded search rules for editor, diff and project search surfaces.
use crate::editor_widget::Position;
pub use crate::settings::EditorFindOptions as FindOptions;

const REGEX_SIZE_LIMIT: usize = 2 * 1024 * 1024;

pub fn pattern(query: &str, options: FindOptions) -> Result<regex::Regex, String> {
    let expression = if options.whole_word { format!(r"\b{{start-half}}(?:{query})\b{{end-half}}") } else { query.to_owned() };
    regex::RegexBuilder::new(&expression)
        .case_insensitive(!options.case_sensitive)
        .size_limit(REGEX_SIZE_LIMIT)
        .dfa_size_limit(2 * REGEX_SIZE_LIMIT)
        .build().map_err(|error| error.to_string())
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
    lines: impl Iterator<Item = T>, query: &str, options: FindOptions,
    cancelled: impl Fn() -> bool,
) -> Result<Option<Vec<(Position, Position)>>, String> {
    if query.is_empty() { return Ok(Some(Vec::new())); }
    if !options.case_sensitive && !options.regex && !options.whole_word {
        return Ok(crate::editor_widget::find_in_lines(lines, query, cancelled));
    }
    let regex = options.regex.then(|| pattern(query, options)).transpose()?;
    let mut matches = Vec::new();
    for (line, text) in lines.enumerate() {
        if cancelled() { return Ok(None); }
        let text = text.as_ref();
        let spans: Box<dyn Iterator<Item = (usize, usize)> + '_> = if let Some(regex) = &regex {
            Box::new(regex.find_iter(text).map(|found| (found.start(), found.end())))
        } else if options.case_sensitive {
            Box::new(text.match_indices(query).map(|(start, _)| (start, start + query.len())))
        } else {
            let Some(found) = crate::editor_widget::find_in_lines(std::iter::once(text), query, &cancelled) else { return Ok(None); };
            Box::new(found.into_iter().map(|(start, end)| (start.column, end.column)))
        };
        for (index, (start, end)) in spans.enumerate() {
            if index % 1024 == 0 && cancelled() { return Ok(None); }
            if options.whole_word && !options.regex && !whole_word(text, start, end) { continue; }
            matches.push((Position { line, column: start }, Position { line, column: end }));
        }
    }
    Ok(Some(matches))
}

/// Plain replacements preserve the matched word's all-upper, all-lower or title case.
pub fn preserve_case(matched: &str, replacement: &str) -> String {
    let letters: Vec<_> = matched.chars().filter(|character| character.is_alphabetic()).collect();
    if letters.is_empty() { return replacement.into(); }
    if letters.iter().all(|character| character.is_uppercase()) { return replacement.to_uppercase(); }
    if letters.iter().all(|character| character.is_lowercase()) { return replacement.to_lowercase(); }
    if letters[0].is_uppercase() && letters[1..].iter().all(|character| character.is_lowercase()) {
        let mut characters = replacement.chars();
        return characters.next().map(|first| first.to_uppercase().chain(characters.flat_map(char::to_lowercase)).collect()).unwrap_or_default();
    }
    replacement.into()
}

pub struct Replacement<'a> {
    regex: Option<regex::Regex>,
    text: &'a str,
    preserve: bool,
}

impl<'a> Replacement<'a> {
    pub fn new(query: &str, text: &'a str, options: FindOptions, preserve: bool) -> Result<Self, String> {
        Ok(Self { regex: options.regex.then(|| pattern(query, options)).transpose()?, text, preserve })
    }

    pub fn expand(&self, line: &str, start: usize, end: usize) -> Result<String, String> {
        if let Some(regex) = &self.regex {
            let captures = regex.captures_at(line, start).filter(|captures| captures.get(0)
                .is_some_and(|found| found.start() == start && found.end() == end))
                .ok_or_else(|| "The selected match changed; search again before replacing.".to_owned())?;
            let mut expanded = String::new();
            captures.expand(self.text, &mut expanded);
            Ok(expanded)
        } else {
            let matched = line.get(start..end).ok_or_else(|| "The selected match is outside the current line.".to_owned())?;
            Ok(if self.preserve { preserve_case(matched, self.text) } else { self.text.into() })
        }
    }
}

pub fn replacement(line: &str, start: usize, end: usize, query: &str, text: &str,
    options: FindOptions, preserve: bool) -> Result<String, String> {
    Replacement::new(query, text, options, preserve)?.expand(line, start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn count(text: &str, query: &str, options: FindOptions) -> usize {
        find_in_lines(text.lines(), query, options, || false).unwrap().unwrap().len()
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
        assert_eq!(count("foo() foo()suffix prefixfoo()", r"foo\(\)", options), 1);
        assert_eq!(count("foobar", "foo|foobar", options), 1);
        assert!(find_in_lines(["a"].into_iter(), "[", options, || false).is_err());
    }
    #[test]
    fn replacements_expand_captures_and_preserve_case() {
        let options = FindOptions { regex: true, ..FindOptions::default() };
        assert_eq!(replacement("item42", 0, 6, r"item(\d+)", "value$1", options, false).unwrap(), "value42");
        for (source, expected) in [("WORD", "VALUE"), ("word", "value"), ("Word", "Value"), ("wOrd", "value")] {
            assert_eq!(preserve_case(source, "value"), expected);
        }
    }
}
