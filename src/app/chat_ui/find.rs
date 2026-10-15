//! Local transcript matching and temporary search disclosure ownership.
use std::{
    collections::{HashMap, HashSet, VecDeque},
    ops::Range,
};
use unicode_casefold::UnicodeCaseFold;

mod index;
pub(super) use index::{Input, Match, Paint, Worker};

pub(super) const HIGHLIGHT_LIMIT: usize = 4096;
pub(super) struct Highlights {
    pub(super) ranges: Vec<Range<usize>>,
    pub(super) truncated: bool,
    #[cfg(test)]
    peak_window: usize,
    #[cfg(test)]
    folded_characters: usize,
}
impl Highlights {
    fn empty() -> Self {
        Self {
            ranges: Vec::new(),
            truncated: false,
            #[cfg(test)]
            peak_window: 0,
            #[cfg(test)]
            folded_characters: 0,
        }
    }
}
/// Full Unicode case folding, with byte ranges in the original text. A match
/// must span whole source characters, never half of a case-fold expansion.
pub(super) fn highlights(text: &str, query: &str) -> Highlights {
    matched_ranges(text, query, HIGHLIGHT_LIMIT)
}
#[cfg(test)]
pub(super) fn ranges(text: &str, query: &str) -> Vec<Range<usize>> {
    highlights(text, query).ranges
}

struct Boundary {
    start: Option<usize>,
    end: Option<usize>,
}
/// KMP scans folded characters once. Only query-length source boundaries are
/// retained: large Unicode output never needs a second copy or per-byte map.
struct Matcher {
    needle: Vec<char>,
    prefix: Vec<usize>,
    matched: usize,
    window: VecDeque<Boundary>,
    limit: usize,
    result: Highlights,
}
impl Matcher {
    fn new(needle: Vec<char>, limit: usize) -> Self {
        let mut prefix = vec![0; needle.len()];
        let mut matched = 0;
        for index in 1..needle.len() {
            while matched > 0 && needle[index] != needle[matched] {
                matched = prefix[matched - 1];
            }
            if needle[index] == needle[matched] {
                matched += 1;
            }
            prefix[index] = matched;
        }
        Self {
            window: VecDeque::with_capacity(needle.len()),
            needle,
            prefix,
            matched: 0,
            limit,
            result: Highlights::empty(),
        }
    }
    /// True after the next eligible match beyond the requested range budget.
    fn push(&mut self, character: char, boundary: Boundary) -> bool {
        if self.window.len() == self.needle.len() {
            self.window.pop_front();
        }
        self.window.push_back(boundary);
        #[cfg(test)]
        {
            self.result.peak_window = self.result.peak_window.max(self.window.len());
            self.result.folded_characters += 1;
        }
        while self.matched > 0 && character != self.needle[self.matched] {
            self.matched = self.prefix[self.matched - 1];
        }
        if character == self.needle[self.matched] {
            self.matched += 1;
        }
        if self.matched != self.needle.len() {
            return false;
        }
        // A folded expansion may match only in part. Keep the KMP fallback so
        // that rejecting it does not skip a later overlapping whole character.
        self.matched = self.prefix[self.matched - 1];
        if let (Some(start), Some(end)) = (
            self.window.front().unwrap().start,
            self.window.back().unwrap().end,
        ) {
            if self.result.ranges.len() == self.limit {
                self.result.truncated = true;
                return true;
            }
            self.result.ranges.push(start..end);
            // Like str::match_indices, successful source matches do not overlap.
            self.matched = 0;
        }
        false
    }
}
fn matched_ranges(text: &str, query: &str, limit: usize) -> Highlights {
    matched_ranges_cancellable(text, query, limit, || true).unwrap()
}

fn matched_ranges_cancellable(text: &str, query: &str, limit: usize, mut current: impl FnMut() -> bool) -> Option<Highlights> {
    if query.is_empty() {
        return Some(Highlights::empty());
    }
    let needle: Vec<char> = query.case_fold().collect();
    if needle.is_empty() {
        return Some(Highlights::empty());
    }
    let mut matcher = Matcher::new(needle, limit);
    let mut next_cancel = 0;
    'source: for (offset, character) in text.char_indices() {
        if offset >= next_cancel {
            if !current() { return None; }
            next_cancel = offset.saturating_add(4096);
        }
        if character.is_ascii() {
            if matcher.push(
                character.to_ascii_lowercase(),
                Boundary {
                    start: Some(offset),
                    end: Some(offset + 1),
                },
            ) {
                break;
            }
            continue;
        }
        let mut expansion = std::iter::once(character).case_fold().peekable();
        let mut first = true;
        while let Some(folded) = expansion.next() {
            let boundary = Boundary {
                start: first.then_some(offset),
                end: expansion
                    .peek()
                    .is_none()
                    .then_some(offset + character.len_utf8()),
            };
            first = false;
            if matcher.push(folded, boundary) {
                break 'source;
            }
        }
    }
    current().then_some(matcher.result)
}
pub(super) fn contains(text: &str, query: &str) -> bool {
    matched_ranges(text, query, 0).truncated
}

