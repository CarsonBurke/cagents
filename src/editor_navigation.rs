//! Source locations, per-card navigation, and bounded saved-file undo retention.
use crate::{
    editor_history::History,
    repository_widget::text_editor::{Cursor, ScrollPosition},
};
use std::{collections::VecDeque, path::PathBuf};

/// Compiler-style suffixes are parsed from the right; ordinary colon-bearing
/// paths remain untouched. Returned coordinates are one based.
pub(crate) fn parse_location(input: &str) -> Option<(String, usize, usize)> {
    let (prefix, last) = input.rsplit_once(':')?;
    let last = last.parse::<usize>().ok().filter(|number| *number > 0)?;
    let (path, line, column) = match prefix
        .rsplit_once(':')
        .filter(|(_, line)| line.parse::<usize>().is_ok())
    {
        Some((path, line)) => (
            path,
            line.parse::<usize>().ok().filter(|line| *line > 0)?,
            last,
        ),
        None => (prefix, last, 1),
    };
    (!path.is_empty()).then(|| (path.to_owned(), line, column))
}

pub(crate) fn file_query(input: &str) -> std::borrow::Cow<'_, str> {
    parse_location(input).map_or_else(
        || std::borrow::Cow::Borrowed(input),
        |(path, _, _)| std::borrow::Cow::Owned(path),
    )
}

