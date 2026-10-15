//! Filesystem stamps and line anchors for asynchronous open-document refresh.
use std::{fs::Metadata, os::unix::fs::MetadataExt, path::Path};
use crate::{editor::{FileContent, LoadedFile, PdfTone}, fs_safety::{self, Root}};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp { device: u64, inode: u64, size: u64, modified: (i64, i64), changed: (i64, i64) }
impl From<Metadata> for FileStamp {
    fn from(meta: Metadata) -> Self {
        Self { device: meta.dev(), inode: meta.ino(), size: meta.len(), modified: (meta.mtime(), meta.mtime_nsec()), changed: (meta.ctime(), meta.ctime_nsec()) }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp { selected: FileStamp, source: FileStamp }

pub fn stamp(worktree: &Path, selected: &Path, source: &Path) -> Result<Option<Stamp>, String> {
    let root = Root::open(worktree).map_err(|error| error.to_string())?;
    let selected = match fs_safety::metadata_beneath(&root, selected) {
        Ok(metadata) => metadata,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let source = match fs_safety::metadata_beneath(&root, source) {
        Ok(metadata) => metadata,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    Ok(Some(Stamp { selected: selected.into(), source: source.into() }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision { Unchanged, Reload, Conflict, Deleted }
pub fn decide(baseline: blake3::Hash, observed: Option<blake3::Hash>, dirty: bool) -> Decision {
    match observed { None => Decision::Deleted, Some(hash) if hash == baseline => Decision::Unchanged,
        Some(_) if dirty => Decision::Conflict, Some(_) => Decision::Reload }
}

#[derive(Debug, Clone, Default)]
pub struct LineMap(Vec<usize>);
impl LineMap {
    pub fn map(&self, line: usize) -> usize { self.0.get(line).copied().unwrap_or_else(|| self.0.last().copied().unwrap_or(0)) }
    pub fn between(before: &str, after: &str) -> Self {
        use cosmic::iced::advanced::graphics::text::cosmic_text::LineIter;
        use imara_diff::{Algorithm, Diff, InternedInput};
        let lines = |text: &str| {
            let mut ranges: Vec<_> = LineIter::new(text).map(|(range, _)| range).collect();
            if ranges.is_empty() || text.ends_with(['\r', '\n']) { ranges.push(text.len()..text.len()); }
            ranges
        };
        let before_ranges = lines(before); let after_ranges = lines(after);
        let mut input = InternedInput::default();
        input.update_before(before_ranges.iter().map(|range| &before[range.clone()]));
        input.update_after(after_ranges.iter().map(|range| &after[range.clone()]));
        let diff = Diff::compute(Algorithm::Histogram, &input);
        let mut mapped = vec![0; before_ranges.len()];
        let (mut old, mut new) = (0, 0);
        for hunk in diff.hunks() {
            while old < hunk.before.start as usize { mapped[old] = new; old += 1; new += 1; }
            for index in hunk.before.clone() {
                mapped[index as usize] = (hunk.after.start as usize + (index - hunk.before.start).min(hunk.after.len().saturating_sub(1) as u32) as usize).min(after_ranges.len().saturating_sub(1));
            }
            old = hunk.before.end as usize; new = hunk.after.end as usize;
        }
        while old < mapped.len() { mapped[old] = new.min(after_ranges.len().saturating_sub(1)); old += 1; new += 1; }
        // A moved, unchanged line should retain its caret, even if the line
        // diff represented its move as a deletion followed by an insertion.
        let mut occurrences = std::collections::HashMap::<&str, Vec<usize>>::new();
        for (line, range) in after_ranges.iter().enumerate() { occurrences.entry(&after[range.clone()]).or_default().push(line); }
        for (line, range) in before_ranges.iter().enumerate() {
            let text = &before[range.clone()];
            if !text.is_empty() && after_ranges.get(mapped[line]).is_none_or(|range| &after[range.clone()] != text)
                && let Some(locations) = occurrences.get(text)
            { mapped[line] = *locations.iter().min_by_key(|candidate| candidate.abs_diff(mapped[line])).unwrap(); }
        }
        Self(mapped)
    }
}

#[derive(Debug, Clone)]
pub struct Version { pub text: String, pub content_hash: blake3::Hash, pub source: std::path::PathBuf, pub markdown: bool, pub mapping: LineMap }
pub fn load(worktree: &Path, path: &Path, before: &str, tone: PdfTone) -> Result<Version, String> {
    match crate::editor::load_file(worktree, path, tone) {
        Ok(LoadedFile { source, content: FileContent::Editable { text, content_hash, markdown } }) => {
            let mapping = LineMap::between(before, &text);
            Ok(Version { text, content_hash, source, markdown, mapping })
        }
        Ok(_) => Err("The file on disk is no longer editable text. Your buffer has been kept.".into()),
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reload_policy_distinguishes_clean_dirty_own_saves_and_deletion() {
        let old = blake3::hash(b"old"); let new = blake3::hash(b"new");
        assert_eq!(decide(old, Some(new), false), Decision::Reload);
        assert_eq!(decide(old, Some(new), true), Decision::Conflict);
        assert_eq!(decide(old, Some(old), true), Decision::Unchanged);
        assert_eq!(decide(old, None, false), Decision::Deleted);
    }
    #[test]
    fn caret_mapping_tracks_inserted_removed_and_moved_lines() {
        let map = LineMap::between("one\ntwo\nthree\n", "added\none\ntwo\nthree\n");
        assert_eq!(map.map(1), 2); assert_eq!(map.map(3), 4);
        let map = LineMap::between("one\ntwo\nthree", "three\none\ntwo");
        assert_eq!(map.map(2), 0); assert_eq!(map.map(1), 2);
        let map = LineMap::between("one\r\ntwo\rthree", "one\r\nthree");
        assert_eq!(map.map(2), 1);
    }
    #[test]
    fn stamps_distinguish_atomic_replacements_and_missing_paths() {
        let root = tempfile::tempdir().unwrap(); let path = Path::new("a.rs");
        std::fs::write(root.path().join(path), "one").unwrap();
        let first = stamp(root.path(), path, path).unwrap();
        std::fs::write(root.path().join("new"), "two").unwrap();
        std::fs::rename(root.path().join("new"), root.path().join(path)).unwrap();
        assert_ne!(first, stamp(root.path(), path, path).unwrap());
        std::fs::remove_file(root.path().join(path)).unwrap();
        assert_eq!(stamp(root.path(), path, path).unwrap(), None);
    }
}
