//! Preflight and disk publication for language-server workspace edits.
//!
//! Every range is resolved before any file or buffer is changed. UI previews
//! retain the exact before/after snapshots; publication uses the editor's
//! descriptor-relative compare-and-swap writer rather than unconditional saves.
use std::{collections::HashMap, io::Read, ops::Range, path::{Path, PathBuf}, sync::Arc};

use crate::{editor::{self, SaveOutcome, SaveDurability}, fs_safety, lsp::{LspCoordinates, LspPosition, LspTextEdit}};

#[derive(Debug, Clone)]
pub struct DocumentEdits {
    pub path: PathBuf,
    pub version: Option<i32>,
    pub observed_version: Option<i32>,
    pub coordinates: LspCoordinates,
    pub edits: Vec<LspTextEdit>,
}

#[derive(Debug, Clone)]
pub struct OpenSnapshot {
    pub text: Arc<str>,
    pub version: i32,
    pub writable: bool,
}

#[derive(Debug, Clone)]
pub struct ResolvedEdit { pub range: Range<usize>, pub new_text: String }

#[derive(Debug, Clone)]
pub struct DocumentPlan {
    pub path: PathBuf,
    pub relative: PathBuf,
    pub before: Arc<str>,
    pub after: Arc<str>,
    pub before_hash: blake3::Hash,
    pub version: Option<i32>,
    pub open: bool,
    /// Original selected-source spellings, captured by the preparation worker.
    pub open_paths: Vec<PathBuf>,
    /// All spans address `before`; apply last-to-first as one buffer undo group.
    pub edits: Vec<ResolvedEdit>,
}

#[derive(Debug, Clone)]
pub struct Plan { pub worktree: PathBuf, pub documents: Vec<DocumentPlan> }

#[derive(Debug, Clone)]
pub struct ResolvedTargets {
    pub worktree: PathBuf,
    pub documents: Vec<DocumentEdits>,
    pub open_paths: Vec<PathBuf>,
}

/// Resolve identities before capturing native text. A workspace with hundreds
/// of open files copies only the buffers actually named by this operation.
pub fn resolve_targets(worktree: PathBuf, documents: Vec<DocumentEdits>, open_paths: Vec<PathBuf>) -> Result<ResolvedTargets, String> {
    if documents.len() > 512 { return Err("This workspace edit exceeds the 512-file preview limit".into()); }
    let worktree = std::fs::canonicalize(worktree).map_err(|error| error.to_string())?;
    let mut targets = std::collections::HashSet::new();
    for document in &documents {
        let path = if document.path.is_absolute() { document.path.clone() } else { worktree.join(&document.path) };
        let path = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
        if !path.starts_with(&worktree) { return Err("The server tried to edit a file outside this worktree".into()); }
        targets.insert(path);
    }
    let open_paths = open_paths.into_iter().filter(|path| std::fs::canonicalize(path).is_ok_and(|path| targets.contains(&path))).collect();
    Ok(ResolvedTargets { worktree, documents, open_paths })
}

fn offset(text: &str, position: LspPosition, coordinates: LspCoordinates) -> Result<usize, String> {
    let mut starts = std::iter::once(0).chain(text.match_indices('\n').map(|(index, _)| index + 1));
    let start = starts.nth(position.line as usize).ok_or("The server edit names a missing line")?;
    let end = text[start..].find('\n').map_or(text.len(), |offset| start + offset);
    let end = if end > start && text.as_bytes()[end - 1] == b'\r' { end - 1 } else { end };
    let mut units = 0u32;
    for (index, character) in text[start..end].char_indices() {
        if units == position.character { return Ok(start + index); }
        units += match coordinates { LspCoordinates::Scalar => 1, LspCoordinates::Utf16 => character.len_utf16() as u32 };
        if units > position.character { return Err("The server edit splits a Unicode character".into()); }
    }
    if units == position.character { Ok(end) } else { Err("The server edit names a missing column".into()) }
}

