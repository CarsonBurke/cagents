//! Preflight and disk publication for language-server workspace edits.
//!
//! Every range is resolved before any file or buffer is changed. UI previews
//! retain the exact before/after snapshots; publication uses the editor's
//! descriptor-relative compare-and-swap writer rather than unconditional saves.
use std::{collections::HashMap, io::Read, ops::Range, os::unix::fs::MetadataExt, path::{Path, PathBuf}, sync::Arc};

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
    /// Actual shared-URI version sent by the language-server manager.
    pub protocol_version: Option<i32>,
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
    pub open: bool,
    /// Original selected-source spellings, captured by the preparation worker.
    pub open_paths: Vec<PathBuf>,
    /// All spans address `before`; apply last-to-first as one buffer undo group.
    pub edits: Vec<ResolvedEdit>,
}

#[derive(Debug, Clone)]
pub struct Plan { pub worktree: PathBuf, pub root: Arc<fs_safety::Root>, pub documents: Vec<DocumentPlan>, recovery_context: Option<crate::filesystem_recovery_context::FilesystemRecoveryContext>, _preview_context: Option<editor::RecoveryContext> }

#[derive(Debug, Clone)]
pub struct ResolvedTargets {
    pub worktree: PathBuf,
    pub root: Arc<fs_safety::Root>,
    protection: Arc<Option<crate::worktree_cleanup::Protection>>,
    pub documents: Vec<DocumentEdits>,
    pub open_paths: Vec<PathBuf>,
}

