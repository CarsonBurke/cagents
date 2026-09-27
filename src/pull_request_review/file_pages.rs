//! Viewed-state pages are demand-driven and never overwrite local mutations.

use super::*;
use crate::github::PullRequestFilePage;

#[derive(Debug, Clone)]
pub enum PageAction {
    Viewport {
        offset: f32,
        height: f32,
    },
    More,
    Loaded {
        generation: u64,
        load: u64,
        cursor: String,
        result: Result<Arc<PullRequestFilePage>, String>,
    },
}

#[derive(Debug)]
pub(super) struct Pages {
    pub cursor: Option<String>,
    pub loading: bool,
    pub error: Option<String>,
    pub offset: f32,
    pub height: f32,
    pub wanted: Option<RepoPath>,
    pub seek_unviewed: bool,
    pub(super) seen: BTreeSet<String>,
}

impl Default for Pages {
    fn default() -> Self {
        Self {
            cursor: None,
            loading: false,
            error: None,
            offset: 0.0,
            height: 640.0,
            wanted: None,
            seek_unviewed: false,
            seen: BTreeSet::new(),
        }
    }
}

pub(super) fn matches_context(page: &PullRequestFilePage, context: &ReviewContext) -> bool {
    page.pull_request_id == context.pull_request_id
        && page.head_oid == context.head_oid
        && page.base_oid == context.base_oid
        && page.viewer == context.viewer
}

impl State {
    pub(super) fn page_action(&mut self, action: PageAction) -> ActionTask {
        match action {
            PageAction::Viewport { offset, height } => {
                if offset.is_finite() && height.is_finite() {
                    self.pages.offset = offset.max(0.0);
                    self.pages.height = height.max(0.0);
                }
            }
            PageAction::More => {
                self.pages.error = None;
                return self.load_file_page(true);
            }
            PageAction::Loaded {
                generation,
                load,
                cursor,
                result,
            } => {
                if generation != self.generation
                    || load != self.load_generation
                    || !self.pages.loading
                    || self.pages.cursor.as_ref() != Some(&cursor)
                {
                    return ActionTask::none();
                }
                self.pages.loading = false;
                let page = match result {
                    Ok(page) => page,
                    Err(error) => {
                        self.pages.error = Some(error);
                        return ActionTask::none();
                    }
                };
                if !self
                    .context
                    .as_ref()
                    .is_some_and(|context| matches_context(&page, context))
                {
                    self.pages.error = Some("The viewer or pull request changed. Refresh to load consistent viewed marks.".into());
                    return ActionTask::none();
                }
                if page
                    .next_cursor
                    .as_ref()
                    .is_some_and(|next| next == &cursor || self.pages.seen.contains(next))
                {
                    self.pages.error =
                        Some("GitHub repeated a file cursor. Refresh to retry.".into());
                    return ActionTask::none();
                }
                self.pages.seen.insert(cursor);
                self.pages.cursor = page.next_cursor.clone();
                let marks: HashMap<_, _> = page
                    .files
                    .iter()
                    .map(|file| (file.path.as_str(), file))
                    .collect();
                for file in &mut self.files {
                    // Only unknown marks can be hydrated. A concurrent user
                    // mutation, including its successful ACK, wins over a page.
                    if file.sync == ViewedSync::Unknown
                        && let Some(remote) = marks.get(file.entry.path.display().as_ref())
                    {
                        file.viewed = remote.viewer_viewed_state == FileViewedState::Viewed;
                        file.sync = ViewedSync::Settled;
                    }
                }
                let existing: BTreeSet<String> = self
                    .files
                    .iter()
                    .map(|file| file.entry.path.display().into_owned())
                    .collect();
                let additional: Vec<_> = page
                    .files
                    .iter()
                    .filter(|file| !existing.contains(&file.path))
                    .cloned()
                    .collect();
                if !additional.is_empty() {
                    self.files.extend(join_files(
                        &[],
                        &PullRequestFiles {
                            files: additional,
                            truncated: false,
                        },
                    ));
                    self.rebuild_rows();
                }
                self.cache_dirty = true;
                if self.pages.seek_unviewed {
                    self.next_unviewed();
                }
            }
        }
        ActionTask::none()
    }

    fn needs_file_page(&self) -> bool {
        let unknown = |path: &RepoPath| {
            self.file_index(path)
                .is_some_and(|index| self.files[index].sync == ViewedSync::Unknown)
        };
        if self.pages.wanted.as_ref().is_some_and(unknown)
            || self.selected.as_ref().is_some_and(unknown)
        {
            return true;
        }
        let displayed = self.displayed_file_rows(&directory_tallies(&self.rows, &self.files));
        window_rows(&self.rows, &displayed, self.pages.offset, self.pages.height)
            .into_iter()
            .any(|position| {
                self.rows[displayed[position]]
                    .file
                    .is_some_and(|file| self.files[file].sync == ViewedSync::Unknown)
            })
    }

