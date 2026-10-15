//! Preflight for language-server edits to current writable open buffers.
//!
//! This cut prepares native buffer edits only. Unopened targets are refused
//! before publication; no temporary directory, file writer or rollback runs.
use std::{collections::HashMap, ops::Range, os::unix::fs::MetadataExt, path::{Path, PathBuf}, sync::Arc};

use crate::{editor, fs_safety, lsp::{LspCoordinates, LspPosition, LspTextEdit}};

#[derive(Debug, Clone)]
pub struct DocumentEdits {
    pub path: PathBuf,
    pub version: Option<i32>,
    pub observed_version: Option<i32>,
    pub observed_incarnation: Option<crate::lsp::LspDocumentIncarnation>,
    pub coordinates: LspCoordinates,
    pub edits: Vec<LspTextEdit>,
}

#[derive(Debug, Clone)]
pub struct OpenSnapshot {
    pub text: Arc<str>,
    pub version: i32,
    /// Actual shared-URI version sent by the language-server manager.
    pub protocol_version: Option<i32>,
    pub protocol_incarnation: Option<crate::lsp::LspDocumentIncarnation>,
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
    pub protocol_version: Option<i32>,
    pub protocol_incarnation: Option<crate::lsp::LspDocumentIncarnation>,
    pub open: bool,
    /// Original selected-source spellings, captured by the preparation worker.
    pub open_paths: Vec<PathBuf>,
    /// All spans address `before`; apply last-to-first as one buffer undo group.
    pub edits: Vec<ResolvedEdit>,
}

#[derive(Debug, Clone)]
pub struct Plan { pub worktree: PathBuf, pub root: Arc<fs_safety::Root>, pub documents: Vec<DocumentPlan> }

#[derive(Debug, Clone)]
pub struct ResolvedTargets {
    pub worktree: PathBuf,
    pub root: Arc<fs_safety::Root>,
    pub documents: Vec<DocumentEdits>,
    pub open_paths: Vec<PathBuf>,
}

/// Resolve identities before capturing native text. A workspace with hundreds
/// of open files copies only the buffers actually named by this operation.
pub fn resolve_targets(worktree: PathBuf, documents: Vec<DocumentEdits>, open_paths: Vec<PathBuf>) -> Result<ResolvedTargets, String> {
    if documents.len() > 512 { return Err("This workspace edit exceeds the 512-file preview limit".into()); }
    let worktree = std::fs::canonicalize(worktree).map_err(|error| error.to_string())?;
    let root = Arc::new(fs_safety::Root::open(&worktree).map_err(|error| error.to_string())?);
    verify_root(&root)?;
    let mut targets = std::collections::HashSet::new();
    for document in &documents {
        let path = if document.path.is_absolute() { document.path.clone() } else { worktree.join(&document.path) };
        let path = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
        if !path.starts_with(&worktree) { return Err("The server tried to edit a file outside this worktree".into()); }
        targets.insert(path);
    }
    let mut resolved_open = std::collections::HashSet::new();
    let open_paths = open_paths.into_iter().filter(|path| {
        let Ok(canonical) = std::fs::canonicalize(path) else { return false; };
        if !targets.contains(&canonical) { return false; }
        resolved_open.insert(canonical);
        true
    }).collect();
    if targets.iter().any(|target| !resolved_open.contains(target)) {
        return Err("Open every target in a writable editor before applying this workspace edit".into());
    }
    verify_root(&root)?;
    Ok(ResolvedTargets { worktree, root, documents, open_paths })
}