pub fn prepare(worktree: &Path, documents: Vec<DocumentEdits>, open: &HashMap<PathBuf, OpenSnapshot>) -> Result<Plan, String> {
    let worktree = std::fs::canonicalize(worktree).map_err(|error| error.to_string())?;
    let mut canonical_open = HashMap::new();
    let mut aliases: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (path, snapshot) in open {
        let Ok(canonical) = std::fs::canonicalize(path) else { continue; };
        aliases.entry(canonical.clone()).or_default().push(path.clone());
        if let Some(previous) = canonical_open.insert(canonical, snapshot) {
            if previous.text != snapshot.text || previous.version != snapshot.version || previous.writable != snapshot.writable {
                return Err("Two open views of the same file disagree; close the stale view before applying this edit".into());
            }
        }
    }
    let root = fs_safety::Root::open(&worktree).map_err(|error| error.to_string())?;
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::with_capacity(documents.len());
    let mut total = 0usize;
    if documents.len() > 512 { return Err("This workspace edit exceeds the 512-file preview limit".into()); }
    for document in documents {
        let candidate = if document.path.is_absolute() { document.path.clone() } else { worktree.join(&document.path) };
        let path = std::fs::canonicalize(candidate).map_err(|error| format!("{}: {error}", document.path.display()))?;
        let relative = path.strip_prefix(&worktree).map_err(|_| "The server tried to edit a file outside this worktree")?.to_path_buf();
        if !seen.insert(path.clone()) { return Err("The server returned duplicate file edit groups".into()); }
        let snapshot = canonical_open.get(&path).copied();
        if snapshot.is_some_and(|snapshot| !snapshot.writable) { return Err(format!("{} is read-only", path.display())); }
        for version in [document.version, document.observed_version].into_iter().flatten() {
            if snapshot.is_none_or(|snapshot| snapshot.version != version) { return Err(format!("{} changed after the server prepared its edits", path.display())); }
        }
        let before = if let Some(snapshot) = snapshot { snapshot.text.clone() } else {
            let file = fs_safety::open_beneath(&root, &relative).map_err(|error| error.to_string())?;
            let metadata = file.metadata().map_err(|error| error.to_string())?;
            if !metadata.is_file() || metadata.len() > editor::MAX_EDITABLE_FILE_BYTES { return Err(format!("{} is not a supported text file", path.display())); }
            let mut text = String::new();
            file.take(editor::MAX_EDITABLE_FILE_BYTES + 1).read_to_string(&mut text).map_err(|error| error.to_string())?;
            if text.len() as u64 > editor::MAX_EDITABLE_FILE_BYTES || text.contains('\0') { return Err(format!("{} is not a supported text file", path.display())); }
            Arc::from(text)
        };
        let mut edits = Vec::with_capacity(document.edits.len());
        for edit in document.edits {
            let start = offset(&before, edit.range.start, document.coordinates)?;
            let end = offset(&before, edit.range.end, document.coordinates)?;
            if start > end { return Err("The server returned a reversed edit range".into()); }
            edits.push(ResolvedEdit { range: start..end, new_text: edit.new_text });
        }
        edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
        if edits.windows(2).any(|pair| pair[0].range.end > pair[1].range.start) {
            return Err("The server returned overlapping edits".into());
        }
        let mut after = before.to_string();
        for edit in edits.iter().rev() { after.replace_range(edit.range.clone(), &edit.new_text); }
        if after.len() as u64 > editor::MAX_EDITABLE_FILE_BYTES { return Err("A server edit would exceed the editable file size limit".into()); }
        total = total.checked_add(before.len() + after.len()).ok_or("The workspace edit is too large")?;
        if total > 64 * 1024 * 1024 { return Err("This workspace edit exceeds the 64 MiB preview budget".into()); }
        let open_paths = aliases.remove(&path).unwrap_or_default();
        result.push(DocumentPlan { before_hash: blake3::hash(before.as_bytes()), path, relative,
            before, after: after.into(), version: snapshot.map(|snapshot| snapshot.version), open: snapshot.is_some(), open_paths, edits });
    }
    result.sort_by(|left, right| left.relative.cmp(&right.relative));
    Ok(Plan { worktree, documents: result })
}

/// Atomically publishes each unopened file. A later refusal rolls back earlier
/// publications only while their exact written hash still matches, preserving
/// concurrent outside edits and reporting any file that could not be restored.
#[derive(Debug, Clone, Default)]
pub struct PublicationReport { pub durability_warnings: Vec<String> }

