//! Palette matching uses the same contextual lowercase for ranking and paint.
use std::ops::Range;

pub(super) fn lowercase(text: &str) -> String {
    text.to_lowercase()
}

/// Map matched lowercase characters back to complete original UTF-8 scalars.
/// `offset` accounts for the palette category prefix, which is not searchable.
pub(super) fn highlighted_ranges(label: &str, query: &str, offset: usize) -> Vec<Range<usize>> {
    let lower = lowercase(label);
    let needle = lowercase(query);
    if needle.is_empty() {
        return Vec::new();
    }
    // Whole-string lowercase changes Sigma contextually, unlike scalar
    // lowercase. Its mapping still has the same cardinality; scalar mappings
    // therefore supply source ranges, while actual characters come from the
    // complete lowercase string. Expansions such as İ belong to one scalar.
    let source_ranges = label.char_indices().flat_map(|(index, character)| {
        character
            .to_lowercase()
            .map(move |_| offset + index..offset + index + character.len_utf8())
    });
    let folded = lower.char_indices().zip(source_ranges);
    let mut matches = Vec::new();
    if let Some(contiguous) = crate::search_match::contiguous_range(&lower, &needle) {
        for ((index, _), range) in folded {
            if index >= contiguous.end {
                break;
            }
            if index >= contiguous.start {
                append_range(&mut matches, range);
            }
        }
    } else {
        let mut remaining = needle.chars();
        let mut expected = remaining.next();
        for ((_, character), range) in folded {
            if Some(character) == expected {
                append_range(&mut matches, range);
                expected = remaining.next();
                if expected.is_none() {
                    break;
                }
            }
        }
        if expected.is_some() {
            // An unmatched query has no ranked entry or partial highlight.
            return Vec::new();
        }
    }
    matches
}

/// Source ranges arrive sorted. Merge expansions and adjacent matches so the
/// painter can emit each original slice once without a scan per character.
fn append_range(matches: &mut Vec<Range<usize>>, range: Range<usize>) {
    if let Some(last) = matches.last_mut()
        && range.start <= last.end
    {
        last.end = last.end.max(range.end);
    } else {
        matches.push(range);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contextual_sigma_highlights_the_same_original_characters_as_ranking() {
        // Actual palette prefix: "Focus pane: " occupies 12 UTF-8 bytes.
        assert_eq!(lowercase("ΟΣ"), "ος");
        assert_eq!(highlighted_ranges("ΟΣ", "ς", 12), vec![14..16]);
        assert_eq!(highlighted_ranges("ΟΣ", "ος", 12), vec![12..16]);
        assert_eq!(highlighted_ranges("ΟΣ", "ΟΣ", 12), vec![12..16]);
        assert!(highlighted_ranges("ΟΣ", "σ", 12).is_empty());
        assert_eq!(highlighted_ranges("ΟΣ\u{301}", "ς", 12), vec![14..16]);
        assert_eq!(highlighted_ranges("ΟΣΑ", "σ", 12), vec![14..16]);
        assert!(highlighted_ranges("ß", "ss", 0).is_empty());
    }

    #[test]
    fn expansions_and_preceding_unicode_preserve_original_utf8_ranges() {
        assert_eq!(highlighted_ranges("é İΣ", "i\u{307}ς", 12), vec![15..19]);
        assert_eq!(highlighted_ranges("é İΣ", "i", 12), vec![15..17]);
        assert_eq!(highlighted_ranges("é İΣ", "\u{307}", 12), vec![15..17]);
        assert_eq!(highlighted_ranges("🦀 ΟΣ", "ος", 12), vec![17..21]);
        assert_eq!(highlighted_ranges("İ🦀ΟΣ", "iς", 0), vec![0..2, 8..10]);
    }

    #[test]
    fn contiguous_boundary_preference_and_fuzzy_order_are_unchanged() {
        assert_eq!(highlighted_ranges("xfoo foo", "foo", 0), vec![5..8]);
        assert_eq!(highlighted_ranges("foo foo", "foo", 0), vec![0..3]);
        assert_eq!(highlighted_ranges("xfooxfoo", "foo", 0), vec![1..4]);
        assert_eq!(highlighted_ranges("xa-a-a", "a-a", 0), vec![3..6]);
        assert_eq!(highlighted_ranges("éxa-a-a", "a-a", 0), vec![5..8]);
        assert_eq!(
            highlighted_ranges("Foo bar", "fbr", 0),
            vec![0..1, 4..5, 6..7]
        );
        assert!(highlighted_ranges("ΟΣ", "", 12).is_empty());
        assert!(highlighted_ranges("", "ος", 12).is_empty());
        assert!(highlighted_ranges("Foo bar", "fbrz", 0).is_empty());
        assert!(highlighted_ranges("ΟΣ", "οσ", 12).is_empty());
    }

    #[test]
    fn long_repeated_labels_use_the_ranked_contiguous_range() {
        let query = "a".repeat(16_384);
        let mut label = format!("x{}", "a".repeat(32_768));
        assert_eq!(highlighted_ranges(&label, &query, 12), vec![13..16_397]);
        assert!(highlighted_ranges(&label, &format!("{query}b"), 12).is_empty());
        label.push(' ');
        label.push_str(&query);
        assert_eq!(highlighted_ranges(&label, &query, 12), vec![32_782..49_166]);

        let query = "å".repeat(2_048);
        let mut label = format!("x{}", "Å".repeat(4_096));
        label.push(' ');
        label.push_str(&"Å".repeat(2_048));
        assert_eq!(highlighted_ranges(&label, &query, 0), vec![8_194..12_290]);
    }

    #[test]
    fn sparse_fuzzy_highlights_merge_once_and_require_the_complete_query() {
        let label = format!(
            "{}a{}b{}c",
            "x".repeat(8_192),
            "x".repeat(8_192),
            "x".repeat(8_192)
        );
        assert_eq!(
            highlighted_ranges(&label, "abc", 12),
            vec![8_204..8_205, 16_397..16_398, 24_590..24_591]
        );
        assert!(highlighted_ranges(&label, "abcd", 12).is_empty());
        let label = "ab_".repeat(4_096);
        let query = "ab".repeat(4_096);
        let ranges = highlighted_ranges(&label, &query, 0);
        assert_eq!(ranges.len(), 4_096);
        assert_eq!(ranges.first(), Some(&(0..2)));
        assert_eq!(ranges.get(2_048), Some(&(6_144..6_146)));
        assert_eq!(ranges.last(), Some(&(12_285..12_287)));
        assert_eq!(ranges.iter().map(|range| range.len()).sum::<usize>(), 8_192);
    }
}