/// Resolve sorted requests in one forward source pass. The callback counts
/// source bytes traversed in the actual resolver for the long-line work test.
fn offsets_with_scan(text: &str, positions: &[LspPosition], coordinates: LspCoordinates, mut scanned: impl FnMut(usize)) -> Result<Vec<usize>, String> {
    let mut requests = positions.iter().copied().enumerate().collect::<Vec<_>>();
    requests.sort_by_key(|(_, position)| (position.line, position.character));
    let mut result = vec![0; positions.len()];
    let mut line = 0;
    let mut start = 0;
    let mut loaded_line = None;
    let mut next_line_start = None;
    let mut characters = "".char_indices();
    let mut units = 0;
    let mut byte = 0;
    for (output, position) in requests {
        while line < position.line {
            if loaded_line == Some(line) {
                start = next_line_start.ok_or("The server edit names a missing line")?;
            } else {
                let length = text[start..].find('\n').ok_or("The server edit names a missing line")? + 1;
                scanned(length);
                start += length;
            }
            line += 1;
        }
        if loaded_line != Some(line) {
            let ending = text[start..].find('\n');
            let length = ending.unwrap_or(text.len() - start);
            scanned(length + usize::from(ending.is_some()));
            next_line_start = ending.map(|offset| start + offset + 1);
            let mut end = start + length;
            if end > start && text.as_bytes()[end - 1] == b'\r' { end -= 1; }
            characters = text[start..end].char_indices();
            units = 0;
            byte = 0;
            loaded_line = Some(line);
        }
        while units < position.character {
            let (offset, character) = characters.next().ok_or("The server edit names a missing column")?;
            scanned(character.len_utf8());
            units += match coordinates { LspCoordinates::Scalar => 1, LspCoordinates::Utf16 => character.len_utf16() as u32 };
            if units > position.character { return Err("The server edit splits a Unicode character".into()); }
            byte = offset + character.len_utf8();
        }
        result[output] = start + byte;
    }
    Ok(result)
}

fn verify_root(root: &fs_safety::Root) -> Result<(), String> {
    let pinned = root.metadata().map_err(|error| error.to_string())?;
    let named = fs_safety::Root::open(root.path()).and_then(|root| root.metadata()).map_err(|error| format!("The previewed worktree is no longer available: {error}"))?;
    if (pinned.dev(), pinned.ino()) != (named.dev(), named.ino()) { return Err("The previewed worktree was replaced; prepare the edit again".into()); }
    Ok(())
}

pub fn prepare(worktree: &Path, documents: Vec<DocumentEdits>, open: &HashMap<PathBuf, OpenSnapshot>) -> Result<Plan, String> {
    let worktree = std::fs::canonicalize(worktree).map_err(|error| error.to_string())?;
    let root = Arc::new(fs_safety::Root::open(&worktree).map_err(|error| error.to_string())?);
    prepare_at(root, documents, open)
}

pub fn prepare_resolved(resolved: ResolvedTargets, open: &HashMap<PathBuf, OpenSnapshot>) -> Result<Plan, String> {
    prepare_at(resolved.root, resolved.documents, open)
}

