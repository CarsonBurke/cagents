//! Exact partial changes assembled from immutable diff coordinates.

use std::{collections::BTreeSet, ops::Range};

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::{
    diff_view::{DiffMode, DiffModel, DiffRowKind},
    repository::RepoPath,
};

/// Apply or undo only the changed rows selected in the displayed layout.
/// Source strings, rather than display text, preserve CRLF and an absent final newline.
pub fn selected_content(
    model: &DiffModel,
    after: &str,
    mode: DiffMode,
    selected: Range<usize>,
    undo: bool,
) -> Option<String> {
    let mut deleted = BTreeSet::new();
    let mut added = BTreeSet::new();
    match mode {
        DiffMode::Inline => {
            for (index, row) in model.inline_rows.iter().enumerate() {
                if selected.contains(&index) && row.kind != DiffRowKind::Context {
                    deleted.extend(row.old_line);
                    added.extend(row.new_line);
                }
            }
        }
        DiffMode::Split => {
            for (index, row) in model.split_rows.iter().enumerate() {
                if selected.contains(&index) && row.kind != DiffRowKind::Context {
                    deleted.extend(row.old_line);
                    added.extend(row.new_line);
                }
            }
        }
    }
    if deleted.is_empty() && added.is_empty() {
        return None;
    }
    let old: Vec<_> = model.before.split_inclusive('\n').collect();
    let new: Vec<_> = after.split_inclusive('\n').collect();
    let mut result = String::new();
    for row in &model.inline_rows {
        match row.kind {
            DiffRowKind::Context => {
                let line = if undo { new.get(row.new_line?.checked_sub(1)? as usize)? }
                    else { old.get(row.old_line?.checked_sub(1)? as usize)? };
                result.push_str(line);
            }
            DiffRowKind::Deletion => {
                let line = row.old_line?;
                if deleted.contains(&line) == undo {
                    result.push_str(old.get(line.checked_sub(1)? as usize)?);
                }
            }
            DiffRowKind::Addition => {
                let line = row.new_line?;
                if added.contains(&line) != undo {
                    result.push_str(new.get(line.checked_sub(1)? as usize)?);
                }
            }
            DiffRowKind::Modification => return None,
        }
    }
    Some(result)
}

/// A minimal contextual unified patch. `None` side denotes file absence;
/// an empty present file remains distinct from a deleted file.
pub fn unified_patch(path: &RepoPath, before: Option<&str>, after: Option<&str>) -> Vec<u8> {
    unified_patch_paths(path, path, before, after)
}

