//! Mutation patches retain modes from the same read as their source bytes.

use std::{ops::Range, sync::Arc};

use crate::{
    diff_view::{DiffMode, DiffModel},
    repository::{GitHeadGuard, RepoPath},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Stage,
    Unstage,
    Revert,
}

#[derive(Debug, Clone)]
pub struct Patch {
    pub(crate) patch: Arc<[u8]>,
    pub(crate) cached: bool,
    pub(crate) reverse: bool,
    pub(crate) expected_head: Option<GitHeadGuard>,
    /// Exact replacement for a dirty live buffer's single undo transaction.
    pub(crate) working_text: Option<Arc<String>>,
    pub(crate) buffer_change: Option<Arc<crate::editor_history::TextChange>>,
    pub(crate) working_present: Option<bool>,
    pub(crate) working_mode: Option<u32>,
}

/// A selected change is rebuilt from exact immutable source strings. The
/// original side-presence markers distinguish empty files from missing ones.
pub(crate) fn selected_patch(
    model: &DiffModel,
    old_path: &RepoPath,
    new_path: &RepoPath,
    mode: DiffMode,
    rows: Range<usize>,
    kind: Kind,
) -> Option<Patch> {
    let undo = kind != Kind::Stage;
    let expected_head = match kind {
        Kind::Unstage => Some(model.source_head.clone()?),
        Kind::Stage | Kind::Revert if model.source_head.is_none() => None,
        _ => return None,
    };
    let desired = crate::diff_patch::selected_content(model, &model.after, mode, rows, undo)?;
    let baseline = model.present[0].then_some(model.before.as_ref());
    let current = model.present[1].then_some(model.after.as_ref());
    let (before, after, modes) = if undo {
        let desired_present = model.present[0] || !desired.is_empty();
        (
            desired_present.then_some(desired.as_str()),
            current,
            [
                desired_present
                    .then_some(model.modes[0].or(model.modes[1]))
                    .flatten(),
                model.modes[1],
            ],
        )
    } else {
        let desired_present = model.present[1] || !desired.is_empty();
        (
            baseline,
            desired_present.then_some(desired.as_str()),
            [
                model.modes[0],
                desired_present
                    .then_some(model.modes[1].or(model.modes[0]))
                    .flatten(),
            ],
        )
    };
    // Undo selected content at the currently displayed path. Renaming it
    // back is a whole-file operation, not part of a hunk/line replacement.
    let patch = if undo {
        mode_aware_patch(new_path, new_path, before, after, modes)?
    } else {
        mode_aware_patch(old_path, new_path, before, after, modes)?
    };
    let working_present = model.present[0] || !desired.is_empty();
    (!patch.is_empty()).then_some(Patch {
        patch: patch.into(),
        cached: kind != Kind::Revert,
        reverse: undo,
        expected_head,
        working_text: (kind == Kind::Revert).then(|| Arc::new(desired)),
        buffer_change: None,
        working_present: (kind == Kind::Revert).then_some(working_present),
        working_mode: model.modes[1],
    })
}