/// Resolve identities before capturing native text. A workspace with hundreds
/// of open files copies only the buffers actually named by this operation.
pub fn resolve_targets(worktree: PathBuf, documents: Vec<DocumentEdits>, open_paths: Vec<PathBuf>) -> Result<ResolvedTargets, String> {
    if documents.len() > 512 { return Err("This workspace edit exceeds the 512-file preview limit".into()); }
    let worktree = std::fs::canonicalize(worktree).map_err(|error| error.to_string())?;
    let root = Arc::new(fs_safety::Root::open(&worktree).map_err(|error| error.to_string())?);
    let protection = Arc::new(crate::worktree_cleanup::protect(&worktree).map_err(|error| error.to_string())?);
    verify_root(&root)?;
    let mut targets = std::collections::HashSet::new();
    for document in &documents {
        let path = if document.path.is_absolute() { document.path.clone() } else { worktree.join(&document.path) };
        let path = std::fs::canonicalize(path).map_err(|error| error.to_string())?;
        if !path.starts_with(&worktree) { return Err("The server tried to edit a file outside this worktree".into()); }
        targets.insert(path);
    }
    let open_paths = open_paths.into_iter().filter(|path| std::fs::canonicalize(path).is_ok_and(|path| targets.contains(&path))).collect();
    verify_root(&root)?;
    Ok(ResolvedTargets { worktree, root, protection, documents, open_paths })
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

fn verify_root(root: &fs_safety::Root) -> Result<(), String> {
    let pinned = root.metadata().map_err(|error| error.to_string())?;
    let named = fs_safety::Root::open(root.path()).and_then(|root| root.metadata()).map_err(|error| format!("The previewed worktree is no longer available: {error}"))?;
    if (pinned.dev(), pinned.ino()) != (named.dev(), named.ino()) { return Err("The previewed worktree was replaced; prepare the edit again".into()); }
    Ok(())
}

pub fn prepare(worktree: &Path, documents: Vec<DocumentEdits>, open: &HashMap<PathBuf, OpenSnapshot>) -> Result<Plan, String> {
    let worktree = std::fs::canonicalize(worktree).map_err(|error| error.to_string())?;
    let root = Arc::new(fs_safety::Root::open(&worktree).map_err(|error| error.to_string())?);
    let protection = Arc::new(crate::worktree_cleanup::protect(&worktree).map_err(|error| error.to_string())?);
    prepare_at(root, protection, documents, open, None)
}

pub fn prepare_resolved(resolved: ResolvedTargets, open: &HashMap<PathBuf, OpenSnapshot>) -> Result<Plan, String> {
    prepare_at(resolved.root, resolved.protection, resolved.documents, open, None)
}

/// Current domain issuance uses the SAME original Root/physical lease from
/// ResolvedTargets. A plain preview may be prepared without a placement, but
/// cannot publish any unopened file until a real issued context is supplied.
pub fn prepare_resolved_in_context(resolved: ResolvedTargets, open: &HashMap<PathBuf, OpenSnapshot>, context: crate::filesystem_recovery_context::FilesystemRecoveryContext) -> Result<Plan, String> {
    context.validate_for_source(&resolved.root).map_err(|error| error.to_string())?;
    prepare_at(resolved.root, resolved.protection, resolved.documents, open, Some(context))
}
impl ResolvedTargets {
    pub fn original_protection(&self) -> Arc<Option<crate::worktree_cleanup::Protection>> { self.protection.clone() }
}
fn prepare_at(root: Arc<fs_safety::Root>, protection: Arc<Option<crate::worktree_cleanup::Protection>>, documents: Vec<DocumentEdits>, open: &HashMap<PathBuf, OpenSnapshot>, recovery_context: Option<crate::filesystem_recovery_context::FilesystemRecoveryContext>) -> Result<Plan, String> {
    verify_root(&root)?;
    let preview_context = if recovery_context.is_none() { Some(editor::recovery_context_at(&root, protection).map_err(|error| error.to_string())?) } else { None };
    let worktree = root.path().to_path_buf();
    let mut canonical_open = HashMap::new();
    let mut aliases: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for (path, snapshot) in open {
        let Ok(canonical) = std::fs::canonicalize(path) else { continue; };
        aliases.entry(canonical.clone()).or_default().push(path.clone());
        if let Some(previous) = canonical_open.insert(canonical, snapshot) {
            if previous.text != snapshot.text || previous.version != snapshot.version || previous.protocol_version != snapshot.protocol_version || previous.writable != snapshot.writable {
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
        let snapshot = canonical_open.get(&path).copied();
        if snapshot.is_some_and(|snapshot| !snapshot.writable) { return Err(format!("{} is read-only", path.display())); }
        for version in [document.version, document.observed_version].into_iter().flatten() {
            if snapshot.is_none_or(|snapshot| snapshot.protocol_version != Some(version)) { return Err(format!("{} changed after the server prepared its edits", path.display())); }
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
            before, after: after.into(), version: snapshot.map(|snapshot| snapshot.version), protocol_version: snapshot.and_then(|snapshot| snapshot.protocol_version), open: snapshot.is_some(), open_paths, edits });
    }
    result.sort_by(|left, right| left.relative.cmp(&right.relative));
    verify_root(&root)?;
    Ok(Plan { worktree, root, documents: result, recovery_context, _preview_context: preview_context })
}

/// Atomically publishes each unopened file. A later refusal rolls back earlier
/// publications only while their exact written hash still matches, preserving
/// concurrent outside edits and reporting any file that could not be restored.
#[derive(Debug, Clone, Default)]
pub struct PublicationReport {
    pub durability_warnings: Vec<String>,
    pub recovery: Option<editor::RecoveryAuthority>,
    pub worktree: Option<PathBuf>,
}
impl PublicationReport {
    fn retain(&mut self, authority: &editor::RecoveryAuthority) {
        self.recovery = Some(match self.recovery.take() { Some(previous) => previous.combine(authority.clone()), None => authority.clone() });
    }
    fn durability(&mut self, path: &Path, durability: SaveDurability, restoring: bool) {
        let action = if restoring { "restored" } else { "replaced" };
        match durability {
            SaveDurability::Durable => {},
            SaveDurability::Unconfirmed { error } => self.durability_warnings.push(format!("{} was {action}, but crash durability is unconfirmed: {error}", path.display())),
            SaveDurability::Recovery { error, authority } => {
                self.retain(&authority);
                self.durability_warnings.push(format!("{} was {action}; retained file recovery is still required: {error}", path.display()));
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PublicationFailure {
    pub message: String,
    pub recovery: Option<editor::RecoveryAuthority>,
    pub worktree: Option<PathBuf>,
}
impl From<String> for PublicationFailure {
    fn from(message: String) -> Self { Self { message, recovery: None, worktree: None } }
}
impl From<&str> for PublicationFailure { fn from(message: &str) -> Self { message.to_owned().into() } }
impl std::fmt::Display for PublicationFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { formatter.write_str(&self.message) }
}
impl std::error::Error for PublicationFailure {}
impl PublicationFailure { pub fn contains(&self, pattern: &str) -> bool { self.message.contains(pattern) } }

pub fn publish_unopened(plan: &Plan) -> Result<PublicationReport, PublicationFailure> {
    publish_unopened_with_before_file(plan, |_| {})
}

fn publish_unopened_with_before_file(plan: &Plan, before_file: impl FnMut(usize)) -> Result<PublicationReport, PublicationFailure> {
    publish_unopened_with_hooks(plan, before_file, |_| Ok(()))
}

fn publish_unopened_with_hooks(plan: &Plan, mut before_file: impl FnMut(usize), mut before_publish: impl FnMut(&DocumentPlan) -> std::io::Result<()>) -> Result<PublicationReport, PublicationFailure> {
    verify_root(&plan.root)?;
    let mut report = PublicationReport { worktree: Some(plan.worktree.clone()), ..Default::default() };
    let mut published: Vec<&DocumentPlan> = Vec::new();
    for (index, document) in plan.documents.iter().filter(|document| !document.open && document.before != document.after).enumerate() {
        let context = plan.recovery_context.as_ref().ok_or_else(|| PublicationFailure::from("Applying unopened-file edits requires the original registered recovery context. No file was changed.".to_owned()))?;
        before_file(index);
        let saved = verify_root(&plan.root).map_err(editor::FileIoError::Io)
            .and_then(|()| editor::save_file_at_with_hooks_in_context(&plan.root, &document.relative, &document.after, document.before_hash, false, || before_publish(document), fs_safety::sync_directory, context));
        let failure = match saved {
            Ok(SaveOutcome::Saved { durability, .. }) => {
                report.durability(&document.relative, durability, false);
                published.push(document); continue;
            },
            Ok(SaveOutcome::Conflict { .. }) => format!("{} changed on disk", document.relative.display()),
            Ok(SaveOutcome::Absent { durability, .. }) => {
                report.durability(&document.relative, durability, false);
                format!("{} unexpectedly became absent; publication was refused", document.relative.display())
            },
            Err(error) => {
                if let Some(authority) = error.recovery_authority() { report.retain(authority); }
                format!("{}: {error}", document.relative.display())
            }
        };
        let mut unrestored = Vec::new();
        for previous in published.into_iter().rev() {
            let restored = editor::save_file_at_in_context(&plan.root, &previous.relative, &previous.before, blake3::hash(previous.after.as_bytes()), false, context);
            match restored {
                Ok(SaveOutcome::Saved { durability, .. }) => report.durability(&previous.relative, durability, true),
                Err(error) => {
                    if let Some(authority) = error.recovery_authority() { report.retain(authority); }
                    unrestored.push(format!("{}: {error}", previous.relative.display()));
                },
                Ok(SaveOutcome::Absent { durability, .. }) => {
                    report.durability(&previous.relative, durability, true);
                    unrestored.push(previous.relative.display().to_string());
                },
                Ok(SaveOutcome::Conflict { .. }) => unrestored.push(previous.relative.display().to_string()),
            }
        }
        let mut message = if unrestored.is_empty() { format!("The workspace edit was canceled: {failure}. Earlier writes were restored.") }
            else { format!("The workspace edit stopped: {failure}. These files could not be restored because they changed or could not be written: {}", unrestored.join(", ")) };
        if !report.durability_warnings.is_empty() { message.push_str("\n"); message.push_str(&report.durability_warnings.join("\n")); }
        return Err(PublicationFailure { message, recovery: report.recovery, worktree: report.worktree });
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    // Pure primitive fixtures issue an actual isolated registration/cleanup
    // context. Production prepare() never invents this domain authority.
    fn prepare(worktree: &Path, documents: Vec<DocumentEdits>, open: &HashMap<PathBuf, OpenSnapshot>) -> Result<Plan, String> {
        let root = Arc::new(editor::worktree_root(worktree).map_err(|error| error.to_string())?);
        let context = crate::filesystem_recovery_context::fixture_context(&root).map_err(|error| error.to_string())?;
        prepare_at(root, Arc::new(None), documents, open, Some(context))
    }

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
        let open = result.open_paths.iter().map(|path| (path.clone(), OpenSnapshot { text: "old()".into(), version: 1, protocol_version: Some(1), writable: true })).collect();
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
    fn actual_publication_recovery_keeps_original_cleanup_lease_after_plan_drop() {
        let directory = tempfile::tempdir().unwrap(); let runtime = tempfile::tempdir().unwrap();
        let named = directory.path().join("worktree"); let moved = directory.path().join("original");
        std::fs::create_dir(&named).unwrap(); std::fs::write(named.join("a.rs"), "old").unwrap();
        let root = Arc::new(fs_safety::Root::open(&named).unwrap());
        let protection = Arc::new(Some(crate::worktree_cleanup::fixture_protection(runtime.path(), &named, false).unwrap()));
        let fixture = crate::filesystem_recovery_context::fixture_context(&root).unwrap();
        let context = crate::filesystem_recovery_context::FilesystemRecoveryContext::new(&root, protection.clone(), fixture.placement().clone()).unwrap();
        drop(fixture);
        let outside = context.placement().directory().try_clone().unwrap();
        let plan = prepare_at(root, protection, vec![DocumentEdits { path: "a.rs".into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "new")] }], &HashMap::new(), Some(context)).unwrap();
        let mut private = None;
        let report = publish_unopened_with_hooks(&plan, |_| {}, |_| {
            let path = std::fs::read_dir(outside.path())?.map(|entry| entry.map(|entry| entry.path())).collect::<std::io::Result<Vec<_>>>()?.into_iter().find(|path| path.file_name().is_some_and(|name| name.to_string_lossy().starts_with(".cagents-save-"))).expect("actual publisher holding");
            let held = fs_safety::Root::open(&path)?;
            std::fs::write(path.join("unresolved"), "preserve")?;
            private = Some(held); Ok(())
        }).unwrap();
        assert_eq!(std::fs::read(named.join("a.rs")).unwrap(), b"new");
        let authority = report.recovery.as_ref().expect("actual cleanup refusal retains authority").clone();
        drop(plan); drop(report);
        std::fs::rename(&named, &moved).unwrap(); std::fs::create_dir(&named).unwrap();
        std::fs::write(named.join("foreign"), "preserve replacement").unwrap();
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).is_err());
        assert!(authority.retry().is_err());
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).is_err());
        fs_safety::unlink_within(private.as_ref().unwrap(), std::ffi::OsStr::new("unresolved")).unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved());
        drop(crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).unwrap());
        assert_eq!(std::fs::read(named.join("foreign")).unwrap(), b"preserve replacement");
        assert_eq!(std::fs::read(moved.join("a.rs")).unwrap(), b"new");
    }

    #[test]
    fn multi_file_preview_fences_versions_and_publishes_unopened_files() {
        let directory = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] { std::fs::write(directory.path().join(name), "old()").unwrap(); }
        let path = std::fs::canonicalize(directory.path().join("a.rs")).unwrap();
        let mut open = HashMap::from([(path, OpenSnapshot { text: "old()".into(), version: 7, protocol_version: Some(7), writable: true })]);
        let documents = || ["a.rs", "b.rs", "c.rs"].into_iter().map(|name| DocumentEdits { path: name.into(), version: (name == "a.rs").then_some(7), observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "renamed")] }).collect();
        let plan = prepare(directory.path(), documents(), &open).unwrap();
        assert_eq!(plan.documents.len(), 3);
        publish_unopened(&plan).unwrap();
        assert_eq!(std::fs::read_to_string(directory.path().join("a.rs")).unwrap(), "old()");
        assert_eq!(std::fs::read_to_string(directory.path().join("b.rs")).unwrap(), "renamed()");
        open.values_mut().next().unwrap().protocol_version = Some(8);
        assert!(prepare(directory.path(), documents(), &open).is_err());
    }
    #[test]
    fn manager_protocol_versions_are_distinct_from_local_buffer_revisions() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a.rs");
        std::fs::write(&path, "old()").unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let mut open = HashMap::from([(path, OpenSnapshot { text: "old()".into(), version: 2, protocol_version: Some(7), writable: true })]);
        let documents = || vec![DocumentEdits { path: "a.rs".into(), version: Some(7), observed_version: Some(7), coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "new")] }];
        let plan = prepare(directory.path(), documents(), &open).unwrap();
        assert_eq!(plan.documents[0].version, Some(2), "runtime freeze retains its own revision");
        assert_eq!(plan.documents[0].protocol_version, Some(7), "protocol checks retain manager URI version");
        open.values_mut().next().unwrap().protocol_version = Some(8);
        assert!(prepare(directory.path(), documents(), &open).is_err());
    }

    #[test]
    fn equal_position_insertions_preserve_server_array_order_and_observed_versions() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("a.rs"), "body").unwrap();
        let path = std::fs::canonicalize(directory.path().join("a.rs")).unwrap();
        let open = HashMap::from([(path, OpenSnapshot { text: "body".into(), version: 7, protocol_version: Some(7), writable: true })]);
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
    fn replacing_previewed_root_with_identical_files_refuses_every_write() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("worktree");
        let moved = parent.path().join("original-worktree");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.rs"), "old").unwrap();
        let documents = vec![DocumentEdits { path: "a.rs".into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "new")] }];
        let resolved = resolve_targets(root.clone(), documents, Vec::new()).unwrap();
        let plan = prepare_resolved(resolved, &HashMap::new()).unwrap();
        std::fs::rename(&root, &moved).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.rs"), "old").unwrap();
        assert!(publish_unopened(&plan).unwrap_err().contains("replaced"));
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(moved.join("a.rs")).unwrap(), "old");
        // A rollback still has authority over the original root after rename,
        // independently of the name now identifying the replacement tree.
        assert!(matches!(editor::save_file_at(&plan.root, Path::new("a.rs"), "original only", blake3::hash(b"old"), false), Ok(SaveOutcome::Saved { .. })));
        assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(moved.join("a.rs")).unwrap(), "original only");
    }

    #[test]
    fn root_replacement_mid_publication_rolls_back_only_the_original_tree() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("worktree");
        let moved = parent.path().join("original");
        std::fs::create_dir(&root).unwrap();
        for name in ["a.rs", "b.rs"] { std::fs::write(root.join(name), "old").unwrap(); }
        let documents = ["a.rs", "b.rs"].into_iter().map(|name| DocumentEdits { path: name.into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "new")] }).collect();
        let plan = prepare(&root, documents, &HashMap::new()).unwrap();
        let result = publish_unopened_with_before_file(&plan, |index| {
            if index == 1 {
                assert_eq!(std::fs::read_to_string(root.join("a.rs")).unwrap(), "new");
                std::fs::rename(&root, &moved).unwrap();
                std::fs::create_dir(&root).unwrap();
                for name in ["a.rs", "b.rs"] { std::fs::write(root.join(name), "old").unwrap(); }
            }
        });
        assert!(result.unwrap_err().contains("Earlier writes were restored"));
        for tree in [&root, &moved] {
            for name in ["a.rs", "b.rs"] { assert_eq!(std::fs::read_to_string(tree.join(name)).unwrap(), "old"); }
        }
    }

    #[test]
    fn root_replacement_between_resolution_and_preparation_is_refused() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("worktree");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.rs"), "old").unwrap();
        let resolved = resolve_targets(root.clone(), vec![DocumentEdits { path: "a.rs".into(), version: None, observed_version: None, coordinates: LspCoordinates::Scalar, edits: vec![edit(0, 3, "new")] }], Vec::new()).unwrap();
        std::fs::rename(&root, parent.path().join("original")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.rs"), "old").unwrap();
        assert!(prepare_resolved(resolved, &HashMap::new()).unwrap_err().contains("replaced"));
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