/// A renamed comparison keeps each tree's path, so applying a selected hunk
/// against its baseline performs the rename as well as the selected edits.
pub fn unified_patch_paths(
    old_path: &RepoPath,
    new_path: &RepoPath,
    before: Option<&str>,
    after: Option<&str>,
) -> Vec<u8> {
    let renamed = old_path != new_path && before.is_some() && after.is_some();
    if before == after && !renamed {
        return Vec::new();
    }
    let input = InternedInput::new(before.unwrap_or_default(), after.unwrap_or_default());
    let mut compared = Diff::compute(Algorithm::Histogram, &input);
    compared.postprocess_lines(&input);
    let old: Vec<_> = input
        .before
        .iter()
        .map(|token| input.interner[*token])
        .collect();
    let new: Vec<_> = input
        .after
        .iter()
        .map(|token| input.interner[*token])
        .collect();
    let changes: Vec<_> = compared
        .hunks()
        .map(|change| {
            (
                change.before.start as usize..change.before.end as usize,
                change.after.start as usize..change.after.end as usize,
            )
        })
        .collect();
    let mut patch = Vec::new();
    patch.extend_from_slice(b"diff --git ");
    patch.extend(quoted_path(b'a', old_path));
    patch.push(b' ');
    patch.extend(quoted_path(b'b', new_path));
    patch.push(b'\n');
    if renamed {
        patch.extend_from_slice(b"rename from ");
        patch.extend(quoted_bytes(old_path.as_bytes()));
        patch.extend_from_slice(b"\nrename to ");
        patch.extend(quoted_bytes(new_path.as_bytes()));
        patch.push(b'\n');
    }
    // /dev/null headers represent text-file presence without guessing the
    // existing file's mode. Empty files have no textual hunk, so their
    // presence-only patch needs the ordinary regular-file mode marker.
    // Mutation callers that restore executable modes must supply mode metadata.
    if before.is_none() && after == Some("") {
        patch.extend_from_slice(b"new file mode 100644\n");
    }
    if after.is_none() && before == Some("") {
        patch.extend_from_slice(b"deleted file mode 100644\n");
    }
    patch.extend_from_slice(b"--- ");
    patch.extend(before.map_or_else(|| b"/dev/null".to_vec(), |_| quoted_path(b'a', old_path)));
    patch.extend_from_slice(b"\n+++ ");
    patch.extend(after.map_or_else(|| b"/dev/null".to_vec(), |_| quoted_path(b'b', new_path)));
    patch.push(b'\n');
    let mut index = 0;
    while index < changes.len() {
        let first = index;
        while index + 1 < changes.len()
            && changes[index + 1]
                .0
                .start
                .saturating_sub(changes[index].0.end)
                <= 6
        {
            index += 1;
        }
        let last = index;
        let lead = changes[first].0.start.min(3);
        let trail = old.len().saturating_sub(changes[last].0.end).min(3);
        let old_start = changes[first].0.start - lead;
        let new_start = changes[first].1.start - lead;
        let old_count = changes[last].0.end + trail - old_start;
        let new_count = changes[last].1.end + trail - new_start;
        patch.extend_from_slice(
            format!(
                "@@ -{},{} +{},{} @@\n",
                old_start + usize::from(old_count > 0),
                old_count,
                new_start + usize::from(new_count > 0),
                new_count
            )
            .as_bytes(),
        );
        let mut cursor = old_start;
        for (removed, inserted) in &changes[first..=last] {
            for line in &old[cursor..removed.start] {
                patch_line(&mut patch, b' ', line);
            }
            for line in &old[removed.clone()] {
                patch_line(&mut patch, b'-', line);
            }
            for line in &new[inserted.clone()] {
                patch_line(&mut patch, b'+', line);
            }
            cursor = removed.end;
        }
        for line in &old[cursor..cursor + trail] {
            patch_line(&mut patch, b' ', line);
        }
        index += 1;
    }
    patch
}

fn patch_line(patch: &mut Vec<u8>, prefix: u8, line: &str) {
    patch.push(prefix);
    patch.extend_from_slice(line.as_bytes());
    if !line.ends_with('\n') {
        patch.extend_from_slice(b"\n\\ No newline at end of file\n");
    }
}

fn quoted_path(side: u8, path: &RepoPath) -> Vec<u8> {
    let mut bytes = vec![side, b'/'];
    bytes.extend_from_slice(path.as_bytes());
    quoted_bytes(&bytes)
}

