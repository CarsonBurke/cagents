use super::*;
use crate::agent::Provider;
use crate::chat;
use crate::persistence::SavedConversation;
use cosmic::iced::futures::StreamExt;
use cosmic::{Renderer, Theme};
use iced::advanced::widget::{Operation, Tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, overlay, renderer};
use iced::{Event, Rectangle, Size, Vector, mouse};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
mod catalog;

/// Conversations read before the first found ones are listed, so the newest
/// appear at once, and between later listings.
const FIRST_BATCH: usize = 64;
const BATCH: usize = 512;
/// Rows listed at a time; more are listed on request.
const PAGE: usize = 100;
const ROW_HEIGHT: f32 = 96.0;
const ROW_SPACING: f32 = 8.0;

#[derive(Default)]
pub(super) struct State {
    pub open: bool,
    generation: u64,
    pub(super) query: String,
    rows: Vec<Row>,
    /// Indices of the rows the query and scope admit, in listed order.
    visible: Vec<usize>,
    /// The sessions found conversations resume with, and their accounts' names.
    seeds: Arc<[chat::Session]>,
    seed_accounts: Vec<String>,
    /// The provider and project `/resume` was asked for in, until the reader
    /// widens the search.
    pub(super) scope: Option<Scope>,
    /// The untouched draft `/resume` was asked from, which the resumed
    /// conversation takes the place of.
    pub(super) replace: Option<CardId>,
    /// Transcripts read so far and transcripts to read, while indexing.
    indexing: Option<(usize, usize)>,
    /// Whether a chosen conversation is being prepared to resume.
    pub(super) loading: bool,
    pub(super) error: Option<String>,
    limit: usize,
    selected: usize,
    /// Once the reader uses the arrows, retain that exact conversation even
    /// when it is the first row. Until then, show the newest arrival first.
    selected_by_reader: bool,
    /// Native geometry before indexing can shorten the list and clamp its
    /// offset. Shared with keyboard reveals so they do not wait for a redraw.
    reader: Arc<Mutex<Reader>>,
    /// What earlier openings read, so reopening reads only what changed.
    cache: Arc<catalog::Cache>,
    indexer: Option<iced::task::Handle>,
    /// Tells the indexer's thread to stop reading, which aborting its
    /// listing alone leaves to the next batch.
    cancel: Arc<AtomicBool>,
}

impl State {
    fn reader_selection(&self) -> Option<CardId> {
        self.selected_by_reader
            .then(|| self.visible.get(self.selected))
            .flatten()
            .and_then(|&index| self.rows.get(index))
            .map(|row| row.card)
    }

    fn reset_reader(&mut self) {
        if let Ok(mut reader) = self.reader.lock() {
            reader.active = false;
            reader.pending = None;
        }
        self.reader = Arc::default();
    }
}

struct Reader {
    active: bool,
    viewport: Option<ReaderViewport>,
    pending: Option<PendingReveal>,
    input_update: bool,
}

impl Default for Reader {
    fn default() -> Self {
        Self {
            active: true,
            viewport: None,
            pending: None,
            input_update: false,
        }
    }
}

impl Reader {
    fn observe(&mut self, viewport: ReaderViewport, content_height: f32) {
        if !self.active {
            return;
        }
        // A redraw can clamp the old offset when indexing shortens the list.
        // Preserve that visibility evidence, but let later manual scrolling
        // retire a reveal queued before the reader moved the viewport.
        if self.input_update
            || self.viewport.is_some_and(|previous| {
                let clamped = previous
                    .top
                    .clamp(0.0, (content_height - viewport.height).max(0.0));
                (viewport.top - clamped).abs() > f32::EPSILON
            })
        {
            self.pending = None;
        }
        self.viewport = Some(viewport);
    }
}

struct PendingReveal {
    card: CardId,
    previous: usize,
    selected: usize,
    viewport: Option<ReaderViewport>,
    force: bool,
}

#[derive(Clone, Copy)]
struct ReaderViewport {
    top: f32,
    height: f32,
}

impl ReaderViewport {
    fn contains(self, index: usize) -> bool {
        let top = index as f32 * (ROW_HEIGHT + ROW_SPACING);
        top + ROW_HEIGHT > self.top && top < self.top + self.height
    }
}

// A passive, local input-cause witness; shared controls/vendor stay unchanged.
struct ObservedScroll<'a> {
    content: Element<'a, Message>,
    reader: Arc<Mutex<Reader>>,
}

impl Widget<Message, Theme, Renderer> for ObservedScroll<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // on_scroll also runs for Redraw/layout clamping. Remember its actual
        // cause during child dispatch, even before this Reader's first frame.
        if let Ok(mut reader) = self.reader.lock() {
            reader.input_update = !matches!(event, Event::Window(_));
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if let Ok(mut reader) = self.reader.lock() {
            reader.input_update = false;
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
    }
}

/// A conversation as the picker lists it.
struct Row {
    card: CardId,
    title: String,
    provider: Provider,
    directory: PathBuf,
    account: String,
    updated_at_unix_ms: i64,
    open: bool,
    /// What the query is matched against, lowercased once.
    haystack: String,
    source: Source,
}

enum Source {
    /// A conversation cagents saved, with the launch it was saved with.
    Saved(Box<SavedConversation>),
    /// A conversation found in a provider's history.
    Found(catalog::Found),
}

impl Row {
    fn saved(entry: SavedConversation, account: String) -> Self {
        let launch = &entry.session.launch;
        let mut row = Self {
            card: entry.card,
            title: entry.title.clone(),
            provider: launch.provider,
            directory: launch.recipe.working_directory.clone(),
            account,
            updated_at_unix_ms: entry.updated_at_unix_ms,
            open: entry.open,
            haystack: String::new(),
            source: Source::Saved(Box::new(entry)),
        };
        row.index();
        row
    }

    fn found(found: catalog::Found, provider: Provider, account: String) -> Self {
        let mut row = Self {
            card: CardId::new(),
            title: found.title.clone(),
            provider,
            directory: found.directory.clone(),
            account,
            updated_at_unix_ms: found.updated_at_unix_ms,
            open: false,
            haystack: String::new(),
            source: Source::Found(found),
        };
        row.index();
        row
    }

    fn index(&mut self) {
        self.haystack = format!(
            "{} {} {} {}",
            self.title,
            self.provider.label(),
            self.directory.display(),
            self.account
        )
        .to_lowercase();
    }
}

/// The provider and project a search is narrowed to.
#[derive(Debug, Clone)]
pub(super) struct Scope {
    pub(super) provider: Provider,
    pub(super) roots: Vec<PathBuf>,
    label: String,
}

