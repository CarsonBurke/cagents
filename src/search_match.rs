//! Shared ranking for already case-folded navigation and command labels.
//! Lower values rank first. Callers retain stable order for equally good matches.
pub fn score(candidate: &str, query: &str) -> Option<usize> {
    if query.is_empty() || candidate == query {
        return Some(0);
    }
    if candidate.starts_with(query) {
        return Some(1_000 + candidate.len().saturating_sub(query.len()).min(999));
    }
    if query.len() > candidate.len() {
        return None;
    }
    if let Some(range) = contiguous_range(candidate, query) {
        let tier = if starts_at_boundary(candidate, range.start) {
            2_000
        } else {
            3_000
        };
        return Some(tier + range.start.min(999));
    }
    let mut remaining = candidate.chars();
    let mut skipped = 0;
    for expected in query.chars() {
        loop {
            let actual = remaining.next()?;
            skipped += 1;
            if actual == expected {
                break;
            }
        }
    }
    Some(4_000 + skipped.min(999))
}

/// Select the contiguous match used by ranking and label highlights.
/// Inputs are already lowercased; the result indexes their UTF-8 bytes.
/// Prefer a prefix, then the earliest word boundary, then the earliest match.
pub(crate) fn contiguous_range(candidate: &str, query: &str) -> Option<std::ops::Range<usize>> {
    if query.is_empty() || query.len() > candidate.len() {
        return None;
    }
    if candidate.starts_with(query) {
        return Some(0..query.len());
    }
    // KMP keeps overlapping matches linear even for a long repeated query.
    // Each prefix entry is the border to retry after a mismatch or full match.
    let needle = query.as_bytes();
    let mut prefix = vec![0; needle.len()];
    for index in 1..needle.len() {
        let mut matched = prefix[index - 1];
        while matched > 0 && needle[index] != needle[matched] {
            matched = prefix[matched - 1];
        }
        if needle[index] == needle[matched] {
            matched += 1;
        }
        prefix[index] = matched;
    }
    let mut contiguous = None;
    let mut matched = 0;
    for (index, &byte) in candidate.as_bytes().iter().enumerate() {
        while matched > 0 && byte != needle[matched] {
            matched = prefix[matched - 1];
        }
        if byte == needle[matched] {
            matched += 1;
        }
        if matched == needle.len() {
            let position = index + 1 - needle.len();
            // A complete valid UTF-8 needle starts at a scalar boundary in
            // the valid candidate, even though the matching scan uses bytes.
            let range = position..index + 1;
            if starts_at_boundary(candidate, position) {
                return Some(range);
            }
            contiguous.get_or_insert(range);
            matched = prefix[matched - 1];
        }
    }
    contiguous
}

fn starts_at_boundary(candidate: &str, position: usize) -> bool {
    candidate[..position]
        .chars()
        .next_back()
        .is_none_or(|ch| !ch.is_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_prefix_boundary_contiguous_and_subsequence_have_distinct_ranks() {
        let labels = [
            "feat",
            "feature login",
            "task feat",
            "alpha-feat",
            "defeated",
            "fast elegant audit trail",
        ];
        let scores: Vec<_> = labels
            .iter()
            .map(|label| score(label, "feat").unwrap())
            .collect();
        assert!(scores.windows(2).all(|pair| pair[0] <= pair[1]));
        assert!(
            score("feature login", "feat") < score("investigate task in alpha-wt-feat", "feat")
        );
        assert!(score("defeated feat", "feat") < score("defeated", "feat"));
        assert_eq!(score("missing", "feat"), None);
    }

    #[test]
    fn overlapping_occurrences_keep_the_best_boundary_rank() {
        assert_eq!(score("xa-a-a", "a-a"), Some(2_003));
        assert_eq!(score("xxxx a-a", "a-a"), Some(2_005));
        assert!(score("xa-a-a", "a-a") < score("xxxx a-a", "a-a"));
        assert_eq!(score("xa-a-a a-a", "a-a"), Some(2_003));
        assert_eq!(score("xaaaaa", "aaa"), Some(3_001));
    }

    #[test]
    fn contiguous_ranges_share_ranked_overlap_and_utf8_boundaries() {
        assert_eq!(contiguous_range("a-a-a", "a-a"), Some(0..3));
        assert_eq!(contiguous_range("xa-a-a a-a", "a-a"), Some(3..6));
        assert_eq!(contiguous_range("éxa-a-a", "a-a"), Some(5..8));
        assert_eq!(contiguous_range("xå-å-å", "å-å"), Some(4..9));
        assert_eq!(contiguous_range("x🦀-🦀-🦀", "🦀-🦀"), Some(6..15));
        assert_eq!(contiguous_range("xååå", "åå"), Some(1..5));
        assert_eq!(contiguous_range("missing", "needle"), None);
        assert_eq!(contiguous_range("å", "åå"), None);
        assert_eq!(contiguous_range("å", ""), None);
        assert_eq!(contiguous_range("", ""), None);
    }

    #[test]
    fn overlapping_matches_preserve_unicode_byte_offsets_and_other_tiers() {
        assert_eq!(score("éxa-a-a", "a-a"), Some(2_005));
        assert_eq!(score("xå-å-å", "å-å"), Some(2_004));
        assert_eq!(score("x🦀-🦀-🦀", "🦀-🦀"), Some(2_006));
        assert_eq!(score("xååå", "åå"), Some(3_001));
        assert_eq!(score("å-å", "å-å"), Some(0));
        assert_eq!(score("å-å-tail", "å-å"), Some(1_005));
        assert_eq!(score("xåzå", "åå"), Some(4_004));
        assert_eq!(score("xå", "åå"), None);
        assert_eq!(score("å", ""), Some(0));
        assert_eq!(score("", ""), Some(0));
        assert_eq!(score("", "å"), None);
    }

    #[test]
    fn long_repeated_inputs_keep_boundary_precedence_and_fuzzy_fallback() {
        let query = "a".repeat(16_384);
        let mut candidate = format!("x{}", "a".repeat(32_768));
        assert_eq!(score(&candidate, &query), Some(3_001));
        let absent = format!("{query}b");
        assert_eq!(score(&candidate, &absent), None);
        candidate.push_str("-b");
        assert_eq!(score(&candidate, &absent), Some(4_999));
        candidate.push(' ');
        candidate.push_str(&query);
        assert_eq!(score(&candidate, &query), Some(2_999));

        let query = "å".repeat(2_048);
        let mut candidate = format!("x{}", "å".repeat(4_096));
        assert_eq!(score(&candidate, &query), Some(3_001));
        candidate.push(' ');
        candidate.push_str(&query);
        assert_eq!(score(&candidate, &query), Some(2_999));
        assert_eq!(score("å", &query), None);
    }
}
