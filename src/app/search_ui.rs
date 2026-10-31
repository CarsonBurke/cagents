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
    snippet_fits: std::cell::RefCell<std::collections::BTreeMap<usize, CachedSnippet>>,
    counts: std::collections::BTreeMap<PathBuf, usize>,
    selected: usize,
    running: bool,
    capped: bool,
    skipped: usize,
    error: Option<String>,
    query_focused: bool,
    requested: Option<Instant>,
}
impl State {
    fn result_target(&self, index: usize) -> Option<ResultTarget> {
        Some(ResultTarget {
            root: self.root.clone()?,
            generation: self.generation,
            index,
            matched: self.matches.get(index)?.clone(),
        })
    }
}

/// One rendered result belongs to one immutable query and selected worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultTarget {
    root: ActiveWorktreeKey,
    generation: u64,
    index: usize,
    matched: project_search::Match,
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
    Retry(u64),
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
    OpenResult(ResultTarget),
    Submit(u64),
}

fn query_id() -> widget::Id {
    widget::Id::new("project-search.query")
}
fn results_id() -> widget::Id {
    widget::Id::new("project-search.results")
}

fn retry_control(state: &State, enabled: bool) -> Element<'_, Message> {
    let retry: Element<'_, Message> = crate::ui_controls::button::quiet("Retry search")
        .on_press_maybe(
            (enabled
                && state.error.is_some()
                && state.root.is_some()
                && !state.options.query.is_empty())
            .then_some(Message::ProjectSearch(Action::Retry(state.generation))),
        )
        .into();
    // A press belongs to the rendered query, even if another error replaces
    // this button before release. No named button state survives keyed diff.
    iced::widget::keyed::Column::with_children([(state.generation, retry)]).into()
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

    pub(super) fn retire_project_search_roots(
        &mut self,
        project: ProjectId,
        retired: &std::collections::BTreeSet<PathBuf>,
    ) -> Task<Message> {
        if self.search_ui.root.as_ref().is_some_and(|root| {
            root.project_id == project && retired.contains(&root.canonical_path)
        }) {
            return self.schedule_project_search();
        }
        Task::none()
    }

    fn schedule_project_search(&mut self) -> Task<Message> {
        if let Some(cancel) = self.search_ui.cancel.take() {
            cancel.cancel();
        }
        self.search_ui.generation = self.search_ui.generation.wrapping_add(1);
        self.search_ui.matches.clear();
        self.search_ui.snippet_fits.borrow_mut().clear();
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
            Action::Retry(generation) => {
                if generation != self.search_ui.generation
                    || self.search_ui.error.is_none()
                    || self.search_ui.root.is_none()
                    || self.search_ui.root.as_ref() != self.sidebar_state.active_worktree()
                    || self.search_ui.options.query.is_empty()
                    || self.context_tab != ContextTab::Search
                    || self.modal_is_open()
                {
                    return Task::none();
                }
                return Task::batch([
                    self.schedule_project_search(),
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
            Action::Submit(generation) => {
                if generation != self.search_ui.generation {
                    return Task::none();
                }
                let Some(target) = self.search_ui.result_target(self.search_ui.selected) else {
                    return Task::none();
                };
                return self.update_project_search(Action::OpenResult(target));
            }
            Action::OpenResult(target) => {
                if target.generation != self.search_ui.generation
                    || self.search_ui.root.as_ref() != Some(&target.root)
                    || self.sidebar_state.active_worktree() != Some(&target.root)
                    || self.search_ui.matches.get(target.index) != Some(&target.matched)
                {
                    return Task::none();
                }
                if self.modal_is_open()
                    || self.refuse_in_deleting_worktree(&target.root.canonical_path)
                {
                    return Task::none();
                }
                let root = target.root;
                let matched = target.matched;
                let Ok(path) = RepoPath::parse(&matched.path) else {
                    return Task::none();
                };
                if let Some(card) = self.repository_card_for(&root.canonical_path)
                    && let Some(CardRuntime::Repository(runtime)) = self.cards.get(&card)
                    && runtime.blocks_navigation()
                {
                    self.status = Some(TransientNotice::warning(
                        "Save the current editor or resolve its pending changes before opening a disk search result.",
                    ));
                    return self.focus_workspace_card(card);
                }
                let task = self.route_panel_open(&root, &path);
                // Location belongs to this concrete path, not the next file
                // loaded by the card. editor_core supplies the deferred open.
                if let Some(card) = self.repository_card_for(&root.canonical_path)
                    && let Some(CardRuntime::Repository(runtime)) = self.cards.get_mut(&card)
                {
                    // This direct location open supersedes only its own parked
                    // file-list navigation; an unrelated card keeps its intent.
                    if self
                        .pending_panel_open
                        .as_ref()
                        .is_some_and(|pending| pending.card == card && pending.path == matched.path)
                    {
                        self.pending_panel_open = None;
                    }
                    let location = repository_task(runtime.open_search_result(card, &matched));
                    // Match the ordinary Repository action boundary: opening
                    // another buffer must drain its old LSP Close immediately.
                    let language = self.drain_lsp_intents(card);
                    return Task::batch([task, location, language]);
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
        let generation = state.generation;
        let query = crate::ui_controls::field::search("Search file contents", &state.options.query)
            .id(query_id())
            .width(Length::Fill)
            .on_submit(move |_| Message::ProjectSearch(Action::Submit(generation)))
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
        if state.error.is_some() {
            controls = controls.push(retry_control(state, enabled));
        }
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
            rows = rows.push(result_row(state, matched, index, enabled));
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

#[derive(Debug)]
struct SnippetPreview {
    text: String,
    highlight: std::ops::Range<usize>,
}

#[derive(Debug)]
struct CachedSnippet {
    width: u32,
    font: cosmic::iced::Font,
    preview: Arc<SnippetPreview>,
}

impl State {
    fn snippet_preview(
        &self,
        index: usize,
        matched: &project_search::Match,
        width: f32,
        font: cosmic::iced::Font,
    ) -> Arc<SnippetPreview> {
        if let Some(cached) = self.snippet_fits.borrow().get(&index)
            && cached.width == width.to_bits()
            && cached.font == font
        {
            return cached.preview.clone();
        }
        let preview = Arc::new(fit_snippet(matched, width.max(0.0), font));
        let mut fits = self.snippet_fits.borrow_mut();
        // At most one visible page. Query replacement clears this cache;
        // appending streamed matches does not reshape already visible rows.
        if fits.len() >= PAGE_SIZE && !fits.contains_key(&index) {
            fits.clear();
        }
        fits.insert(
            index,
            CachedSnippet {
                width: width.to_bits(),
                font,
                preview: preview.clone(),
            },
        );
        preview
    }
}

fn result_row<'a>(
    state: &'a State,
    matched: &'a project_search::Match,
    index: usize,
    enabled: bool,
) -> Element<'a, Message> {
    let snippet = crate::ui_controls::responsive(move |slot| {
        let font = cosmic::font::default();
        let preview = state.snippet_preview(index, matched, slot.width, font);
        widget::container(
            cosmic::iced::widget::rich_text::<(), _, _, _>(snippet_spans(
                matched.line,
                &preview,
                font,
            ))
            .font(font)
            .size(design::TEXT_BODY)
            .wrapping(iced::widget::text::Wrapping::None)
            .width(Length::Fill),
        )
        .width(Length::Fill)
        .clip(true)
        .into()
    });
    let row = crate::interaction::keyboard_list_row(
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
                enabled
                    .then(|| state.result_target(index))
                    .flatten()
                    .map(|target| Message::ProjectSearch(Action::OpenResult(target))),
            ),
    );
    // Streamed batches only append within a generation. A new query/root can
    // reuse this position, but must not inherit the previous button's press.
    widget::container(crate::ui_controls::owned((state.generation, index), row))
        .width(Length::Fill)
        .into()
}

fn snippet_spans(
    line: usize,
    preview: &SnippetPreview,
    font: cosmic::iced::Font,
) -> Vec<cosmic::iced::widget::text::Span<'static, (), cosmic::iced::Font>> {
    use cosmic::iced::widget::text::Span;
    vec![
        Span::new(format!("{}: ", line + 1)),
        Span::new(preview.text[..preview.highlight.start].to_owned()),
        Span::new(preview.text[preview.highlight.clone()].to_owned())
            .color(design::accent_color(&cosmic::theme::active()))
            .font(cosmic::iced::Font {
                weight: cosmic::iced::font::Weight::Bold,
                ..font
            }),
        Span::new(preview.text[preview.highlight.end..].to_owned()),
    ]
}

type SnippetParagraph = <cosmic::Renderer as iced::advanced::text::Renderer>::Paragraph;

fn snippet_paragraph(
    line: usize,
    preview: &SnippetPreview,
    font: cosmic::iced::Font,
) -> SnippetParagraph {
    use iced::advanced::text::Paragraph as _;
    SnippetParagraph::with_spans(iced::advanced::text::Text {
        content: &snippet_spans(line, preview, font),
        bounds: iced::Size::new(f32::INFINITY, f32::INFINITY),
        size: iced::Pixels(design::TEXT_BODY),
        line_height: Default::default(),
        font,
        align_x: iced::advanced::text::Alignment::Default,
        align_y: iced::alignment::Vertical::Top,
        shaping: iced::advanced::text::Shaping::Advanced,
        wrapping: iced::widget::text::Wrapping::None,
        ellipsize: iced::widget::text::Ellipsize::None,
    })
}

/// Fit real shaped spans, preserving the hit before context. A long regex
/// match may itself need a shortened preview; its original result and editor
/// range remain intact. Every cut is a UTF-8 boundary, with explicit ellipses.
fn fit_snippet(
    matched: &project_search::Match,
    width: f32,
    font: cosmic::iced::Font,
) -> SnippetPreview {
    use iced::advanced::text::Paragraph as _;
    let before = &matched.snippet[..matched.highlight.start];
    let hit = &matched.snippet[matched.highlight.clone()];
    let after = &matched.snippet[matched.highlight.end..];
    let mut before_count = before.chars().count().min(16);
    let mut after_count = after.chars().count().min(32);
    let mut hit_count = hit.chars().count();
    loop {
        let start = if before_count == 0 {
            before.len()
        } else {
            before.char_indices().rev().nth(before_count - 1).unwrap().0
        };
        let hit_end = hit
            .char_indices()
            .nth(hit_count)
            .map_or(hit.len(), |(index, _)| index);
        let after_end = if hit_end < hit.len() {
            0
        } else {
            after
                .char_indices()
                .nth(after_count)
                .map_or(after.len(), |(index, _)| index)
        };
        let mut text = String::new();
        if start > 0 {
            text.push('…');
        }
        text.push_str(&before[start..]);
        let highlight_start = text.len();
        text.push_str(&hit[..hit_end]);
        let highlight = highlight_start..text.len();
        text.push_str(&after[..after_end]);
        if hit_end < hit.len() || after_end < after.len() {
            text.push('…');
        }
        let preview = SnippetPreview { text, highlight };
        if snippet_paragraph(matched.line, &preview, font).min_width() <= width {
            return preview;
        }
        if before_count > 0 || after_count > 0 {
            before_count /= 2;
            after_count /= 2;
        } else if hit_count > 1 {
            hit_count /= 2;
        } else {
            // No glyph fits this degenerate slot. Do not claim a different hit.
            return SnippetPreview {
                text: String::new(),
                highlight: 0..0,
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn result_pointer_step(
        state: &State,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        event: iced::mouse::Event,
    ) -> Vec<Message> {
        pointer_step(
            result_row(state, &state.matches[0], 0, true),
            tree,
            renderer,
            event,
        )
    }

    fn pointer_step(
        mut row: Element<'_, Message>,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        event: iced::mouse::Event,
    ) -> Vec<Message> {
        use iced::advanced::{Layout, Shell, clipboard, layout, widget::Operation};
        use iced::{Event, Rectangle, Size, mouse};
        struct ButtonBounds(Option<Rectangle>);
        impl Operation for ButtonBounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: Rectangle,
                _: &mut dyn iced::advanced::widget::operation::Focusable,
            ) {
                assert!(self.0.replace(bounds).is_none(), "one actual result button");
            }
        }
        tree.diff(row.as_widget_mut());
        let viewport = Rectangle::with_size(Size::new(350.0, design::DENSE_ROW_HEIGHT));
        let node = row.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(viewport.size(), viewport.size()),
        );
        let mut bounds = ButtonBounds(None);
        row.as_widget_mut()
            .operate(tree, Layout::new(&node), renderer, &mut bounds);
        let bounds = bounds.0.expect("production control has a focusable button");
        let mut messages = Vec::new();
        row.as_widget_mut().update(
            tree,
            &Event::Mouse(event),
            Layout::new(&node),
            mouse::Cursor::Available(bounds.center()),
            renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        messages
    }

    #[test]
    fn retry_press_cannot_cross_query_generations_but_survives_ordinary_rerendering() {
        use iced::advanced::widget::Tree;
        use iced::mouse::{Button, Event};
        let renderer = super::super::tests::headless_renderer();
        for replace in [false, true] {
            let model = sidebar::tests::panel_model();
            let sidebar = sidebar::tests::panel_state(&model);
            let mut state = State {
                root: sidebar.active_worktree().cloned(),
                generation: 41,
                options: Options {
                    query: "needle".into(),
                    ..Options::default()
                },
                error: Some("original error".into()),
                ..State::default()
            };
            let control = retry_control(&state, true);
            let mut tree = Tree::new(control.as_widget());
            drop(control);
            assert!(
                pointer_step(
                    retry_control(&state, true),
                    &mut tree,
                    &renderer,
                    Event::ButtonPressed(Button::Left)
                )
                .is_empty()
            );
            state.error = Some("new diagnostic".into());
            if replace {
                state.generation += 1;
            }
            let released = pointer_step(
                retry_control(&state, true),
                &mut tree,
                &renderer,
                Event::ButtonReleased(Button::Left),
            );
            if replace {
                assert!(
                    released.is_empty(),
                    "held press cannot activate a replacement query"
                );
                assert!(
                    pointer_step(
                        retry_control(&state, true),
                        &mut tree,
                        &renderer,
                        Event::ButtonPressed(Button::Left)
                    )
                    .is_empty()
                );
                assert!(matches!(
                    pointer_step(
                        retry_control(&state, true),
                        &mut tree,
                        &renderer,
                        Event::ButtonReleased(Button::Left)
                    )
                    .as_slice(),
                    [Message::ProjectSearch(Action::Retry(42))]
                ));
            } else {
                assert!(matches!(
                    released.as_slice(),
                    [Message::ProjectSearch(Action::Retry(41))]
                ));
            }
        }
    }

    #[test]
    fn a_native_result_press_cannot_cross_query_generations_but_survives_streamed_batches() {
        use iced::advanced::widget::Tree;
        use iced::mouse::{Button, Event};
        let renderer = super::super::tests::headless_renderer();
        for replace in [false, true] {
            let model = sidebar::tests::panel_model();
            let sidebar = sidebar::tests::panel_state(&model);
            let mut state = State {
                root: sidebar.active_worktree().cloned(),
                generation: 41,
                matches: vec![project_search::Match {
                    path: "original.rs".into(),
                    line: 0,
                    column: 0,
                    length: 3,
                    line_identity: project_search::LineIdentity::new("hit"),
                    snippet: "hit".into(),
                    highlight: 0..3,
                }],
                ..State::default()
            };
            let row = result_row(&state, &state.matches[0], 0, true);
            let mut tree = Tree::new(row.as_widget());
            drop(row);
            assert!(
                result_pointer_step(
                    &state,
                    &mut tree,
                    &renderer,
                    Event::ButtonPressed(Button::Left)
                )
                .is_empty()
            );
            if replace {
                state.generation += 1;
                state.matches[0].path = "replacement.rs".into();
                state.snippet_fits.borrow_mut().clear();
            } else {
                state.matches.push(project_search::Match {
                    path: "streamed.rs".into(),
                    ..state.matches[0].clone()
                });
            }
            let messages = result_pointer_step(
                &state,
                &mut tree,
                &renderer,
                Event::ButtonReleased(Button::Left),
            );
            if replace {
                assert!(
                    messages.is_empty(),
                    "old press must not invoke the replacement callback"
                );
                assert!(
                    result_pointer_step(
                        &state,
                        &mut tree,
                        &renderer,
                        Event::ButtonPressed(Button::Left)
                    )
                    .is_empty()
                );
                let messages = result_pointer_step(
                    &state,
                    &mut tree,
                    &renderer,
                    Event::ButtonReleased(Button::Left),
                );
                assert!(
                    matches!(messages.as_slice(), [Message::ProjectSearch(Action::OpenResult(target))]
                    if target.generation == 42 && target.matched.path == Path::new("replacement.rs"))
                );
            } else {
                assert!(
                    matches!(messages.as_slice(), [Message::ProjectSearch(Action::OpenResult(target))]
                    if target.generation == 41 && target.matched.path == Path::new("original.rs"))
                );
            }
        }
    }

    #[test]
    fn held_results_refuse_a_modal_or_deleting_worktree_before_editor_effects() {
        for modal in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            app.sidebar_model = sidebar::tests::panel_model();
            app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
            let root = app.sidebar_state.active_worktree().unwrap().clone();
            app.search_ui.root = Some(root.clone());
            app.search_ui.matches.push(project_search::Match {
                path: "held.rs".into(),
                line: 0,
                column: 0,
                length: 3,
                line_identity: project_search::LineIdentity::new("hit"),
                snippet: "hit".into(),
                highlight: 0..3,
            });
            let target = app.search_ui.result_target(0).unwrap();
            if modal {
                app.settings_reset_confirm = true;
                assert!(app.modal_is_open());
            } else {
                app.project_ui.begin_deleting(root.canonical_path);
            }
            drop(app.update_project_search(Action::OpenResult(target)));
            assert!(app.durable.cards.is_empty());
            assert!(app.pending_panel_open.is_none());
            assert!(app.workspace.focused().is_none());
            if !modal {
                assert!(
                    app.status
                        .as_ref()
                        .is_some_and(|notice| notice.message.contains("being deleted"))
                );
            }
        }
    }

    #[test]
    fn narrow_result_rows_keep_the_actual_highlight_visible_and_cache_the_fit() {
        use iced::advanced::{
            Layout, layout,
            text::Paragraph as _,
            widget::{Operation, Tree},
        };
        use iced::{Rectangle, Size};
        struct Bounds(Option<Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn container(&mut self, _: Option<&widget::Id>, bounds: Rectangle) {
                self.0 = Some(bounds);
            }
        }
        let renderer = super::super::tests::headless_renderer();
        let state = State::default();
        for (prefix, hit, suffix) in [
            ("W".repeat(120), "needle".into(), "tail".repeat(60)),
            ("éλ".repeat(40), "λé".into(), "λé".repeat(50)),
            ("prefix".into(), "λ".repeat(150), "suffix".into()),
        ] {
            let matched = project_search::Match {
                path: "long.rs".into(),
                line: 12,
                column: prefix.len(),
                length: hit.len(),
                line_identity: project_search::LineIdentity::new(&format!("{prefix}{hit}{suffix}")),
                snippet: format!("{prefix}{hit}{suffix}"),
                highlight: prefix.len()..prefix.len() + hit.len(),
            };
            // The original no-wrap spans put the W-prefixed hit outside 250px.
            if prefix.starts_with('W') {
                let original = SnippetPreview {
                    text: matched.snippet.clone(),
                    highlight: matched.highlight.clone(),
                };
                let paragraph = snippet_paragraph(matched.line, &original, cosmic::font::default());
                assert!(paragraph.span_bounds(2)[0].x > 250.0);
            }
            state.snippet_fits.borrow_mut().clear();
            for width in [180.0, 250.0, 430.0] {
                let mut row = result_row(&state, &matched, 0, false);
                let mut tree = Tree::new(row.as_widget());
                let size = Size::new(width, design::DENSE_ROW_HEIGHT);
                let limits = layout::Limits::new(size, size);
                let node = row.as_widget_mut().layout(&mut tree, &renderer, &limits);
                let mut bounds = Bounds(None);
                row.as_widget_mut()
                    .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
                let fitted = state.snippet_fits.borrow();
                let cached = fitted
                    .get(&0)
                    .expect("mounted responsive row fitted its actual slot");
                let preview = cached.preview.clone();
                let slot = f32::from_bits(cached.width);
                let mounted = bounds.0.expect("actual clipped snippet container");
                assert!(slot > 0.0 && slot <= width);
                assert!((mounted.width - slot).abs() <= 0.1);
                let visible = &preview.text[preview.highlight.clone()];
                assert!(!visible.is_empty() && hit.starts_with(visible));
                let paragraph = snippet_paragraph(matched.line, &preview, cached.font);
                assert!(paragraph.min_width() <= slot);
                let highlighted = paragraph.span_bounds(2);
                assert!(!highlighted.is_empty());
                assert!(highlighted.iter().all(|ink| {
                    let left = mounted.x + ink.x;
                    let right = left + ink.width;
                    left >= mounted.x && right <= mounted.x + mounted.width + 0.1
                }));
                drop(fitted);
                drop(row);
                let mut again = result_row(&state, &matched, 0, false);
                let mut tree = Tree::new(again.as_widget());
                again.as_widget_mut().layout(&mut tree, &renderer, &limits);
                assert!(Arc::ptr_eq(
                    &preview,
                    &state.snippet_fits.borrow()[&0].preview
                ));
                assert_eq!(matched.length, hit.len());
            }
        }
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.search_ui = state;
        drop(app.update_project_search(Action::Query("new query".into())));
        assert!(app.search_ui.snippet_fits.borrow().is_empty());
    }

    #[test]
    fn opening_a_disk_search_result_preserves_a_blocked_editor_even_for_the_same_file() {
        // Disk offsets cannot identify the same text in an unsaved buffer.
        for same_file in [false, true] {
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
            app.card_state_mut(card).unwrap().kind =
                CardKind::Repository(runtime.descriptor.clone());
            app.cards
                .insert(card, CardRuntime::Repository(Box::new(runtime)));
            app.search_ui.root = Some(root);
            app.search_ui.matches.push(project_search::Match {
                path: if same_file {
                    selected.clone()
                } else {
                    "another-search-result.rs".into()
                },
                line: 0,
                column: 0,
                length: 3,
                line_identity: project_search::LineIdentity::new("hit"),
                snippet: "hit".into(),
                highlight: 0..3,
            });
            let target = app.search_ui.result_target(0).unwrap();
            drop(app.update_project_search(Action::OpenResult(target)));
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
    }

    #[test]
    fn held_result_and_submit_cannot_open_a_new_queries_same_index() {
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
        let navigation = runtime.navigation();
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(runtime.descriptor.clone());
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        app.search_ui.root = Some(root.clone());
        app.search_ui.generation = 41;
        app.search_ui.matches.push(project_search::Match {
            path: "original.rs".into(),
            line: 0,
            column: 0,
            length: 1,
            line_identity: project_search::LineIdentity::new("a"),
            snippet: "a".into(),
            highlight: 0..1,
        });
        let held = app.search_ui.result_target(0).unwrap();
        app.context_tab = ContextTab::Search;
        drop(app.update_project_search(Action::Query("replacement".into())));
        let generation = app.search_ui.generation;
        drop(app.update_project_search(Action::Progress {
            generation,
            root,
            progress: Progress::Batch(vec![project_search::Match {
                path: "replacement.rs".into(),
                ..held.matched.clone()
            }]),
        }));
        assert_eq!(app.search_ui.matches[0].path, Path::new("replacement.rs"));
        app.status = None;
        drop(app.update_project_search(Action::OpenResult(held)));
        drop(app.update_project_search(Action::Submit(41)));
        assert!(
            app.status.is_none(),
            "a stale activation must not even reach the current editor guard"
        );
        let CardRuntime::Repository(runtime) = &app.cards[&card] else {
            panic!("original editor");
        };
        assert_eq!(runtime.navigation(), navigation);
        assert!(runtime.is_dirty());
        assert!(app.pending_panel_open.is_none());
    }

    #[test]
    fn delivered_root_retirement_rejects_queued_old_progress_under_the_same_key() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        let root = app.sidebar_state.active_worktree().unwrap().clone();
        app.search_ui.root = Some(root.clone());
        app.search_ui.generation = 10;
        app.search_ui.options.query = "needle".into();
        app.context_tab = ContextTab::Search;
        let cancel = CancellationToken::default();
        app.search_ui.cancel = Some(cancel.clone());
        let matched = project_search::Match {
            path: "original.rs".into(),
            line: 0,
            column: 0,
            length: 6,
            line_identity: project_search::LineIdentity::new("needle"),
            snippet: "needle".into(),
            highlight: 0..6,
        };
        app.search_ui.matches.push(matched.clone());
        app.search_ui.error = Some("original error".into());
        for (project, change) in [
            (
                root.project_id,
                crate::git_watch::Change {
                    topology: true,
                    membership: true,
                    coverage: Some(false),
                    ..Default::default()
                },
            ),
            (
                root.project_id,
                crate::git_watch::Change {
                    retired_roots: [root.canonical_path.join("other")].into(),
                    ..Default::default()
                },
            ),
            (
                ProjectId::new(),
                crate::git_watch::Change {
                    retired_roots: [root.canonical_path.clone()].into(),
                    ..Default::default()
                },
            ),
        ] {
            drop(app.apply_git_watch_change(project, change));
            assert_eq!(app.search_ui.generation, 10);
            assert!(!cancel.is_cancelled());
            assert_eq!(app.search_ui.matches.len(), 1);
        }
        drop(app.apply_git_watch_change(
            root.project_id,
            crate::git_watch::Change {
                retired_roots: [root.canonical_path.clone()].into(),
                ..Default::default()
            },
        ));
        let generation = app.search_ui.generation;
        assert_ne!(generation, 10);
        assert_eq!(app.search_ui.root.as_ref(), Some(&root));
        assert!(cancel.is_cancelled());
        assert!(app.search_ui.matches.is_empty());
        assert!(app.search_ui.error.is_none());
        assert!(app.search_ui.running);
        for progress in [
            Progress::Batch(vec![matched.clone()]),
            Progress::Error("late original error".into()),
            Progress::Finished {
                capped: true,
                skipped: 99,
            },
        ] {
            drop(cosmic::Application::update(
                &mut app,
                Message::ProjectSearch(Action::Progress {
                    generation: 10,
                    root: root.clone(),
                    progress,
                }),
            ));
            assert!(app.search_ui.matches.is_empty());
            assert!(app.search_ui.error.is_none());
            assert!(app.search_ui.running);
            assert!(!app.search_ui.capped);
            assert_eq!(app.search_ui.skipped, 0);
        }
        drop(cosmic::Application::update(
            &mut app,
            Message::ProjectSearch(Action::Progress {
                generation,
                root: root.clone(),
                progress: Progress::Batch(vec![matched]),
            }),
        ));
        assert_eq!(app.search_ui.matches.len(), 1);
        drop(cosmic::Application::update(
            &mut app,
            Message::ProjectSearch(Action::Progress {
                generation,
                root,
                progress: Progress::Finished {
                    capped: false,
                    skipped: 2,
                },
            }),
        ));
        assert!(!app.search_ui.running);
        assert_eq!(app.search_ui.skipped, 2);
    }

    #[test]
    fn retry_after_temporary_root_loss_preserves_the_query_and_fences_old_controls() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        let root = app.sidebar_state.active_worktree().unwrap().clone();
        app.search_ui.root = Some(root.clone());
        app.search_ui.generation = 10;
        app.search_ui.options.query = "needle".into();
        app.search_ui.options.include = "src/**".into();
        app.search_ui.options.exclude = "tests/**".into();
        app.search_ui.options.case_sensitive = true;
        app.context_tab = ContextTab::Search;
        drop(app.apply_git_watch_change(
            root.project_id,
            crate::git_watch::Change {
                retired_roots: [root.canonical_path.clone()].into(),
                ..Default::default()
            },
        ));
        let failed_generation = app.search_ui.generation;
        drop(cosmic::Application::update(
            &mut app,
            Message::ProjectSearch(Action::Progress {
                generation: failed_generation,
                root: root.clone(),
                progress: Progress::Error("Folder is temporarily absent".into()),
            }),
        ));
        assert!(!app.search_ui.running);
        assert!(app.search_ui.error.is_some());
        drop(cosmic::Application::update(
            &mut app,
            Message::ProjectSearch(Action::Retry(10)),
        ));
        assert_eq!(app.search_ui.generation, failed_generation);
        assert!(app.search_ui.error.is_some());
        // The replacement keeps the same logical key. Its user retries the
        // rendered error; no query edit or periodic automatic retry is needed.
        drop(cosmic::Application::update(
            &mut app,
            Message::ProjectSearch(Action::Retry(failed_generation)),
        ));
        let retried = app.search_ui.generation;
        assert_ne!(retried, failed_generation);
        assert_eq!(app.search_ui.root.as_ref(), Some(&root));
        assert_eq!(app.search_ui.options.query, "needle");
        assert_eq!(app.search_ui.options.include, "src/**");
        assert_eq!(app.search_ui.options.exclude, "tests/**");
        assert!(app.search_ui.options.case_sensitive);
        assert!(app.search_ui.running);
        assert!(app.search_ui.error.is_none());
        drop(cosmic::Application::update(
            &mut app,
            Message::ProjectSearch(Action::Progress {
                generation: failed_generation,
                root: root.clone(),
                progress: Progress::Error("late original error".into()),
            }),
        ));
        assert!(app.search_ui.error.is_none());
        assert!(app.search_ui.running);
        drop(cosmic::Application::update(
            &mut app,
            Message::ProjectSearch(Action::Progress {
                generation: retried,
                root,
                progress: Progress::Batch(vec![project_search::Match {
                    path: "src/replacement.rs".into(),
                    line: 0,
                    column: 0,
                    length: 6,
                    line_identity: project_search::LineIdentity::new("needle"),
                    snippet: "needle".into(),
                    highlight: 0..6,
                }]),
            }),
        ));
        assert_eq!(
            app.search_ui.matches[0].path,
            Path::new("src/replacement.rs")
        );
        drop(cosmic::Application::update(
            &mut app,
            Message::ProjectSearch(Action::Retry(failed_generation)),
        ));
        assert_eq!(app.search_ui.generation, retried);
        assert_eq!(app.search_ui.matches.len(), 1);
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
            line_identity: project_search::LineIdentity::new("new query"),
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
