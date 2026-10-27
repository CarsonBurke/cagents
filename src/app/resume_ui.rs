use super::*;
use crate::agent::Provider;
use crate::chat;
use crate::persistence::SavedConversation;
use cosmic::iced::futures::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
mod catalog;
mod channel_filing;

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
    /// What earlier openings read, so reopening reads only what changed.
    cache: Arc<catalog::Cache>,
    indexer: Option<iced::task::Handle>,
    /// Tells the indexer's thread to stop reading, which aborting its
    /// listing alone leaves to the next batch.
    cancel: Arc<AtomicBool>,
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
        // Reject only conflicting known recorded account contexts. If either
        // is unknown, preserve the existing predicate above so a legacy live
        // writer cannot be duplicated. Unknown filing still grants no placement.
        && match (channel_filing::conversation_key(a), channel_filing::conversation_key(b)) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
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
    selected: usize,
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
        let y =
            resume_selection_offset(self.selected, bounds.height, content.height, translation.y);
        state.scroll_to(
            iced::advanced::widget::operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        );
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
                let count = self.resume_ui.visible.len().min(self.resume_ui.limit);
                self.resume_ui.selected =
                    (self.resume_ui.selected + 1).min(count.saturating_sub(1));
                Some(self.reveal_resume_selection())
            }
            keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                self.resume_ui.selected = self.resume_ui.selected.saturating_sub(1);
                Some(self.reveal_resume_selection())
            }
            _ => None,
        }
    }

    fn reveal_resume_selection(&self) -> Task<Message> {
        iced::advanced::widget::operate(RevealSelection {
            selected: self.resume_ui.selected,
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
                self.resume_indexed(progress);
                Task::none()
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
        let selected = state
            .visible
            .get(state.selected)
            .filter(|_| state.selected > 0)
            .and_then(|&index| state.rows.get(index))
            .map(|row| row.card);
        // Stable, so rows already listed keep their order among equals.
        state
            .rows
            .sort_by_key(|row| std::cmp::Reverse(row.updated_at_unix_ms));
        state.visible = resume_matches(&state.rows, &state.query, state.scope.as_ref());
        state.selected = selected
            .and_then(|card| {
                state
                    .visible
                    .iter()
                    .position(|&index| state.rows[index].card == card)
            })
            .unwrap_or(0);
        // Newer rows landing above it never page the selection out of view.
        state.limit = state.limit.max((state.selected + 1).next_multiple_of(PAGE));
    }

    /// Lists the rows the query and scope admit, from the first.
    fn refilter_resume(&mut self) {
        let state = &mut self.resume_ui;
        state.limit = PAGE;
        state.selected = 0;
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
        let scope = match self.select_recorded_conversation_channel(&session) {
            Some(scope) => scope,
            None => {
                let (selected, scope) = self.select_worktree_checked(
                    session.launch.recipe.working_directory.clone(),
                    false,
                );
                if !selected && !session.launch.needs_read_only_history() {
                    return scope;
                }
                scope
            }
        };
        self.close_resume();
        self.recently_closed
            .retain(|(card, _)| card.id != entry.card);
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
            widget::scrollable(widget::column::with_children(rows).spacing(ROW_SPACING))
                .id(widget::Id::new("conversation-resume.results"))
                .height(Length::Fill)
                .into(),
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

    fn provider_root(seed: &chat::Session) -> PathBuf {
        seed.launch
            .provider
            .session_root(&seed.launch.recipe)
            .unwrap()
    }
}