/// Existing regular files keep their current mode during a partial change.
/// File creation/deletion carries its measured mode so reversing a deleted
/// executable never recreates it as a non-executable file.
pub(crate) fn mode_aware_patch(
    old_path: &RepoPath,
    new_path: &RepoPath,
    before: Option<&str>,
    after: Option<&str>,
    modes: [Option<u32>; 2],
) -> Option<Vec<u8>> {
    for (text, mode) in [before, after].into_iter().zip(modes) {
        if text.is_some() && !matches!(mode, Some(0o100644 | 0o100755)) {
            return None;
        }
    }
    let patch = crate::diff_patch::unified_patch_paths(old_path, new_path, before, after);
    if patch.is_empty() {
        return Some(patch);
    }
    let marker = match (before, after) {
        (None, Some(_)) => Some(format!("new file mode {:o}\n", modes[1]?)),
        (Some(_), None) => Some(format!("deleted file mode {:o}\n", modes[0]?)),
        _ => None,
    };
    let Some(marker) = marker else {
        return Some(patch);
    };
    let mut result = Vec::with_capacity(patch.len() + marker.len());
    let mut header = true;
    for (index, line) in patch.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if line.starts_with(b"--- ") {
            header = false;
        }
        if header
            && (line.starts_with(b"new file mode ") || line.starts_with(b"deleted file mode "))
        {
            continue;
        }
        result.extend_from_slice(line);
        if index == 0 {
            result.extend_from_slice(marker.as_bytes());
        }
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Write,
        path::Path,
        process::{Command, Stdio},
    };

    fn git(root: &Path, arguments: &[&str], input: Option<&[u8]>) -> Vec<u8> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(root)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Fixture")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if input.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().unwrap();
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "git {arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn executable_creation_and_empty_file_modes_round_trip_through_the_index() {
        for text in ["#!/bin/sh\r\necho hello", ""] {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-b", "main"], None);
            let path = RepoPath::parse("script.sh").unwrap();
            let patch =
                mode_aware_patch(&path, &path, None, Some(text), [None, Some(0o100755)]).unwrap();
            git(root.path(), &["apply", "--cached", "--check"], Some(&patch));
            git(root.path(), &["apply", "--cached"], Some(&patch));
            assert!(git(root.path(), &["ls-files", "--stage"], None).starts_with(b"100755 "));
            assert_eq!(
                git(root.path(), &["show", ":script.sh"], None),
                text.as_bytes()
            );
            git(
                root.path(),
                &["apply", "--cached", "--reverse", "--check"],
                Some(&patch),
            );
            git(
                root.path(),
                &["apply", "--cached", "--reverse"],
                Some(&patch),
            );
            assert!(git(root.path(), &["ls-files"], None).is_empty());
        }
    }

    #[test]
    fn deleted_executable_is_restored_with_its_original_mode_and_exact_bytes() {
        for text in ["#!/bin/sh\r\necho hello", ""] {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-b", "main"], None);
            fs::write(root.path().join("script.sh"), text).unwrap();
            git(root.path(), &["add", "script.sh"], None);
            git(
                root.path(),
                &["update-index", "--chmod=+x", "script.sh"],
                None,
            );
            git(root.path(), &["commit", "-m", "original"], None);
            let path = RepoPath::parse("script.sh").unwrap();
            let patch =
                mode_aware_patch(&path, &path, Some(text), None, [Some(0o100755), None]).unwrap();
            git(root.path(), &["apply", "--cached", "--check"], Some(&patch));
            git(root.path(), &["apply", "--cached"], Some(&patch));
            assert!(git(root.path(), &["ls-files"], None).is_empty());
            git(
                root.path(),
                &["apply", "--cached", "--reverse", "--check"],
                Some(&patch),
            );
            git(
                root.path(),
                &["apply", "--cached", "--reverse"],
                Some(&patch),
            );
            assert!(git(root.path(), &["ls-files", "--stage"], None).starts_with(b"100755 "));
            assert_eq!(
                git(root.path(), &["show", ":script.sh"], None),
                text.as_bytes()
            );
        }
    }

    #[test]
    fn partial_unstage_restores_only_the_selected_change_and_requires_reader_head() {
        use crate::{
            diff_view::{DiffRowKind, build_diff},
            repository::Oid,
        };
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-b", "main"], None);
            let before = (0..40)
                .map(|index| format!("line {index}\r\n"))
                .collect::<String>();
            let after = before
                .replace("line 3\r\n", "FIRST\r\n")
                .replace("line 30\r\n", "SECOND\r\n");
            fs::write(root.path().join("file.txt"), &before).unwrap();
            git(root.path(), &["add", "."], None);
            git(root.path(), &["commit", "-m", "baseline"], None);
            let head = String::from_utf8(git(root.path(), &["rev-parse", "HEAD"], None)).unwrap();
            let mut model = build_diff(&before, &after);
            model.source_head = Some(GitHeadGuard::Commit(Oid::parse(head.trim()).unwrap()));
            model.modes = [Some(0o100644); 2];
            model.present = [true; 2];
            let selected = match mode {
                DiffMode::Inline => model
                    .inline_rows
                    .iter()
                    .position(|row| row.kind != DiffRowKind::Context)
                    .unwrap(),
                DiffMode::Split => model
                    .split_rows
                    .iter()
                    .position(|row| row.kind != DiffRowKind::Context)
                    .unwrap(),
            };
            // Inline selection includes the removed and inserted half of one edit.
            let end = selected + if mode == DiffMode::Inline { 2 } else { 1 };
            fs::write(root.path().join("file.txt"), &after).unwrap();
            git(root.path(), &["add", "."], None);
            let path = RepoPath::parse("file.txt").unwrap();
            let plan =
                selected_patch(&model, &path, &path, mode, selected..end, Kind::Unstage).unwrap();
            assert!(plan.cached && plan.reverse && plan.expected_head.is_some());
            git(
                root.path(),
                &["apply", "--cached", "--reverse", "--check"],
                Some(&plan.patch),
            );
            git(
                root.path(),
                &["apply", "--cached", "--reverse"],
                Some(&plan.patch),
            );
            assert_eq!(
                git(root.path(), &["show", ":file.txt"], None),
                before.replace("line 30\r\n", "SECOND\r\n").as_bytes()
            );
            assert_eq!(
                fs::read(root.path().join("file.txt")).unwrap(),
                after.as_bytes()
            );
            model.source_head = None;
            assert!(
                selected_patch(&model, &path, &path, mode, selected..end, Kind::Unstage).is_none()
            );
        }
    }

    #[test]
    fn partial_unstage_of_a_rename_preserves_the_current_index_path() {
        use crate::{
            diff_view::{DiffRowKind, build_diff},
            repository::Oid,
        };
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let root = tempfile::tempdir().unwrap();
            git(root.path(), &["init", "-b", "main"], None);
            let before = (0..40)
                .map(|line| format!("line {line}\n"))
                .collect::<String>();
            let after = before
                .replace("line 3\n", "FIRST\n")
                .replace("line 30\n", "SECOND\n");
            fs::write(root.path().join("old.txt"), &before).unwrap();
            git(root.path(), &["add", "."], None);
            git(root.path(), &["commit", "-m", "baseline"], None);
            let head = String::from_utf8(git(root.path(), &["rev-parse", "HEAD"], None)).unwrap();
            git(root.path(), &["mv", "old.txt", "new.txt"], None);
            fs::write(root.path().join("new.txt"), &after).unwrap();
            git(root.path(), &["add", "new.txt"], None);
            let mut model = build_diff(&before, &after);
            model.source_head = Some(GitHeadGuard::Commit(Oid::parse(head.trim()).unwrap()));
            model.modes = [Some(0o100644); 2];
            model.present = [true; 2];
            let first = match mode {
                DiffMode::Inline => model
                    .inline_rows
                    .iter()
                    .position(|row| row.kind != DiffRowKind::Context)
                    .unwrap(),
                DiffMode::Split => model
                    .split_rows
                    .iter()
                    .position(|row| row.kind != DiffRowKind::Context)
                    .unwrap(),
            };
            let end = first + if mode == DiffMode::Inline { 2 } else { 1 };
            let plan = selected_patch(
                &model,
                &RepoPath::parse("old.txt").unwrap(),
                &RepoPath::parse("new.txt").unwrap(),
                mode,
                first..end,
                Kind::Unstage,
            )
            .unwrap();
            git(
                root.path(),
                &["apply", "--cached", "--reverse", "--check"],
                Some(&plan.patch),
            );
            git(
                root.path(),
                &["apply", "--cached", "--reverse"],
                Some(&plan.patch),
            );
            assert_eq!(git(root.path(), &["ls-files", "-z"], None), b"new.txt\0");
            assert_eq!(
                git(root.path(), &["show", ":new.txt"], None),
                before.replace("line 30\n", "SECOND\n").as_bytes()
            );
            assert_eq!(
                fs::read(root.path().join("new.txt")).unwrap(),
                after.as_bytes()
            );
            assert!(!root.path().join("old.txt").exists());
        }
    }

    #[test]
    fn unknown_and_nonregular_modes_never_authorize_a_text_mutation() {
        let path = RepoPath::parse("path").unwrap();
        for mode in [None, Some(0o120000), Some(0o160000)] {
            assert!(mode_aware_patch(&path, &path, None, Some("text"), [None, mode]).is_none());
        }
    }
}