fn prepare_at(root: Arc<fs_safety::Root>, documents: Vec<DocumentEdits>, open: &HashMap<PathBuf, OpenSnapshot>) -> Result<Plan, String> {
    verify_root(&root)?;
    let worktree = root.path().to_path_buf();
    let mut canonical_open = HashMap::new();
    let mut aliases: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (path, snapshot) in open {
        let Ok(canonical) = std::fs::canonicalize(path) else { continue; };
        aliases.entry(canonical.clone()).or_default().push(path.clone());
        if let Some(previous) = canonical_open.insert(canonical, snapshot) {
            if previous.text != snapshot.text || previous.version != snapshot.version || previous.protocol_version != snapshot.protocol_version || previous.protocol_incarnation != snapshot.protocol_incarnation || previous.writable != snapshot.writable {
                return Err("Two open views of the same file disagree; close the stale view before applying this edit".into());
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    let mut result = Vec::with_capacity(documents.len());
    let mut total = 0usize;
    if documents.len() > 512 { return Err("This workspace edit exceeds the 512-file preview limit".into()); }
    for document in documents {
        let candidate = if document.path.is_absolute() { document.path.clone() } else { worktree.join(&document.path) };
        let path = std::fs::canonicalize(candidate).map_err(|error| format!("{}: {error}", document.path.display()))?;
        let relative = path.strip_prefix(&worktree).map_err(|_| "The server tried to edit a file outside this worktree")?.to_path_buf();
        if !seen.insert(path.clone()) { return Err("The server returned duplicate file edit groups".into()); }
        let snapshot = canonical_open.get(&path).copied().ok_or_else(||
            format!("{} must be open in a writable editor before applying this edit", path.display()))?;
        if !snapshot.writable { return Err(format!("{} is read-only", path.display())); }
        for version in [document.version, document.observed_version].into_iter().flatten() {
            if snapshot.protocol_version != Some(version) { return Err(format!("{} changed after the server prepared its edits", path.display())); }
        }
        if let Some(expected) = &document.observed_incarnation
            && snapshot.protocol_incarnation.as_ref() != Some(expected) {
            return Err(format!("{} was closed or reopened after the server prepared its edits", path.display()));
        }
        let before = Arc::clone(&snapshot.text);
        if before.len() as u64 > editor::MAX_EDITABLE_FILE_BYTES {
            return Err("An open target exceeds the editable file size limit".into());
        }
        let positions = document.edits.iter().flat_map(|edit| [edit.range.start, edit.range.end]).collect::<Vec<_>>();
        let offsets = offsets_with_scan(&before, &positions, document.coordinates, |_| {})?;
        let mut edits = Vec::with_capacity(document.edits.len());
        for (index, edit) in document.edits.into_iter().enumerate() {
            let start = offsets[index * 2];
            let end = offsets[index * 2 + 1];
            if start > end { return Err("The server returned a reversed edit range".into()); }
            edits.push(ResolvedEdit { range: start..end, new_text: edit.new_text });
        }
        edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
        if edits.windows(2).any(|pair| pair[0].range.end > pair[1].range.start) {
            return Err("The server returned overlapping edits".into());
        }
        let after_len = edits.iter().try_fold(before.len(), |length, edit| {
            length.checked_sub(edit.range.len())?.checked_add(edit.new_text.len())
        }).ok_or("The workspace edit is too large")?;
        if after_len as u64 > editor::MAX_EDITABLE_FILE_BYTES { return Err("A server edit would exceed the editable file size limit".into()); }
        total = total.checked_add(before.len()).and_then(|total| total.checked_add(after_len)).ok_or("The workspace edit is too large")?;
        if total > 64 * 1024 * 1024 { return Err("This workspace edit exceeds the 64 MiB preview budget".into()); }
        let mut after = String::with_capacity(after_len);
        let mut copied = 0;
        for edit in &edits {
            after.push_str(&before[copied..edit.range.start]);
            after.push_str(&edit.new_text);
            copied = edit.range.end;
        }
        after.push_str(&before[copied..]);
        let open_paths = aliases.remove(&path).unwrap_or_default();
        result.push(DocumentPlan { before_hash: blake3::hash(before.as_bytes()), path, relative,
            before, after: after.into(), version: Some(snapshot.version), protocol_version: snapshot.protocol_version, protocol_incarnation: snapshot.protocol_incarnation.clone(), open: true, open_paths, edits });
    }
    result.sort_by(|left, right| left.relative.cmp(&right.relative));
    verify_root(&root)?;
    Ok(Plan { worktree, root, documents: result })
}

/// Recheck the read-only preparation proof before the controller applies its
/// frozen native buffers. This function never writes files.
pub fn validate_for_publication(plan: &Plan) -> Result<(), String> {
    if plan.documents.iter().any(|document| !document.open || document.open_paths.is_empty()) {
        return Err("This workspace edit contains a target without an open editor".into());
    }
    verify_root(&plan.root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::LspRange;

    #[test]
    fn many_late_endpoints_scan_long_lines_and_prefixes_only_once() {
        for text in ["a".repeat(8 * 1024 * 1024), "a\n".repeat(4 * 1024 * 1024)] {
            let lines = text.bytes().filter(|byte| *byte == b'\n').count() as u32;
            let positions = (0..2048).rev().map(|index| {
                if lines == 0 { LspPosition::new(0, (text.len() - 2048 + index) as u32) }
                else { LspPosition::new(lines - 2048 + index as u32, 1) }
            }).collect::<Vec<_>>();
            let mut visited = 0;
            let offsets = offsets_with_scan(&text, &positions, LspCoordinates::Utf16, |bytes| visited += bytes).unwrap();
            assert_eq!(offsets.len(), positions.len());
            assert!(visited <= text.len() * 2, "endpoint conversion revisited {visited} source bytes");
            for (position, offset) in positions.iter().zip(offsets) {
                let expected = if lines == 0 { position.character as usize } else { position.line as usize * 2 + 1 };
                assert_eq!(offset, expected);
            }
        }
    }

    fn edit(path: &str, version: Option<i32>, start: u32, end: u32, text: &str) -> DocumentEdits {
        DocumentEdits {
            path: path.into(),
            version,
            observed_version: None,
            observed_incarnation: None,
            coordinates: LspCoordinates::Utf16,
            edits: vec![LspTextEdit {
                range: LspRange::new(LspPosition::new(0, start), LspPosition::new(0, end)),
                new_text: text.into(),
            }],
        }
    }

    fn snapshot(text: &str) -> OpenSnapshot {
        OpenSnapshot {
            text: text.into(),
            version: 7,
            protocol_version: Some(12),
            // These planner tests exercise text/identity admission, not an
            // actual server registration or its publication lease.
            protocol_incarnation: None,
            writable: true,
        }
    }

    #[test]
    fn unopened_target_is_refused_before_any_buffer_or_file_changes() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a.rs");
        let b = directory.path().join("b.rs");
        std::fs::write(&a, "original_a").unwrap();
        std::fs::write(&b, "original_b").unwrap();
        let documents = || vec![edit("a.rs", None, 0, 8, "changed"), edit("b.rs", None, 0, 8, "changed")];
        assert!(resolve_targets(directory.path().into(), documents(), vec![a.clone()]).is_err());
        let open = HashMap::from([(a.clone(), snapshot("unsaved_a"))]);
        assert!(prepare(directory.path(), documents(), &open).is_err());
        assert_eq!(&*open[&a].text, "unsaved_a");
        assert_eq!(std::fs::read_to_string(a).unwrap(), "original_a");
        assert_eq!(std::fs::read_to_string(b).unwrap(), "original_b");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn open_only_plan_uses_unsaved_text_and_never_writes_disk() {
        let directory = tempfile::tempdir().unwrap();
        let a = directory.path().join("a.rs");
        std::fs::write(&a, "old disk").unwrap();
        let open = HashMap::from([(a.clone(), snapshot("α😀old"))]);
        let resolved = resolve_targets(directory.path().into(), vec![edit("a.rs", Some(12), 3, 6, "new")], vec![a.clone()]).unwrap();
        let plan = prepare_resolved(resolved, &open).unwrap();
        assert_eq!(&*plan.documents[0].before, "α😀old");
        assert_eq!(&*plan.documents[0].after, "α😀new");
        validate_for_publication(&plan).unwrap();
        assert_eq!(&*open[&a].text, "α😀old");
        assert_eq!(std::fs::read_to_string(a).unwrap(), "old disk");
    }

    #[test]
    fn read_only_stale_version_and_split_surrogate_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.rs");
        std::fs::write(&path, "a😀b").unwrap();
        let mut open = HashMap::from([(path.clone(), snapshot("a😀b"))]);
        assert!(prepare(directory.path(), vec![edit("a.rs", Some(11), 1, 3, "x")], &open).is_err());
        assert!(prepare(directory.path(), vec![edit("a.rs", Some(12), 2, 3, "x")], &open).is_err());
        open.get_mut(&path).unwrap().writable = false;
        assert!(prepare(directory.path(), vec![edit("a.rs", Some(12), 1, 3, "x")], &open).is_err());
        assert_eq!(&*open[&path].text, "a😀b");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "a😀b");
    }

    #[test]
    fn replacing_the_prepared_root_refuses_publication_without_touching_either_tree() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("root");
        let moved = directory.path().join("moved");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("a.rs");
        std::fs::write(&path, "old").unwrap();
        let open = HashMap::from([(path, snapshot("old"))]);
        let plan = prepare(&root, vec![edit("a.rs", Some(12), 0, 3, "new")], &open).unwrap();
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.rs"), "replacement").unwrap();
        assert!(validate_for_publication(&plan).is_err());
        assert_eq!(std::fs::read_to_string(moved.join("a.rs")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "replacement");
    }

    #[test]
    fn output_budget_refuses_expansion_before_building_a_preview() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.rs");
        std::fs::write(&path, "a").unwrap();
        let open = HashMap::from([(path.clone(), snapshot("a"))]);
        let text = "x".repeat(editor::MAX_EDITABLE_FILE_BYTES as usize);
        assert!(prepare(directory.path(), vec![edit("a.rs", Some(12), 0, 0, &text)], &open).is_err());
        assert_eq!(&*open[&path].text, "a");
        assert_eq!(std::fs::read_to_string(path).unwrap(), "a");
    }
}
