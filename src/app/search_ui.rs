//! Project content search is independent of Git refresh and file-name filters.
use super::*;
use crate::{
    project_search::{self, Options, Progress},
    ui_controls::FindOption,
};
use cosmic::iced::futures::StreamExt;

const PAGE_SIZE: usize = 100;

#[derive(Debug, Default)]
pub(super) struct State {
    root: Option<ActiveWorktreeKey>,
    options: Options,
    generation: u64,
    cancel: Option<CancellationToken>,
    matches: Vec<project_search::Match>,
    counts: std::collections::BTreeMap<PathBuf, usize>,
    selected: usize,
    running: bool,
    capped: bool,
    skipped: usize,
    error: Option<String>,
    query_focused: bool,
    requested: Option<Instant>,
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Open,
    Query(String),
    Include(String),
    Exclude(String),
    Option(FindOption, bool),
    Ignored(bool),
    Hidden(bool),
    QueryFocus(bool),
    Run(u64),
    Progress {
        generation: u64,
        root: ActiveWorktreeKey,
        progress: Progress,
    },
    Select(usize),
    OpenResult(usize),
    Submit,
}

fn query_id() -> widget::Id {
    widget::Id::new("project-search.query")
}
fn results_id() -> widget::Id {
    widget::Id::new("project-search.results")
}

impl Cagents {
    pub(super) fn rebind_project_search(&mut self) -> Task<Message> {
        let root = self.sidebar_state.active_worktree().cloned();
        if self.search_ui.root == root {
            return Task::none();
        }
        self.search_ui.root = root;
        self.schedule_project_search()
    }

    fn schedule_project_search(&mut self) -> Task<Message> {
        if let Some(cancel) = self.search_ui.cancel.take() {
            cancel.cancel();
        }
        self.search_ui.generation = self.search_ui.generation.wrapping_add(1);
        self.search_ui.matches.clear();
        self.search_ui.counts.clear();
        self.search_ui.selected = 0;
        self.search_ui.error = None;
        self.search_ui.capped = false;
        self.search_ui.skipped = 0;
        self.search_ui.running = self.context_tab == ContextTab::Search
            && self.search_ui.root.is_some()
            && !self.search_ui.options.query.is_empty()
            && !crate::device::is_remote();
        if !self.search_ui.running {
            return Task::none();
        }
        self.search_ui.requested = Some(Instant::now());
        let generation = self.search_ui.generation;
        cosmic::task::future(async move {
            tokio::time::sleep(Duration::from_millis(75)).await;
            Message::ProjectSearch(Action::Run(generation))
        })
    }