fn quoted_bytes(bytes: &[u8]) -> Vec<u8> {
    let mut result = vec![b'"'];
    for byte in bytes {
        match byte {
            b'"' | b'\\' => {
                result.push(b'\\');
                result.push(*byte);
            }
            32..=126 => result.push(*byte),
            _ => result.extend_from_slice(format!("\\{byte:03o}").as_bytes()),
        }
    }
    result.push(b'"');
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Write,
        process::{Command, Stdio},
    };

    fn git(root: &std::path::Path, args: &[&str], stdin: Option<&[u8]>) -> Vec<u8> {
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        if let Some(bytes) = stdin {
            child.stdin.take().unwrap().write_all(bytes).unwrap();
        } else {
            drop(child.stdin.take());
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn exact_patch_round_trips_crlf_no_newline_additions_and_deletions() {
        for (before, after) in [
            (Some("a\r\nb\r\n"), Some("a\r\nchanged\r\n")),
            (Some("no newline"), Some("changed tail")),
            (None, Some("new\r\nlast")),
            (Some("deleted\n"), None),
            (Some(""), Some("new\n")),
            (None, Some("")),
            (Some(""), None),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            git(root, &["init", "-q"], None);
            let path = RepoPath::parse("file with spaces.txt").unwrap();
            if let Some(text) = before {
                fs::write(root.join(path.as_path()), text).unwrap();
                git(root, &["add", "--", "file with spaces.txt"], None);
            }
            let patch = unified_patch(&path, before, after);
            git(root, &["apply", "--cached", "--check", "-"], Some(&patch));
            git(root, &["apply", "--cached", "-"], Some(&patch));
            if let Some(expected) = after {
                assert_eq!(
                    git(root, &["show", ":file with spaces.txt"], None),
                    expected.as_bytes()
                );
            }
            git(
                root,
                &["apply", "--cached", "--check", "-R", "-"],
                Some(&patch),
            );
            git(root, &["apply", "--cached", "-R", "-"], Some(&patch));
            if let Some(expected) = before {
                assert_eq!(
                    git(root, &["show", ":file with spaces.txt"], None),
                    expected.as_bytes()
                );
            }
        }
    }

    #[test]
    fn partial_selection_preserves_other_changes_in_both_directions() {
        let before = "first\nkeep\nthird\n";
        let after = "FIRST\nkeep\nTHIRD\n";
        let model = crate::diff_view::build_diff(before, after);
        let start = model.inline_change_starts[0];
        let end = model
            .inline_rows
            .iter()
            .skip(start)
            .position(|row| row.kind == DiffRowKind::Context)
            .map_or(model.inline_rows.len(), |offset| start + offset);
        assert_eq!(
            selected_content(&model, after, DiffMode::Inline, start..end, false).unwrap(),
            "FIRST\nkeep\nthird\n"
        );
        assert_eq!(
            selected_content(&model, after, DiffMode::Inline, start..end, true).unwrap(),
            "first\nkeep\nTHIRD\n"
        );
    }
    #[test]
    fn undoing_selected_hunk_preserves_unselected_ignored_whitespace() {
        let before = (0..80).map(|line| format!("    line {line}\n")).collect::<String>();
        let after = before.replace("    ","\t").replace("line 20\n","changed 20\n").replace("line 60\n","changed 60\n");
        let model = crate::diff_view::build_document_diff_with_options(before.as_str(),&after,std::path::Path::new("ignored.rs"),crate::settings::EditorSyntaxTheme::Auto,true,&crate::repository::CancellationToken::new()).unwrap();
        for mode in [DiffMode::Inline,DiffMode::Split] {
            let start = model.change_starts(mode)[0];
            let end = match mode {
                DiffMode::Inline => model.inline_rows.iter().skip(start).position(|row| row.kind == DiffRowKind::Context).map_or(model.inline_rows.len(),|offset| start+offset),
                DiffMode::Split => model.split_rows.iter().skip(start).position(|row| row.kind == DiffRowKind::Context).map_or(model.split_rows.len(),|offset| start+offset),
            };
            let selected = selected_content(&model,&after,mode,start..end,false).unwrap();
            assert_eq!(selected,before.replace("    line 20\n","\tchanged 20\n"));
            let undo = selected_content(&model,&after,mode,start..end,true).unwrap();
            // The selected row returns to its exact old bytes; ignored
            // whitespace outside that selected hunk stays on the new side.
            let expected = after.replace("\tchanged 20\n","    line 20\n");
            assert_eq!(undo,expected);
            let temp = tempfile::tempdir().unwrap();
            git(temp.path(),&["init","-q"],None);
            fs::write(temp.path().join("ignored.rs"),&after).unwrap();
            git(temp.path(),&["add","ignored.rs"],None);
            let patch = unified_patch(&RepoPath::parse("ignored.rs").unwrap(),Some(&after),Some(&undo));
            git(temp.path(),&["apply","--cached","--check","-"],Some(&patch));
            git(temp.path(),&["apply","--cached","-"],Some(&patch));
            assert_eq!(git(temp.path(),&["show",":ignored.rs"],None),expected.as_bytes());
        }
    }

    #[test]
    fn selected_hunk_patch_applies_to_index_without_staging_other_hunks() {
        let before = (0..50)
            .map(|line| format!("line {line}\r\n"))
            .collect::<String>();
        let first = before.replace("line 5\r\n", "new 5a\r\nnew 5b\r\n");
        let after = first.replace("line 40\r\n", "changed 40\r\n");
        let model = crate::diff_view::build_diff(&before, &after);
        for mode in [DiffMode::Inline, DiffMode::Split] {
            let start = model.change_starts(mode)[0];
            let end = match mode {
                DiffMode::Inline => model
                    .inline_rows
                    .iter()
                    .skip(start)
                    .position(|row| row.kind == DiffRowKind::Context)
                    .map_or(model.inline_rows.len(), |offset| start + offset),
                DiffMode::Split => model
                    .split_rows
                    .iter()
                    .skip(start)
                    .position(|row| row.kind == DiffRowKind::Context)
                    .map_or(model.split_rows.len(), |offset| start + offset),
            };
            let selected = selected_content(&model, &after, mode, start..end, false).unwrap();
            assert_eq!(selected, first);
            let undo = selected_content(&model, &after, mode, start..end, true).unwrap();
            assert_eq!(undo, before.replace("line 40\r\n", "changed 40\r\n"));
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            git(root, &["init", "-q"], None);
            fs::write(root.join("selected.rs"), &before).unwrap();
            git(root, &["add", "selected.rs"], None);
            let patch = unified_patch(
                &RepoPath::parse("selected.rs").unwrap(),
                Some(&before),
                Some(&selected),
            );
            git(root, &["apply", "--cached", "--check", "-"], Some(&patch));
            git(root, &["apply", "--cached", "-"], Some(&patch));
            assert_eq!(git(root, &["show", ":selected.rs"], None), first.as_bytes());
            git(
                root,
                &["apply", "--cached", "--check", "-R", "-"],
                Some(&patch),
            );
            git(root, &["apply", "--cached", "-R", "-"], Some(&patch));
            assert_eq!(
                git(root, &["show", ":selected.rs"], None),
                before.as_bytes()
            );
        }
    }

    #[test]
    fn renamed_hunk_patch_keeps_both_tree_paths_and_reverses() {
        for after in ["changed\n", "old\n"] {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path();
            git(root, &["init", "-q"], None);
            fs::write(root.join("old name.txt"), "old\n").unwrap();
            git(root, &["add", "old name.txt"], None);
            let patch = unified_patch_paths(
                &RepoPath::parse("old name.txt").unwrap(),
                &RepoPath::parse("new name.txt").unwrap(),
                Some("old\n"),
                Some(after),
            );
            git(root, &["apply", "--cached", "--check", "-"], Some(&patch));
            git(root, &["apply", "--cached", "-"], Some(&patch));
            assert_eq!(
                git(root, &["show", ":new name.txt"], None),
                after.as_bytes()
            );
            assert!(git(root, &["ls-files", "--", "old name.txt"], None).is_empty());
            git(
                root,
                &["apply", "--cached", "--check", "-R", "-"],
                Some(&patch),
            );
            git(root, &["apply", "--cached", "-R", "-"], Some(&patch));
            assert_eq!(git(root, &["show", ":old name.txt"], None), b"old\n");
        }
    }

    #[test]
    #[cfg(unix)]
    fn deleting_executable_content_does_not_guess_its_existing_mode() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        git(root, &["init", "-q"], None);
        fs::write(root.join("run.sh"), "#!/bin/sh\necho works\n").unwrap();
        fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        git(root, &["add", "run.sh"], None);
        let patch = unified_patch(
            &RepoPath::parse("run.sh").unwrap(),
            Some("#!/bin/sh\necho works\n"),
            None,
        );
        git(root, &["apply", "--cached", "--check", "-"], Some(&patch));
        git(root, &["apply", "--cached", "-"], Some(&patch));
        assert!(git(root, &["ls-files", "--", "run.sh"], None).is_empty());
    }
}
