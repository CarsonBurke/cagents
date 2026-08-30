//! Immutable native diff models computed away from the application update thread.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::editor::{FileIoError, validate_repo_path};

/// A text diff larger than this on either side becomes an explicit read-only state.
pub const MAX_TEXT_DIFF_BYTES_PER_SIDE: u64 = 16 * 1024 * 1024;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSide {
    Previous,
    WorkingTree,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffContent {
    Text(DiffModel),
    Binary {
        side: DiffSide,
    },
    InvalidUtf8 {
        side: DiffSide,
    },
    Symlink {
        target: PathBuf,
    },
    Oversize {
        side: DiffSide,
        bytes: u64,
        limit: u64,
    },
    NotRegularFile,
    Error(String),
}

pub fn load_worktree_diff(worktree: &Path, relative_path: &Path, tracked: bool) -> DiffContent {
    load_worktree_diff_from(worktree, relative_path, tracked.then_some(relative_path))
}

pub fn load_worktree_diff_from(
    worktree: &Path,
    relative_path: &Path,
    previous_path: Option<&Path>,
) -> DiffContent {
    if let Err(error) = validate_repo_path(relative_path) {
        return DiffContent::Error(error.to_string());
    }
    if let Some(previous_path) = previous_path
        && let Err(error) = validate_repo_path(previous_path)
    {
        return DiffContent::Error(error.to_string());
    }
    let after = match read_worktree_side(worktree, relative_path) {
        Ok(SideContent::Text(text)) => text,
        Ok(SideContent::Binary) => {
            return DiffContent::Binary {
                side: DiffSide::WorkingTree,
            };
        }
        Ok(SideContent::InvalidUtf8) => {
            return DiffContent::InvalidUtf8 {
                side: DiffSide::WorkingTree,
            };
        }
        Ok(SideContent::Symlink(target)) => return DiffContent::Symlink { target },
        Ok(SideContent::Oversize(bytes)) => {
            return DiffContent::Oversize {
                side: DiffSide::WorkingTree,
                bytes,
                limit: MAX_TEXT_DIFF_BYTES_PER_SIDE,
            };
        }
        Ok(SideContent::NotRegularFile) => return DiffContent::NotRegularFile,
        Err(error) => return DiffContent::Error(error.to_string()),
    };
    let before = if let Some(previous_path) = previous_path {
        match read_head_side(worktree, previous_path) {
            Ok(SideContent::Text(text)) => text,
            Ok(SideContent::Binary) => {
                return DiffContent::Binary {
                    side: DiffSide::Previous,
                };
            }
            Ok(SideContent::InvalidUtf8) => {
                return DiffContent::InvalidUtf8 {
                    side: DiffSide::Previous,
                };
            }
            Ok(SideContent::Oversize(bytes)) => {
                return DiffContent::Oversize {
                    side: DiffSide::Previous,
                    bytes,
                    limit: MAX_TEXT_DIFF_BYTES_PER_SIDE,
                };
            }
            Ok(SideContent::Symlink(target)) => return DiffContent::Symlink { target },
            Ok(SideContent::NotRegularFile) => String::new(),
            Err(error) => return DiffContent::Error(error.to_string()),
        }
    } else {
        String::new()
    };
    DiffContent::Text(build_diff(&before, &after))
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
        for (offset, text) in before_lines[old_start..old_end].iter().enumerate() {
            inline_rows.push(InlineDiffRow {
                old_line: Some((old_start + offset + 1) as u32),
                new_line: None,
                text: text.clone(),
                kind: DiffRowKind::Deletion,
            });
        }
        for (offset, text) in after_lines[new_start..new_end].iter().enumerate() {
            inline_rows.push(InlineDiffRow {
                old_line: None,
                new_line: Some((new_start + offset + 1) as u32),
                text: text.clone(),
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
            split_rows.push(SplitDiffRow {
                old_line: old_text.map(|_| (old_start + offset + 1) as u32),
                new_line: new_text.map(|_| (new_start + offset + 1) as u32),
                old_text: old_text.cloned(),
                new_text: new_text.cloned(),
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
    before: &[String],
    after: &[String],
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
        let text = before[*old_cursor].clone();
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
            new_text: Some(after[*new_cursor].clone()),
            kind: DiffRowKind::Context,
        });
        *old_cursor += 1;
        *new_cursor += 1;
    }
}

fn display_line(line: &str) -> String {
    line.strip_suffix('\n')
        .and_then(|line| line.strip_suffix('\r').or(Some(line)))
        .unwrap_or(line)
        .to_owned()
}

enum SideContent {
    Text(String),
    Binary,
    InvalidUtf8,
    Symlink(PathBuf),
    Oversize(u64),
    NotRegularFile,
}

fn read_worktree_side(worktree: &Path, relative_path: &Path) -> Result<SideContent, FileIoError> {
    let root = fs::canonicalize(worktree)
        .map_err(|error| FileIoError::InvalidRepositoryRoot(error.to_string()))?;
    let path = root.join(relative_path);
    let parent = path.parent().ok_or(FileIoError::InvalidRepositoryPath)?;
    let canonical_parent =
        fs::canonicalize(parent).map_err(|error| FileIoError::Io(error.to_string()))?;
    if !canonical_parent.starts_with(&root) {
        return Err(FileIoError::OutsideWorktree);
    }
    let metadata =
        fs::symlink_metadata(&path).map_err(|error| FileIoError::Io(error.to_string()))?;
    if metadata.file_type().is_symlink() {
        return fs::read_link(path)
            .map(SideContent::Symlink)
            .map_err(|error| FileIoError::Io(error.to_string()));
    }
    if !metadata.is_file() {
        return Ok(SideContent::NotRegularFile);
    }
    if metadata.len() > MAX_TEXT_DIFF_BYTES_PER_SIDE {
        return Ok(SideContent::Oversize(metadata.len()));
    }
    let bytes = fs::read(path).map_err(|error| FileIoError::Io(error.to_string()))?;
    classify_bytes(bytes)
}

fn read_head_side(worktree: &Path, relative_path: &Path) -> Result<SideContent, FileIoError> {
    let head = git(
        worktree,
        [
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("HEAD"),
        ],
    )?;
    if !head.status.success() {
        return Ok(SideContent::Text(String::new()));
    }
    let listing = git(
        worktree,
        [
            OsString::from("ls-tree"),
            OsString::from("-z"),
            OsString::from("--full-tree"),
            OsString::from("HEAD"),
            OsString::from("--"),
            relative_path.as_os_str().to_owned(),
        ],
    )?;
    if !listing.status.success() {
        return Err(FileIoError::Io(format!(
            "Git could not inspect the previous file: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        )));
    }
    let Some((mode, object)) = parse_tree_entry(&listing.stdout)? else {
        return Ok(SideContent::Text(String::new()));
    };
    if !matches!(mode, b"100644" | b"100755" | b"120000") {
        return Ok(SideContent::NotRegularFile);
    }
    let size = git(
        worktree,
        [
            OsString::from("cat-file"),
            OsString::from("-s"),
            object.clone(),
        ],
    )?;
    if !size.status.success() {
        return Err(FileIoError::Io(format!(
            "Git could not inspect the previous file size: {}",
            String::from_utf8_lossy(&size.stderr).trim()
        )));
    }
    let bytes = std::str::from_utf8(&size.stdout)
        .ok()
        .and_then(|size| size.trim().parse::<u64>().ok())
        .ok_or_else(|| FileIoError::Io("Git returned an invalid blob size".into()))?;
    if bytes > MAX_TEXT_DIFF_BYTES_PER_SIDE {
        return Ok(SideContent::Oversize(bytes));
    }
    let output = git(
        worktree,
        [OsString::from("cat-file"), OsString::from("blob"), object],
    )?;
    if !output.status.success() {
        return Err(FileIoError::Io(format!(
            "Git could not read the previous file: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    if mode == b"120000" {
        return Ok(SideContent::Symlink(bytes_to_path(&output.stdout)));
    }
    classify_bytes(output.stdout)
}

fn parse_tree_entry(output: &[u8]) -> Result<Option<(&[u8], OsString)>, FileIoError> {
    if output.is_empty() {
        return Ok(None);
    }
    if !output.ends_with(&[0]) || output[..output.len() - 1].contains(&0) {
        return Err(FileIoError::Io(
            "Git returned a malformed tree entry".into(),
        ));
    }
    let record = &output[..output.len() - 1];
    let separator = record
        .iter()
        .position(|byte| *byte == b'\t')
        .ok_or_else(|| FileIoError::Io("Git returned a malformed tree entry".into()))?;
    let header = &record[..separator];
    let mut fields = header.split(|byte| *byte == b' ');
    let mode = fields
        .next()
        .filter(|field| !field.is_empty())
        .ok_or_else(|| FileIoError::Io("Git returned a malformed tree mode".into()))?;
    let _kind = fields
        .next()
        .filter(|field| !field.is_empty())
        .ok_or_else(|| FileIoError::Io("Git returned a malformed tree type".into()))?;
    let object = fields
        .next()
        .filter(|field| !field.is_empty())
        .ok_or_else(|| FileIoError::Io("Git returned a malformed tree object".into()))?;
    if fields.next().is_some() {
        return Err(FileIoError::Io(
            "Git returned a malformed tree entry".into(),
        ));
    }
    Ok(Some((mode, bytes_to_os_string(object))))
}

fn git<const N: usize>(
    worktree: &Path,
    arguments: [OsString; N],
) -> Result<std::process::Output, FileIoError> {
    Command::new("git")
        .arg("--literal-pathspecs")
        .arg("-C")
        .arg(worktree)
        .args(arguments)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(|error| FileIoError::Io(error.to_string()))
}

fn bytes_to_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(bytes_to_os_string(bytes))
}

fn bytes_to_os_string(bytes: &[u8]) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes.to_vec())
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

fn classify_bytes(bytes: Vec<u8>) -> Result<SideContent, FileIoError> {
    if bytes.len() as u64 > MAX_TEXT_DIFF_BYTES_PER_SIDE {
        return Ok(SideContent::Oversize(bytes.len() as u64));
    }
    if bytes.contains(&0) {
        return Ok(SideContent::Binary);
    }
    match String::from_utf8(bytes) {
        Ok(text) => Ok(SideContent::Text(text)),
        Err(_) => Ok(SideContent::InvalidUtf8),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use tempfile::tempdir;

    use super::*;

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
    fn pure_insertion_and_deletion_align_split_gutters() {
        let model = build_diff("a\nb\n", "b\nc\n");
        assert_eq!(model.split_rows[0].old_line, Some(1));
        assert_eq!(model.split_rows[0].new_line, None);
        assert_eq!(model.split_rows[2].old_line, None);
        assert_eq!(model.split_rows[2].new_line, Some(2));
    }

    #[test]
    fn real_git_fixture_reads_head_and_worktree_without_shell_or_lossy_text() {
        let root = tempdir().unwrap();
        run_git(root.path(), ["init", "-q"]);
        run_git(root.path(), ["config", "user.name", "Fixture"]);
        run_git(
            root.path(),
            ["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(root.path().join("sample.txt"), "one\ntwo\n").unwrap();
        run_git(root.path(), ["add", "sample.txt"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);
        fs::write(root.path().join("sample.txt"), "one\nsecond\nthree\n").unwrap();

        let DiffContent::Text(model) =
            load_worktree_diff(root.path(), Path::new("sample.txt"), true)
        else {
            panic!("fixture should produce a text diff");
        };
        assert_eq!((model.additions, model.deletions), (2, 1));
        assert_eq!(model.inline_rows[1].old_line, Some(2));
        assert_eq!(model.inline_rows[2].new_line, Some(2));
    }

    #[test]
    fn head_lookup_handles_added_renamed_and_prior_symlink_paths() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        run_git(root.path(), ["init", "-q"]);
        run_git(root.path(), ["config", "user.name", "Fixture"]);
        run_git(
            root.path(),
            ["config", "user.email", "fixture@example.invalid"],
        );
        fs::write(root.path().join("original.txt"), "same\n").unwrap();
        symlink("destination", root.path().join("link")).unwrap();
        run_git(root.path(), ["add", "original.txt", "link"]);
        run_git(root.path(), ["commit", "-qm", "initial"]);

        fs::write(root.path().join("added.txt"), "added\n").unwrap();
        run_git(root.path(), ["add", "added.txt"]);
        let DiffContent::Text(added) =
            load_worktree_diff(root.path(), Path::new("added.txt"), true)
        else {
            panic!("a staged addition should compare against an empty previous side");
        };
        assert_eq!((added.additions, added.deletions), (1, 0));

        run_git(root.path(), ["mv", "original.txt", "renamed.txt"]);
        let DiffContent::Text(renamed) = load_worktree_diff_from(
            root.path(),
            Path::new("renamed.txt"),
            Some(Path::new("original.txt")),
        ) else {
            panic!("a rename should compare against its original HEAD path");
        };
        assert!(renamed.is_empty());

        fs::remove_file(root.path().join("link")).unwrap();
        fs::write(root.path().join("link"), "ordinary file\n").unwrap();
        assert_eq!(
            load_worktree_diff(root.path(), Path::new("link"), true),
            DiffContent::Symlink {
                target: PathBuf::from("destination")
            }
        );
    }

    #[test]
    fn diff_inputs_have_typed_binary_invalid_utf8_and_symlink_states() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        fs::write(root.path().join("binary"), b"a\0b").unwrap();
        fs::write(root.path().join("invalid"), [0xff]).unwrap();
        symlink("binary", root.path().join("link")).unwrap();
        assert_eq!(
            load_worktree_diff(root.path(), Path::new("binary"), false),
            DiffContent::Binary {
                side: DiffSide::WorkingTree
            }
        );
        assert_eq!(
            load_worktree_diff(root.path(), Path::new("invalid"), false),
            DiffContent::InvalidUtf8 {
                side: DiffSide::WorkingTree
            }
        );
        assert_eq!(
            load_worktree_diff(root.path(), Path::new("link"), false),
            DiffContent::Symlink {
                target: PathBuf::from("binary")
            }
        );
    }

    fn run_git<const N: usize>(root: &Path, args: [&str; N]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }
}