/// Save only choices changed by search. A manual toggle relinquishes search
/// ownership before editing the visible choice, so closing search preserves it.
#[derive(Default)]
pub(super) struct Disclosure {
    items: HashSet<String>,
    /// Coordination card parts unfolded, by their `unfolded` keys.
    parts: HashSet<String>,
    turns: HashMap<String, Option<bool>>,
}
impl Disclosure {
    pub(super) fn item(&mut self, id: &str, expanded: &mut HashSet<String>) -> bool {
        if expanded.insert(id.to_owned()) {
            self.items.insert(id.to_owned());
            true
        } else {
            false
        }
    }
    pub(super) fn part(&mut self, key: &str, unfolded: &mut HashSet<String>) {
        if unfolded.insert(key.to_owned()) {
            self.parts.insert(key.to_owned());
        }
    }
    pub(super) fn turn(&mut self, key: &str, opened: &mut HashMap<String, bool>) {
        if opened.get(key) != Some(&true) {
            self.turns
                .entry(key.to_owned())
                .or_insert_with(|| opened.get(key).copied());
            opened.insert(key.to_owned(), true);
        }
    }
    /// Read the reader's choice beneath any temporary search disclosure.
    pub(super) fn reader_turn(&self, key: &str, opened: &HashMap<String, bool>) -> Option<bool> {
        self.turns
            .get(key)
            .copied()
            .unwrap_or_else(|| opened.get(key).copied())
    }
    /// Regrouping inherits the reader's choice without taking ownership away
    /// from an active search that temporarily opened the destination group.
    pub(super) fn inherit_turn(
        &mut self,
        key: &str,
        choice: bool,
        opened: &mut HashMap<String, bool>,
    ) {
        if let Some(previous) = self.turns.get_mut(key) {
            *previous = Some(choice);
        } else {
            opened.insert(key.to_owned(), choice);
        }
    }
    pub(super) fn manual_item(&mut self, id: &str) {
        self.items.remove(id);
    }
    pub(super) fn manual_part(&mut self, key: &str) {
        self.parts.remove(key);
    }
    pub(super) fn manual_turn(&mut self, key: &str) {
        self.turns.remove(key);
    }
    pub(super) fn restore(
        &mut self,
        expanded: &mut HashSet<String>,
        unfolded: &mut HashSet<String>,
        opened: &mut HashMap<String, bool>,
    ) {
        for id in self.items.drain() {
            expanded.remove(&id);
        }
        for key in self.parts.drain() {
            unfolded.remove(&key);
        }
        for (key, previous) in self.turns.drain() {
            if let Some(previous) = previous {
                opened.insert(key, previous);
            } else {
                opened.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matching_uses_unicode_fold_and_preserves_original_byte_boundaries() {
        assert_eq!(ranges("Straße CAFÉ", "STRASSE"), vec![0..7]);
        assert_eq!(ranges("Straße CAFÉ", "café"), vec![8..13]);
        assert!(ranges("ß", "s").is_empty());
        assert_eq!(ranges("ßs", "s"), vec![2..3]);
        assert_eq!(ranges("ﬃfi", "fi"), vec![3..5]);
        assert_eq!(ranges("a A", " "), vec![1..2]);
        assert!(ranges("anything", "").is_empty());
    }
    #[test]
    fn common_queries_bound_highlights_without_limiting_discovery() {
        let exact = " ".repeat(HIGHLIGHT_LIMIT);
        let marks = highlights(&exact, " ");
        assert_eq!(marks.ranges.len(), HIGHLIGHT_LIMIT);
        assert!(!marks.truncated);
        let huge = format!("{} late NEEDLE", " ".repeat(HIGHLIGHT_LIMIT * 16));
        let marks = highlights(&huge, " ");
        assert_eq!(marks.ranges.len(), HIGHLIGHT_LIMIT);
        assert!(marks.truncated);
        assert!(contains(&huge, "needle"));
        let unicode = "ß".repeat(HIGHLIGHT_LIMIT + 1);
        let marks = highlights(&unicode, "ss");
        assert!(marks.truncated);
        assert_eq!(marks.ranges.len(), HIGHLIGHT_LIMIT);
        assert_eq!(
            marks.ranges.last(),
            Some(&(2 * (HIGHLIGHT_LIMIT - 1)..2 * HIGHLIGHT_LIMIT))
        );
        assert!(!highlights(&unicode, "s").truncated);
    }

    #[test]
    fn huge_unicode_output_has_query_sized_scratch_and_first_hit_exits_early() {
        let huge = format!("Straße {} late NEEDLE", "漢ß".repeat(100_000));
        let first = matched_ranges(&huge, "STRASSE", 0);
        assert!(first.truncated);
        assert_eq!(first.folded_characters, 7);
        assert!(first.peak_window <= 7);
        let late = highlights(&huge, "needle");
        assert_eq!(late.ranges.len(), 1);
        assert_eq!(&huge[late.ranges[0].clone()], "NEEDLE");
        assert!(late.peak_window <= 6);
        let common = highlights(&huge, "SS");
        assert!(common.truncated);
        assert_eq!(common.ranges.len(), HIGHLIGHT_LIMIT);
        assert!(common.peak_window <= 2);
        assert!(common.folded_characters < 100_000);
    }

    #[test]
    fn streaming_matches_agree_with_whole_source_character_reference() {
        fn reference(text: &str, query: &str) -> Vec<Range<usize>> {
            let query: String = query.case_fold().collect();
            let offsets: Vec<_> = text
                .char_indices()
                .map(|(offset, _)| offset)
                .chain(std::iter::once(text.len()))
                .collect();
            let mut matches = Vec::new();
            let mut after = 0;
            for &start in &offsets {
                if start < after {
                    continue;
                }
                for &end in offsets.iter().filter(|&&end| end > start) {
                    let folded: String = text[start..end].case_fold().collect();
                    if folded == query {
                        matches.push(start..end);
                        after = end;
                        break;
                    }
                }
            }
            matches
        }
        let tokens = ["s", "ß", "f", "ﬃ", "İ", "i", "\u{307}", "漢", " "];
        let queries = [
            "s", "ss", "sss", "f", "fi", "ffi", "SSf", "İ", "i", "i\u{307}", "漢", " ",
        ];
        for a in tokens {
            for b in tokens {
                for c in tokens {
                    let text = format!("{a}{b}{c}");
                    for query in queries {
                        assert_eq!(
                            ranges(&text, query),
                            reference(&text, query),
                            "{text:?} / {query:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn search_restores_only_its_disclosures_and_preserves_manual_changes() {
        let mut expanded = HashSet::from(["manual".to_owned()]);
        let mut opened = HashMap::from([("closed".to_owned(), false)]);
        let mut unfolded = HashSet::from(["card#manual".to_owned()]);
        let mut search = Disclosure::default();
        search.part("card#manual", &mut unfolded);
        search.part("card#body", &mut unfolded);
        search.part("card#prompt-0", &mut unfolded);
        search.manual_part("card#prompt-0");
        assert!(!search.item("manual", &mut expanded));
        assert!(search.item("automatic", &mut expanded));
        assert!(search.item("changed", &mut expanded));
        search.turn("closed", &mut opened);
        search.turn("default", &mut opened);
        search.turn("changed", &mut opened);
        search.manual_item("changed");
        // Explicitly choosing to leave this visible is now a manual choice.
        search.manual_turn("changed");
        search.restore(&mut expanded, &mut unfolded, &mut opened);
        assert_eq!(expanded, HashSet::from(["manual".into(), "changed".into()]));
        assert_eq!(
            unfolded,
            HashSet::from(["card#manual".into(), "card#prompt-0".into()])
        );
        assert_eq!(
            opened,
            HashMap::from([("closed".into(), false), ("changed".into(), true)])
        );
        assert!(search.item("automatic", &mut expanded));
        search.manual_item("automatic");
        expanded.remove("automatic");
        search.restore(&mut expanded, &mut unfolded, &mut opened);
        assert!(!expanded.contains("automatic"));
    }
}
