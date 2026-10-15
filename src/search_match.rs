//! Shared ranking for already case-folded navigation and command labels.
//! Lower values rank first. Callers retain stable order for equally good matches.
pub fn score(candidate: &str, query: &str) -> Option<usize> {
    if query.is_empty() || candidate == query {
        return Some(0);
    }
    if candidate.starts_with(query) {
        return Some(1_000 + candidate.len().saturating_sub(query.len()).min(999));
    }
    let contiguous = candidate
        .match_indices(query)
        .map(|(position, _)| {
            let boundary = candidate[..position]
                .chars()
                .next_back()
                .is_none_or(|ch| !ch.is_alphanumeric());
            (if boundary { 2_000 } else { 3_000 }) + position.min(999)
        })
        .min();
    if contiguous.is_some() {
        return contiguous;
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
}