pub(crate) fn parse_line_column(input: &str) -> Option<(usize, usize)> {
    let (line, column) = input
        .trim()
        .split_once(':')
        .map_or((input.trim(), "1"), |pair| pair);
    Some((
        line.parse::<usize>().ok()?.checked_sub(1)?,
        column.parse::<usize>().ok()?.checked_sub(1)?,
    ))
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Location {
    pub path: PathBuf,
    pub cursor: Cursor,
    pub scroll: ScrollPosition,
}
#[derive(Default)]
pub(crate) struct Locations {
    entries: Vec<Location>,
    current: usize,
}
impl Locations {
    pub fn jump(&mut self, from: Location, to: Location) {
        if self.entries.is_empty() {
            self.entries.push(from.clone());
            self.current = 0;
        } else {
            self.entries[self.current] = from.clone();
        }
        self.entries.truncate(self.current + 1);
        if from.path == to.path && from.cursor == to.cursor {
            return;
        }
        self.entries.push(to);
        self.current += 1;
        if self.entries.len() > 128 {
            self.entries.remove(0);
            self.current -= 1;
        }
    }
    pub fn peek(&self, forward: bool) -> Option<&Location> {
        let next = if forward {
            self.current.checked_add(1)?
        } else {
            self.current.checked_sub(1)?
        };
        self.entries.get(next)
    }
    pub fn navigate(&mut self, from: Location, forward: bool) -> Option<Location> {
        let next = if forward {
            self.current.checked_add(1)?
        } else {
            self.current.checked_sub(1)?
        };
        if next >= self.entries.len() {
            return None;
        }
        self.entries[self.current] = from;
        self.current = next;
        Some(self.entries[next].clone())
    }
}

/// A history and the immutable metadata identities its presence edges name.
/// Cloned Arcs retain the existing restoration permits; the cache cannot
/// reconstruct or silently substitute a missing template.
#[derive(Default)]
pub(crate) struct SavedPresence {
    pub present: bool,
    pub id: u64,
    pub next_id: u64,
    pub deletion_mode: Option<u32>,
    pub restorations:
        std::collections::BTreeMap<u64, std::sync::Arc<crate::editor::FileRestoration>>,
    pub captured_ids: std::collections::BTreeSet<u64>,
}
pub(crate) struct SavedUndo {
    pub source: PathBuf,
    pub disk_hash: blake3::Hash,
    pub history: History,
    pub cursor: Cursor,
    pub root: Option<crate::editor::FileRootIdentity>,
    pub presence: SavedPresence,
}
impl SavedUndo {
    fn retained_bytes(&self) -> usize {
        let mut seen = std::collections::BTreeSet::new();
        self.presence
            .restorations
            .values()
            .filter(|template| seen.insert(std::sync::Arc::as_ptr(template)))
            .fold(
                self.history
                    .retained_bytes()
                    .saturating_add(std::mem::size_of::<Self>())
                    .saturating_add(self.source.as_os_str().len())
                    .saturating_add(self.presence.restorations.len().saturating_mul(96))
                    .saturating_add(self.presence.captured_ids.len().saturating_mul(32)),
                |bytes, template| bytes.saturating_add(template.retained_bytes()),
            )
    }
}
#[derive(Default)]
pub(crate) struct UndoLru {
    entries: VecDeque<(PathBuf, SavedUndo)>,
    bytes: usize,
}
impl UndoLru {
    pub(crate) fn retained_root_identities(
        &self,
    ) -> impl Iterator<Item = &crate::editor::FileRootIdentity> {
        self.entries
            .iter()
            .filter_map(|(_, undo)| undo.root.as_ref())
    }
    /// At most eight cached histories; clone original descriptor Arcs only.
    pub(crate) fn retained_roots(
        &self,
    ) -> impl Iterator<Item = std::sync::Arc<crate::fs_safety::Root>> + '_ {
        self.entries.iter().filter_map(|(_, undo)| {
            undo.root
                .as_ref()
                .map(crate::editor::FileRootIdentity::retained_root)
        })
    }
    pub fn insert(&mut self, path: PathBuf, mut undo: SavedUndo) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(candidate, _)| *candidate == path)
        {
            if let Some((_, previous)) = self.entries.remove(index) {
                self.bytes -= previous.retained_bytes();
            }
        }
        undo.history.seal();
        let bytes = undo.retained_bytes();
        if bytes > 32 * 1024 * 1024 {
            return;
        }
        self.bytes += bytes;
        self.entries.push_back((path, undo));
        while self.entries.len() > 8 || self.bytes > 32 * 1024 * 1024 {
            if let Some((_, oldest)) = self.entries.pop_front() {
                self.bytes -= oldest.retained_bytes();
            }
        }
    }
    pub fn expected_absence(
        &self,
        path: &std::path::Path,
    ) -> Option<(PathBuf, crate::editor::FileRootIdentity)> {
        self.entries
            .iter()
            .find(|(candidate, _)| candidate == path)
            .filter(|(_, undo)| !undo.presence.present)
            .and_then(|(_, undo)| Some((undo.source.clone(), undo.root.clone()?)))
    }
    pub fn validate_root(
        &mut self,
        path: &std::path::Path,
        root: Option<&crate::editor::FileRootIdentity>,
    ) {
        if self
            .entries
            .iter()
            .any(|(candidate, undo)| candidate == path && undo.root.as_ref() != root)
        {
            self.remove(path);
        }
    }
    pub fn remove(&mut self, path: &std::path::Path) {
        if let Some(index) = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == path)
            && let Some((_, previous)) = self.entries.remove(index)
        {
            self.bytes -= previous.retained_bytes();
        }
    }
    pub fn take(
        &mut self,
        path: &std::path::Path,
        source: &std::path::Path,
        hash: blake3::Hash,
        present: bool,
    ) -> Option<SavedUndo> {
        let index = self
            .entries
            .iter()
            .position(|(candidate, _)| candidate == path)?;
        let (_, undo) = self.entries.remove(index)?;
        self.bytes -= undo.retained_bytes();
        (undo.source == source && undo.disk_hash == hash && undo.presence.present == present)
            .then_some(undo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository_widget::text_editor::Position;
    fn location(path: &str, line: usize) -> Location {
        Location {
            path: path.into(),
            cursor: Cursor {
                position: Position { line, column: 0 },
                selection: None,
            },
            scroll: ScrollPosition::default(),
        }
    }
    #[test]
    fn compiler_locations_preserve_colon_paths_and_validate_coordinates() {
        assert_eq!(
            parse_location("src/app.rs:2285"),
            Some(("src/app.rs".into(), 2285, 1))
        );
        assert_eq!(
            parse_location("src/app.rs:120:8"),
            Some(("src/app.rs".into(), 120, 8))
        );
        assert_eq!(parse_location("a:b.rs:12"), Some(("a:b.rs".into(), 12, 1)));
        assert_eq!(parse_location("a.rs:0"), None);
        assert_eq!(parse_location("a.rs:12:0"), None);
        assert_eq!(parse_location("a:b.rs"), None);
        assert_eq!(parse_line_column("30000:5"), Some((29999, 4)));
        assert_eq!(parse_line_column("0"), None);
    }
    #[test]
    fn back_and_forward_keep_positions_and_discard_a_departed_forward_branch() {
        let mut history = Locations::default();
        history.jump(location("a", 1), location("b", 2));
        assert_eq!(
            history.navigate(location("b", 3), false),
            Some(location("a", 1))
        );
        assert_eq!(
            history.navigate(location("a", 1), true),
            Some(location("b", 3))
        );
        history.navigate(location("b", 3), false);
        history.jump(location("a", 1), location("c", 4));
        assert!(history.navigate(location("c", 4), true).is_none());
    }
    #[test]
    fn eviction_releases_immutable_metadata_and_accounts_aliased_templates_once() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("source"), "saved").unwrap();
        let root = std::sync::Arc::new(crate::editor::worktree_root(directory.path()).unwrap());
        let template = crate::editor::capture_restoration_at(
            &root,
            std::path::Path::new("source"),
            blake3::hash(b"saved"),
        )
        .unwrap();
        let weak = std::sync::Arc::downgrade(&template);
        let mut presence = SavedPresence {
            present: true,
            ..Default::default()
        };
        presence.restorations.insert(1, template.clone());
        presence.restorations.insert(2, template.clone());
        let saved = SavedUndo {
            source: "source".into(),
            disk_hash: blake3::hash(b"saved"),
            history: History::default(),
            cursor: location("", 0).cursor,
            root: Some(crate::editor::FileRootIdentity::of(root).unwrap()),
            presence,
        };
        assert_eq!(
            saved.retained_bytes(),
            std::mem::size_of::<SavedUndo>() + "source".len() + 2 * 96 + template.retained_bytes()
        );
        drop(template);
        let mut cache = UndoLru::default();
        cache.insert("retained".into(), saved);
        assert!(weak.upgrade().is_some());
        for index in 0..8 {
            cache.insert(
                format!("{index}").into(),
                SavedUndo {
                    source: "source".into(),
                    disk_hash: blake3::hash(b"saved"),
                    history: History::default(),
                    cursor: location("", 0).cursor,
                    root: None,
                    presence: SavedPresence {
                        present: true,
                        ..Default::default()
                    },
                },
            );
        }
        assert!(
            weak.upgrade().is_none(),
            "eviction must release the retained descriptor permit"
        );
        assert_eq!(cache.entries.len(), 8);
    }

    #[test]
    fn undo_lru_requires_matching_source_and_disk_hash_and_is_bounded() {
        let mut cache = UndoLru::default();
        let hash = blake3::hash(b"same");
        for index in 0..10 {
            cache.insert(
                format!("{index}").into(),
                SavedUndo {
                    source: "source".into(),
                    disk_hash: hash,
                    history: History::default(),
                    cursor: location("", 0).cursor,
                    root: None,
                    presence: SavedPresence {
                        present: true,
                        ..Default::default()
                    },
                },
            );
        }
        assert!(
            cache
                .take(
                    std::path::Path::new("0"),
                    std::path::Path::new("source"),
                    hash,
                    true
                )
                .is_none()
        );
        assert!(
            cache
                .take(
                    std::path::Path::new("9"),
                    std::path::Path::new("source"),
                    blake3::hash(b"changed"),
                    true
                )
                .is_none()
        );
        assert!(
            cache
                .take(
                    std::path::Path::new("8"),
                    std::path::Path::new("source"),
                    hash,
                    true
                )
                .is_some()
        );
    }
}
