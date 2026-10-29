//! Change-tree ordering and display coordinates, derived once per snapshot.

use std::{cmp::Ordering, ops::Range};

use crate::repository::{FileTreeEntry, RepoPath};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactRow {
    pub path: RepoPath,
    pub label: String,
    pub depth: usize,
    pub directory: bool,
}

/// Compare digit runs by magnitude without integer parsing or overflow.
/// Distinct spellings stay deterministic: `m2`, `m02`, then `m10`.
pub(crate) fn natural_cmp(mut left: &[u8], mut right: &[u8]) -> Ordering {
    while !left.is_empty() && !right.is_empty() {
        if left[0].is_ascii_digit() && right[0].is_ascii_digit() {
            let a = left
                .iter()
                .position(|byte| !byte.is_ascii_digit())
                .unwrap_or(left.len());
            let b = right
                .iter()
                .position(|byte| !byte.is_ascii_digit())
                .unwrap_or(right.len());
            let left_digits = &left[..a];
            let right_digits = &right[..b];
            let left_value = &left_digits[left_digits
                .iter()
                .position(|byte| *byte != b'0')
                .unwrap_or(a)..];
            let right_value = &right_digits[right_digits
                .iter()
                .position(|byte| *byte != b'0')
                .unwrap_or(b)..];
            let order = left_value
                .len()
                .cmp(&right_value.len())
                .then_with(|| left_value.cmp(right_value))
                .then_with(|| a.cmp(&b));
            if order != Ordering::Equal {
                return order;
            }
            left = &left[a..];
            right = &right[b..];
        } else {
            let order = left[0].cmp(&right[0]);
            if order != Ordering::Equal {
                return order;
            }
            left = &left[1..];
            right = &right[1..];
        }
    }
    left.len().cmp(&right.len())
}

fn path_cmp(left: &FileTreeEntry, right: &FileTreeEntry) -> Ordering {
    let mut a = left.path.components().peekable();
    let mut b = right.path.components().peekable();
    loop {
        match (a.next(), b.next()) {
            (Some(a_component), Some(b_component)) => {
                if a_component != b_component {
                    let a_directory = a.peek().is_some() || left.directory;
                    let b_directory = b.peek().is_some() || right.directory;
                    return b_directory
                        .cmp(&a_directory)
                        .then_with(|| natural_cmp(a_component, b_component));
                }
            }
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
        }
    }
}

/// Fold directory-only single-child chains while keeping the final path as
/// the disclosure identity. A sibling always ends a chain.
pub(crate) fn compact_tree(mut rows: Vec<FileTreeEntry>) -> Vec<CompactRow> {
    rows.sort_unstable_by(path_cmp);
    let mut ends = vec![rows.len(); rows.len()];
    let mut stack: Vec<usize> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        while stack
            .last()
            .is_some_and(|last| rows[*last].depth >= row.depth)
        {
            ends[stack.pop().unwrap()] = index;
        }
        stack.push(index);
    }
    let mut output = Vec::with_capacity(rows.len());
    let mut ancestors: Vec<(usize, usize)> = Vec::new();
    let mut index = 0;
    while index < rows.len() {
        while ancestors.last().is_some_and(|(end, _)| *end <= index) {
            ancestors.pop();
        }
        let depth = ancestors.last().map_or(0, |(_, depth)| depth + 1);
        let start = index;
        while rows[index].directory
            && index + 1 < ends[index]
            && rows[index + 1].directory
            && rows[index + 1].depth == rows[index].depth + 1
            && ends[index + 1] == ends[index]
        {
            index += 1;
        }
        let prefix = rows[start].path.components().count().saturating_sub(1);
        let label = rows[index]
            .path
            .components()
            .skip(prefix)
            .map(String::from_utf8_lossy)
            .collect::<Vec<_>>()
            .join("/");
        output.push(CompactRow {
            path: rows[index].path.clone(),
            label,
            depth,
            directory: rows[index].directory,
        });
        if rows[index].directory {
            ancestors.push((ends[start], depth));
        }
        index += 1;
    }
    output
}

pub(crate) const VIEW_ROWS: usize = 16;
pub(crate) const OVERSCAN: usize = 2;

/// Bounded row construction and full scroll extent; a final scroll offset
/// remains valid when a new snapshot shrinks the list.
pub(crate) fn visible_range(total: usize, first: usize) -> Range<usize> {
    let first = first.min(total.saturating_sub(VIEW_ROWS));
    first.saturating_sub(OVERSCAN)..first.saturating_add(VIEW_ROWS + OVERSCAN).min(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::build_file_tree;

    fn tree(paths: &[&str]) -> Vec<CompactRow> {
        compact_tree(build_file_tree(
            &paths
                .iter()
                .map(|path| RepoPath::parse(path).unwrap())
                .collect::<Vec<_>>(),
        ))
    }

    #[test]
    fn compact_chains_preserve_branch_points_and_disclosure_paths() {
        let compact = tree(&[
            "a/b/c/m10.rs",
            "a/b/c/m2.rs",
            "a/d/e/file.rs",
            "a.b/other.rs",
        ]);
        assert_eq!(
            compact
                .iter()
                .map(|row| (row.label.as_str(), row.depth, row.directory))
                .collect::<Vec<_>>(),
            vec![
                ("a", 0, true),
                ("b/c", 1, true),
                ("m2.rs", 2, false),
                ("m10.rs", 2, false),
                ("d/e", 1, true),
                ("file.rs", 2, false),
                ("a.b", 0, true),
                ("other.rs", 1, false),
            ]
        );
        assert_eq!(compact[1].path, RepoPath::parse("a/b/c").unwrap());
        assert_eq!(compact[4].path, RepoPath::parse("a/d/e").unwrap());
        let chain = tree(&["one/two/three/file.rs"]);
        assert_eq!(chain[0].label, "one/two/three");
        assert_eq!(chain[1].depth, 1);
    }

    #[test]
    fn sibling_directories_stay_before_files_without_breaking_subtree_order() {
        let compact = tree(&["README.md", "src/m10.rs", "src/m2.rs", "src/a.rs", "src.rs"]);
        assert_eq!(
            compact
                .iter()
                .map(|row| row.path.display().into_owned())
                .collect::<Vec<_>>(),
            [
                "src",
                "src/a.rs",
                "src/m2.rs",
                "src/m10.rs",
                "README.md",
                "src.rs"
            ]
        );
    }

    #[test]
    fn natural_order_handles_large_numbers_leading_zeros_and_byte_paths() {
        let mut names = vec![
            b"m10.rs".as_slice(),
            b"m100.rs",
            b"m2.rs",
            b"m02.rs",
            b"m0.rs",
            b"m999999999999999999999999999999.rs",
            b"m\xff.rs",
        ];
        names.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(
            names,
            vec![
                b"m0.rs".as_slice(),
                b"m2.rs",
                b"m02.rs",
                b"m10.rs",
                b"m100.rs",
                b"m999999999999999999999999999999.rs",
                b"m\xff.rs"
            ]
        );
    }

    #[test]
    fn forty_thousand_rows_build_only_the_viewport_and_overscan() {
        for first in [0, 20, 20_000, 39_984, usize::MAX] {
            let visible = visible_range(40_000, first);
            assert!(visible.len() <= VIEW_ROWS + 2 * OVERSCAN);
            assert!(visible.end <= 40_000);
        }
        assert_eq!(visible_range(0, 300), 0..0);
        assert_eq!(visible_range(5, 300), 0..5);
        assert_eq!(visible_range(40_000, 40_000).end, 40_000);
    }
}