    pub(super) fn update_project_search(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Open => {
                if self.modal_is_open() {
                    return Task::none();
                }
                self.context_tab = ContextTab::Search;
                self.search_ui.root = self.sidebar_state.active_worktree().cloned();
                let open = self.set_context_inspector_open(true);
                let search = self.schedule_project_search();
                return Task::batch([
                    open,
                    search,
                    iced::widget::operation::focus(query_id()).map(cosmic::Action::App),
                ]);
            }
            Action::Query(value) => self.search_ui.options.query = value,
            Action::Include(value) => self.search_ui.options.include = value,
            Action::Exclude(value) => self.search_ui.options.exclude = value,
            Action::Ignored(value) => self.search_ui.options.include_ignored = value,
            Action::Hidden(value) => self.search_ui.options.include_hidden = value,
            Action::Option(option, value) => match option {
                FindOption::CaseSensitive => self.search_ui.options.case_sensitive = value,
                FindOption::Regex => self.search_ui.options.regex = value,
                FindOption::WholeWord => self.search_ui.options.whole_word = value,
            },
            Action::QueryFocus(focused) => {
                self.search_ui.query_focused = focused;
                return Task::none();
            }
            Action::Run(generation) => {
                if generation != self.search_ui.generation || !self.search_ui.running {
                    return Task::none();
                }
                let Some(root) = self.search_ui.root.clone() else {
                    return Task::none();
                };
                let options = self.search_ui.options.clone();
                let token = CancellationToken::default();
                self.search_ui.cancel = Some(token.clone());
                let directory = root.canonical_path.clone();
                let (sender, receiver) = async_channel::bounded(4);
                let spawned = std::thread::Builder::new()
                    .name("project-search".into())
                    .spawn(move || {
                        project_search::search(&directory, &options, &token, |event| {
                            // Try/send in short waits makes cancellation independent
                            // of a slow GUI or a receiver waiting behind a modal.
                            let mut event = event;
                            loop {
                                if token.is_cancelled() {
                                    return false;
                                }
                                match sender.try_send(event) {
                                    Ok(()) => return true,
                                    Err(async_channel::TrySendError::Closed(_)) => return false,
                                    Err(async_channel::TrySendError::Full(value)) => {
                                        event = value;
                                        std::thread::sleep(Duration::from_millis(2));
                                    }
                                }
                            }
                        });
                    });
                if let Err(error) = spawned {
                    self.search_ui.error = Some(format!("Could not start search: {error}"));
                    self.search_ui.running = false;
                    return Task::none();
                }
                let events = receiver.chain(cosmic::iced::futures::stream::once(async {
                    Progress::Finished {
                        capped: false,
                        skipped: 0,
                    }
                }));
                return Task::stream(events.map(move |progress| {
                    cosmic::Action::App(Message::ProjectSearch(Action::Progress {
                        generation,
                        root: root.clone(),
                        progress,
                    }))
                }));
            }
            Action::Progress {
                generation,
                root,
                progress,
            } => {
                if generation != self.search_ui.generation
                    || self.search_ui.root.as_ref() != Some(&root)
                    || self.sidebar_state.active_worktree() != Some(&root)
                {
                    return Task::none();
                }
                match progress {
                    Progress::Batch(matches) => {
                        if self.search_ui.matches.is_empty()
                            && !matches.is_empty()
                            && let Some(requested) = self.search_ui.requested
                        {
                            tracing::debug!(
                                generation,
                                elapsed_ms = requested.elapsed().as_millis() as u64,
                                "project search first result delivered"
                            );
                        }
                        for matched in &matches {
                            *self
                                .search_ui
                                .counts
                                .entry(matched.path.clone())
                                .or_default() += 1;
                        }
                        self.search_ui.matches.extend(
                            matches.into_iter().take(
                                project_search::RESULT_LIMIT
                                    .saturating_sub(self.search_ui.matches.len()),
                            ),
                        );
                    }
                    Progress::Finished { capped, skipped } => {
                        // The stream's final sentinel must not erase the real
                        // worker's cap/count once its final message arrived.
                        if self.search_ui.running {
                            self.search_ui.capped = capped;
                            self.search_ui.skipped = skipped;
                            self.search_ui.running = false;
                        }
                    }
                    Progress::Error(error) => {
                        self.search_ui.error = Some(error);
                        self.search_ui.running = false;
                    }
                }
                return Task::none();
            }
            Action::Select(index) => {
                self.search_ui.selected = index.min(self.search_ui.matches.len().saturating_sub(1));
                let start = self.search_ui.selected / PAGE_SIZE * PAGE_SIZE;
                let mut headers = 0;
                let mut previous = None;
                for matched in self
                    .search_ui
                    .matches
                    .iter()
                    .skip(start)
                    .take(self.search_ui.selected - start + 1)
                {
                    if previous != Some(&matched.path) {
                        headers += 1;
                        previous = Some(&matched.path);
                    }
                }
                return crate::interaction::reveal_list_selection(
                    results_id(),
                    self.search_ui.selected - start + headers,
                    design::DENSE_ROW_HEIGHT,
                    space::XXS,
                )
                .map(cosmic::Action::App);
            }
            Action::Submit => {
                return self.update_project_search(Action::OpenResult(self.search_ui.selected));
            }
            Action::OpenResult(index) => {
                let Some(root) = self
                    .search_ui
                    .root
                    .clone()
                    .filter(|root| self.sidebar_state.active_worktree() == Some(root))
                else {
                    return Task::none();
                };
                let Some(matched) = self.search_ui.matches.get(index).cloned() else {
                    return Task::none();
                };
                let Ok(path) = RepoPath::parse(&matched.path) else {
                    return Task::none();
                };
                if let Some(card) = self.repository_card_for(&root.canonical_path)
                    && let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card)
                    && runtime.selected_path() != Some(matched.path.as_path())
                    && runtime.blocks_navigation()
                {
                    self.status = Some(TransientNotice::warning(
                        "Save the current editor or resolve its pending changes before opening another search result.",
                    ));
                    return self.focus_workspace_card(card);
                }
                let task = self.route_panel_open(&root, &path);
                self.pending_panel_open = None;
                // Location belongs to this concrete path, not the next file
                // loaded by the card. editor_core supplies the deferred open.
                if let Some(card) = self.repository_card_for(&root.canonical_path)
                    && let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card)
                {
                    return Task::batch([
                        task,
                        repository_task(runtime.open_file_location(
                            card,
                            matched.path,
                            matched.line,
                            matched.column,
                            matched.length,
                        )),
                    ]);
                }
                return task;
            }
        }
        self.schedule_project_search()
    }

    pub(super) fn project_search_key_event(
        &mut self,
        event: &keyboard::Event,
    ) -> Option<Task<Message>> {
        if self.modal_is_open()
            || self.context_tab != ContextTab::Search
            || !self.context_inspector_open
            || !self.search_ui.query_focused
        {
            return None;
        }
        let keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modifiers,
            ..
        } = event
        else {
            return None;
        };
        if !modifiers.is_empty() {
            return None;
        }
        let count = self.search_ui.matches.len();
        match key {
            keyboard::key::Named::ArrowDown if count > 0 => Some(
                self.update_project_search(Action::Select((self.search_ui.selected + 1) % count)),
            ),
            keyboard::key::Named::ArrowUp if count > 0 => Some(self.update_project_search(
                Action::Select((self.search_ui.selected + count - 1) % count),
            )),
            _ => None,
        }
    }

    pub(super) fn project_search_panel(&self) -> Element<'_, Message> {
        let state = &self.search_ui;
        let enabled = !self.modal_is_open();
        if crate::device::is_remote() {
            return widget::container(design::caption(
                "Content search is available for local worktrees.",
            ))
            .padding(space::M)
            .into();
        }
        let query = crate::ui_controls::field::search("Search file contents", &state.options.query)
            .id(query_id())
            .width(Length::Fill)
            .on_submit(|_| Message::ProjectSearch(Action::Submit))
            .on_focus(Message::ProjectSearch(Action::QueryFocus(true)))
            .on_unfocus(Message::ProjectSearch(Action::QueryFocus(false)));
        let query = if enabled {
            query.on_input(|value| Message::ProjectSearch(Action::Query(value)))
        } else {
            query
        };
        let mut include = crate::ui_controls::field::text(
            "Include files (e.g. src/**, *.rs)",
            &state.options.include,
        )
        .width(Length::Fill);
        let mut exclude = crate::ui_controls::field::text(
            "Exclude files (e.g. tests/**)",
            &state.options.exclude,
        )
        .width(Length::Fill);
        if enabled {
            include = include.on_input(|value| Message::ProjectSearch(Action::Include(value)));
            exclude = exclude.on_input(|value| Message::ProjectSearch(Action::Exclude(value)));
        }
        let mut ignored = crate::ui_controls::field::checkbox(state.options.include_ignored)
            .label("Ignored files");
        let mut hidden =
            crate::ui_controls::field::checkbox(state.options.include_hidden).label("Hidden files");
        if enabled {
            ignored = ignored.on_toggle(|value| Message::ProjectSearch(Action::Ignored(value)));
            hidden = hidden.on_toggle(|value| Message::ProjectSearch(Action::Hidden(value)));
        }
        let mut controls = widget::column::with_children(vec![
            query.into(),
            crate::ui_controls::find_options(
                state.options.case_sensitive,
                state.options.regex,
                state.options.whole_word,
                enabled,
                |option, value| Message::ProjectSearch(Action::Option(option, value)),
            ),
            include.into(),
            exclude.into(),
            widget::row::with_children(vec![ignored.into(), hidden.into()])
                .spacing(space::S)
                .into(),
        ])
        .spacing(space::XS);
        let notice = if let Some(error) = &state.error {
            error.clone()
        } else if state.root.is_none() {
            "Choose a worktree to search.".into()
        } else if state.options.query.is_empty() {
            "Search text across this worktree. Enter opens a match; ↑/↓ chooses a result.".into()
        } else if state.running {
            format!("{} results · Searching…", state.matches.len())
        } else if state.capped {
            format!("{}+ results, refine the query", state.matches.len())
        } else {
            format!(
                "{} results in {} files{}",
                state.matches.len(),
                state.counts.len(),
                if state.skipped > 0 {
                    " · Some binary, large or unreadable files skipped"
                } else {
                    ""
                }
            )
        };
        controls = controls.push(design::caption(notice));
        let page = state.selected / PAGE_SIZE;
        let start = page * PAGE_SIZE;
        let mut rows = widget::column::with_capacity(PAGE_SIZE + 20)
            .spacing(space::XXS)
            .width(Length::Fill);
        let mut previous = None;
        for (index, matched) in state.matches.iter().enumerate().skip(start).take(PAGE_SIZE) {
            if previous != Some(&matched.path) {
                rows = rows.push(
                    widget::container(
                        design::heading(format!(
                            "{} ({})",
                            matched.path.display(),
                            state.counts.get(&matched.path).unwrap_or(&0)
                        ))
                        .wrapping(iced::widget::text::Wrapping::None)
                        .ellipsize(iced::widget::text::Ellipsize::End(
                            EllipsizeHeightLimit::Lines(1),
                        )),
                    )
                    .height(design::DENSE_ROW_HEIGHT)
                    .center_y(design::DENSE_ROW_HEIGHT),
                );
                previous = Some(&matched.path);
            }
            use cosmic::iced::widget::text::Span;
            let before = &matched.snippet[..matched.highlight.start];
            let found = &matched.snippet[matched.highlight.clone()];
            let after = &matched.snippet[matched.highlight.end..];
            let snippet = cosmic::iced::widget::rich_text::<(), _, _, _>([
                Span::new(format!("{}: ", matched.line + 1)),
                Span::new(before),
                Span::new(found)
                    .color(design::accent_color(&cosmic::theme::active()))
                    .font(cosmic::font::Font {
                        weight: cosmic::iced::font::Weight::Bold,
                        ..cosmic::font::default()
                    }),
                Span::new(after),
            ])
            .size(design::TEXT_BODY)
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill);
            rows = rows.push(crate::interaction::keyboard_list_row(
                widget::button::custom(snippet)
                    .name(format!(
                        "{} line {} column {}: {}",
                        matched.path.display(),
                        matched.line + 1,
                        matched.column + 1,
                        matched.snippet
                    ))
                    .class(crate::ui_controls::control_style(
                        index == state.selected && state.query_focused,
                        None,
                    ))
                    .width(Length::Fill)
                    .height(design::DENSE_ROW_HEIGHT)
                    .padding([space::XS, space::S])
                    .on_press_maybe(
                        enabled.then_some(Message::ProjectSearch(Action::OpenResult(index))),
                    ),
            ));
        }
        let pages = state.matches.len().div_ceil(PAGE_SIZE);
        if pages > 1 {
            controls = controls.push(
                widget::row::with_children(vec![
                    crate::ui_controls::button::quiet("Previous")
                        .on_press_maybe((enabled && page > 0).then_some(Message::ProjectSearch(
                            Action::Select(start.saturating_sub(PAGE_SIZE)),
                        )))
                        .into(),
                    design::caption(format!("{} / {pages}", page + 1)).into(),
                    crate::ui_controls::button::quiet("Next")
                        .on_press_maybe(
                            (enabled && page + 1 < pages).then_some(Message::ProjectSearch(
                                Action::Select(start + PAGE_SIZE),
                            )),
                        )
                        .into(),
                ])
                .align_y(Alignment::Center)
                .spacing(space::XS),
            );
        }
        widget::container(
            widget::column::with_children(vec![
                controls.into(),
                crate::interaction::keyboard_list(
                    widget::scrollable(rows)
                        .id(results_id())
                        .height(Length::Fill),
                    enabled,
                )
                .into(),
            ])
            .spacing(space::S),
        )
        .padding(space::S)
        .height(Length::Fill)
        .width(Length::Fill)
        .into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn opening_another_search_file_preserves_a_blocked_editor_and_explains_the_guard() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        let root = app.sidebar_state.active_worktree().unwrap().clone();
        let card = app.workspace.focused().unwrap();
        let mut runtime = RepositoryRuntime::dirty_fixture(card);
        runtime.descriptor.project = root.project_id;
        runtime.descriptor.repository = root.canonical_path.clone();
        runtime.descriptor.worktree = root.canonical_path.clone();
        runtime.descriptor.selected_file = None;
        let selected = runtime.selected_path().unwrap().to_path_buf();
        let navigation = runtime.navigation();
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(runtime.descriptor.clone());
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        app.search_ui.root = Some(root);
        app.search_ui.matches.push(project_search::Match {
            path: "another-search-result.rs".into(),
            line: 0,
            column: 0,
            length: 3,
            snippet: "hit".into(),
            highlight: 0..3,
        });
        drop(app.update_project_search(Action::OpenResult(0)));
        assert!(
            app.status
                .as_ref()
                .is_some_and(|notice| notice.severity == NoticeSeverity::Warning)
        );
        let CardRuntime::Repository(runtime) = &app.cards[&card] else {
            panic!("editor")
        };
        assert_eq!(runtime.selected_path(), Some(selected.as_path()));
        assert_eq!(runtime.navigation(), navigation);
        assert!(runtime.is_dirty());
        assert!(app.pending_panel_open.is_none());
    }

    #[test]
    fn new_queries_cancel_old_search_and_reject_stale_batches() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        let root = app.sidebar_state.active_worktree().unwrap().clone();
        app.search_ui.root = Some(root.clone());
        app.search_ui.generation = 10;
        app.context_tab = ContextTab::Search;
        let token = CancellationToken::default();
        app.search_ui.cancel = Some(token.clone());
        drop(app.update_project_search(Action::Query("new query".into())));
        assert!(token.is_cancelled());
        let matched = project_search::Match {
            path: "src/main.rs".into(),
            line: 0,
            column: 0,
            length: 3,
            snippet: "new query".into(),
            highlight: 0..3,
        };
        drop(app.update_project_search(Action::Progress {
            generation: 10,
            root: root.clone(),
            progress: Progress::Batch(vec![matched.clone()]),
        }));
        assert!(app.search_ui.matches.is_empty());
        let generation = app.search_ui.generation;
        drop(app.update_project_search(Action::Progress {
            generation,
            root: root.clone(),
            progress: Progress::Batch(vec![matched]),
        }));
        assert_eq!(app.search_ui.matches.len(), 1);
        drop(app.update_project_search(Action::Progress {
            generation,
            root: root.clone(),
            progress: Progress::Finished {
                capped: true,
                skipped: 2,
            },
        }));
        drop(app.update_project_search(Action::Progress {
            generation,
            root,
            progress: Progress::Finished {
                capped: false,
                skipped: 0,
            },
        }));
        assert!(app.search_ui.capped);
        assert_eq!(app.search_ui.skipped, 2);
    }
}