impl Scope {
    fn admits(&self, row: &Row) -> bool {
        row.provider == self.provider
            && self
                .roots
                .iter()
                .any(|root| row.directory.starts_with(root))
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Open,
    Close,
    Query(String),
    /// Geometry is observed directly during native input, before later
    /// indexing updates can change the scroll extent.
    ViewportObserved,
    /// Widens a search narrowed to one provider and project to every one.
    Unscope,
    Indexed(u64, Progress),
    Resume(CardId),
    More,
    Submit,
    Prepared(u64, SavedConversation, Result<chat::Session, String>),
}

/// What indexing has found so far, in the order it is found.
#[derive(Debug, Clone)]
pub enum Progress {
    /// Conversations cagents saved, and the sessions found ones resume with.
    Saved {
        entries: Vec<SavedConversation>,
        seeds: Arc<[chat::Session]>,
        error: Option<String>,
    },
    /// How many transcripts the providers' histories hold.
    Counted(usize),
    /// Nothing more is coming.
    Finished,
    /// Conversations found among the next transcripts, and better titles for
    /// saved conversations found among them.
    Found {
        conversations: Vec<catalog::Found>,
        titles: Vec<(CardId, String)>,
        read: usize,
    },
}

pub(super) fn focus_id() -> widget::Id {
    widget::Id::new("conversation-resume.search")
}

fn same_conversation(a: &chat::Session, b: &chat::Session) -> bool {
    a.launch.provider == b.launch.provider
        && a.launch.session_id == b.launch.session_id
        && a.launch.recipe.working_directory == b.launch.recipe.working_directory
        && crate::account::home_variable(a.launch.provider).is_none_or(|variable| {
            a.launch.recipe.environment.get(variable) == b.launch.recipe.environment.get(variable)
        })
}

/// What makes two listings the same conversation: its provider, its ID, and
/// the account home its history is kept in.
type Identity = (Provider, Option<String>, Option<PathBuf>);

fn identity(provider: Provider, id: Option<&str>, recipe: &LaunchRecipe) -> Identity {
    (
        provider,
        id.map(str::to_owned),
        crate::account::home_variable(provider)
            .and_then(|variable| recipe.environment.get(variable))
            .map(|home| {
                let path = PathBuf::from(home);
                path.canonicalize().unwrap_or(path)
            }),
    )
}

/// Lists saved conversations, then every conversation the seeds' providers
/// keep, newest first, until `send` reports that nobody is listening or
/// `cancel` is set.
fn index(
    saved: Result<Vec<SavedConversation>, String>,
    seeds: Vec<chat::Session>,
    cache: &catalog::Cache,
    cancel: &AtomicBool,
    mut send: impl FnMut(Progress) -> bool,
) {
    let (mut entries, error) = match saved {
        Ok(entries) => (entries, None),
        Err(error) => (Vec::new(), Some(error)),
    };
    entries.sort_by_key(|entry| {
        (
            std::cmp::Reverse(entry.open),
            std::cmp::Reverse(entry.updated_at_unix_ms),
        )
    });
    // Persisted descriptors retain their launch configuration and ownership
    // evidence even when a log file is newer, so they win over a found copy.
    let mut seen = HashMap::new();
    entries.retain(|entry| {
        let launch = &entry.session.launch;
        let key = identity(
            launch.provider,
            launch.session_id.as_deref(),
            &launch.recipe,
        );
        let untitled = entry.title == chat_ui::chat_card_title(launch.provider);
        seen.insert(key, Some((entry.card, untitled))).is_none()
    });
    let seeds: Arc<[chat::Session]> = seeds.into();
    if !send(Progress::Saved {
        entries,
        seeds: seeds.clone(),
        error,
    }) {
        return;
    }
    let candidates = catalog::candidates(&seeds, cancel);
    if !send(Progress::Counted(candidates.len())) {
        return;
    }
    let names = catalog::names(&seeds);
    // Each seed's account home, resolved once rather than per conversation.
    let homes = seeds
        .iter()
        .map(|seed| identity(seed.launch.provider, None, &seed.launch.recipe).2)
        .collect::<Vec<_>>();
    // Transcripts are many small reads, which a cold disk answers faster
    // side by side.
    let workers = std::thread::available_parallelism()
        .map_or(4, std::num::NonZero::get)
        .min(8);
    let mut read = 0;
    while read < candidates.len() && !cancel.load(Ordering::Relaxed) {
        let batch = &candidates[read..candidates
            .len()
            .min(read + if read == 0 { FIRST_BATCH } else { BATCH })];
        read += batch.len();
        let mut conversations = Vec::new();
        let mut titles = Vec::new();
        for mut found in catalog::find_all(cache, &seeds, batch, workers, cancel) {
            if let Some(name) = names.get(found.seed).and_then(|names| names.get(&found.id)) {
                found.title = name.clone();
                found.titled = true;
                found.named = true;
            }
            let provider = seeds[found.seed].launch.provider;
            match seen.entry((provider, Some(found.id.clone()), homes[found.seed].clone())) {
                std::collections::hash_map::Entry::Vacant(vacant) => {
                    vacant.insert(None);
                    conversations.push(found);
                }
                // A saved conversation goes by the name its provider gives
                // it, and one cagents never named by its first message,
                // unless the reader renamed it here.
                std::collections::hash_map::Entry::Occupied(mut occupied) => {
                    if let Some((card, untitled)) = occupied.get_mut().take()
                        && (found.named || found.titled && untitled)
                    {
                        titles.push((card, found.title));
                    }
                }
            }
        }
        if !send(Progress::Found {
            conversations,
            titles,
            read,
        }) {
            return;
        }
    }
}

/// Reveal against the scrollable's current layout, including short or scaled windows.
struct RevealSelection {
    /// Every queued operation consumes the latest intent. A keyboard reveal
    /// can be followed by more indexing before any native operation runs.
    reader: Arc<Mutex<Reader>>,
}
impl iced::advanced::widget::Operation<Message> for RevealSelection {
    fn traverse(
        &mut self,
        visit: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation<Message>),
    ) {
        visit(self);
    }
    fn scrollable(
        &mut self,
        id: Option<&widget::Id>,
        bounds: iced::Rectangle,
        content: iced::Rectangle,
        translation: iced::Vector,
        state: &mut dyn iced::advanced::widget::operation::Scrollable,
    ) {
        if id != Some(&widget::Id::new("conversation-resume.results")) {
            return;
        }
        let Ok(mut reader) = self.reader.lock() else {
            return;
        };
        if !reader.active {
            return;
        }
        let Some(pending) = reader.pending.take() else {
            return;
        };
        let current = ReaderViewport {
            top: translation.y,
            height: bounds.height,
        };
        reader.viewport = Some(current);
        if !pending.force {
            if !pending
                .viewport
                .unwrap_or(current)
                .contains(pending.previous)
            {
                return;
            }
        }
        let y = resume_selection_offset(
            pending.selected,
            bounds.height,
            content.height,
            translation.y,
        );
        state.scroll_to(
            iced::advanced::widget::operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        );
        reader.viewport = Some(ReaderViewport { top: y, ..current });
    }
}
fn resume_selection_offset(selected: usize, viewport: f32, content: f32, current: f32) -> f32 {
    let top = selected as f32 * (ROW_HEIGHT + ROW_SPACING);
    let bottom = top + ROW_HEIGHT;
    let target = if top < current {
        top
    } else if bottom > current + viewport {
        bottom - viewport
    } else {
        current
    };
    target.clamp(0.0, (content - viewport).max(0.0))
}

impl Cagents {
    fn resume_history_is_available(&mut self) -> bool {
        if crate::device::is_remote() {
            self.status = Some(TransientNotice::warning(
                "Conversation history is not available on remote devices yet",
            ));
            return false;
        }
        true
    }