pub fn publish_unopened(plan: &Plan) -> Result<PublicationReport, String> {
    let mut report = PublicationReport::default();
    let mut published: Vec<&DocumentPlan> = Vec::new();
    for document in plan.documents.iter().filter(|document| !document.open && document.before != document.after) {
        let saved = editor::save_file(&plan.worktree, &document.relative, &document.after, document.before_hash, false);
        let failure = match saved {
            Ok(SaveOutcome::Saved { durability, .. }) => {
                if let SaveDurability::Unconfirmed { error } = durability { report.durability_warnings.push(format!("{} was replaced, but durability is unconfirmed: {error}", document.relative.display())); }
                published.push(document); continue;
            },
            Ok(SaveOutcome::Conflict { .. }) => format!("{} changed on disk", document.relative.display()),
            Err(error) => format!("{}: {error}", document.relative.display()),
        };
        let mut unrestored = Vec::new();
        for previous in published.into_iter().rev() {
            if !matches!(editor::save_file(&plan.worktree, &previous.relative, &previous.before, blake3::hash(previous.after.as_bytes()), false), Ok(SaveOutcome::Saved { .. })) {
                unrestored.push(previous.relative.display().to_string());
            }
        }
        return Err(if unrestored.is_empty() { format!("The workspace edit was canceled: {failure}. Earlier writes were restored.") }
            else { format!("The workspace edit stopped: {failure}. These files could not be restored because they changed or could not be written: {}", unrestored.join(", ")) });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::LspRange;
    fn edit(start: u32, end: u32, new_text: &str) -> LspTextEdit {
        LspTextEdit { range: LspRange::new(LspPosition::new(0, start), LspPosition::new(0, end)), new_text: new_text.into() }
    }
    #[test]
    fn target_resolution_captures_only_changed_open_files_and_keeps_aliases() {
        let directory = tempfile::tempdir().unwrap();
        let mut paths = Vec::new();
        for index in 0..100 {
            let path = directory.path().join(format!("file{index}.rs"));
            std::fs::write(&path, "old()").unwrap();
            paths.push(path);
        }
        let alias = directory.path().join("alias.rs");
        std::os::unix::fs::symlink(&paths[7], &alias).unwrap();
        paths.push(alias.clone());
        let result = resolve_targets(directory.path().into(), vec![DocumentEdits { path: "file7.rs".into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "new")] }], paths).unwrap();
        assert_eq!(result.open_paths.len(), 2);
        assert!(result.open_paths.contains(&alias));
        let open = result.open_paths.iter().map(|path| (path.clone(), OpenSnapshot { text: "old()".into(), version: 1, writable: true })).collect();
        let plan = prepare(&result.worktree, result.documents, &open).unwrap();
        assert_eq!(plan.documents[0].open_paths.len(), 2);
    }

    #[test]
    fn unicode_ranges_reject_surrogate_splits_and_handle_scalar_and_utf16() {
        assert_eq!(offset("a😀b\r\nnext", LspPosition::new(0, 3), LspCoordinates::Utf16), Ok(5));
        assert_eq!(offset("a😀b", LspPosition::new(0, 2), LspCoordinates::Scalar), Ok(5));
        assert!(offset("a😀b", LspPosition::new(0, 2), LspCoordinates::Utf16).is_err());
        assert_eq!(offset("a\r\n", LspPosition::new(1, 0), LspCoordinates::Utf16), Ok(3));
    }
    #[test]
    fn multi_file_preview_fences_versions_and_publishes_unopened_files() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] { std::fs::write(directory.path().join(name), "old()").unwrap(); }
        let path = std::fs::canonicalize(directory.path().join("a.rs")).unwrap();
        let mut open = HashMap::from([(path, OpenSnapshot { text: "old()".into(), version: 7, writable: true })]);
        let documents = || ["a.rs", "b.rs", "c.rs"].into_iter().map(|name| DocumentEdits { path: name.into(), version: (name == "a.rs").then_some(7), observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "renamed")] }).collect();
        let plan = prepare(directory.path(), documents(), &open).unwrap();
        assert_eq!(plan.documents.len(), 3);
        publish_unopened(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(directory.path().join("a.rs")).unwrap(), "old()");
        assert_eq!(std::fs::read_to_string(directory.path().join("b.rs")).unwrap(), "renamed()");
        open.values_mut().next().unwrap().version = 8;
        assert!(prepare(directory.path(), documents(), &open).is_err());
    }
    #[test]
    fn equal_position_insertions_preserve_server_array_order_and_observed_versions() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.rs"), "body").unwrap();
        let path = std::fs::canonicalize(directory.path().join("a.rs")).unwrap();
        let open = HashMap::from([(path, OpenSnapshot { text: "body".into(), version: 7, writable: true })]);
        let input = |observed_version| vec![DocumentEdits { path: "a.rs".into(), version: None, observed_version, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 0, "first"), edit(0, 0, "second")] }];
        assert_eq!(&*prepare(directory.path(), input(Some(7)), &open).unwrap().documents[0].after, "firstsecondbody");
        assert!(prepare(directory.path(), input(Some(6)), &open).is_err());
    }

    #[test]
    fn overlap_and_external_paths_are_refused_before_writes() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.rs"), "abcdef").unwrap();
        let documents = vec![DocumentEdits { path: "a.rs".into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(1, 3, "x"), edit(2, 4, "y")] }];
        assert!(prepare(directory.path(), documents, &HashMap::new()).is_err());
        assert_eq!(std::fs::read_to_string(directory.path().join("a.rs")).unwrap(), "abcdef");
        let outside = tempfile::NamedTempFile::new().unwrap();
        assert!(prepare(directory.path(), vec![DocumentEdits { path: outside.path().into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![] }], &HashMap::new()).is_err());
    }
    #[test]
    fn outside_change_cancels_batch_and_restores_preceding_atomic_writes() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs"] { std::fs::write(directory.path().join(name), "old").unwrap(); }
        let documents = ["a.rs", "b.rs"].into_iter().map(|name| DocumentEdits { path: name.into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "new")] }).collect();
        let plan = prepare(directory.path(), documents, &HashMap::new()).unwrap();
        std::fs::write(directory.path().join("b.rs"), "outside").unwrap();
        assert!(publish_unopened(&plan).is_err());
        assert_eq!(std::fs::read_to_string(directory.path().join("a.rs")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(directory.path().join("b.rs")).unwrap(), "outside");
    }
}
