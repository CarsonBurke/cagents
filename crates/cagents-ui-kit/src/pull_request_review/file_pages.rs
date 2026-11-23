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
    pub hydrate_all: bool,
    pub(super) seen: BTreeSet<String>,
    /// Settled later-page marks a refresh could not confirm, with the cursor
    /// of the page that last delivered each. They stay visible until demanded,
    /// then only their own page is reread.
    pub(super) revalidate: BTreeMap<RepoPath, String>,
    /// The request cursor of the page that last delivered each later mark.
    /// Cursors stay valid while the head, base and viewer do.
    pub(super) origin: HashMap<RepoPath, String>,
    /// While `loading`, the page being reread for revalidation; otherwise the
    /// in-flight request is the next unread page at `cursor`.
    pub(super) revalidating: Option<String>,
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
            hydrate_all: false,
            seen: BTreeSet::new(),
            revalidate: BTreeMap::new(),
            origin: HashMap::new(),
            revalidating: None,
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
                let requested = self
                    .pages
                    .revalidating
                    .as_ref()
                    .or(self.pages.cursor.as_ref());
                if generation != self.generation
                    || load != self.load_generation
                    || !self.pages.loading
                    || requested != Some(&cursor)
                {
                    return ActionTask::none();
                }
                self.pages.loading = false;
                let revalidation = self.pages.revalidating.take().is_some();
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
                // A reread page never advances pagination.
                if !revalidation {
                    if page
                        .next_cursor
                        .as_ref()
                        .is_some_and(|next| next == &cursor || self.pages.seen.contains(next))
                    {
                        self.pages.error =
                            Some("GitHub repeated a file cursor. Refresh to retry.".into());
                        return ActionTask::none();
                    }
                    self.pages.seen.insert(cursor.clone());
                    self.pages.cursor = page.next_cursor.clone();
                    if self.pages.cursor.is_none() {
                        self.pages.hydrate_all = false;
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
                let marks: HashMap<_, _> = page
                    .files
                    .iter()
                    .map(|file| (file.path.as_str(), file))
                    .collect();
                for file in &mut self.files {
                    let Some(remote) = marks.get(file.entry.path.display().as_ref()) else {
                        continue;
                    };
                    self.pages
                        .origin
                        .insert(file.entry.path.clone(), cursor.clone());
                    // Mutations remove revalidation eligibility, including
                    // when their ACK arrives before this older page does.
                    if file.sync == ViewedSync::Unknown
                        || self.pages.revalidate.remove(&file.entry.path).is_some()
                    {
                        file.viewed = remote.viewer_viewed_state == FileViewedState::Viewed;
                        file.sync = ViewedSync::Settled;
                    }
                }
                if revalidation {
                    // Settle the page's stale marks even if GitHub no longer
                    // lists them there, so they cannot demand it again.
                    self.pages.revalidate.retain(|_, origin| *origin != cursor);
                }
                self.cache_dirty = true;
                if self.pages.seek_unviewed {
                    self.next_unviewed();
                }
            }
        }
        ActionTask::none()
    }

    /// The page to read next with whether it rereads a stale mark: the page
    /// of a demanded stale mark, otherwise the next unread page when it is
    /// demanded. Stale marks never restart pagination.
    fn page_request(&self, force: bool) -> Option<(String, bool)> {
        if self.pages.cursor.is_none() && self.pages.revalidate.is_empty() {
            return None;
        }
        let mut demanded: Vec<usize> = [self.pages.wanted.as_ref(), self.selected.as_ref()]
            .into_iter()
            .flatten()
            .filter_map(|path| self.file_index(path))
            .collect();
        let next_forced = force || self.pages.hydrate_all;
        if !next_forced || !self.pages.revalidate.is_empty() {
            let displayed = self.displayed_file_rows(&directory_tallies(&self.rows, &self.files));
            demanded.extend(
                window_rows(&self.rows, &displayed, self.pages.offset, self.pages.height)
                    .into_iter()
                    .filter_map(|position| self.rows[displayed[position]].file),
            );
        }
        if let Some(origin) = demanded
            .iter()
            .find_map(|&index| self.pages.revalidate.get(&self.files[index].entry.path))
        {
            return Some((origin.clone(), true));
        }
        let next = next_forced
            || demanded
                .iter()
                .any(|&index| self.files[index].sync == ViewedSync::Unknown);
        Some((self.pages.cursor.clone().filter(|_| next)?, false))
    }

    pub(super) fn load_file_page(&mut self, force: bool) -> ActionTask {
        if self.pages.loading
            || self.pages.error.is_some()
            || self.refreshing
            || self.target.as_ref().is_some_and(|target| {
                !crate::github::polling::ready(&target.repository.authority(), false)
            })
            || self.load != LoadState::Ready
            || self.context.is_none()
        {
            return ActionTask::none();
        }
        let (Some((cursor, revalidation)), Some(target)) =
            (self.page_request(force), self.target.as_ref())
        else {
            return ActionTask::none();
        };
        let Ok(key) = PullRequestKey::new(target.repository.clone(), target.pull_request.number)
        else {
            return ActionTask::none();
        };
        if !revalidation && self.pages.seen.len() >= 99 {
            self.pages.error =
                Some("Reached the file-page limit. Unloaded viewed states remain unknown.".into());
            return ActionTask::none();
        }
        self.pages.loading = true;
        self.pages.revalidating = revalidation.then(|| cursor.clone());
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
    use super::super::test_support::{entry, finish_load, loaded, remote_file, state, target};
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
        answer_next(state, files, viewer, None);
    }

    fn answer_next(
        state: &mut State,
        files: Vec<crate::github::PullRequestFile>,
        viewer: &str,
        next_cursor: Option<String>,
    ) {
        answer_at(state, "page-2", files, viewer, next_cursor);
    }

    fn answer_at(
        state: &mut State,
        cursor: &str,
        files: Vec<crate::github::PullRequestFile>,
        viewer: &str,
        next_cursor: Option<String>,
    ) {
        let context = state.context.as_ref().unwrap();
        let page = PullRequestFilePage {
            files,
            next_cursor,
            viewer: viewer.into(),
            pull_request_id: context.pull_request_id.clone(),
            head_oid: context.head_oid.clone(),
            base_oid: context.base_oid.clone(),
        };
        drop(state.update(Action::FilePage(PageAction::Loaded {
            generation: state.generation,
            load: state.load_generation,
            cursor: cursor.into(),
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
    fn refresh_keeps_later_page_marks_and_cursor_for_same_authority() {
        let mut state = thousand_files();
        drop(state.update(Action::FilePage(PageAction::More)));
        answer(&mut state, vec![remote_file("file-0105.rs", true)], "me");
        let mut snapshot = loaded(
            state.files.iter().map(|file| file.entry.clone()).collect(),
            (0..100)
                .map(|i| remote_file(&format!("file-{i:04}.rs"), true))
                .collect(),
        );
        snapshot.next_file_cursor = Some("page-2".into());
        finish_load(&mut state, snapshot.clone());
        finish_load(&mut state, snapshot.clone());
        assert!(state.files[105].viewed);
        assert_eq!(state.files[105].sync, ViewedSync::Settled);
        assert!(state.pages.cursor.is_none());
        assert!(state.pages.seen.contains("page-2"));
        snapshot.context.head_oid = "c".repeat(40);
        snapshot.range.head = Oid::parse(&snapshot.context.head_oid).unwrap();
        finish_load(&mut state, snapshot);
        assert_eq!(state.files[105].sync, ViewedSync::Unknown);
        assert!(
            state.pages.loading,
            "changed heads automatically hydrate later pages"
        );
        answer(&mut state, vec![remote_file("file-0105.rs", true)], "me");
        assert!(state.files[105].viewed);
        assert_eq!(state.files[105].sync, ViewedSync::Settled);
    }

    #[tokio::test]
    async fn refresh_revalidates_demanded_settled_marks_after_completed_pagination() {
        use crate::repository::{CommandSpec, ProcessExecutor, ProcessFailure, ProcessOutput};
        use cosmic::iced::futures::StreamExt;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        struct Remote {
            viewed: AtomicBool,
            later_reads: AtomicUsize,
        }
        impl ProcessExecutor for Remote {
            fn execute(
                &self,
                command: &CommandSpec,
                _: &CancellationToken,
            ) -> Result<ProcessOutput, ProcessFailure> {
                let later = command.arguments.iter().any(|arg| arg == "cursor=page-2");
                assert!(command.arguments.iter().any(|arg| {
                    arg.to_string_lossy().contains(if later {
                        "files(first:100,after:$cursor)"
                    } else {
                        "files(first:100)"
                    })
                }));
                if later {
                    self.later_reads.fetch_add(1, Ordering::Relaxed);
                }
                let files: Vec<_> = (if later { 100..105 } else { 0..100 })
                    .map(|i| {
                        serde_json::json!({
                            "path": format!("file-{i:04}.rs"),
                            "viewerViewedState": if i != 104 || self.viewed.load(Ordering::Relaxed) {
                                "VIEWED"
                            } else {
                                "UNVIEWED"
                            }
                        })
                    })
                    .collect();
                let body = serde_json::json!({"data": {
                    "viewer": {"login": "me"},
                    "repository": {"pullRequest": {
                        "id": "PR_1", "number": 1, "title": "Review", "state": "OPEN",
                        "headRefName": "feature", "headRefOid": "a".repeat(40),
                        "baseRefName": "main", "baseRefOid": "b".repeat(40),
                        "updatedAt": "2026-11-19T00:00:00Z",
                        "url": "https://viewed-refresh-test.invalid/o/r/pull/1",
                        "viewerDidAuthor": false,
                        "reviews": {"nodes": [], "pageInfo": {"hasNextPage": false}},
                        "files": {"nodes": files, "pageInfo": {
                            "hasNextPage": !later,
                            "endCursor": if later { None } else { Some("page-2") }
                        }}
                    }}
                }});
                Ok(ProcessOutput {
                    success: true,
                    exit_code: Some(0),
                    signal: None,
                    stdout: format!("HTTP/2.0 200 OK\r\n\r\n{body}").into_bytes(),
                    stderr: vec![],
                })
            }
        }

        async fn complete(state: &mut State, task: ActionTask) {
            let mut stream = iced::runtime::task::into_stream(task).unwrap();
            while let Some(action) = stream.next().await {
                if let iced::runtime::Action::Output(action) = action {
                    drop(state.update(action));
                }
            }
        }

        let remote = Arc::new(Remote {
            viewed: AtomicBool::new(true),
            later_reads: AtomicUsize::new(0),
        });
        let api = Arc::new(crate::github::GitHubService::new("gh", remote.clone()));
        let mut state = State::new(api, RepositoryService::system("git"));
        let mut target = target(1);
        target.repository =
            GitHubRepoKey::new("viewed-refresh-test.invalid", None, "o", "r").unwrap();
        drop(state.update(Action::Open(target)));
        let mut snapshot = loaded(
            (0..105)
                .map(|i| entry(&format!("file-{i:04}.rs"), ChangeKind::Modified))
                .collect(),
            (0..100)
                .map(|i| remote_file(&format!("file-{i:04}.rs"), true))
                .collect(),
        );
        snapshot.next_file_cursor = Some("page-2".into());
        finish_load(&mut state, snapshot);
        let task = state.update(Action::FilePage(PageAction::Viewport {
            offset: 102.0 * design::DENSE_ROW_HEIGHT,
            height: 2.0 * design::DENSE_ROW_HEIGHT,
        }));
        complete(&mut state, task).await;
        assert!(state.pages.cursor.is_none());
        assert!(state.files[104].viewed);
        assert_eq!(state.files[104].sync, ViewedSync::Settled);

        for (refresh, viewed) in [false, true].into_iter().enumerate() {
            state.selected = None;
            drop(state.update(Action::FilePage(PageAction::Viewport {
                offset: 0.0,
                height: 2.0 * design::DENSE_ROW_HEIGHT,
            })));
            remote.viewed.store(viewed, Ordering::Relaxed);
            let task = state.update(Action::Refresh);
            complete(&mut state, task).await;
            assert!(!state.refreshing);
            assert!(state.notice.is_none(), "refresh failed: {:?}", state.notice);
            assert_eq!(
                state.files[104].viewed, !viewed,
                "retain the old mark while loading"
            );
            assert_eq!(state.files[104].sync, ViewedSync::Settled);
            assert!(!state.pages.loading, "offscreen marks remain demand-driven");
            let task = state.update(Action::FilePage(PageAction::Viewport {
                offset: 102.0 * design::DENSE_ROW_HEIGHT,
                height: 2.0 * design::DENSE_ROW_HEIGHT,
            }));
            assert!(
                state.pages.loading,
                "returning to page two must revalidate viewed marks after Refresh"
            );
            complete(&mut state, task).await;
            assert_eq!(remote.later_reads.load(Ordering::Relaxed), refresh + 2);
            assert_eq!(state.files[104].viewed, viewed);
            assert_eq!(state.files[104].sync, ViewedSync::Settled);
            assert!(state.pages.cursor.is_none());
            drop(state.update(Action::NextUnviewed));
            assert_eq!(state.selected.is_some(), !viewed);
            if !viewed {
                assert_eq!(
                    state.selected,
                    Some(RepoPath::parse("file-0104.rs").unwrap())
                );
            }
        }
    }

    fn refresh_later_pages(state: &mut State) {
        let mut snapshot = loaded(
            state.files.iter().map(|file| file.entry.clone()).collect(),
            (0..100)
                .map(|i| remote_file(&format!("file-{i:04}.rs"), true))
                .collect(),
        );
        snapshot.next_file_cursor = Some("page-2".into());
        drop(state.update(Action::Refresh));
        finish_load(state, snapshot);
    }

    #[test]
    fn refresh_rereads_only_the_page_of_a_demanded_later_mark() {
        let mut state = thousand_files();
        drop(state.update(Action::FilePage(PageAction::More)));
        answer_next(
            &mut state,
            vec![remote_file("file-0105.rs", true)],
            "me",
            Some("page-3".into()),
        );
        drop(state.update(Action::FilePage(PageAction::More)));
        answer_at(
            &mut state,
            "page-3",
            vec![remote_file("file-0205.rs", true)],
            "me",
            Some("page-4".into()),
        );
        assert_eq!(state.pages.cursor.as_deref(), Some("page-4"));
        refresh_later_pages(&mut state);
        assert!(!state.pages.loading);
        assert_eq!(state.pages.cursor.as_deref(), Some("page-4"));
        assert!(state.files[205].viewed);
        assert_eq!(state.files[205].sync, ViewedSync::Settled);

        drop(state.update(Action::SelectFile(RepoPath::parse("file-0205.rs").unwrap())));
        assert!(state.pages.loading);
        assert_eq!(state.pages.revalidating.as_deref(), Some("page-3"));
        assert_eq!(state.pages.cursor.as_deref(), Some("page-4"));
        answer_at(
            &mut state,
            "page-3",
            vec![remote_file("file-0205.rs", false)],
            "me",
            Some("page-4".into()),
        );
        assert!(!state.files[205].viewed);
        assert!(!state.pages.loading, "offscreen pages remain lazy");
        assert_eq!(state.pages.cursor.as_deref(), Some("page-4"));
        assert_eq!(state.files[105].sync, ViewedSync::Settled);
        assert!(
            state
                .pages
                .revalidate
                .contains_key(&RepoPath::parse("file-0105.rs").unwrap()),
            "page two waits until one of its marks is demanded"
        );

        drop(state.update(Action::FilePage(PageAction::More)));
        assert!(state.pages.loading);
        assert_eq!(
            state.pages.revalidating, None,
            "Load more continues past the pages already read"
        );
        answer_at(
            &mut state,
            "page-4",
            vec![remote_file("file-0305.rs", true)],
            "me",
            Some("page-5".into()),
        );
        assert_eq!(state.files[305].sync, ViewedSync::Settled);
        assert_eq!(state.pages.cursor.as_deref(), Some("page-5"));
    }

    #[test]
    fn a_failed_reread_waits_for_retry_and_then_settles_its_whole_page() {
        let mut state = thousand_files();
        drop(state.update(Action::FilePage(PageAction::More)));
        answer(
            &mut state,
            vec![
                remote_file("file-0105.rs", true),
                remote_file("file-0106.rs", true),
            ],
            "me",
        );
        assert!(state.pages.cursor.is_none());
        refresh_later_pages(&mut state);
        drop(state.update(Action::SelectFile(RepoPath::parse("file-0105.rs").unwrap())));
        assert_eq!(state.pages.revalidating.as_deref(), Some("page-2"));
        drop(state.update(Action::FilePage(PageAction::Loaded {
            generation: state.generation,
            load: state.load_generation,
            cursor: "page-2".into(),
            result: Err("network".into()),
        })));
        assert!(!state.pages.loading, "a failed reread waits for Retry");
        assert_eq!(state.pages.error.as_deref(), Some("network"));
        assert_eq!(state.pages.revalidate.len(), 2);

        drop(state.update(Action::FilePage(PageAction::More)));
        assert!(state.pages.error.is_none());
        assert_eq!(state.pages.revalidating.as_deref(), Some("page-2"));
        answer(&mut state, vec![remote_file("file-0105.rs", false)], "me");
        assert!(!state.files[105].viewed);
        assert!(
            state.pages.revalidate.is_empty(),
            "a reread settles marks its page no longer lists"
        );
        assert!(state.files[106].viewed);
        assert_eq!(state.files[106].sync, ViewedSync::Settled);
        drop(state.update(Action::SelectFile(RepoPath::parse("file-0106.rs").unwrap())));
        assert!(!state.pages.loading);
    }

    #[test]
    fn revalidation_never_overwrites_a_newer_local_mutation_or_its_receipt() {
        for outcome in [
            None,
            Some(ViewedOutcome::Applied),
            Some(ViewedOutcome::NotApplied("refused".into())),
            Some(ViewedOutcome::Unknown("lost receipt".into())),
        ] {
            let mut state = thousand_files();
            let path = RepoPath::parse("file-0105.rs").unwrap();
            drop(state.update(Action::FilePage(PageAction::More)));
            answer(
                &mut state,
                vec![
                    remote_file("file-0105.rs", true),
                    remote_file("file-0106.rs", true),
                ],
                "me",
            );
            refresh_later_pages(&mut state);
            drop(state.update(Action::SelectFile(path.clone())));
            assert!(state.pages.loading);
            assert!(state.pages.revalidate.contains_key(&path));
            drop(state.update(Action::ToggleViewed(path.clone())));
            assert!(!state.pages.revalidate.contains_key(&path));
            if let Some(outcome) = outcome {
                drop(state.update(Action::ViewedApplied {
                    generation: state.generation,
                    authority_revision: state.authority_revision,
                    results: vec![(path.clone(), false, outcome)],
                }));
            }
            let expected = (state.files[105].viewed, state.files[105].sync);
            answer(
                &mut state,
                vec![
                    remote_file("file-0105.rs", true),
                    remote_file("file-0106.rs", false),
                ],
                "me",
            );
            assert_eq!((state.files[105].viewed, state.files[105].sync), expected);
            assert!(
                !state.files[106].viewed,
                "the same page still revalidates its other marks"
            );
            assert!(state.pages.revalidate.is_empty());
        }
    }

    #[test]
    fn refresh_stays_lazy_after_full_hydration_finishes() {
        let mut state = thousand_files();
        drop(state.update(Action::SelectFile(RepoPath::parse("file-0105.rs").unwrap())));
        drop(state.update(Action::ToggleViewedAndAdvance));
        assert!(state.pages.hydrate_all);
        answer(
            &mut state,
            (100..1000)
                .map(|i| remote_file(&format!("file-{i:04}.rs"), true))
                .collect(),
            "me",
        );
        assert!(state.pages.cursor.is_none());
        assert!(!state.pages.hydrate_all);
        drop(state.update(Action::SelectFile(RepoPath::parse("file-0000.rs").unwrap())));
        refresh_later_pages(&mut state);
        assert!(
            !state.pages.loading,
            "completed hydration must not make every refresh eager"
        );
        assert!(state.pages.cursor.is_none());
        assert!(state.files[105].viewed);
    }

    #[test]
    fn refresh_rechecks_uncertain_later_marks_after_completed_pagination() {
        for remote_viewed in [false, true] {
            let mut state = thousand_files();
            let path = RepoPath::parse("file-0105.rs").unwrap();
            drop(state.update(Action::FilePage(PageAction::More)));
            answer(&mut state, vec![remote_file("file-0105.rs", true)], "me");
            assert!(state.pages.cursor.is_none());
            let mut snapshot = loaded(
                state.files.iter().map(|file| file.entry.clone()).collect(),
                (0..100)
                    .map(|i| remote_file(&format!("file-{i:04}.rs"), true))
                    .collect(),
            );
            snapshot.next_file_cursor = Some("page-2".into());

            // An already-started refresh must not overwrite the newer lost
            // receipt, even though its optimistic value differs from GitHub.
            drop(state.update(Action::Refresh));
            drop(state.update(Action::ToggleViewed(path.clone())));
            drop(state.update(Action::ViewedApplied {
                generation: state.generation,
                authority_revision: state.authority_revision,
                results: vec![(path, false, ViewedOutcome::Unknown("lost receipt".into()))],
            }));
            finish_load(&mut state, snapshot.clone());
            assert!(!state.files[105].viewed);
            assert_eq!(state.files[105].sync, ViewedSync::Unverified);
            assert!(state.pages.cursor.is_none());

            // The requested recovery read starts after the outcome. Reopen
            // its completed page without throwing away known earlier marks.
            drop(state.update(Action::Refresh));
            finish_load(&mut state, snapshot);
            assert!(!state.files[105].viewed);
            assert_eq!(state.files[105].sync, ViewedSync::Unknown);
            assert_eq!(state.files[0].sync, ViewedSync::Settled);
            assert!(state.files[0].viewed);
            assert_eq!(state.pages.cursor.as_deref(), Some("page-2"));
            assert!(state.pages.loading);
            answer(
                &mut state,
                vec![remote_file("file-0105.rs", remote_viewed)],
                "me",
            );
            assert_eq!(state.files[105].viewed, remote_viewed);
            assert_eq!(state.files[105].sync, ViewedSync::Settled);
            assert!(state.pages.cursor.is_none());
        }
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

/// Reveal against the actual laid-out viewport, including before the first
/// wheel event and after a resize. The result updates the virtual row window.
pub(super) fn reveal_row(id: widget::Id, position: usize, depth: usize) -> iced::Task<PageAction> {
    use iced::advanced::widget::{Operation, operation};
    struct Reveal {
        id: widget::Id,
        position: usize,
        depth: usize,
        viewport: Option<(f32, f32)>,
    }
    impl Operation<PageAction> for Reveal {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation<PageAction>)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            content: iced::Rectangle,
            translation: iced::Vector,
            state: &mut dyn operation::Scrollable,
        ) {
            if id != Some(&self.id) {
                return;
            }
            let padding = space::XS;
            let offset = reveal_offset(
                self.position,
                self.depth,
                padding,
                translation.y,
                bounds.height,
                content.height,
            );
            state.scroll_to(operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(offset),
            });
            self.viewport = Some((offset, bounds.height));
        }
        fn finish(&self) -> operation::Outcome<PageAction> {
            self.viewport
                .map_or(operation::Outcome::None, |(offset, height)| {
                    operation::Outcome::Some(PageAction::Viewport { offset, height })
                })
        }
    }
    iced::advanced::widget::operate(Reveal {
        id,
        position,
        depth,
        viewport: None,
    })
}

fn reveal_offset(
    position: usize,
    depth: usize,
    padding: f32,
    current: f32,
    height: f32,
    content: f32,
) -> f32 {
    let row = design::DENSE_ROW_HEIGHT;
    let top = padding + position as f32 * row;
    // At very small heights a complete ancestor stack cannot fit. Still reveal
    // as much of the selected row as the viewport permits.
    let headers = (depth as f32 * row).min((height - row).max(0.0));
    let next = if top < current + headers {
        top - headers
    } else if top + row > current + height {
        top + row - height
    } else {
        current
    };
    next.clamp(0.0, (content - height).max(0.0))
}

#[cfg(test)]
mod reveal_tests {
    use super::*;
    #[test]
    fn keyboard_reveal_uses_initial_short_viewport_and_all_sticky_ancestors() {
        let row = design::DENSE_ROW_HEIGHT;
        let forward = reveal_offset(150, 3, 2.0, 0.0, 120.0, 1000.0 * row);
        assert_eq!(forward, 2.0 + 151.0 * row - 120.0);
        let backward = reveal_offset(10, 3, 2.0, 900.0, 600.0, 1000.0 * row);
        assert_eq!(backward, 2.0 + 7.0 * row);
        assert_eq!(
            reveal_offset(10, 3, 2.0, backward, 600.0, 1000.0 * row),
            backward
        );
    }
}