    pub(super) fn resume_key_event(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        if !self.resume_ui.open {
            return None;
        }
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };
        if !modifiers.is_empty() {
            return None;
        }
        match key {
            keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                if let Ok(mut reader) = self.resume_ui.reader.lock() {
                    reader.pending = None;
                }
                let count = self.resume_ui.visible.len().min(self.resume_ui.limit);
                self.resume_ui.selected_by_reader = count > 0;
                self.resume_ui.selected =
                    (self.resume_ui.selected + 1).min(count.saturating_sub(1));
                Some(self.reveal_resume_selection())
            }
            keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                if let Ok(mut reader) = self.resume_ui.reader.lock() {
                    reader.pending = None;
                }
                self.resume_ui.selected_by_reader = !self.resume_ui.visible.is_empty();
                self.resume_ui.selected = self.resume_ui.selected.saturating_sub(1);
                Some(self.reveal_resume_selection())
            }
            _ => None,
        }
    }

    fn reveal_resume_selection(&self) -> Task<Message> {
        let selected = self.resume_ui.selected;
        let card = self
            .resume_ui
            .visible
            .get(selected)
            .and_then(|&index| self.resume_ui.rows.get(index))
            .map(|row| row.card);
        if let Ok(mut reader) = self.resume_ui.reader.lock() {
            reader.pending = card.map(|card| PendingReveal {
                card,
                previous: selected,
                selected,
                viewport: None,
                force: true,
            });
        }
        if card.is_none() {
            return Task::none();
        }
        iced::advanced::widget::operate(RevealSelection {
            reader: self.resume_ui.reader.clone(),
        })
        .map(cosmic::Action::App)
    }

    /// Opens the picker from `card`'s composer, narrowed to its provider and
    /// project and searching for `query`, unless something else holds the
    /// window.
    pub(super) fn open_resume_from(
        &mut self,
        card: CardId,
        query: String,
    ) -> Option<Task<Message>> {
        // The composer calls this directly, before the normal message gate.
        // Refuse before interpreting any host path on the displaying device.
        if !self.resume_history_is_available() {
            return None;
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return None;
        };
        let provider = runtime.session.launch.provider;
        let directory = runtime.session.launch.recipe.working_directory.clone();
        let (project, roots) = self.project_roots(&directory).unwrap_or_else(|| {
            let name = directory.file_name().map_or_else(
                || directory.display().to_string(),
                |name| name.to_string_lossy().into_owned(),
            );
            (name, vec![directory.clone()])
        });
        // Transcripts record the directory as their provider resolved it.
        let roots = roots
            .into_iter()
            .flat_map(|root| {
                let canonical = root.canonicalize().ok().filter(|path| *path != root);
                std::iter::once(root).chain(canonical)
            })
            .collect();
        let task = self.open_resume(Some(Scope {
            provider,
            roots,
            label: format!("{} · {project}", provider.label()),
        }))?;
        Some(Task::batch([
            task,
            self.update_resume(Action::Query(query)),
        ]))
    }

    /// Opens the picker, or answers `None` while another dialog holds the
    /// window.
    fn open_resume(&mut self, scope: Option<Scope>) -> Option<Task<Message>> {
        if !self.resume_history_is_available() || self.modal_is_open() {
            return None;
        }
        self.chrome_ui.close();
        let state = &mut self.resume_ui;
        state.cancel.store(true, Ordering::Relaxed);
        if let Some(indexer) = state.indexer.take() {
            indexer.abort();
        }
        state.open = true;
        state.generation = state.generation.wrapping_add(1);
        state.query.clear();
        state.rows.clear();
        state.visible.clear();
        state.seeds = Arc::from([]);
        state.seed_accounts.clear();
        state.scope = scope;
        state.replace = None;
        state.error = None;
        state.loading = false;
        state.indexing = Some((0, 0));
        state.limit = PAGE;
        state.selected = 0;
        state.selected_by_reader = false;
        state.reset_reader();
        let generation = state.generation;
        let cache = state.cache.clone();
        state.cancel = Arc::default();
        let cancel = state.cancel.clone();
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        let current = self.durable.cards.clone();
        let accounts = self.resume_catalog_accounts();
        let directory = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
        let (sender, receiver) = async_channel::bounded(2);
        let spawned = std::thread::Builder::new()
            .name("resume-index".into())
            .spawn(move || {
                let mut saved = store
                    .map_or(Ok(Vec::new()), |store| store.saved_conversations())
                    .map_err(|error| error.to_string());
                if let Ok(entries) = &mut saved {
                    for card in current {
                        if let CardKind::Chat(session) = card.kind
                            && session.launch.session_id.is_some()
                        {
                            let updated_at_unix_ms = entries
                                .iter()
                                .find(|entry| entry.card == card.id)
                                .map_or_else(
                                    || chat::now_ms().unwrap_or(0) as i64,
                                    |entry| entry.updated_at_unix_ms,
                                );
                            entries.retain(|entry| entry.card != card.id);
                            entries.push(SavedConversation {
                                card: card.id,
                                title: card.title,
                                session,
                                open: card.existence.is_open(),
                                updated_at_unix_ms,
                            });
                        }
                    }
                }
                let seeds = accounts
                    .into_iter()
                    .filter_map(|account| {
                        chat::fresh_session(
                            account.provider,
                            TerminalId::new(),
                            directory.clone(),
                            &account,
                        )
                        .ok()
                    })
                    .collect();
                index(saved, seeds, &cache, &cancel, |progress| {
                    sender.send_blocking(progress).is_ok()
                });
            });
        if let Err(error) = spawned {
            self.resume_ui.error = Some(format!("Could not look for conversations: {error}"));
        }
        // The end is marked whatever ends it, so a listing never waits on
        // an indexer that is gone.
        let progress = receiver.chain(cosmic::iced::futures::stream::once(async {
            Progress::Finished
        }));
        let (indexer, handle) = Task::stream(progress.map(move |progress| {
            cosmic::Action::App(Message::ResumeConversation(Action::Indexed(
                generation, progress,
            )))
        }))
        .abortable();
        self.resume_ui.indexer = Some(handle);
        Some(Task::batch([
            iced::widget::operation::focus(focus_id()).map(cosmic::Action::App),
            indexer,
        ]))
    }

    pub(super) fn update_resume(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Open => self.open_resume(None).unwrap_or_else(Task::none),
            Action::Close => {
                self.close_resume();
                // Whatever was being prepared is abandoned with the picker.
                self.resume_ui.loading = false;
                self.resume_ui.generation = self.resume_ui.generation.wrapping_add(1);
                self.workspace.focused().map_or_else(Task::none, |card| {
                    iced::widget::operation::focus(self.card_keyboard_focus(card))
                        .map(cosmic::Action::App)
                })
            }
            Action::Query(query) => {
                self.resume_ui.query = query;
                self.refilter_resume();
                self.reveal_resume_selection()
            }
            Action::ViewportObserved => Task::none(),
            Action::Unscope => {
                self.resume_ui.scope = None;
                self.refilter_resume();
                Task::batch([
                    self.reveal_resume_selection(),
                    iced::widget::operation::focus(focus_id()).map(cosmic::Action::App),
                ])
            }
            Action::More => {
                self.resume_ui.limit += PAGE;
                Task::none()
            }
            Action::Indexed(generation, progress) => {
                if !self.resume_ui.open || generation != self.resume_ui.generation {
                    return Task::none();
                }
                let previous = self.resume_ui.selected;
                let selected = self.resume_ui.reader_selection();
                self.resume_indexed(progress);
                if selected.is_some()
                    && selected == self.resume_ui.reader_selection()
                    && previous != self.resume_ui.selected
                {
                    if let Ok(mut reader) = self.resume_ui.reader.lock() {
                        let card = selected.unwrap();
                        if let Some(pending) = reader
                            .pending
                            .as_mut()
                            .filter(|pending| pending.card == card)
                        {
                            pending.selected = self.resume_ui.selected;
                        } else {
                            reader.pending = Some(PendingReveal {
                                card,
                                previous,
                                selected: self.resume_ui.selected,
                                viewport: reader.viewport,
                                force: false,
                            });
                        }
                    }
                    iced::advanced::widget::operate(RevealSelection {
                        reader: self.resume_ui.reader.clone(),
                    })
                    .map(cosmic::Action::App)
                } else {
                    if selected != self.resume_ui.reader_selection() {
                        if let Ok(mut reader) = self.resume_ui.reader.lock() {
                            reader.pending = None;
                        }
                    }
                    Task::none()
                }
            }
            Action::Resume(card) => {
                if !self.resume_ui.open || self.resume_ui.loading {
                    return Task::none();
                }
                let Some(row) = self.resume_ui.rows.iter().find(|row| row.card == card) else {
                    return Task::none();
                };
                let entry = match &row.source {
                    // Resumed under the title it is listed by.
                    Source::Saved(entry) => Some(SavedConversation {
                        title: row.title.clone(),
                        ..SavedConversation::clone(entry)
                    }),
                    Source::Found(found) => self
                        .resume_ui
                        .seeds
                        .get(found.seed)
                        .and_then(|seed| catalog::session(seed, found))
                        .map(|session| SavedConversation {
                            card,
                            title: row.title.clone(),
                            session,
                            open: false,
                            updated_at_unix_ms: row.updated_at_unix_ms,
                        }),
                };
                let Some(entry) = entry else {
                    self.resume_error("This conversation can no longer be resumed.");
                    return Task::none();
                };
                self.resume_saved_conversation(entry)
            }
            Action::Submit => {
                let card = self
                    .resume_ui
                    .visible
                    .get(self.resume_ui.selected)
                    .map(|&index| self.resume_ui.rows[index].card);
                card.map_or_else(Task::none, |card| self.update_resume(Action::Resume(card)))
            }
            Action::Prepared(generation, mut entry, result) => {
                if generation != self.resume_ui.generation {
                    return Task::none();
                }
                self.resume_ui.loading = false;
                match result {
                    Ok(session) => {
                        entry.session = session;
                        if self.existing_conversation_card(&entry.session).is_some() {
                            return self.resume_saved_conversation(entry);
                        }
                        self.insert_resumed_conversation(entry)
                    }
                    Err(error) => {
                        self.resume_error(&error);
                        Task::none()
                    }
                }
            }
        }
    }

    fn resume_indexed(&mut self, progress: Progress) {
        match progress {
            Progress::Saved {
                entries,
                seeds,
                error,
            } => {
                let accounts = seeds
                    .iter()
                    .map(|seed| self.resume_account_label(seed))
                    .collect();
                let rows = entries
                    .into_iter()
                    .map(|entry| {
                        let account = self.resume_account_label(&entry.session);
                        Row::saved(entry, account)
                    })
                    .collect::<Vec<_>>();
                let state = &mut self.resume_ui;
                state.seeds = seeds;
                state.seed_accounts = accounts;
                state.error = error;
                state.rows.extend(rows);
            }
            Progress::Finished => self.resume_ui.indexing = None,
            Progress::Counted(total) => {
                let state = &mut self.resume_ui;
                state.indexing = (total > 0).then_some((0, total));
            }
            Progress::Found {
                conversations,
                titles,
                read,
            } => {
                let state = &mut self.resume_ui;
                for (card, title) in titles
                    .into_iter()
                    .filter(|(card, _)| !self.chrome_ui.user_titles.contains_key(card))
                {
                    if let Some(row) = state.rows.iter_mut().find(|row| row.card == card) {
                        row.title = title;
                        row.index();
                    }
                }
                for found in conversations {
                    let Some(seed) = state.seeds.get(found.seed) else {
                        continue;
                    };
                    let (provider, account) = (
                        seed.launch.provider,
                        state.seed_accounts[found.seed].clone(),
                    );
                    state.rows.push(Row::found(found, provider, account));
                }
                state.indexing = state
                    .indexing
                    .map(|(_, total)| (read, total))
                    .filter(|(read, total)| read < total);
            }
        }
        let state = &mut self.resume_ui;
        // A row the reader moved to stays selected wherever more rows land;
        // otherwise the newest stays first.
        let selected = state.reader_selection();
        // Stable, so rows already listed keep their order among equals.
        state
            .rows
            .sort_by_key(|row| std::cmp::Reverse(row.updated_at_unix_ms));
        state.visible = resume_matches(&state.rows, &state.query, state.scope.as_ref());
        let retained = selected.and_then(|card| {
            state
                .visible
                .iter()
                .position(|&index| state.rows[index].card == card)
        });
        state.selected = retained.unwrap_or(0);
        state.selected_by_reader = retained.is_some();
        // Newer rows landing above it never page the selection out of view.
        state.limit = state.limit.max((state.selected + 1).next_multiple_of(PAGE));
    }

    /// Lists the rows the query and scope admit, from the first.
    fn refilter_resume(&mut self) {
        let state = &mut self.resume_ui;
        state.limit = PAGE;
        state.selected = 0;
        state.selected_by_reader = false;
        state.reset_reader();
        state.visible = resume_matches(&state.rows, &state.query, state.scope.as_ref());
    }

    fn existing_conversation_card(&self, session: &chat::Session) -> Option<CardId> {
        self.durable
            .cards
            .iter()
            .find(|card| {
                card.existence.is_open()
                    && !self.chat_awaiting_admission(card.id)
                    && match &card.kind {
                        CardKind::Chat(current) => same_conversation(current, session),
                        CardKind::Terminal(current) => chat_ui::conversion(current)
                            .is_ok_and(|current| same_conversation(&current, session)),
                        _ => false,
                    }
            })
            .map(|card| card.id)
    }

    pub(super) fn resume_saved_conversation(&mut self, entry: SavedConversation) -> Task<Message> {
        if self.resume_ui.loading {
            return Task::none();
        }
        self.resume_ui.error = None;
        let existing = self.existing_conversation_card(&entry.session);
        if let Some(card) = existing {
            self.close_resume();
            let scope = if let Some(channel) = self
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&card))
                .map(|channel| channel.id)
            {
                self.select_channel(channel)
            } else {
                let owner = self
                    .card_state(card)
                    .and_then(|card| self.card_worktree(card))
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| entry.session.launch.recipe.working_directory.clone());
                self.select_worktree(owner, false)
            };
            return Task::batch([scope, cosmic::task::message(Message::FocusCard(card))]);
        }
        let local_pending = self.chat_awaiting_admission(entry.card)
            && matches!(self.card_state(entry.card).map(|card| &card.kind),
                Some(CardKind::Chat(session)) if session == &entry.session);
        if entry.open && !local_pending {
            self.resume_error("This conversation is open outside this window. Return to its original window before resuming.");
            return Task::none();
        }
        let session = &entry.session;
        if matches!(
            session.native_continuity,
            chat::NativeContinuity::Durable { .. }
        ) {
            let generation = self.resume_ui.generation;
            self.resume_ui.loading = true;
            return cosmic::task::future(async move {
                let source = entry.session.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let socket = crate::terminal::ipc::daemon_socket_path()
                        .map_err(|error| error.to_string())?;
                    crate::control::ControlClient::connect(&socket)
                        .map_err(|error| error.to_string())?
                        .prepare_chat_resume(source)
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                Message::ResumeConversation(Action::Prepared(generation, entry, result))
            });
        }
        self.insert_resumed_conversation(entry)
    }

    fn insert_resumed_conversation(&mut self, entry: SavedConversation) -> Task<Message> {
        let mut session = entry.session;
        if !session.launch.recipe.working_directory.is_dir()
            && !session.launch.needs_read_only_history()
        {
            self.resume_error("This conversation's project folder is unavailable. Restore the folder before resuming.");
            return Task::none();
        }
        if !matches!(
            session.native_continuity,
            chat::NativeContinuity::Durable { .. }
        ) {
            session.reset_owned_codex_listener();
            session.id = TerminalId::new();
            session.proof = None;
            session.terminal = None;
        }
        let replace = self.resume_ui.replace;
        self.close_resume();
        self.recently_closed
            .retain(|(card, _)| card.id != entry.card);
        let scope = self.select_worktree(session.launch.recipe.working_directory.clone(), false);
        // The draft `/resume` was asked from gives its place to the
        // conversation, where both belong to the same strip.
        let replace = replace.filter(|card| {
            self.chat_is_unused(*card) && self.workspace.order().any(|id| id == *card)
        });
        let insert = self.insert_chat_card(session, replace, None);
        if let Some(card) = self.workspace.focused() {
            self.apply_reported_title(card, entry.title);
        }
        let discard = replace.map_or_else(Task::none, |card| self.close_card_unasked(card));
        Task::batch([scope, insert, discard])
    }

    /// Closes the picker and stops indexing. Focus is left to whatever the
    /// caller shows next.
    fn close_resume(&mut self) {
        let state = &mut self.resume_ui;
        state.open = false;
        state.selected_by_reader = false;
        state.reset_reader();
        state.indexing = None;
        state.replace = None;
        state.cancel.store(true, Ordering::Relaxed);
        if let Some(indexer) = state.indexer.take() {
            indexer.abort();
        }
        // Rows hold sessions, which are not small; the cache keeps what
        // reopening needs.
        state.rows = Vec::new();
        state.visible = Vec::new();
        state.seeds = Arc::from([]);
        state.seed_accounts = Vec::new();
    }

    fn resume_error(&mut self, error: &str) {
        self.resume_ui.error = Some(error.into());
        if !self.resume_ui.open {
            self.status = Some(TransientNotice::error(error));
        }
    }

    pub(super) fn resume_dialog(&self) -> Option<Element<'_, Message>> {
        if !self.resume_ui.open {
            return None;
        }
        let state = &self.resume_ui;
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        for (index, row) in state
            .visible
            .iter()
            .take(state.limit)
            .map(|&index| &state.rows[index])
            .enumerate()
        {
            let label = format!(
                "{} · {}{}",
                row.provider.label(),
                row.account,
                if row.open { " · Open" } else { "" }
            );
            let date = chat::local_time(row.updated_at_unix_ms / 1000)
                .map(|time| {
                    format!(
                        "{:04}-{:02}-{:02} {}",
                        time.year + 1900,
                        time.month + 1,
                        time.day,
                        time.clock()
                    )
                })
                .unwrap_or_default();
            let detail = format!("{} · {}", row.directory.display(), date);
            let contents = widget::column::with_children(vec![
                design::body(&row.title)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
                design::caption(label)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
                design::caption(detail)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(space::XS);
            rows.push(
                widget::button::custom(contents)
                    .name(format!("Resume {}", row.title))
                    .width(Length::Fill)
                    .height(Length::Fixed(ROW_HEIGHT))
                    .class(cosmic::theme::Button::ListItem([8.0; 4]))
                    .selected(index == state.selected)
                    .on_press_maybe(
                        (!state.loading)
                            .then_some(Message::ResumeConversation(Action::Resume(row.card))),
                    )
                    .into(),
            );
        }
        if state.visible.len() > state.limit {
            rows.push(
                crate::ui_controls::button::secondary("Show more conversations")
                    .on_press(Message::ResumeConversation(Action::More))
                    .into(),
            );
        }
        if rows.is_empty() {
            rows.push(
                design::caption(if state.indexing.is_some() {
                    "Looking for saved conversations…"
                } else if state.rows.is_empty() {
                    "No saved conversations yet."
                } else if state.scope.is_some() && state.query.is_empty() {
                    "No conversations in this project yet."
                } else {
                    "No matching conversations."
                })
                .into(),
            );
        }
        let progress = match state.indexing {
            Some((_, 0)) => "Looking for conversations…".to_owned(),
            Some((read, total)) => format!("Reading {read} of {total} conversations…"),
            None => match state.visible.len() {
                1 => "1 conversation".to_owned(),
                count => format!("{count} conversations"),
            },
        };
        let mut filters: Vec<Element<'_, Message>> = Vec::new();
        if let Some(scope) = &state.scope {
            filters.push(
                widget::button::standard(scope.label.clone())
                    .trailing_icon(Icon::Close)
                    .name("Search every provider and project")
                    .on_press(Message::ResumeConversation(Action::Unscope))
                    .into(),
            );
        }
        filters.push(iced::widget::Space::new().width(Length::Fill).into());
        filters.push(design::caption(progress).into());
        let mut content = widget::column::with_children(vec![
            design::title("Resume conversation").into(),
            design::caption(
                "Return to a saved conversation with its original project and account.",
            )
            .into(),
            crate::ui_controls::field::search(
                "Search conversations, projects, or accounts…",
                &state.query,
            )
            .id(focus_id())
            .on_input(|query| Message::ResumeConversation(Action::Query(query)))
            .on_submit(|_| Message::ResumeConversation(Action::Submit))
            .into(),
            widget::row::with_children(filters)
                .align_y(Alignment::Center)
                .spacing(space::S)
                // As tall as the scope it may hold, so widening the search
                // does not move the list.
                .height(Length::Fixed(32.0))
                .into(),
            Element::new(ObservedScroll {
                reader: state.reader.clone(),
                content: widget::scrollable(
                    widget::column::with_children(rows).spacing(ROW_SPACING),
                )
                .id(widget::Id::new("conversation-resume.results"))
                .on_scroll({
                    let observed = state.reader.clone();
                    move |viewport| {
                        if let Ok(mut observed) = observed.lock() {
                            observed.observe(
                                ReaderViewport {
                                    top: viewport.absolute_offset().y,
                                    height: viewport.bounds().height,
                                },
                                viewport.content_bounds().height,
                            );
                        }
                        Message::ResumeConversation(Action::ViewportObserved)
                    }
                })
                .height(Length::Fill)
                .into(),
            }),
        ])
        .spacing(space::M)
        .height(Length::Fill);
        if let Some(error) = &state.error {
            content = content.push(design::caption(error));
        }
        content = content.push(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("conversation-resume.cancel"))
                .on_press(Message::ResumeConversation(Action::Close)),
        );
        Some(dialog_ui::overlay(
            widget::container(dialog_ui::surface(content, 800.0))
                .height(Length::Fill)
                .max_height(680.0)
                .into(),
            Some(Message::ResumeConversation(Action::Close)),
            1.0,
        ))
    }
}