    pub(super) fn load_file_page(&mut self, force: bool) -> ActionTask {
        if self.pages.loading
            || self.pages.error.is_some()
            || self.refreshing
            || self.load != LoadState::Ready
            || self.context.is_none()
            || (!force && !self.needs_file_page())
        {
            return ActionTask::none();
        }
        let (Some(cursor), Some(target)) = (self.pages.cursor.clone(), self.target.as_ref()) else {
            return ActionTask::none();
        };
        let Ok(key) = PullRequestKey::new(target.repository.clone(), target.pull_request.number)
        else {
            return ActionTask::none();
        };
        if self.pages.seen.len() >= 99 {
            self.pages.error =
                Some("Reached the file-page limit. Unloaded viewed states remain unknown.".into());
            return ActionTask::none();
        }
        self.pages.loading = true;
        let api = self.api.clone();
        let cancellation = self.cancellation.clone();
        let generation = self.generation;
        let load = self.load_generation;
        ActionTask::perform(
            async move {
                let request_cursor = cursor.clone();
                let result = tokio::task::spawn_blocking(move || {
                    api.pull_request_file_page(&key, Some(&request_cursor), &cancellation)
                        .map(Arc::new)
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                Action::FilePage(PageAction::Loaded {
                    generation,
                    load,
                    cursor,
                    result,
                })
            },
            |action| action,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{entry, finish_load, loaded, remote_file, state, target};
    use super::*;
    use crate::repository::ChangeKind;

    fn thousand_files() -> State {
        let (mut state, _) = state();
        drop(state.update(Action::Open(target(1))));
        let mut snapshot = loaded(
            (0..1000)
                .map(|i| entry(&format!("file-{i:04}.rs"), ChangeKind::Modified))
                .collect(),
            (0..100)
                .map(|i| remote_file(&format!("file-{i:04}.rs"), true))
                .collect(),
        );
        snapshot.next_file_cursor = Some("page-2".into());
        finish_load(&mut state, snapshot);
        state
    }

    fn answer(state: &mut State, files: Vec<crate::github::PullRequestFile>, viewer: &str) {
        let context = state.context.as_ref().unwrap();
        let page = PullRequestFilePage {
            files,
            next_cursor: None,
            viewer: viewer.into(),
            pull_request_id: context.pull_request_id.clone(),
            head_oid: context.head_oid.clone(),
            base_oid: context.base_oid.clone(),
        };
        drop(state.update(Action::FilePage(PageAction::Loaded {
            generation: state.generation,
            load: state.load_generation,
            cursor: "page-2".into(),
            result: Ok(Arc::new(page)),
        })));
    }

    #[test]
    fn thousand_file_review_loads_only_demanded_viewed_pages() {
        let mut state = thousand_files();
        assert_eq!(state.files.len(), 1000);
        assert_eq!(
            state
                .files
                .iter()
                .filter(|file| file.sync == ViewedSync::Unknown)
                .count(),
            900
        );
        assert!(!state.pages.loading, "offscreen pages must remain lazy");
        assert!(
            state
                .take_effects()
                .iter()
                .all(|effect| !matches!(effect, Effect::OpenFile { .. }))
        );
        drop(state.update(Action::FilePage(PageAction::Viewport {
            offset: design::DENSE_ROW_HEIGHT * 105.0,
            height: 400.0,
        })));
        assert!(state.pages.loading);
        answer(&mut state, vec![remote_file("file-0105.rs", true)], "me");
        assert_eq!(state.files[105].sync, ViewedSync::Settled);
        assert!(state.files[105].viewed);
        assert_eq!(
            state.files[999].sync,
            ViewedSync::Unknown,
            "absent state is not unviewed"
        );
    }

    #[test]
    fn later_pages_reject_changed_viewers_and_preserve_mutation_receipts() {
        let mut state = thousand_files();
        drop(state.update(Action::FilePage(PageAction::More)));
        answer(
            &mut state,
            vec![remote_file("file-0105.rs", true)],
            "someone-else",
        );
        assert!(state.pages.error.is_some());
        assert_eq!(state.files[105].sync, ViewedSync::Unknown);
        drop(state.update(Action::FilePage(PageAction::More)));
        state.files[0].viewed = false; // a completed concurrent mutation
        answer(
            &mut state,
            vec![
                remote_file("file-0000.rs", true),
                remote_file("file-0105.rs", true),
            ],
            "me",
        );
        assert!(!state.files[0].viewed);
        assert!(state.files[105].viewed);
    }

    #[test]
    fn unknown_marks_cannot_be_toggled_or_used_as_next_unviewed() {
        let mut state = thousand_files();
        let path = RepoPath::parse("file-0100.rs").unwrap();
        drop(state.update(Action::SelectFile(path.clone())));
        drop(state.update(Action::ToggleViewedAndAdvance));
        assert_eq!(state.selected.as_ref(), Some(&path));
        assert_eq!(state.files[100].sync, ViewedSync::Unknown);
        state.selected = None;
        drop(state.update(Action::NextUnviewed));
        assert!(state.selected.is_none());
        assert!(state.pages.seek_unviewed);
        answer(&mut state, vec![remote_file("file-0100.rs", false)], "me");
        assert_eq!(state.selected.as_ref(), Some(&path));
    }

    #[test]
    fn a_thousand_file_window_retains_ancestors_without_mounting_offscreen_rows() {
        let files: Vec<_> = (0..1000)
            .map(|i| ReviewFile {
                entry: entry(
                    &format!("src/group-{}/{i:04}.rs", i / 100),
                    ChangeKind::Modified,
                ),
                viewed: false,
                sync: ViewedSync::Unknown,
            })
            .collect();
        let rows = tree_rows(&files);
        let displayed: Vec<_> = (0..rows.len()).collect();
        let positions = window_rows(&rows, &displayed, 520.0 * design::DENSE_ROW_HEIGHT, 640.0);
        assert!(
            positions.len() < 60,
            "only visible rows and ancestors are mounted: {}",
            positions.len()
        );
        assert_eq!(positions[0], 0, "root ancestor remains sticky");
        assert!(positions.contains(&520));
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn refresh_preserves_the_viewport_and_fetches_its_unknown_marks() {
        let mut state = thousand_files();
        state.pages.offset = 700.0 * design::DENSE_ROW_HEIGHT;
        state.pages.height = 900.0;
        let mut snapshot = loaded(
            state.files.iter().map(|file| file.entry.clone()).collect(),
            (0..100)
                .map(|i| remote_file(&format!("file-{i:04}.rs"), true))
                .collect(),
        );
        snapshot.next_file_cursor = Some("page-2".into());
        finish_load(&mut state, snapshot);
        assert_eq!(state.pages.offset, 700.0 * design::DENSE_ROW_HEIGHT);
        assert_eq!(state.pages.height, 900.0);
        assert!(
            state.pages.loading,
            "refresh must hydrate the still-visible deep viewport"
        );
    }

    #[test]
    fn hiding_viewed_directories_uses_the_same_clamped_window_for_loading() {
        let (mut state, _) = state();
        drop(state.update(Action::Open(target(1))));
        let names: Vec<_> = (0..101).map(|i| format!("group-{i:03}/file.rs")).collect();
        let mut snapshot = loaded(
            names
                .iter()
                .map(|path| entry(path, ChangeKind::Added))
                .collect(),
            names[..100]
                .iter()
                .map(|path| remote_file(path, true))
                .collect(),
        );
        snapshot.next_file_cursor = Some("page-2".into());
        finish_load(&mut state, snapshot);
        state.pages.offset = 5000.0;
        drop(state.update(Action::ToggleHideViewed));
        let displayed = state.displayed_file_rows(&directory_tallies(&state.rows, &state.files));
        assert_eq!(displayed.len(), 2);
        assert!(
            state.pages.loading,
            "the visible unknown file must be requested after the list shrinks"
        );
    }

    #[test]
    fn a_refresh_rejects_old_page_completion() {
        let mut state = thousand_files();
        drop(state.update(Action::FilePage(PageAction::More)));
        let old_load = state.load_generation;
        drop(state.update(Action::Refresh));
        drop(state.update(Action::FilePage(PageAction::Loaded {
            generation: state.generation,
            load: old_load,
            cursor: "page-2".into(),
            result: Err("stale failure".into()),
        })));
        assert!(state.pages.error.is_none());
    }
}

/// Keep the viewport, overscan, and its open folder ancestors. Gaps become
/// spacers in the same tree, preserving scroll extent and sticky headers.
pub(super) fn window_rows(
    rows: &[Row],
    displayed: &[usize],
    offset: f32,
    height: f32,
) -> Vec<usize> {
    const OVERSCAN: usize = 8;
    let offset = offset.clamp(
        0.0,
        (displayed.len() as f32 * design::DENSE_ROW_HEIGHT - height).max(0.0),
    );
    let first = ((offset / design::DENSE_ROW_HEIGHT).floor() as usize)
        .min(displayed.len())
        .saturating_sub(OVERSCAN);
    let end = (first + (height.max(0.0) / design::DENSE_ROW_HEIGHT).ceil() as usize + OVERSCAN * 2)
        .min(displayed.len());
    let mut ancestors: Vec<usize> = Vec::new();
    for position in 0..first {
        let row = &rows[displayed[position]];
        while ancestors
            .last()
            .is_some_and(|parent| rows[displayed[*parent]].depth >= row.depth)
        {
            ancestors.pop();
        }
        if row.file.is_none() {
            ancestors.push(position);
        }
    }
    if let Some(index) = displayed.get(first) {
        while ancestors
            .last()
            .is_some_and(|parent| rows[displayed[*parent]].depth >= rows[*index].depth)
        {
            ancestors.pop();
        }
    }
    ancestors.extend(first..end);
    ancestors
}
