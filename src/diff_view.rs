//! Immutable native diff models computed away from the application update thread.

use std::{collections::HashMap, path::Path};

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::repository::{
    CancellationToken, DiffBasis, DiffContent, DiffRequest, RepoPath, RepositoryService,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffMode {
    Inline,
    Split,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffRowKind {
    Context,
    Addition,
    Deletion,
    Modification,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineDiffRow {
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub text: String,
    pub kind: DiffRowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitDiffRow {
    pub old_line: Option<u32>,
    pub new_line: Option<u32>,
    pub old_text: Option<String>,
    pub new_text: Option<String>,
    pub kind: DiffRowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffModel {
    pub inline_rows: Vec<InlineDiffRow>,
    pub split_rows: Vec<SplitDiffRow>,
    pub inline_change_starts: Vec<usize>,
    pub split_change_starts: Vec<usize>,
    pub additions: u32,
    pub deletions: u32,
}

impl DiffModel {
    pub fn is_empty(&self) -> bool {
        self.additions == 0 && self.deletions == 0
    }

    pub fn change_starts(&self, mode: DiffMode) -> &[usize] {
        match mode {
            DiffMode::Inline => &self.inline_change_starts,
            DiffMode::Split => &self.split_change_starts,
        }
    }

    pub fn row_count(&self, mode: DiffMode) -> usize {
        match mode {
            DiffMode::Inline => self.inline_rows.len(),
            DiffMode::Split => self.split_rows.len(),
        }
    }
}

/// What the diff surface draws for one path and one comparison basis.
///
/// Built on a worker thread: reading both sides and turning them into rows are
/// each unbounded in the size of the file, and neither belongs on the update
/// thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenderedDiff {
    /// Line rows, with the labels of the two sides they were computed from.
    ///
    /// The labels are the service's own, so the chrome names the comparison
    /// being shown — `Index`/`Working tree`, `HEAD`/`Index`,
    /// `main (a1b2c3d4)`/`HEAD`, or a pair of commit ids — instead of one
    /// wording for every basis.
    Text {
        model: DiffModel,
        old_label: String,
        new_label: String,
    },
    /// This comparison has no text rows. The service's account of the sides is
    /// kept verbatim, so the surface reports what they are — absent, binary,
    /// not UTF-8, oversize, a symlink, a submodule, unreadable, or the three
    /// stages of a conflict — rather than an empty or invented diff.
    NoText(DiffContent),
    /// The comparison could not be read at all.
    Error(String),
}

impl RenderedDiff {
    /// Turns one comparison into rows, or into the fact that it has none.
    ///
    /// An added or deleted path still diffs:
    /// [`Absent`](crate::repository::DiffSide::Absent) reads as an empty side.
    /// An unmerged path never does — it has three index stages and no single
    /// old side — so it is passed through instead of being flattened into a
    /// two-sided diff.
    pub fn from_content(content: DiffContent) -> Self {
        match content {
            DiffContent::Comparison {
                old,
                new,
                old_label,
                new_label,
            } => {
                if !old.is_diffable() || !new.is_diffable() {
                    return Self::NoText(DiffContent::Comparison {
                        old,
                        new,
                        old_label,
                        new_label,
                    });
                }
                Self::Text {
                    model: build_diff(
                        old.text().unwrap_or_default(),
                        new.text().unwrap_or_default(),
                    ),
                    old_label,
                    new_label,
                }
            }
            unmerged @ DiffContent::Unmerged { .. } => Self::NoText(unmerged),
        }
    }
}

/// Reads one path's comparison through [`RepositoryService::diff`] and renders
/// it.
///
/// `previous_path` is a rename or copy source: the old side reads it instead of
/// `relative_path`, so a renamed file compares against its own former content
/// rather than against nothing.
pub fn load_repository_diff(
    worktree: &Path,
    relative_path: &Path,
    basis: &DiffBasis,
    previous_path: Option<&Path>,
) -> RenderedDiff {
    // `RepoPath` *is* the path validation: it rejects an absolute path, `.`,
    // `..`, an empty component and an interior NUL, which is what makes the
    // pathspec safe to hand to Git.
    let path = match RepoPath::parse(relative_path) {
        Ok(path) => path,
        Err(error) => return RenderedDiff::Error(error.to_string()),
    };
    let old_path = match previous_path.map(RepoPath::parse).transpose() {
        Ok(old_path) => old_path,
        Err(error) => return RenderedDiff::Error(error.to_string()),
    };
    let request = DiffRequest {
        worktree: worktree.to_path_buf(),
        basis: basis.clone(),
        path,
        old_path,
    };
    // Fenced by the request counter in `RepositoryRuntime`, not by
    // cancellation: a superseded result is rejected on arrival rather than
    // aborted in flight, so this call owns a token nobody else can cancel.
    match RepositoryService::system("git").diff(&request, &CancellationToken::new()) {
        Ok(content) => RenderedDiff::from_content(content),
        Err(error) => RenderedDiff::Error(error.to_string()),
    }
}

pub fn build_diff(before: &str, after: &str) -> DiffModel {
    let input = InternedInput::new(before, after);
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    diff.postprocess_lines(&input);

    let before_lines = input
        .before
        .iter()
        .map(|token| display_line(input.interner[*token]))
        .collect::<Vec<_>>();
    let after_lines = input
        .after
        .iter()
        .map(|token| display_line(input.interner[*token]))
        .collect::<Vec<_>>();
    let hunks = diff.hunks().collect::<Vec<_>>();
    let mut inline_rows = Vec::new();
    let mut split_rows = Vec::new();
    let mut inline_change_starts = Vec::new();
    let mut split_change_starts = Vec::new();
    let mut old_cursor = 0usize;
    let mut new_cursor = 0usize;
    let mut before_endings = HashMap::<&str, u8>::new();
    let mut after_endings = HashMap::<&str, u8>::new();

    for hunk in hunks {
        append_context(
            &before_lines,
            &after_lines,
            &mut old_cursor,
            hunk.before.start as usize,
            &mut new_cursor,
            hunk.after.start as usize,
            &mut inline_rows,
            &mut split_rows,
        );
        inline_change_starts.push(inline_rows.len());
        split_change_starts.push(split_rows.len());

        let old_start = hunk.before.start as usize;
        let old_end = hunk.before.end as usize;
        let new_start = hunk.after.start as usize;
        let new_end = hunk.after.end as usize;
        before_endings.clear();
        after_endings.clear();
        for line in &before_lines[old_start..old_end] {
            *before_endings.entry(&line.text).or_default() |= line.ending.bit();
        }
        for line in &after_lines[new_start..new_end] {
            *after_endings.entry(&line.text).or_default() |= line.ending.bit();
        }
        for (offset, line) in before_lines[old_start..old_end].iter().enumerate() {
            let ending_changed = line.ending_differs(&after_endings);
            inline_rows.push(InlineDiffRow {
                old_line: Some((old_start + offset + 1) as u32),
                new_line: None,
                text: if ending_changed {
                    line.changed_text()
                } else {
                    line.text.clone()
                },
                kind: DiffRowKind::Deletion,
            });
        }
        for (offset, line) in after_lines[new_start..new_end].iter().enumerate() {
            let ending_changed = line.ending_differs(&before_endings);
            inline_rows.push(InlineDiffRow {
                old_line: None,
                new_line: Some((new_start + offset + 1) as u32),
                text: if ending_changed {
                    line.changed_text()
                } else {
                    line.text.clone()
                },
                kind: DiffRowKind::Addition,
            });
        }

        let paired = (old_end - old_start).max(new_end - new_start);
        for offset in 0..paired {
            let old_text = before_lines
                .get(old_start + offset)
                .filter(|_| old_start + offset < old_end);
            let new_text = after_lines
                .get(new_start + offset)
                .filter(|_| new_start + offset < new_end);
            let old_ending_changed =
                old_text.is_some_and(|line| line.ending_differs(&after_endings));
            let new_ending_changed =
                new_text.is_some_and(|line| line.ending_differs(&before_endings));
            split_rows.push(SplitDiffRow {
                old_line: old_text.map(|_| (old_start + offset + 1) as u32),
                new_line: new_text.map(|_| (new_start + offset + 1) as u32),
                old_text: old_text.map(|line| {
                    if old_ending_changed {
                        line.changed_text()
                    } else {
                        line.text.clone()
                    }
                }),
                new_text: new_text.map(|line| {
                    if new_ending_changed {
                        line.changed_text()
                    } else {
                        line.text.clone()
                    }
                }),
                kind: match (old_text, new_text) {
                    (Some(_), Some(_)) => DiffRowKind::Modification,
                    (Some(_), None) => DiffRowKind::Deletion,
                    (None, Some(_)) => DiffRowKind::Addition,
                    (None, None) => unreachable!("a diff hunk cannot be empty on both sides"),
                },
            });
        }
        old_cursor = old_end;
        new_cursor = new_end;
    }
    append_context(
        &before_lines,
        &after_lines,
        &mut old_cursor,
        before_lines.len(),
        &mut new_cursor,
        after_lines.len(),
        &mut inline_rows,
        &mut split_rows,
    );

    DiffModel {
        inline_rows,
        split_rows,
        inline_change_starts,
        split_change_starts,
        additions: diff.count_additions(),
        deletions: diff.count_removals(),
    }
}

#[allow(clippy::too_many_arguments)]
fn append_context(
    before: &[DisplayLine],
    after: &[DisplayLine],
    old_cursor: &mut usize,
    old_end: usize,
    new_cursor: &mut usize,
    new_end: usize,
    inline: &mut Vec<InlineDiffRow>,
    split: &mut Vec<SplitDiffRow>,
) {
    debug_assert_eq!(old_end - *old_cursor, new_end - *new_cursor);
    while *old_cursor < old_end && *new_cursor < new_end {
        let old_line = *old_cursor + 1;
        let new_line = *new_cursor + 1;
        let text = before[*old_cursor].text.clone();
        inline.push(InlineDiffRow {
            old_line: Some(old_line as u32),
            new_line: Some(new_line as u32),
            text: text.clone(),
            kind: DiffRowKind::Context,
        });
        split.push(SplitDiffRow {
            old_line: Some(old_line as u32),
            new_line: Some(new_line as u32),
            old_text: Some(text),
            new_text: Some(after[*new_cursor].text.clone()),
            kind: DiffRowKind::Context,
        });
        *old_cursor += 1;
        *new_cursor += 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LineEnding {
    Lf,
    CrLf,
    None,
}

impl LineEnding {
    const fn bit(self) -> u8 {
        match self {
            Self::Lf => 1,
            Self::CrLf => 2,
            Self::None => 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DisplayLine {
    text: String,
    ending: LineEnding,
}

impl DisplayLine {
    fn ending_differs(&self, other: &HashMap<&str, u8>) -> bool {
        other
            .get(self.text.as_str())
            .is_some_and(|endings| endings & !self.ending.bit() != 0)
    }

    fn changed_text(&self) -> String {
        let ending = match self.ending {
            LineEnding::Lf => "[LF]",
            LineEnding::CrLf => "[CRLF]",
            LineEnding::None => "[no newline]",
        };
        format!("{} {ending}", self.text)
    }
}

fn display_line(line: &str) -> DisplayLine {
    if let Some(text) = line.strip_suffix("\r\n") {
        DisplayLine {
            text: text.into(),
            ending: LineEnding::CrLf,
        }
    } else if let Some(text) = line.strip_suffix('\n') {
        DisplayLine {
            text: text.into(),
            ending: LineEnding::Lf,
        }
    } else {
        DisplayLine {
            text: line.into(),
            ending: LineEnding::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf, process::Command};

    use tempfile::tempdir;

    use super::*;
    use crate::repository::{DiffSide, Oid, ValidatedRefName};

    #[test]
    fn line_model_has_correct_numbers_and_kinds() {
        let model = build_diff("one\ntwo\nthree\n", "one\nsecond\nthree\nfour\n");
        assert_eq!(model.additions, 2);
        assert_eq!(model.deletions, 1);
        assert_eq!(model.inline_change_starts, vec![1, 4]);
        assert_eq!(
            model.inline_rows,
            vec![
                InlineDiffRow {
                    old_line: Some(1),
                    new_line: Some(1),
                    text: "one".into(),
                    kind: DiffRowKind::Context
                },
                InlineDiffRow {
                    old_line: Some(2),
                    new_line: None,
                    text: "two".into(),
                    kind: DiffRowKind::Deletion
                },
                InlineDiffRow {
                    old_line: None,
                    new_line: Some(2),
                    text: "second".into(),
                    kind: DiffRowKind::Addition
                },
                InlineDiffRow {
                    old_line: Some(3),
                    new_line: Some(3),
                    text: "three".into(),
                    kind: DiffRowKind::Context
                },
                InlineDiffRow {
                    old_line: None,
                    new_line: Some(4),
                    text: "four".into(),
                    kind: DiffRowKind::Addition
                },
            ]
        );
        assert_eq!(model.split_change_starts, vec![1, 3]);
        assert_eq!(model.split_rows[1].kind, DiffRowKind::Modification);
        assert_eq!(model.split_rows[1].old_line, Some(2));
        assert_eq!(model.split_rows[1].new_line, Some(2));
    }

    #[test]
    fn line_ending_only_changes_have_distinct_visible_rows() {
        let crlf_to_lf = build_diff("same\r\n", "same\n");
        assert_eq!(crlf_to_lf.inline_rows[0].text, "same [CRLF]");
        assert_eq!(crlf_to_lf.inline_rows[1].text, "same [LF]");
        assert_eq!(
            crlf_to_lf.split_rows[0].old_text.as_deref(),
            Some("same [CRLF]")
        );
        assert_eq!(
            crlf_to_lf.split_rows[0].new_text.as_deref(),
            Some("same [LF]")
        );

        let missing_newline = build_diff("same\n", "same");
        assert_eq!(missing_newline.inline_rows[0].text, "same [LF]");
        assert_eq!(missing_newline.inline_rows[1].text, "same [no newline]");

        let mixed = build_diff("same\r\nold\n", "same\nnew\n");
        assert!(
            mixed
                .inline_rows
                .iter()
                .any(|row| row.text == "same [CRLF]")
        );
        assert!(mixed.inline_rows.iter().any(|row| row.text == "same [LF]"));

        let shifted = build_diff("same\r\n", "added\nsame\n");
        assert!(
            shifted
                .inline_rows
                .iter()
                .any(|row| row.text == "same [CRLF]")
        );
        assert!(
            shifted
                .inline_rows
                .iter()
                .any(|row| row.text == "same [LF]")
        );
    }

    #[test]
    fn pure_insertion_and_deletion_align_split_gutters() {
        let model = build_diff("a\nb\n", "b\nc\n");
        assert_eq!(model.split_rows[0].old_line, Some(1));
        assert_eq!(model.split_rows[0].new_line, None);
        assert_eq!(model.split_rows[2].old_line, None);
        assert_eq!(model.split_rows[2].new_line, Some(2));
    }

    /// Only a side that is text — or absent, which is an empty side — becomes
    /// rows. Every other side the service can report is passed through as the
    /// fact it is, so the surface never draws an empty diff over a file it
    /// could not read.
    #[test]
    fn only_text_and_absent_sides_become_rows() {
        let comparison = |old: DiffSide, new: DiffSide| {
            RenderedDiff::from_content(DiffContent::Comparison {
                old,
                new,
                old_label: "old".into(),
                new_label: "new".into(),
            })
        };
        let text = |text: &str| DiffSide::Text {
            text: text.into(),
            bytes: text.len() as u64,
        };

        assert!(matches!(
            comparison(text("a\n"), text("b\n")),
            RenderedDiff::Text { .. }
        ));
        assert!(matches!(
            comparison(DiffSide::Absent, text("a\n")),
            RenderedDiff::Text { .. }
        ));
        assert!(matches!(
            comparison(text("a\n"), DiffSide::Absent),
            RenderedDiff::Text { .. }
        ));
        assert!(matches!(
            comparison(DiffSide::Absent, DiffSide::Absent),
            RenderedDiff::Text { .. }
        ));

        for opaque in [
            DiffSide::Binary { bytes: 3 },
            DiffSide::InvalidUtf8 { bytes: 1 },
            DiffSide::Oversize { bytes: u64::MAX },
            DiffSide::Submodule {
                oid: Some(fixture_oid()),
            },
            DiffSide::Submodule { oid: None },
            DiffSide::Symlink {
                target: PathBuf::from("elsewhere"),
            },
            DiffSide::NotRegularFile,
            DiffSide::Unreadable {
                reason: "permission denied".into(),
            },
        ] {
            assert!(
                matches!(
                    comparison(opaque.clone(), text("a\n")),
                    RenderedDiff::NoText(_)
                ),
                "{opaque:?} must not be diffed as text on the old side"
            );
            assert!(
                matches!(
                    comparison(text("a\n"), opaque.clone()),
                    RenderedDiff::NoText(_)
                ),
                "{opaque:?} must not be diffed as text on the new side"
            );
        }

        assert!(matches!(
            RenderedDiff::from_content(DiffContent::Unmerged {
                base: text("base\n"),
                ours: text("ours\n"),
                theirs: text("theirs\n"),
                worktree: text("merged\n"),
            }),
            RenderedDiff::NoText(DiffContent::Unmerged { .. })
        ));
    }

    /// A path that could name something outside the worktree is refused before
    /// any Git process starts, on either side of a rename.
    #[test]
    fn an_escaping_path_is_refused_on_either_side() {
        let root = tempdir().unwrap();
        for (path, previous) in [
            (Path::new("../escape.txt"), None),
            (Path::new("/etc/passwd"), None),
            (Path::new(""), None),
            (Path::new("ok.txt"), Some(Path::new("../escape.txt"))),
            (Path::new("ok.txt"), Some(Path::new("nested/../escape.txt"))),
        ] {
            let content =
                load_repository_diff(root.path(), path, &DiffBasis::IndexToWorktree, previous);
            assert!(
                matches!(content, RenderedDiff::Error(_)),
                "{path:?} with previous {previous:?} must be refused, got {content:?}"
            );
        }
    }

    #[test]
    fn real_git_fixture_reads_head_and_worktree_without_shell_or_lossy_text() {
        let root = git_fixture();
        fs::write(root.path().join("sample.txt"), "one\ntwo\n").unwrap();
        run_git(root.path(), ["add", "sample.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::write(root.path().join("sample.txt"), "one\nsecond\nthree\n").unwrap();

        let RenderedDiff::Text {
            model,
            old_label,
            new_label,
        } = load_repository_diff(
            root.path(),
            Path::new("sample.txt"),
            &DiffBasis::IndexToWorktree,
            None,
        )
        else {
            panic!("fixture should produce a text diff");
        };
        assert_eq!((model.additions, model.deletions), (2, 1));
        assert_eq!(model.inline_rows[1].old_line, Some(2));
        assert_eq!(model.inline_rows[2].new_line, Some(2));
        assert_eq!(
            (old_label.as_str(), new_label.as_str()),
            ("Index", "Working tree")
        );
    }

    /// Every one of the service's four bases reaches the loader and reads the
    /// pair of trees it names.
    ///
    /// `other.txt` is the load-bearing assertion for the branch basis: it
    /// exists only on the base branch's tip, so a comparison against that tip
    /// would report it deleted. Against the *merge base* it is absent on both
    /// sides, which is what a branch review must show.
    #[test]
    fn every_basis_reads_the_pair_of_trees_it_names() {
        let root = git_fixture();
        fs::write(root.path().join("base.txt"), "one\n").unwrap();
        run_git(root.path(), ["add", "base.txt"]);
        run_git(root.path(), ["commit", "-qm", "base"]);
        run_git(root.path(), ["checkout", "-q", "-b", "feature"]);
        fs::write(root.path().join("base.txt"), "one\ntwo\n").unwrap();
        run_git(root.path(), ["commit", "-qam", "feature"]);
        run_git(root.path(), ["checkout", "-q", "main"]);
        fs::write(root.path().join("other.txt"), "main only\n").unwrap();
        run_git(root.path(), ["add", "other.txt"]);
        run_git(root.path(), ["commit", "-qm", "main only"]);
        run_git(root.path(), ["checkout", "-q", "feature"]);
        // A staged and an unstaged change on top, so all four bases have
        // something distinct to report for the same path.
        fs::write(root.path().join("base.txt"), "one\ntwo\nstaged\n").unwrap();
        run_git(root.path(), ["add", "base.txt"]);
        fs::write(root.path().join("base.txt"), "one\ntwo\nstaged\nworking\n").unwrap();

        let head = revision(root.path(), "HEAD");
        let parent = revision(root.path(), "HEAD^");

        let unstaged = text_diff(root.path(), "base.txt", &DiffBasis::IndexToWorktree, None);
        assert_eq!(unstaged.0.additions, 1);
        assert_eq!(unstaged.0.deletions, 0);
        assert_eq!(
            (unstaged.1.as_str(), unstaged.2.as_str()),
            ("Index", "Working tree")
        );

        let staged = text_diff(root.path(), "base.txt", &DiffBasis::HeadToIndex, None);
        assert_eq!((staged.0.additions, staged.0.deletions), (1, 0));
        assert_eq!((staged.1.as_str(), staged.2.as_str()), ("HEAD", "Index"));

        let branch = DiffBasis::MergeBaseToHead {
            base_ref: ValidatedRefName::parse("refs/heads/main").unwrap(),
        };
        let merge_base = merge_base(root.path(), "main", "HEAD");
        let compared = text_diff(root.path(), "base.txt", &branch, None);
        // The merge base has `one`; HEAD has `one` and `two`. The staged and
        // working changes are deliberately absent: a committed comparison
        // never reads the index or the worktree.
        assert_eq!((compared.0.additions, compared.0.deletions), (1, 0));
        assert_eq!(compared.0.inline_rows[1].text, "two");
        assert_eq!(
            (compared.1.as_str(), compared.2.as_str()),
            (format!("main ({})", merge_base.short()).as_str(), "HEAD")
        );
        let untouched = text_diff(root.path(), "other.txt", &branch, None);
        assert!(
            untouched.0.is_empty(),
            "a file only the base branch tip has must be absent on both sides of a merge-base comparison, got {:?}",
            untouched.0
        );

        let commit = DiffBasis::CommitToParent {
            oid: head.clone(),
            parent: Some(parent.clone()),
        };
        let committed = text_diff(root.path(), "base.txt", &commit, None);
        assert_eq!((committed.0.additions, committed.0.deletions), (1, 0));
        assert_eq!(committed.0.inline_rows[1].text, "two");
        assert_eq!(
            (committed.1.as_str(), committed.2.as_str()),
            (parent.short(), head.short())
        );
    }

    /// A root commit has no parent, so it compares against the empty tree and
    /// every one of its lines is an addition.
    #[test]
    fn a_root_commit_diffs_against_the_empty_tree() {
        let root = git_fixture();
        fs::write(root.path().join("first.txt"), "one\ntwo\n").unwrap();
        run_git(root.path(), ["add", "first.txt"]);
        run_git(root.path(), ["commit", "-qm", "root"]);
        let first = revision(root.path(), "HEAD");

        let (model, old_label, new_label) = text_diff(
            root.path(),
            "first.txt",
            &DiffBasis::CommitToParent {
                oid: first.clone(),
                parent: None,
            },
            None,
        );
        assert_eq!((model.additions, model.deletions), (2, 0));
        assert_eq!(model.inline_rows[0].old_line, None);
        assert_eq!(model.inline_rows[0].new_line, Some(1));
        assert_eq!(
            (old_label.as_str(), new_label.as_str()),
            ("root commit", first.short())
        );
    }

    /// A conflicted path has no stage 0. Reading that absence as an empty old
    /// side would claim the file was added, so the three conflict stages are
    /// reported instead of a two-sided diff.
    #[test]
    fn a_conflicted_path_renders_as_its_three_stages() {
        let root = git_fixture();
        fs::write(root.path().join("conflict.txt"), "base\n").unwrap();
        run_git(root.path(), ["add", "conflict.txt"]);
        run_git(root.path(), ["commit", "-qm", "base"]);
        run_git(root.path(), ["checkout", "-q", "-b", "other"]);
        fs::write(root.path().join("conflict.txt"), "theirs\n").unwrap();
        run_git(root.path(), ["commit", "-qam", "theirs"]);
        run_git(root.path(), ["checkout", "-q", "main"]);
        fs::write(root.path().join("conflict.txt"), "ours\n").unwrap();
        run_git(root.path(), ["commit", "-qam", "ours"]);
        let merge = Command::new("git")
            .arg("-C")
            .arg(root.path())
            .args(["merge", "other"])
            .output()
            .unwrap();
        assert!(!merge.status.success(), "the merge must conflict");

        let content = load_repository_diff(
            root.path(),
            Path::new("conflict.txt"),
            &DiffBasis::IndexToWorktree,
            None,
        );
        let RenderedDiff::NoText(DiffContent::Unmerged {
            base,
            ours,
            theirs,
            worktree,
        }) = &content
        else {
            panic!("a conflicted path must render as unmerged, got {content:?}");
        };
        assert_eq!(base.text(), Some("base\n"));
        assert_eq!(ours.text(), Some("ours\n"));
        assert_eq!(theirs.text(), Some("theirs\n"));
        // The worktree holds Git's conflict markers, which is neither side.
        assert!(
            worktree.text().is_some_and(|text| text.contains("<<<<<<<")),
            "the worktree stage should hold the conflict markers, got {worktree:?}"
        );
    }

    #[test]
    fn head_lookup_handles_added_renamed_and_prior_symlink_paths() {
        use std::os::unix::fs::symlink;

        let root = git_fixture();
        fs::write(root.path().join("original.txt"), "same\n").unwrap();
        symlink("destination", root.path().join("link")).unwrap();
        run_git(root.path(), ["add", "original.txt", "link"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);

        fs::write(root.path().join("added.txt"), "added\n").unwrap();
        run_git(root.path(), ["add", "added.txt"]);
        let (added, ..) = text_diff(root.path(), "added.txt", &DiffBasis::HeadToIndex, None);
        assert_eq!((added.additions, added.deletions), (1, 0));

        run_git(root.path(), ["mv", "original.txt", "renamed.txt"]);
        let (renamed, ..) = text_diff(
            root.path(),
            "renamed.txt",
            &DiffBasis::HeadToIndex,
            Some(Path::new("original.txt")),
        );
        assert!(
            renamed.is_empty(),
            "a rename should compare against its original HEAD path"
        );

        fs::remove_file(root.path().join("link")).unwrap();
        fs::write(root.path().join("link"), "ordinary file\n").unwrap();
        let content = load_repository_diff(
            root.path(),
            Path::new("link"),
            &DiffBasis::IndexToWorktree,
            None,
        );
        let RenderedDiff::NoText(DiffContent::Comparison { old, new, .. }) = &content else {
            panic!("a symlink replaced by a file has no text diff, got {content:?}");
        };
        assert_eq!(
            old,
            &DiffSide::Symlink {
                target: PathBuf::from("destination")
            }
        );
        assert!(matches!(new, DiffSide::Text { .. }));
    }

    /// A rename is read from its source on the old side for a committed
    /// comparison too, not only for the index bases.
    #[test]
    fn a_committed_rename_reads_the_old_path_on_the_old_side() {
        let root = git_fixture();
        fs::write(root.path().join("old.txt"), "same\n").unwrap();
        run_git(root.path(), ["add", "old.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        run_git(root.path(), ["mv", "old.txt", "new.txt"]);
        run_git(root.path(), ["commit", "-qm", "rename"]);
        let basis = DiffBasis::CommitToParent {
            oid: revision(root.path(), "HEAD"),
            parent: Some(revision(root.path(), "HEAD^")),
        };

        let (renamed, ..) = text_diff(root.path(), "new.txt", &basis, Some(Path::new("old.txt")));
        assert!(
            renamed.is_empty(),
            "the old side must read old.txt, got {renamed:?}"
        );

        // Without the source, the old side is absent and the whole file reads
        // as added — which is exactly why the source has to be carried.
        let (without_source, ..) = text_diff(root.path(), "new.txt", &basis, None);
        assert_eq!((without_source.additions, without_source.deletions), (1, 0));
    }

    #[test]
    fn diff_inputs_have_typed_binary_invalid_utf8_and_symlink_states() {
        use std::os::unix::fs::symlink;

        let root = git_fixture();
        fs::write(root.path().join("binary"), b"a\0b").unwrap();
        fs::write(root.path().join("invalid"), [0xff]).unwrap();
        symlink("binary", root.path().join("link")).unwrap();

        assert_eq!(
            working_side(root.path(), "binary"),
            DiffSide::Binary { bytes: 3 }
        );
        assert_eq!(
            working_side(root.path(), "invalid"),
            DiffSide::InvalidUtf8 { bytes: 1 }
        );
        assert_eq!(
            working_side(root.path(), "link"),
            DiffSide::Symlink {
                target: PathBuf::from("binary")
            }
        );
    }

    /// A tracked file replaced by a directory is reported as what the worktree
    /// now holds, never read as though it were still a file.
    #[test]
    fn a_tracked_path_replaced_by_a_directory_is_not_a_regular_file() {
        let root = git_fixture();
        fs::write(root.path().join("swapped"), "content\n").unwrap();
        run_git(root.path(), ["add", "swapped"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::remove_file(root.path().join("swapped")).unwrap();
        fs::create_dir(root.path().join("swapped")).unwrap();

        assert_eq!(
            working_side(root.path(), "swapped"),
            DiffSide::NotRegularFile
        );
    }

    #[test]
    fn staged_and_unstaged_bases_preserve_opposing_changes() {
        let root = git_fixture();
        fs::write(root.path().join("opposite.txt"), "head\n").unwrap();
        run_git(root.path(), ["add", "opposite.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::write(root.path().join("opposite.txt"), "staged\n").unwrap();
        run_git(root.path(), ["add", "opposite.txt"]);
        fs::write(root.path().join("opposite.txt"), "head\n").unwrap();

        let (staged, ..) = text_diff(root.path(), "opposite.txt", &DiffBasis::HeadToIndex, None);
        let (unstaged, ..) = text_diff(
            root.path(),
            "opposite.txt",
            &DiffBasis::IndexToWorktree,
            None,
        );
        assert_eq!((staged.additions, staged.deletions), (1, 1));
        assert_eq!((unstaged.additions, unstaged.deletions), (1, 1));
        assert_eq!(staged.inline_rows[0].text, "head");
        assert_eq!(staged.inline_rows[1].text, "staged");
        assert_eq!(unstaged.inline_rows[0].text, "staged");
        assert_eq!(unstaged.inline_rows[1].text, "head");
    }

    #[test]
    fn staged_change_plus_unstaged_deletion_has_two_independent_diffs() {
        let root = git_fixture();
        fs::write(root.path().join("deleted.txt"), "head\n").unwrap();
        run_git(root.path(), ["add", "deleted.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::write(root.path().join("deleted.txt"), "indexed\n").unwrap();
        run_git(root.path(), ["add", "deleted.txt"]);
        fs::remove_file(root.path().join("deleted.txt")).unwrap();

        let (staged, ..) = text_diff(root.path(), "deleted.txt", &DiffBasis::HeadToIndex, None);
        let (unstaged, ..) = text_diff(
            root.path(),
            "deleted.txt",
            &DiffBasis::IndexToWorktree,
            None,
        );
        assert_eq!((staged.additions, staged.deletions), (1, 1));
        assert_eq!((unstaged.additions, unstaged.deletions), (0, 1));
        assert_eq!(unstaged.inline_rows[0].text, "indexed");
    }

    #[test]
    fn unstaged_deletion_with_missing_parent_has_an_empty_worktree_side() {
        let root = git_fixture();
        fs::create_dir(root.path().join("nested")).unwrap();
        fs::write(root.path().join("nested/deleted.txt"), "indexed\n").unwrap();
        run_git(root.path(), ["add", "nested/deleted.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::remove_dir_all(root.path().join("nested")).unwrap();

        let (unstaged, ..) = text_diff(
            root.path(),
            "nested/deleted.txt",
            &DiffBasis::IndexToWorktree,
            None,
        );
        assert_eq!((unstaged.additions, unstaged.deletions), (0, 1));
        assert_eq!(unstaged.inline_rows[0].text, "indexed");
    }

    #[test]
    fn unstaged_rename_reads_the_old_index_path() {
        let root = git_fixture();
        fs::write(root.path().join("old.txt"), "same\n").unwrap();
        run_git(root.path(), ["add", "old.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::rename(root.path().join("old.txt"), root.path().join("new.txt")).unwrap();

        let (renamed, ..) = text_diff(
            root.path(),
            "new.txt",
            &DiffBasis::IndexToWorktree,
            Some(Path::new("old.txt")),
        );
        assert!(renamed.is_empty());
    }

    #[test]
    fn staged_deletion_with_untracked_replacement_has_two_bases() {
        let root = git_fixture();
        fs::write(root.path().join("replacement.txt"), "content\n").unwrap();
        run_git(root.path(), ["add", "replacement.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        run_git(root.path(), ["rm", "--cached", "-q", "replacement.txt"]);

        let (staged, ..) = text_diff(
            root.path(),
            "replacement.txt",
            &DiffBasis::HeadToIndex,
            None,
        );
        let (unstaged, ..) = text_diff(
            root.path(),
            "replacement.txt",
            &DiffBasis::IndexToWorktree,
            None,
        );
        assert_eq!((staged.additions, staged.deletions), (0, 1));
        assert_eq!((unstaged.additions, unstaged.deletions), (1, 0));
    }

    /// The rows of a comparison that must be text, with both side labels.
    fn text_diff(
        worktree: &Path,
        path: &str,
        basis: &DiffBasis,
        previous_path: Option<&Path>,
    ) -> (DiffModel, String, String) {
        let content = load_repository_diff(worktree, Path::new(path), basis, previous_path);
        let RenderedDiff::Text {
            model,
            old_label,
            new_label,
        } = content
        else {
            panic!("{path} on {basis:?} should produce a text diff, got {content:?}");
        };
        (model, old_label, new_label)
    }

    /// The working-tree side of an untracked or replaced path, which the
    /// unstaged basis reports as its new side.
    fn working_side(worktree: &Path, path: &str) -> DiffSide {
        let content =
            load_repository_diff(worktree, Path::new(path), &DiffBasis::IndexToWorktree, None);
        let RenderedDiff::NoText(DiffContent::Comparison { new, .. }) = content else {
            panic!("{path} should have no text diff, got {content:?}");
        };
        new
    }

    fn revision(root: &Path, revision: &str) -> Oid {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "--verify", revision])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git rev-parse {revision} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
    }

    fn merge_base(root: &Path, left: &str, right: &str) -> Oid {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["merge-base", left, right])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git merge-base {left} {right} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Oid::parse_object(String::from_utf8(output.stdout).unwrap().trim()).unwrap()
    }

    fn fixture_oid() -> Oid {
        Oid::parse_object(format!("{}1", "0".repeat(39))).unwrap()
    }

    fn git_fixture() -> tempfile::TempDir {
        let root = tempdir().unwrap();
        // An explicit initial branch: `main` is named by the branch-comparison
        // tests, and Git's default depends on the host's configuration.
        run_git(root.path(), ["init", "-q", "-b", "main"]);
        run_git(root.path(), ["config", "user.name", "Fixture"]);
        run_git(
            root.path(),
            ["config", "user.email", "fixture@example.invalid"],
        );
        root
    }

    fn run_git<const N: usize>(root: &Path, args: [&str; N]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