/// Indices of the rows matching `query` within `scope`.
fn resume_matches(rows: &[Row], query: &str, scope: Option<&Scope>) -> Vec<usize> {
    let query = query.trim().to_lowercase();
    // Every word is looked for on its own, so "rust review" finds "Review
    // peripheral Rust modules".
    let words = query.split_whitespace().collect::<Vec<_>>();
    rows.iter()
        .enumerate()
        .filter(|(_, row)| scope.is_none_or(|scope| scope.admits(row)))
        .filter(|(_, row)| words.iter().all(|word| row.haystack.contains(word)))
        .map(|(index, _)| index)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(directory: &Path) -> SavedConversation {
        let mut session = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            directory.into(),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
        SavedConversation {
            card: CardId::new(),
            title: "Review peripheral Rust modules".into(),
            session,
            open: false,
            updated_at_unix_ms: 1,
        }
    }

    /// Delivers `entries` to an open picker as indexing would.
    fn list(app: &mut Cagents, generation: u64, entries: Vec<SavedConversation>) {
        let _ = app.update_resume(Action::Indexed(
            generation,
            Progress::Saved {
                entries,
                seeds: Arc::from([]),
                error: None,
            },
        ));
        let _ = app.update_resume(Action::Indexed(generation, Progress::Counted(0)));
    }

    #[test]
    fn closed_conversation_resumes_exact_identity_in_its_original_project() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let entry = saved(directory.path());
        let original = entry.session.clone();
        let _ = app.resume_saved_conversation(entry);
        let card = app.card_state(app.workspace.focused().unwrap()).unwrap();
        let CardKind::Chat(session) = &card.kind else {
            panic!("native conversation")
        };
        assert_ne!(session.id, original.id);
        assert_eq!(session.launch, original.launch);
        assert!(session.proof.is_none());
        assert!(session.terminal.is_none());
        assert_eq!(app.active_workspace.as_deref(), Some(directory.path()));
        assert_eq!(card.title, "Review peripheral Rust modules");
        // An already-open conversation focuses its original pane instead of
        // creating another provider writer.
        let entry = SavedConversation {
            card: card.id,
            title: card.title.clone(),
            session: session.clone(),
            open: true,
            updated_at_unix_ms: 1,
        };
        let before = app.durable.cards.len();
        let _ = app.resume_saved_conversation(entry);
        assert_eq!(app.durable.cards.len(), before);
    }

    #[test]
    fn resume_footer_remains_inside_short_and_scaled_window_layouts() {
        use iced::advanced::{
            layout,
            widget::{Operation, Tree, operation},
        };
        #[derive(Default)]
        struct Bounds {
            list: Option<iced::Rectangle>,
            cancel: Option<iced::Rectangle>,
        }
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: iced::Rectangle,
                _: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&widget::Id::new("conversation-resume.results")) {
                    self.list = Some(bounds);
                }
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                if id == Some(&widget::Id::new("conversation-resume.cancel")) {
                    self.cancel = Some(bounds);
                }
            }
        }
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.limit = PAGE;
        for _ in 0..20 {
            app.resume_ui.rows.push(Row::saved(
                saved(std::path::Path::new("/fixture")),
                "Fixture".into(),
            ));
        }
        app.resume_ui.visible = (0..20).collect();
        let renderer = crate::app::tests::headless_renderer();
        for size in [
            iced::Size::new(1100.0, 900.0),
            iced::Size::new(1100.0, 600.0),
            iced::Size::new(880.0, 480.0),
        ] {
            let mut element = app.resume_dialog().unwrap();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, size),
            );
            let mut bounds = Bounds::default();
            element.as_widget_mut().operate(
                &mut tree,
                iced::advanced::Layout::new(&node),
                &renderer,
                &mut bounds,
            );
            let cancel = bounds.cancel.expect("Cancel remains mounted");
            let list = bounds.list.expect("results viewport remains mounted");
            assert!(list.height > 96.0, "{size:?}: {list:?}");
            assert!(
                list.y + list.height <= cancel.y,
                "list cannot displace footer"
            );
            assert!(
                cancel.y + cancel.height <= size.height,
                "{size:?}: {cancel:?}"
            );
        }
    }

    #[test]
    fn resume_selection_uses_the_actual_viewport_and_preserves_visible_rows() {
        assert_eq!(resume_selection_offset(3, 160.0, 1040.0, 0.0), 248.0);
        assert_eq!(resume_selection_offset(3, 420.0, 1040.0, 0.0), 0.0);
        assert_eq!(resume_selection_offset(0, 160.0, 1040.0, 248.0), 0.0);
        assert_eq!(resume_selection_offset(9, 160.0, 1040.0, 248.0), 872.0);
    }

    fn arrow(app: &mut Cagents, key: keyboard::key::Named) -> Task<Message> {
        app.resume_key_event(&keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })
        .unwrap()
    }

    #[test]
    fn batched_indexing_and_keyboard_intent_reveal_only_the_final_selected_conversation() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation},
        };
        #[derive(Default)]
        struct Viewport {
            height: f32,
            top: f32,
        }
        impl Operation for Viewport {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&widget::Id::new("conversation-resume.results")) {
                    self.height = bounds.height;
                    self.top = translation.y;
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let renderer = crate::app::tests::headless_renderer();
        for size in [
            iced::Size::new(1100.0, 900.0),
            iced::Size::new(880.0, 480.0),
        ] {
            for shrinking in [false, true] {
                for navigation_between_batches in [false, true] {
                    for manually_scrolled in [false, true] {
                        let mut app = crate::app::tests::context_inspector_test_app(false);
                        app.resume_ui.open = true;
                        app.resume_ui.limit = PAGE;
                        let generation = app.resume_ui.generation;
                        list(
                            &mut app,
                            generation,
                            (0..if shrinking { 100 } else { 24 })
                                .map(|_| saved(directory.path()))
                                .collect(),
                        );
                        if shrinking {
                            drop(app.update_resume(Action::Query("peripheral".into())));
                        }
                        let mut key_reveal = Task::none();
                        for _ in 0..if shrinking { 70 } else { 1 } {
                            key_reveal = arrow(&mut app, keyboard::key::Named::ArrowDown);
                        }
                        let mut selected = app.resume_ui.reader_selection().unwrap();
                        let mut before = app.resume_dialog().unwrap();
                        let mut tree = Tree::new(before.as_widget());
                        let limits = layout::Limits::new(iced::Size::ZERO, size);
                        let node = before.as_widget_mut().layout(&mut tree, &renderer, &limits);
                        iced::futures::executor::block_on(async {
                            let mut effects = iced::runtime::task::into_stream(key_reveal).unwrap();
                            let Some(iced::runtime::Action::Widget(mut operation)) =
                                effects.next().await
                            else {
                                panic!(
                                    "the initial keyboard reveal must run before manual scrolling"
                                );
                            };
                            before.as_widget_mut().operate(
                                &mut tree,
                                Layout::new(&node),
                                &renderer,
                                operation.as_mut(),
                            );
                        });
                        let mut scroll = operation::scrollable::scroll_to::<()>(
                            widget::Id::new("conversation-resume.results"),
                            iced::widget::scrollable::AbsoluteOffset {
                                x: None,
                                y: Some(match (shrinking, manually_scrolled) {
                                    (true, false) => 70.0 * (ROW_HEIGHT + ROW_SPACING),
                                    (true, true) => 85.0 * (ROW_HEIGHT + ROW_SPACING),
                                    (false, true) => 1500.0,
                                    (false, false) => 0.0,
                                }),
                            },
                        );
                        before.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &mut scroll,
                        );
                        let mut old = Viewport::default();
                        before.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &mut old,
                        );
                        // Let the actual native scroll callback observe geometry
                        // before indexing shortens and clamps the scrollable.
                        let mut messages = Vec::new();
                        before.as_widget_mut().update(
                            &mut tree,
                            &iced::Event::Window(iced::window::Event::RedrawRequested(
                                iced::time::Instant::now(),
                            )),
                            Layout::new(&node),
                            iced::mouse::Cursor::Unavailable,
                            &renderer,
                            &mut iced::advanced::clipboard::Null,
                            &mut iced::advanced::Shell::new(&mut messages),
                            &iced::Rectangle::with_size(size),
                        );
                        assert_eq!(
                            app.resume_ui.reader.lock().unwrap().viewport.unwrap().top,
                            old.top
                        );
                        drop(before);

                        let progress = if shrinking {
                            Progress::Found {
                                conversations: Vec::new(),
                                titles: app.resume_ui.rows[..35]
                                    .iter()
                                    .map(|row| (row.card, "An unrelated conversation".into()))
                                    .collect(),
                                read: 35,
                            }
                        } else {
                            Progress::Saved {
                                entries: (0..PAGE)
                                    .map(|_| {
                                        let mut entry = saved(directory.path());
                                        entry.updated_at_unix_ms = 2;
                                        entry
                                    })
                                    .collect(),
                                seeds: Arc::from([]),
                                error: None,
                            }
                        };
                        let first = app.update_resume(Action::Indexed(generation, progress));
                        let key = if navigation_between_batches {
                            let task = arrow(&mut app, keyboard::key::Named::ArrowDown);
                            selected = app.resume_ui.reader_selection().unwrap();
                            Some(task)
                        } else {
                            None
                        };
                        // The pinned native runtime drains several model messages
                        // before executing their widget effects. Preserve the
                        // original visibility intent and reveal the final index.
                        let progress = if shrinking {
                            Progress::Found {
                                conversations: Vec::new(),
                                titles: app.resume_ui.rows[35..70]
                                    .iter()
                                    .map(|row| (row.card, "An unrelated conversation".into()))
                                    .collect(),
                                read: 70,
                            }
                        } else {
                            Progress::Saved {
                                entries: (0..PAGE)
                                    .map(|_| {
                                        let mut entry = saved(directory.path());
                                        entry.updated_at_unix_ms = 3;
                                        entry
                                    })
                                    .collect(),
                                seeds: Arc::from([]),
                                error: None,
                            }
                        };
                        let second = app.update_resume(Action::Indexed(generation, progress));
                        assert_eq!(app.resume_ui.reader_selection(), Some(selected));
                        assert_eq!(
                            app.resume_ui.selected,
                            if shrinking {
                                usize::from(navigation_between_batches)
                            } else {
                                2 * PAGE + 1 + usize::from(navigation_between_batches)
                            }
                        );
                        let mut after = app.resume_dialog().unwrap();
                        tree.diff(after.as_widget_mut());
                        let node = after.as_widget_mut().layout(&mut tree, &renderer, &limits);
                        let mut clamped = Viewport::default();
                        after.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &mut clamped,
                        );
                        if shrinking {
                            assert!(
                                clamped.top < old.top,
                                "the shortened native list must reproduce offset clamping"
                            );
                            assert!(app.resume_ui.reader.lock().unwrap().pending.is_some());
                            // The real redraw callback reports this layout-only
                            // clamp before queued reveal effects are consumed.
                            messages.clear();
                            after.as_widget_mut().update(
                                &mut tree,
                                &iced::Event::Window(iced::window::Event::RedrawRequested(
                                    iced::time::Instant::now(),
                                )),
                                Layout::new(&node),
                                iced::mouse::Cursor::Unavailable,
                                &renderer,
                                &mut iced::advanced::clipboard::Null,
                                &mut iced::advanced::Shell::new(&mut messages),
                                &iced::Rectangle::with_size(size),
                            );
                            let reader = app.resume_ui.reader.lock().unwrap();
                            assert_eq!(reader.viewport.unwrap().top, clamped.top);
                            assert!(
                                reader.pending.is_some(),
                                "a layout clamp is not manual input"
                            );
                        }
                        iced::futures::executor::block_on(async {
                            for task in std::iter::once(first)
                                .chain(key)
                                .chain(std::iter::once(second))
                            {
                                let mut effects = iced::runtime::task::into_stream(task).expect(
                                    "a moved reader selection returns its reveal operation",
                                );
                                let Some(iced::runtime::Action::Widget(mut operation)) =
                                    effects.next().await
                                else {
                                    panic!(
                                        "indexing must reveal through the actual native widget operation"
                                    );
                                };
                                after.as_widget_mut().operate(
                                    &mut tree,
                                    Layout::new(&node),
                                    &renderer,
                                    operation.as_mut(),
                                );
                            }
                        });
                        let mut current = Viewport::default();
                        after.as_widget_mut().operate(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &mut current,
                        );
                        if manually_scrolled && !navigation_between_batches {
                            assert_eq!(
                                current.top, clamped.top,
                                "indexing respects manual scrolling"
                            );
                        } else {
                            let row_top =
                                app.resume_ui.selected as f32 * (ROW_HEIGHT + ROW_SPACING);
                            assert!(row_top >= current.top);
                            assert!(row_top + ROW_HEIGHT <= current.top + current.height + 0.1);
                        }
                        drop(after);
                        let unchanged =
                            app.update_resume(Action::Indexed(generation, Progress::Counted(512)));
                        assert!(
                            iced::runtime::task::into_stream(unchanged).is_none(),
                            "indexing progress without row movement must not scroll"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn manual_native_scroll_retires_queued_index_and_query_reveals() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation},
        };
        #[derive(Default)]
        struct Viewport {
            bounds: iced::Rectangle,
            top: f32,
        }
        impl Operation for Viewport {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: iced::Rectangle,
                _: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&widget::Id::new("conversation-resume.results")) {
                    self.bounds = bounds;
                    self.top = translation.y;
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        for query_reset in [false, true] {
            let renderer = crate::app::tests::headless_renderer();
            let size = iced::Size::new(880.0, 480.0);
            let limits = layout::Limits::new(iced::Size::ZERO, size);
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.resume_ui.open = true;
            app.resume_ui.limit = PAGE;
            let generation = app.resume_ui.generation;
            list(
                &mut app,
                generation,
                (0..24).map(|_| saved(directory.path())).collect(),
            );
            drop(arrow(&mut app, keyboard::key::Named::ArrowUp));
            let selected = app.resume_ui.reader_selection().unwrap();
            let mut before = app.resume_dialog().unwrap();
            let mut tree = Tree::new(before.as_widget());
            let node = before.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut messages = Vec::new();
            before.as_widget_mut().update(
                &mut tree,
                &iced::Event::Window(iced::window::Event::RedrawRequested(
                    iced::time::Instant::now(),
                )),
                Layout::new(&node),
                iced::mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut iced::advanced::Shell::new(&mut messages),
                &iced::Rectangle::with_size(size),
            );
            drop(before);
            // The selection is visible when the arrival queues its reveal.
            let queued = if query_reset {
                let task = app.update_resume(Action::Query(String::new()));
                assert!(app.resume_ui.reader.lock().unwrap().viewport.is_none());
                task
            } else {
                app.update_resume(Action::Indexed(
                    generation,
                    Progress::Saved {
                        entries: (0..PAGE)
                            .map(|_| {
                                let mut entry = saved(directory.path());
                                entry.updated_at_unix_ms = 2;
                                entry
                            })
                            .collect(),
                        seeds: Arc::from([]),
                        error: None,
                    },
                ))
            };
            assert_eq!(
                app.resume_ui.rows[app.resume_ui.visible[app.resume_ui.selected]].card,
                selected
            );
            assert!(app.resume_ui.reader.lock().unwrap().pending.is_some());
            let mut after = app.resume_dialog().unwrap();
            tree.diff(after.as_widget_mut());
            let node = after.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut initial = Viewport::default();
            after
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut initial);
            messages.clear();
            after.as_widget_mut().update(
                &mut tree,
                &iced::Event::Mouse(iced::mouse::Event::WheelScrolled {
                    delta: iced::mouse::ScrollDelta::Lines { x: 0.0, y: -12.0 },
                }),
                Layout::new(&node),
                iced::mouse::Cursor::Available(initial.bounds.center()),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut iced::advanced::Shell::new(&mut messages),
                &iced::Rectangle::with_size(size),
            );
            let mut manual = Viewport::default();
            after
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut manual);
            assert!(
                manual.top > initial.top,
                "real wheel input must move the list"
            );
            assert!(app.resume_ui.reader.lock().unwrap().pending.is_none());
            iced::futures::executor::block_on(async {
                let mut effects = iced::runtime::task::into_stream(queued).unwrap();
                let Some(iced::runtime::Action::Widget(mut operation)) = effects.next().await
                else {
                    panic!("the queued indexing task must contain its real native operation");
                };
                after.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    operation.as_mut(),
                );
            });
            let mut current = Viewport::default();
            after
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut current);
            assert_eq!(
                current.top, manual.top,
                "older indexing cannot override later input"
            );
            drop(after);
            assert_eq!(
                app.resume_ui.rows[app.resume_ui.visible[app.resume_ui.selected]].card,
                selected
            );
            // Explicit navigation after that manual scroll still reveals its row.
            let key = arrow(&mut app, keyboard::key::Named::ArrowUp);
            let mut after = app.resume_dialog().unwrap();
            tree.diff(after.as_widget_mut());
            let node = after.as_widget_mut().layout(&mut tree, &renderer, &limits);
            iced::futures::executor::block_on(async {
                let mut effects = iced::runtime::task::into_stream(key).unwrap();
                let Some(iced::runtime::Action::Widget(mut operation)) = effects.next().await
                else {
                    panic!("fresh keyboard navigation must retain its reveal operation");
                };
                after.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    operation.as_mut(),
                );
            });
            after
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut current);
            let row_top = app.resume_ui.selected as f32 * (ROW_HEIGHT + ROW_SPACING);
            assert!(row_top >= current.top);
            assert!(row_top + ROW_HEIGHT <= current.top + current.bounds.height + 0.1);
        }
    }

    #[test]
    fn deliberate_first_row_navigation_retains_identity_but_empty_navigation_does_not() {
        let directory = tempfile::tempdir().unwrap();
        for (count, keys) in [
            (1, vec![keyboard::key::Named::ArrowDown]),
            (2, vec![keyboard::key::Named::ArrowUp]),
            (
                2,
                vec![
                    keyboard::key::Named::ArrowDown,
                    keyboard::key::Named::ArrowUp,
                ],
            ),
        ] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.resume_ui.open = true;
            app.resume_ui.limit = PAGE;
            let generation = app.resume_ui.generation;
            list(
                &mut app,
                generation,
                (0..count).map(|_| saved(directory.path())).collect(),
            );
            for key in keys {
                drop(arrow(&mut app, key));
            }
            let selected = app.resume_ui.reader_selection().unwrap();
            assert_eq!(app.resume_ui.selected, 0);
            let mut newer = saved(directory.path());
            newer.updated_at_unix_ms = 2;
            list(&mut app, generation, vec![newer]);
            assert_eq!(app.resume_ui.reader_selection(), Some(selected));
            assert_eq!(app.resume_ui.selected, 1);
            drop(app.update_resume(Action::Query(String::new())));
            assert!(app.resume_ui.reader_selection().is_none());
            assert_eq!(app.resume_ui.selected, 0);
            drop(arrow(&mut app, keyboard::key::Named::ArrowDown));
            drop(app.update_resume(Action::Unscope));
            assert!(app.resume_ui.reader_selection().is_none());
            drop(arrow(&mut app, keyboard::key::Named::ArrowUp));
            drop(app.update_resume(Action::Close));
            assert!(app.resume_ui.reader_selection().is_none());
        }
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.limit = PAGE;
        drop(arrow(&mut app, keyboard::key::Named::ArrowDown));
        assert!(!app.resume_ui.selected_by_reader);
        let generation = app.resume_ui.generation;
        let older = saved(directory.path());
        list(&mut app, generation, vec![older]);
        let mut newer = saved(directory.path());
        newer.updated_at_unix_ms = 2;
        let newest = newer.card;
        list(&mut app, generation, vec![newer]);
        assert_eq!(app.resume_ui.rows[app.resume_ui.visible[0]].card, newest);
        assert!(app.resume_ui.reader_selection().is_none());
    }

    #[test]
    fn indexed_title_changes_release_a_selection_that_no_longer_matches_the_query() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.limit = PAGE;
        let generation = app.resume_ui.generation;
        let chosen = saved(directory.path());
        let chosen_card = chosen.card;
        list(&mut app, generation, vec![chosen, saved(directory.path())]);
        drop(app.update_resume(Action::Query("peripheral".into())));
        drop(arrow(&mut app, keyboard::key::Named::ArrowUp));
        assert_eq!(app.resume_ui.reader_selection(), Some(chosen_card));
        let task = app.update_resume(Action::Indexed(
            generation,
            Progress::Found {
                conversations: Vec::new(),
                titles: vec![(chosen_card, "An unrelated conversation".into())],
                read: 1,
            },
        ));
        assert_eq!(app.resume_ui.visible.len(), 1);
        assert_ne!(
            app.resume_ui.rows[app.resume_ui.visible[0]].card,
            chosen_card
        );
        assert!(app.resume_ui.reader_selection().is_none());
        assert_eq!(app.resume_ui.selected, 0);
        assert!(iced::runtime::task::into_stream(task).is_none());
    }

    #[test]
    fn submitting_after_indexed_arrivals_activates_the_original_selected_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let chosen = saved(directory.path());
        drop(app.resume_saved_conversation(chosen.clone()));
        let original = app.workspace.focused().unwrap();
        app.resume_ui.open = true;
        app.resume_ui.limit = PAGE;
        let generation = app.resume_ui.generation;
        list(&mut app, generation, vec![chosen]);
        drop(arrow(&mut app, keyboard::key::Named::ArrowUp));
        let mut newer = saved(directory.path());
        newer.session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        newer.updated_at_unix_ms = 2;
        list(&mut app, generation, vec![newer]);
        assert_eq!(app.resume_ui.selected, 1);
        drop(app.update_resume(Action::Submit));
        assert_eq!(app.workspace.focused(), Some(original));
        assert_eq!(app.durable.cards.len(), 1);
        assert!(!app.resume_ui.open);
    }

    #[test]
    fn queued_indexed_reveals_cannot_override_a_later_query_or_picker_scope() {
        use iced::advanced::widget::operation::scrollable::{
            AbsoluteOffset, RelativeOffset, Scrollable,
        };
        #[derive(Default)]
        struct ScrollRequests(Vec<f32>);
        impl Scrollable for ScrollRequests {
            fn snap_to(&mut self, _: RelativeOffset<Option<f32>>) {}
            fn scroll_to(&mut self, offset: AbsoluteOffset<Option<f32>>) {
                if let Some(y) = offset.y {
                    self.0.push(y);
                }
            }
            fn scroll_by(&mut self, _: AbsoluteOffset, _: iced::Rectangle, _: iced::Rectangle) {}
        }
        let directory = tempfile::tempdir().unwrap();
        for later in ["query", "unscope", "close", "filtered title"] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.resume_ui.open = true;
            app.resume_ui.limit = PAGE;
            let generation = app.resume_ui.generation;
            list(
                &mut app,
                generation,
                (0..24).map(|_| saved(directory.path())).collect(),
            );
            drop(app.update_resume(Action::Query("peripheral".into())));
            drop(arrow(&mut app, keyboard::key::Named::ArrowDown));
            let chosen = app.resume_ui.reader_selection().unwrap();
            let mut newer = saved(directory.path());
            newer.updated_at_unix_ms = 2;
            let queued = app.update_resume(Action::Indexed(
                generation,
                Progress::Saved {
                    entries: vec![newer],
                    seeds: Arc::from([]),
                    error: None,
                },
            ));
            assert_eq!(app.resume_ui.reader_selection(), Some(chosen));
            match later {
                "query" => drop(app.update_resume(Action::Query("rust".into()))),
                "unscope" => drop(app.update_resume(Action::Unscope)),
                "close" => {
                    drop(app.update_resume(Action::Close));
                    app.resume_ui.open = true;
                    app.resume_ui.generation += 1;
                }
                "filtered title" => drop(app.update_resume(Action::Indexed(
                    generation,
                    Progress::Found {
                        conversations: Vec::new(),
                        titles: vec![(chosen, "An unrelated conversation".into())],
                        read: 1,
                    },
                ))),
                _ => unreachable!(),
            }
            let mut requests = ScrollRequests::default();
            iced::futures::executor::block_on(async {
                let mut effects = iced::runtime::task::into_stream(queued).unwrap();
                let Some(iced::runtime::Action::Widget(mut operation)) = effects.next().await
                else {
                    panic!("the queued indexing action must be a real widget operation");
                };
                operation.scrollable(
                    Some(&widget::Id::new("conversation-resume.results")),
                    iced::Rectangle::with_size(iced::Size::new(500.0, 200.0)),
                    iced::Rectangle::with_size(iced::Size::new(500.0, 2600.0)),
                    iced::Vector::new(0.0, 0.0),
                    &mut requests,
                );
            });
            assert!(
                requests.0.is_empty(),
                "{later} must retire the old indexed reveal"
            );
            let stale = app.update_resume(Action::Indexed(
                generation.wrapping_sub(1),
                Progress::Finished,
            ));
            assert!(iced::runtime::task::into_stream(stale).is_none());
        }
    }

    #[test]
    fn cancelling_resume_ignores_late_preparation_and_allows_another_attempt() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let entry = saved(directory.path());
        app.resume_ui.open = true;
        app.resume_ui.loading = true;
        app.resume_ui.generation = 7;
        let _ = app.update_resume(Action::Close);
        assert!(!app.resume_ui.loading);
        let _ = app.update_resume(Action::Prepared(
            7,
            entry.clone(),
            Ok(entry.session.clone()),
        ));
        assert!(app.durable.cards.is_empty());
        let _ = app.resume_saved_conversation(entry);
        assert_eq!(app.durable.cards.len(), 1);
    }

    #[test]
    fn preparation_rechecks_a_conversation_opened_while_waiting() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let entry = saved(directory.path());
        let _ = app.resume_saved_conversation(entry.clone());
        let card = app.workspace.focused();
        app.resume_ui.loading = true;
        let generation = app.resume_ui.generation;
        let _ = app.update_resume(Action::Prepared(
            generation,
            entry.clone(),
            Ok(entry.session),
        ));
        assert_eq!(app.durable.cards.len(), 1);
        assert_eq!(app.workspace.focused(), card);
        assert!(!app.resume_ui.loading);
    }

    #[test]
    fn picker_search_and_stale_results_do_not_change_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.generation = 2;
        app.resume_ui.limit = PAGE;
        list(&mut app, 1, vec![saved(directory.path())]);
        assert!(app.resume_ui.rows.is_empty());
        list(&mut app, 2, vec![saved(directory.path())]);
        assert!(app.resume_ui.indexing.is_none());
        let _ = app.update_resume(Action::Query("PERIPHERAL".into()));
        assert_eq!(app.resume_ui.visible.len(), 1);
        let _ = app.update_resume(Action::Query("rust review".into()));
        assert_eq!(app.resume_ui.visible.len(), 1);
        let _ = app.update_resume(Action::Query("not this conversation".into()));
        assert!(app.resume_ui.visible.is_empty());
        let _ = app.update_resume(Action::Submit);
        assert!(app.durable.cards.is_empty());
        let _ = app.update_resume(Action::Close);
        list(&mut app, 2, vec![saved(directory.path())]);
        assert!(!app.resume_ui.open);
        assert!(app.durable.cards.is_empty());
    }

    /// Newer conversations found while the picker is open land above the
    /// selection, which stays on the newest until the reader moves it, and
    /// then stays with the row they moved to.
    #[test]
    fn the_selection_follows_the_reader_not_the_arrivals() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.limit = PAGE;
        let generation = app.resume_ui.generation;
        let older = saved(directory.path());
        let _ = app.update_resume(Action::Indexed(
            generation,
            Progress::Saved {
                entries: vec![older.clone()],
                seeds: Arc::from([older.session.clone()]),
                error: None,
            },
        ));
        let found = |updated_at_unix_ms| catalog::Found {
            seed: 0,
            path: directory.path().join("transcript.jsonl"),
            id: uuid::Uuid::new_v4().to_string(),
            directory: directory.path().into(),
            title: "Newer".into(),
            titled: true,
            named: true,
            updated_at_unix_ms,
        };
        let arrive = |app: &mut Cagents, updated_at_unix_ms| {
            let _ = app.update_resume(Action::Indexed(
                generation,
                Progress::Found {
                    conversations: vec![found(updated_at_unix_ms)],
                    titles: Vec::new(),
                    read: 1,
                },
            ));
        };
        let selected = |app: &Cagents| {
            let state = &app.resume_ui;
            state.rows[state.visible[state.selected]].card
        };
        arrive(&mut app, 2);
        assert_ne!(selected(&app), older.card, "the newest is first");
        let _ = app.resume_key_event(&keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::ArrowDown),
            modified_key: keyboard::Key::Named(keyboard::key::Named::ArrowDown),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        assert_eq!(selected(&app), older.card);
        arrive(&mut app, 3);
        assert_eq!(selected(&app), older.card, "the reader's choice holds");
        let _ = app.update_resume(Action::Indexed(
            generation,
            Progress::Found {
                conversations: (4..4 + PAGE as i64).map(found).collect(),
                titles: Vec::new(),
                read: 1,
            },
        ));
        assert_eq!(selected(&app), older.card);
        assert!(
            app.resume_ui.selected >= PAGE && app.resume_ui.selected < app.resume_ui.limit,
            "a page of newer rows leaves it listed"
        );
    }

    /// A search narrowed to one provider and project lists only its
    /// conversations until the reader widens it.
    #[test]
    fn a_scoped_search_lists_its_project_until_widened() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.scope = Some(Scope {
            provider: Provider::Claude,
            roots: vec![project.path().into()],
            label: "Claude · project".into(),
        });
        let inside = saved(&project.path().join("src"));
        let mut outside = saved(elsewhere.path());
        outside.card = CardId::new();
        outside.session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
        let generation = app.resume_ui.generation;
        list(&mut app, generation, vec![inside.clone(), outside]);
        let listed = |app: &Cagents| {
            app.resume_ui
                .visible
                .iter()
                .map(|&index| app.resume_ui.rows[index].card)
                .collect::<Vec<_>>()
        };
        assert_eq!(listed(&app), [inside.card]);
        let _ = app.update_resume(Action::Unscope);
        assert_eq!(listed(&app).len(), 2);
    }

    /// Saved conversations are listed before any history is read; a found
    /// copy of one only lends it the provider's title; and the newest found
    /// arrive first and alone.
    #[test]
    fn indexing_lists_saved_conversations_first_and_found_ones_newest_first() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("claude");
        let account = crate::account::Account {
            provider: Provider::Claude,
            home: crate::account::AccountHome::Linked {
                key: uuid::Uuid::new_v4(),
                path: home.clone(),
            },
        };
        let seed = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.path().into(),
            &account,
        )
        .unwrap();
        let projects = provider_root(&seed).join("projects/project");
        std::fs::create_dir_all(&projects).unwrap();
        let ids = (0..FIRST_BATCH + 2)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect::<Vec<_>>();
        for (age, id) in ids.iter().enumerate() {
            let path = projects.join(format!("{id}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{}\n",
                    serde_json::json!({"type":"user","sessionId":id,"cwd":root.path(),"message":{"content":format!("Review prompt {age}")}})
                ),
            )
            .unwrap();
            let modified = std::time::SystemTime::now() - Duration::from_secs(age as u64 * 60);
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(modified)
                .unwrap();
        }
        let mut entry = saved(root.path());
        entry.session = seed.clone();
        entry.session.launch.session_id = Some(ids[1].clone());
        entry.title = chat_ui::chat_card_title(Provider::Claude).into();
        // One cagents already named keeps its name over a first message.
        let mut named = saved(root.path());
        named.session = seed.clone();
        named.session.launch.session_id = Some(ids[3].clone());
        named.title = "Flaky tests".into();
        let mut progress = Vec::new();
        index(
            Ok(vec![entry.clone(), named]),
            vec![seed],
            &catalog::Cache::default(),
            &AtomicBool::new(false),
            |step| {
                progress.push(step);
                true
            },
        );
        let Progress::Saved { entries, .. } = &progress[0] else {
            panic!("saved conversations come first")
        };
        assert_eq!(entries.len(), 2);
        assert!(matches!(progress[1], Progress::Counted(count) if count == ids.len()));
        let Progress::Found {
            conversations,
            titles,
            read,
        } = &progress[2]
        else {
            panic!("the newest conversations come next")
        };
        assert_eq!(*read, FIRST_BATCH);
        assert_eq!(conversations.len(), FIRST_BATCH - 2);
        assert_eq!(conversations[0].id, ids[0]);
        assert_eq!(conversations[1].id, ids[2]);
        assert_eq!(titles, &[(entry.card, "Review prompt 1".to_owned())]);
        let Progress::Found { read, .. } = &progress[3] else {
            panic!("the rest follow")
        };
        assert_eq!(*read, ids.len());
        // Nobody listening stops indexing at once.
        let mut steps = 0;
        index(
            Ok(Vec::new()),
            Vec::new(),
            &catalog::Cache::default(),
            &AtomicBool::new(false),
            |_| {
                steps += 1;
                false
            },
        );
        assert_eq!(steps, 1);
    }

    #[test]
    fn duplicate_saved_descriptors_cannot_redirect_the_retained_rows_catalog_title() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("claude");
        std::fs::create_dir(&home).unwrap();
        let home = home.canonicalize().unwrap();
        let account = crate::account::Account {
            provider: Provider::Claude,
            home: crate::account::AccountHome::Linked {
                key: uuid::Uuid::new_v4(),
                path: home,
            },
        };
        let mut seed = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.path().into(),
            &account,
        )
        .unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        seed.launch.session_id = Some(id.clone());
        let title = "Recover the preferred saved conversation title";
        // Two catalog files describe the same conversation. The first title
        // observation must target the saved winner exactly once, even when
        // another copy of the transcript is encountered in this same batch.
        for copy in 0..2 {
            let project = provider_root(&seed).join(format!("projects/project-{copy}"));
            std::fs::create_dir_all(&project).unwrap();
            let path = project.join(format!("{id}.jsonl"));
            std::fs::write(
                &path,
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type":"user", "sessionId":id, "cwd":root.path(),
                        "message":{"content":title}
                    })
                ),
            )
            .unwrap();
            std::fs::File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(std::time::SystemTime::now() - Duration::from_secs(copy * 60))
                .unwrap();
        }
        for preferred_open in [false, true] {
            for dropped_untitled in [false, true] {
                let mut preferred = saved(root.path());
                preferred.session = seed.clone();
                preferred.session.proof = Some(crate::model::HostSessionProof {
                    host_session_id: "preferred-saved-host".into(),
                    boot_id: "fixture-boot".into(),
                    host_incarnation: "preferred-incarnation".into(),
                });
                preferred.title = chat_ui::chat_card_title(Provider::Claude).into();
                preferred.open = preferred_open;
                preferred.updated_at_unix_ms = if preferred_open { 1 } else { 99 };
                let mut dropped = preferred.clone();
                dropped.card = CardId::new();
                dropped.session.id = TerminalId::new();
                dropped.session.proof = None;
                dropped.open = false;
                dropped.updated_at_unix_ms = if preferred_open { 99 } else { 1 };
                if !dropped_untitled {
                    dropped.title = "A name belonging to the dropped listing".into();
                }
                let mut progress = Vec::new();
                index(
                    Ok(vec![dropped.clone(), preferred.clone()]),
                    vec![seed.clone()],
                    &catalog::Cache::default(),
                    &AtomicBool::new(false),
                    |step| {
                        progress.push(step);
                        true
                    },
                );
                let Progress::Saved { entries, .. } = &progress[0] else {
                    panic!("real indexing lists saved descriptors first")
                };
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].card, preferred.card);
                assert_eq!(
                    entries[0].session, preferred.session,
                    "catalog discovery must preserve the preferred saved launch and ownership descriptor"
                );
                assert!(matches!(progress[1], Progress::Counted(2)));
                let mut title_updates = Vec::new();
                for step in &progress {
                    if let Progress::Found {
                        conversations,
                        titles,
                        ..
                    } = step
                    {
                        assert!(
                            conversations.is_empty(),
                            "a catalog duplicate is not a second conversation"
                        );
                        title_updates.extend(titles.iter().cloned());
                    }
                }
                assert_eq!(
                    title_updates,
                    [(preferred.card, title.to_owned())],
                    "open={preferred_open}, dropped untitled={dropped_untitled}: the dropped card cannot steal the title recipient or its eligibility flag"
                );

                let mut app = crate::app::tests::context_inspector_test_app(false);
                app.resume_ui.open = true;
                app.resume_ui.limit = PAGE;
                let generation = app.resume_ui.generation;
                for step in progress {
                    drop(app.update_resume(Action::Indexed(generation, step)));
                }
                drop(app.update_resume(Action::Query("preferred saved".into())));
                let state = &app.resume_ui;
                assert_eq!(state.rows.len(), 1);
                assert_eq!(
                    state
                        .visible
                        .iter()
                        .map(|index| state.rows[*index].card)
                        .collect::<Vec<_>>(),
                    [preferred.card],
                    "the retained conversation is searchable by its actual catalog title"
                );
                assert_eq!(state.rows[0].title, title);
                let Source::Saved(entry) = &state.rows[0].source else {
                    panic!("title enrichment must retain the saved selection source")
                };
                assert_eq!(entry.card, preferred.card);
                assert_eq!(entry.session, preferred.session);
                assert_ne!(entry.card, dropped.card);
            }
        }
    }

    fn provider_root(seed: &chat::Session) -> PathBuf {
        seed.launch
            .provider
            .session_root(&seed.launch.recipe)
            .unwrap()
    }
}
