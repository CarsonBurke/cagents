use super::*;
use crate::agent::Provider;
use crate::chat;
use crate::persistence::SavedConversation;
use cosmic::iced::futures::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
mod catalog;
pub(in crate::app) mod placement;

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
    report_query: Option<ReportQuery>,
    report_restore_nonce: Option<uuid::Uuid>,
    deferred_report: Option<(
        u64,
        uuid::Uuid,
        SavedConversation,
        Arc<Result<Option<crate::automations::ArchiveReport>, String>>,
    )>,
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

struct ReportQuery {
    nonce: uuid::Uuid,
    registry: uuid::Uuid,
    card: CardId,
    session: chat::Session,
    picker: bool,
}

/// A qualified report travels with destination preparation. It never becomes
/// an ordinary Resume intent, which would reset the provider identity.
pub(in crate::app) struct ReportRestore {
    nonce: uuid::Uuid,
    generation: u64,
    registry: uuid::Uuid,
    picker_origin: bool,
    entry: SavedConversation,
    report: crate::automations::ArchiveReport,
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
    ReportSource(
        u64,
        uuid::Uuid,
        SavedConversation,
        Arc<Result<Option<crate::automations::ArchiveReport>, String>>,
    ),
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
        models: Vec<(CardId, String)>,
        read: usize,
    },
}

pub(super) fn focus_id() -> widget::Id {
    widget::Id::new("conversation-resume.search")
}

fn same_conversation(a: &chat::Session, b: &chat::Session) -> bool {
    if a.archive_report.is_some() || b.archive_report.is_some() {
        let (Some(left), Some(right)) = (&a.archive_report, &b.archive_report) else {
            return false;
        };
        if left.registry != right.registry
            || left.agent != right.agent
            || a.id != b.id
            || a.proof != b.proof
        {
            return false;
        }
    }
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
        let mut models = Vec::new();
        for mut found in catalog::find_all(cache, &seeds, batch, workers, cancel) {
            if let Some(name) = names.get(&found.id) {
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
                    if let Some((card, untitled)) = occupied.get_mut().take() {
                        if let Some(model) = found.model {
                            models.push((card, model));
                        }
                        if found.named || found.titled && untitled {
                            titles.push((card, found.title));
                        }
                    }
                }
            }
        }
        if !send(Progress::Found {
            conversations,
            titles,
            models,
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
        state.report_query = None;
        state.report_restore_nonce = None;
        state.deferred_report = None;
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
        Some(Task::batch([self.modal_focus(focus_id()), indexer]))
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
                    self.modal_restore_focus(self.card_keyboard_focus(card))
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
                Task::batch([self.reveal_resume_selection(), self.modal_focus(focus_id())])
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
            Action::ReportSource(generation, nonce, entry, result) => {
                let Some(query) = self.resume_ui.report_query.as_ref() else {
                    return Task::none();
                };
                if generation != self.resume_ui.generation
                    || nonce != query.nonce
                    || query.card != entry.card
                    || query.session != entry.session
                {
                    return Task::none();
                }
                let registry = query.registry;
                let same_origin = self.resume_ui.open == query.picker;
                let navigates = self.report_resume_navigation_allowed(query.picker);
                if !navigates {
                    self.resume_ui.loading = false;
                    if same_origin && registry == self.workspace_model.identity {
                        // Keep one exact receipt for the original intent. The
                        // body is fetched lazily after admission, so a hidden
                        // modal never retains the saved transcript payload.
                        let mut receipt = result.as_ref().clone();
                        if let Ok(Some(report)) = &mut receipt {
                            report.archive.snapshot.items.clear();
                            for child in &mut report.archive.snapshot.subagents {
                                child.items.clear();
                            }
                        }
                        self.resume_ui.deferred_report =
                            Some((generation, nonce, entry, Arc::new(receipt)));
                    } else {
                        self.resume_ui.report_query = None;
                        self.resume_ui.deferred_report = None;
                    }
                    return Task::none();
                }
                self.resume_ui.report_query = None;
                self.resume_ui.deferred_report = None;
                self.resume_ui.loading = false;
                if registry != self.workspace_model.identity {
                    self.resume_error("The selected registry changed. Choose this saved result again after it reconnects.");
                    return Task::none();
                }
                match result.as_ref() {
                    Ok(Some(report)) => self.restore_archive_report(entry, report.clone()),
                    Ok(None) if !self.saved_conversation_is_automation_report(&entry) => {
                        self.prepare_provider_conversation_resume(entry)
                    }
                    Ok(None) => {
                        self.resume_error("The saved automation report has no current private receipt. Open its run history; no provider was resumed.");
                        Task::none()
                    }
                    Err(error) => {
                        self.resume_error(error);
                        Task::none()
                    }
                }
            }
            Action::Prepared(generation, mut entry, result) => {
                if generation != self.resume_ui.generation {
                    return Task::none();
                }
                self.resume_ui.loading = false;
                match result {
                    Ok(session) => {
                        if self.saved_conversation_is_automation_report(&entry)
                            && self.existing_conversation_card(&entry.session).is_none()
                        {
                            self.resume_error("This is a saved automation run report. Open Automations and its run history to view the result; it cannot resume a provider conversation.");
                            return Task::none();
                        }
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
                models,
                read,
            } => {
                let state = &mut self.resume_ui;
                for (card, model) in models {
                    if let Some(row) = state.rows.iter().find(|row| row.card == card)
                        && let Source::Saved(entry) = &row.source
                        && let Some(key) = chat_ui::closing::draft_key(&entry.session)
                    {
                        Arc::make_mut(&mut self.durable)
                            .conversation_models
                            .insert(key, model);
                    }
                }
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

    pub(super) fn existing_conversation_card(&self, session: &chat::Session) -> Option<CardId> {
        self.durable
            .cards
            .iter()
            .find(|card| {
                card.existence.is_open()
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
        if self
            .resume_ui
            .deferred_report
            .as_ref()
            .is_some_and(|(_, _, original, _)| {
                original.card == entry.card && original.session == entry.session
            })
        {
            let (generation, nonce, original, receipt) =
                self.resume_ui.deferred_report.take().unwrap();
            return self.update_resume(Action::ReportSource(generation, nonce, original, receipt));
        }
        let known_report = self.saved_conversation_is_automation_report(&entry);
        let existing = self.existing_conversation_card(&entry.session).filter(|card| {
            !known_report || self.durable.automation_archives.contains_key(card)
                || matches!(self.card_state(*card).map(|state|&state.kind),Some(CardKind::Chat(session)) if session.archive_report.is_some())
        });
        if let Some(card) = existing {
            self.close_resume();
            if let Some(channel) = self
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&card))
                .map(|channel| channel.id)
            {
                return self.select_channel_then(
                    channel,
                    worktree_ui::selection::Intent::Focus {
                        card,
                        activate: false,
                    },
                );
            } else {
                let owner = self
                    .card_state(card)
                    .and_then(|card| self.card_worktree(card))
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| entry.session.launch.recipe.working_directory.clone());
                return self.select_worktree_then(
                    owner,
                    false,
                    worktree_ui::selection::Intent::Focus {
                        card,
                        activate: false,
                    },
                );
            }
        }
        if entry.open {
            self.resume_error("This conversation is open outside this window. Return to its original window before resuming.");
            return Task::none();
        }
        if entry.session.archive_report.is_some()
            || entry
                .session
                .proof
                .as_ref()
                .is_some_and(HostSessionProof::is_complete)
            || self.saved_conversation_is_automation_report(&entry)
        {
            return self.lookup_archive_report_source(entry);
        }
        self.prepare_provider_conversation_resume(entry)
    }

    fn prepare_provider_conversation_resume(&mut self, entry: SavedConversation) -> Task<Message> {
        if self.saved_conversation_is_automation_report(&entry)
            || entry.session.archive_report.is_some()
        {
            self.resume_error("This is a saved automation run report. Open Automations and its run history to view the result; it cannot resume a provider conversation.");
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

    // The async receipt retires even when another modal owns navigation. A
    // retained picker is not permission to select a destination behind it.
    fn report_resume_navigation_allowed(&self, picker: bool) -> bool {
        !self.update_prepared
            && self.resume_ui.open == picker
            && matches!(
                (picker, self.top_modal()),
                (true, Some(modal_ui::Modal::Resume)) | (false, None)
            )
    }

    fn lookup_archive_report_source(&mut self, entry: SavedConversation) -> Task<Message> {
        let identity = self.workspace_model.identity;
        if identity == crate::workspace_model::NO_REGISTRY {
            self.resume_error(
                "Wait for the current project registry before opening this saved conversation.",
            );
            return Task::none();
        }
        let generation = self.resume_ui.generation;
        let nonce = uuid::Uuid::new_v4();
        self.resume_ui.deferred_report = None;
        self.resume_ui.report_restore_nonce = None;
        self.resume_ui.report_query = Some(ReportQuery {
            nonce,
            registry: identity,
            card: entry.card,
            session: entry.session.clone(),
            picker: self.resume_ui.open,
        });
        self.resume_ui.loading = true;
        cosmic::task::future(async move {
            let descriptor =
                crate::automations::ArchiveReportDescriptor::from_session(&entry.session);
            let request =
                descriptor.map(
                    |descriptor| crate::automations::Request::ArchiveReportSource {
                        identity,
                        card: entry.card,
                        descriptor,
                    },
                );
            let result = tokio::task::spawn_blocking(move || {
                let socket = crate::terminal::ipc::daemon_socket_path()
                    .map_err(|error| error.to_string())?;
                let mut client = crate::control::ControlClient::connect(&socket)
                    .map_err(|error| error.to_string())?;
                match client
                    .automation(crate::agent_execution::Authority::default(), request?)
                    .map_err(|error| error.to_string())?
                {
                    crate::automations::Response::ArchiveReportSource(report) => {
                        Ok(report.map(|report| *report))
                    }
                    _ => Err("The saved conversation returned no report-source receipt".into()),
                }
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ResumeConversation(Action::ReportSource(
                generation,
                nonce,
                entry,
                Arc::new(result),
            ))
        })
    }

    fn restore_archive_report(
        &mut self,
        entry: SavedConversation,
        mut report: crate::automations::ArchiveReport,
    ) -> Task<Message> {
        let mut issued = entry.session.clone();
        issued.archive_report = Some(report.reference.clone());
        if report.reference.registry != self.workspace_model.identity
            || crate::automations::archive::ArchivedPaneReadiness::qualify_report(&report, &issued)
                .is_err()
        {
            self.resume_error(
                "The saved report's private source changed. No provider was resumed.",
            );
            return Task::none();
        }
        // Destination preparation retains the exact private qualification
        // metadata, while the saved body is fetched only after admission.
        report.archive.snapshot.items.clear();
        for child in &mut report.archive.snapshot.subagents {
            child.items.clear();
        }
        let restore = ReportRestore {
            nonce: uuid::Uuid::new_v4(),
            generation: self.resume_ui.generation,
            registry: self.workspace_model.identity,
            picker_origin: self.resume_ui.open,
            entry,
            report,
        };
        // Hand off only our own picker. The captured intent survives in the
        // selection receipt; another modal cannot be closed by this path.
        if restore.picker_origin {
            self.close_resume();
        }
        self.resume_ui.report_restore_nonce = Some(restore.nonce);
        if !self.archive_report_restore_is_current(&restore) {
            self.status = Some(TransientNotice::warning(
                "The saved report's original project or destination changed. No pane was opened.",
            ));
            return self.abandon_archive_report_restore(restore);
        }
        let scope = restore.report.scope.clone();
        match scope {
            crate::agent_execution::Placement::Channel(channel) => self.select_channel_then(
                channel,
                worktree_ui::selection::Intent::ArchiveReport(Box::new(restore)),
            ),
            crate::agent_execution::Placement::Worktree { path, .. } => self.select_worktree_then(
                path,
                false,
                worktree_ui::selection::Intent::ArchiveReport(Box::new(restore)),
            ),
        }
    }

    pub(in crate::app) fn archive_report_restore_is_current(
        &self,
        restore: &ReportRestore,
    ) -> bool {
        let mut issued = restore.entry.session.clone();
        issued.archive_report = Some(restore.report.reference.clone());
        self.resume_ui.report_restore_nonce == Some(restore.nonce)
            && self.resume_ui.generation == restore.generation
            && self.workspace_model.identity == restore.registry
            && restore.report.reference.registry == restore.registry
            && self
                .workspace_model
                .projects
                .iter()
                .any(|project| project.repository == restore.report.repository)
            && match &restore.report.scope {
                crate::agent_execution::Placement::Channel(channel) => {
                    self.durable
                        .channels
                        .iter()
                        .any(|current| current.id == *channel && current.shared)
                        && self
                            .workspace_model
                            .channel(*channel)
                            .is_some_and(|current| current.repository == restore.report.repository)
                }
                crate::agent_execution::Placement::Worktree { path, repository } => {
                    repository == &restore.report.repository
                        && self.worktree_view_scope(path).as_ref() == Some(&restore.report.scope)
                }
            }
            && crate::automations::archive::ArchivedPaneReadiness::qualify_report(
                &restore.report,
                &issued,
            )
            .is_ok()
    }

    fn archive_report_destination_selected(&self, restore: &ReportRestore) -> bool {
        match &restore.report.scope {
            crate::agent_execution::Placement::Channel(channel) => {
                self.durable.active_channel == Some(*channel)
            }
            crate::agent_execution::Placement::Worktree { path, .. } => {
                self.durable.active_channel.is_none()
                    && self.active_workspace.as_ref() == Some(path)
            }
        }
    }

    pub(in crate::app) fn abandon_archive_report_restore(
        &mut self,
        restore: ReportRestore,
    ) -> Task<Message> {
        if self.resume_ui.report_restore_nonce == Some(restore.nonce) {
            self.resume_ui.report_restore_nonce = None;
        }
        Task::none()
    }

    pub(in crate::app) fn finish_archive_report_restore(
        &mut self,
        restore: ReportRestore,
    ) -> Task<Message> {
        if !self.archive_report_restore_is_current(&restore)
            || self.update_prepared
            || self.modal_is_open()
            || !self.archive_report_destination_selected(&restore)
        {
            return self.abandon_archive_report_restore(restore);
        }
        self.resume_ui.report_restore_nonce = None;
        let mut issued = restore.entry.session.clone();
        issued.archive_report = Some(restore.report.reference.clone());
        if let Some(card) = self.existing_conversation_card(&issued) {
            // Reuse only a pane actually in the now-committed destination.
            // A separately moved report must be reached through its own scope.
            return if self.workspace.card(card).is_some() {
                cosmic::task::message(Message::FocusCard(card))
            } else {
                Task::none()
            };
        }
        let Some((_card, inserted)) =
            self.insert_archive_report_card(issued, &restore.report, restore.entry.title)
        else {
            return Task::none();
        };
        self.recently_closed
            .retain(|(closed, _)| closed.id != restore.entry.card);
        self.persist();
        inserted
    }

    fn saved_conversation_is_automation_report(&self, entry: &SavedConversation) -> bool {
        entry.session.archive_report.is_some()
            || self.durable.automation_archives.contains_key(&entry.card)
            || self
                .workspace_model
                .archived_automation_agents
                .iter()
                .any(|agent| {
                    agent.closed
                        && matches!(
                            agent.origin,
                            crate::agent_execution::Origin::Automation { .. }
                        )
                        && agent.id.as_uuid() == entry.card.as_uuid()
                        && matches!(&agent.session,
                        Some(crate::agent_execution::HostedSession::Chat(saved))
                            if saved.id == entry.session.id)
                })
    }

    fn insert_resumed_conversation(&mut self, entry: SavedConversation) -> Task<Message> {
        // Preparation is asynchronous. A closed owner/map may have arrived
        // while the backend was qualifying the original provider source.
        if self.saved_conversation_is_automation_report(&entry) {
            self.resume_error("This is a saved automation run report. Open Automations and its run history to view the result; it cannot resume a provider conversation.");
            return Task::none();
        }
        let mut session = entry.session;
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
        let entry = SavedConversation { session, ..entry };
        self.select_resume_then(entry, replace)
    }

    pub(super) fn commit_resumed_conversation(
        &mut self,
        entry: SavedConversation,
        replace: Option<CardId>,
    ) -> Task<Message> {
        self.recently_closed
            .retain(|(card, _)| card.id != entry.card);
        let replace = replace.filter(|card| {
            self.chat_is_unused(*card) && self.workspace.order().any(|id| id == *card)
        });
        let insert = self.insert_chat_card(entry.session, replace, None);
        if let Some(card) = self.workspace.focused() {
            if self.durable.active_channel.is_none()
                && let Some(scope) = self.active_workspace.clone()
            {
                // A catalog cwd can be outside the current worktree when it
                // was not known locally. Filing and executable cwd are distinct.
                Arc::make_mut(&mut self.durable)
                    .card_scopes
                    .insert(card, scope.clone());
                self.worktree_owners.insert(card, scope);
            }
            self.apply_reported_title(card, entry.title);
        }
        let discard = replace.map_or_else(Task::none, |card| self.close_card_unasked(card));
        self.refresh_sidebar_channels();
        self.refresh_sidebar_worktree_panes();
        self.persist();
        Task::batch([insert, discard])
     }

    /// Closes the picker and stops indexing. Focus is left to whatever the
    /// caller shows next.
    fn close_resume(&mut self) {
        let state = &mut self.resume_ui;
        state.open = false;
        state.indexing = None;
        state.report_query = None;
        state.report_restore_nonce = None;
        state.deferred_report = None;
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
pub(in crate::app) fn drive_resume_task_for_test(app: &mut Cagents, task: Task<Message>) {
    use cosmic::Application as _;
    let Some(mut stream) = iced::runtime::task::into_stream(task) else { return; };
    let executor = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let messages = executor.block_on(async {
        let mut messages = Vec::new();
        while let Some(action) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(message)) = action {
                messages.push(message);
            }
            // These fixtures assert scope/identity, not widget focus. Modal
            // focus operations can precede the actual selection receipt.
        }
        messages
    });
    for message in messages {
        let preparing = matches!(&message, Message::ResumeConversation(Action::Prepared(..)));
        let followup = app.update(message);
        if preparing || app.worktree_selection.pending {
            drive_resume_task_for_test(app, followup);
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_resume_joins_recorded_project_channel_and_attention() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let project = ProjectId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "Recorded project".into(),
                directory.path().into(),
                Vec::new(),
                Vec::new(),
            ));
        let entry = saved(directory.path());
        let task = app.resume_saved_conversation(entry);
        drive_resume_task_for_test(&mut app, task);
        let card = app.workspace.focused().unwrap();
        let channel = app
            .durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&card))
            .unwrap();
        assert_eq!(channel.project_id, project);
        let channel = channel.id;
        app.observe_agent_activity_for_card(
            card,
            Some(crate::agent::Activity {
                provider: Provider::Claude,
                state: crate::agent::ActivityState::Waiting,
            }),
        );
        app.refresh_sidebar_channels();
        let row = app
            .sidebar_model
            .projects
            .iter()
            .find(|row| row.id == project)
            .unwrap()
            .channels
            .iter()
            .find(|row| row.id == channel)
            .unwrap();
        assert_eq!(row.activity, Some(crate::agent::ActivityState::Waiting));
        assert!(row.has_panes);
    }

    #[test]
    fn catalog_resume_joins_exact_linked_worktree_and_waiting_attention() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let linked = directory.path().join("linked");
        let nested = linked.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        let project = ProjectId::new();
        let worktree = crate::model::WorktreeId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "Recorded project".into(),
                directory.path().into(),
                vec![crate::sidebar::SidebarWorktree::new(
                    worktree,
                    linked.clone(),
                    Some("linked-branch".into()),
                    false,
                    false,
                    None,
                    None,
                    None,
                )],
                Vec::new(),
            ));
        let entry = saved(&nested);
        let launch = entry.session.launch.clone();
        let task = app.resume_saved_conversation(entry);
        drive_resume_task_for_test(&mut app, task);
        let card = app.workspace.focused().unwrap();
        assert_eq!(app.active_workspace.as_ref(), Some(&linked));
        assert!(app.durable.active_channel.is_none());
        assert!(app.durable.channels.is_empty());
        assert_eq!(app.durable.card_scopes.get(&card), Some(&linked));
        assert!(app.sidebar_state.worktree_has_panes(project, worktree));
        let CardKind::Chat(resumed) = &app.card_state(card).unwrap().kind else {
            panic!("native chat");
        };
        assert_eq!(resumed.launch, launch);
        app.observe_agent_activity_for_card(
            card,
            Some(crate::agent::Activity {
                provider: Provider::Claude,
                state: crate::agent::ActivityState::Waiting,
            }),
        );
        assert_eq!(
            app.sidebar_state.agent_activity(project, worktree),
            Some(crate::agent::ActivityState::Waiting)
        );
    }

    #[test]
    fn catalog_model_updates_saved_display_metadata_without_rewriting_issued_launch() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        let entry = saved(directory.path());
        let key = chat_ui::closing::draft_key(&entry.session).unwrap();
        let issued = entry.session.clone();
        app.resume_indexed(Progress::Saved {
            entries: vec![entry.clone()],
            seeds: Vec::new().into(),
            error: None,
        });
        app.resume_indexed(Progress::Found {
            conversations: Vec::new(),
            titles: Vec::new(),
            models: vec![(entry.card, "catalog-model".into())],
            read: 1,
        });
        assert_eq!(app.durable.conversation_models[&key], "catalog-model");
        let Source::Saved(saved) = &app.resume_ui.rows[0].source else {
            panic!("saved descriptor");
        };
        assert_eq!(saved.session, issued);
    }

    #[test]
    fn catalog_resume_falls_back_to_the_current_worktree_without_changing_launch_cwd() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let current = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        let project = ProjectId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "Current project".into(),
                current.path().into(),
                Vec::new(),
                Vec::new(),
            ));
        app.active_workspace = Some(current.path().into());
        let entry = saved(foreign.path());
        let launch = entry.session.launch.clone();
        let task = app.resume_saved_conversation(entry);
        drive_resume_task_for_test(&mut app, task);
        let card = app.workspace.focused().unwrap();
        assert_eq!(
            app.durable.card_scopes.get(&card).map(PathBuf::as_path),
            Some(current.path())
        );
        let CardKind::Chat(resumed) = &app.card_state(card).unwrap().kind else {
            panic!("native chat");
        };
        assert_eq!(resumed.launch, launch);
    }

    fn resume_selection_receipt(task: Task<Message>) -> (Message, Vec<Message>) {
        use iced::futures::StreamExt;
        let mut stream = iced::runtime::task::into_stream(task).expect("selection produces a receipt");
        let executor = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let mut selection = None;
        let mut lifecycle = Vec::new();
        while let Some(action) = executor.block_on(stream.next()) {
            match action {
                iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                    if matches!(&message, Message::WorktreeSelectionPrepared { .. }) && selection.is_none() {
                        selection = Some(message);
                    } else { lifecycle.push(message); }
                }
                // This scope/lease fixture does not assert focus. Normal
                // close tasks can emit Widget operations before the receipt.
                _ => {}
            }
        }
        (selection.expect("exact production prepared-scope receipt"), lifecycle)
    }

    #[test]
    fn catalog_resume_waits_for_real_physical_scope_receipts_and_the_modal_barrier() {
        for in_channel in [false, true] {
            let old_scope = tempfile::tempdir().unwrap();
            let destination = tempfile::tempdir().unwrap();
            let lease_runtime = tempfile::tempdir().unwrap();
            let mut app = super::super::tests::context_inspector_test_app(false);
            let project = ProjectId::new();
            let channel = crate::model::ChannelId::new();
            app.sidebar_model.projects.push(crate::sidebar::SidebarProject::new(
                project, crate::repository::RepositoryId::new(), "Recorded project".into(),
                destination.path().into(), Vec::new(), Vec::new()));
            app.known_worktree_roots.extend([old_scope.path().to_owned(), destination.path().to_owned()]);
            app.active_workspace = Some(old_scope.path().to_owned());
            app.worktree_protections.install_prepared(Some(
                crate::worktree_cleanup::fixture_protection(lease_runtime.path(), old_scope.path(), false).unwrap()));
            let canonicalize_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            app.force_local_selection_with_counter_for_test(Arc::clone(&canonicalize_calls));
            app.force_selection_guard_runtime_for_test(lease_runtime.path().to_owned());
            let entry = saved(destination.path());
            let conversation = entry.session.launch.session_id.clone();
            let launch = entry.session.launch.clone();
            let key = chat_ui::closing::draft_key(&entry.session).unwrap();
            let durable = Arc::make_mut(&mut app.durable);
            durable.conversation_drafts.insert(key.clone(), "Keep my resumed prompt".into());
            durable.conversation_models.insert(key.clone(), "recorded-before-first-turn".into());
            if in_channel {
                durable.channels.push(crate::model::ProjectChannel { id: channel, project_id: project,
                    title: "Original channel".into(), title_source: Default::default(), ordinal: 0,
                    shared: false, cards: Vec::new(), focused: None });
                durable.conversation_channels.insert(key.clone(), channel);
            }
            let task = app.resume_saved_conversation(entry);
            assert_eq!(app.active_workspace.as_deref(), Some(old_scope.path()));
            assert!(app.durable.active_channel.is_none());
            assert!(app.durable.cards.iter().all(|card| !matches!(&card.kind,
                CardKind::Chat(session) if session.launch.session_id == conversation)), "resume insertion must not race lease preparation");
            let (prepared, lifecycle) = resume_selection_receipt(task);
            for message in lifecycle { drop(app.update(message)); }
            assert!(matches!(&prepared, Message::WorktreeSelectionPrepared { .. }), "production preparation receipt");
            assert!(canonicalize_calls.load(std::sync::atomic::Ordering::Relaxed) > 0);
            assert!(matches!(crate::worktree_cleanup::fixture_protection(lease_runtime.path(), destination.path(), true),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock), "receipt owns the actual filesystem lease before exposure");
            app.settings_reset_confirm = true;
            drop(app.finish_modal_update(Task::none()));
            drop(app.update(prepared));
            assert_eq!(app.deferred_navigation.len(), 1);
            assert_eq!(app.active_workspace.as_deref(), Some(old_scope.path()));
            assert!(app.durable.cards.iter().all(|card| !matches!(&card.kind,
                CardKind::Chat(session) if session.launch.session_id == conversation)));
            let ready = app.update(Message::CancelResetSettings);
            let (receipt, lifecycle) = resume_selection_receipt(ready);
            drop(app.update(receipt));
            for message in lifecycle { drop(app.update(message)); }
            assert_eq!(app.active_workspace.as_deref(), Some(destination.path()));
            assert_eq!(app.durable.active_channel, in_channel.then_some(channel));
            assert!(app.deferred_navigation.is_empty());
            let card = app.durable.cards.iter().find(|card| matches!(&card.kind,
                CardKind::Chat(session) if session.launch.session_id == conversation)).expect("resumed conversation");
            let card_id = card.id;
            let CardKind::Chat(session) = &card.kind else { unreachable!() };
            assert_eq!(session.launch, launch);
            if in_channel {
                assert!(app.durable.channels.iter().find(|saved| saved.id == channel).unwrap().cards.contains(&card_id));
            } else {
                assert_eq!(app.durable.card_scopes.get(&card_id).map(PathBuf::as_path), Some(destination.path()));
            }
            assert_eq!(app.durable.conversation_drafts[&key], "Keep my resumed prompt");
            assert_eq!(app.durable.conversation_models[&key], "recorded-before-first-turn");
            assert!(matches!(crate::worktree_cleanup::fixture_protection(lease_runtime.path(), destination.path(), true),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock), "visible destination retains the installed lease");
        }
    }

    fn saved(directory: &Path) -> SavedConversation {
        let mut session = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            directory.into(),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
        // This fixture intentionally exercises recorded metadata reuse, without
        // claiming live account or physical home authority.
        let variable = crate::account::home_variable(session.launch.provider).unwrap();
        session.launch.recipe.environment.insert(variable.into(), "/fixture/resume-account".into());
        session.launch.recipe.unset_environment.remove(variable);
        for variable in crate::account::credential_overrides(session.launch.provider) {
            session.launch.recipe.environment.remove(*variable);
            session.launch.recipe.unset_environment.insert((*variable).into());
        }
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
    fn a_known_closed_automation_report_cannot_become_a_new_provider_conversation() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let entry = saved(directory.path());
        let original = entry.session.clone();
        let id = crate::model::AgentId::from_uuid(entry.card.as_uuid());
        Arc::make_mut(&mut app.durable)
            .automation_archives
            .insert(entry.card, id);
        let cards = app.durable.cards.clone();
        let order = app.workspace.order().collect::<Vec<_>>();
        let task = app.resume_saved_conversation(entry);
        assert!(iced::runtime::task::into_stream(task).is_none());
        assert_eq!(app.durable.cards, cards);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        assert!(app.resume_ui.error.as_deref().unwrap().contains("registry"));
        assert!(app.cards.values().all(|runtime| !matches!(runtime,
            CardRuntime::Chat(chat) if chat.session.id == original.id)));
    }


    // Joint fixture: requires the real protected-selection provider plus the
    // ArchiveReport feature and modal adapters. No provider/daemon task runs.
    fn report_physical_fixture(
        destination: &Path,
        old_scope: &Path,
        runtime: &Path,
        channel: bool,
    ) -> (Cagents, SavedConversation, crate::automations::ArchiveReport, Arc<std::sync::atomic::AtomicUsize>) {
        let (mut app, old_card, owner, archive) =
            crate::app::chat_ui::qualified_closed_console_archive_at_root_for_test(destination);
        let CardKind::Chat(original) = app.card_state(old_card).unwrap().kind.clone() else { unreachable!() };
        let entry=SavedConversation{card:old_card,title:"Exact saved report".into(),session:original.clone(),open:false,updated_at_unix_ms:2};
        app.workspace.request_close(old_card).unwrap();
        drop(app.commit_card_removal(old_card,None).unwrap());
        app.workspace_model.views[0].panes.clear();
        app.workspace_model.views[0].retired.push(old_card);
        app.workspace_model.views[0].revision+=1;
        let mut scope=app.workspace_model.views[0].scope.clone();
        if channel {
            let id=ChannelId::new();
            let project=app.sidebar_model.projects[0].id;
            Arc::make_mut(&mut app.durable).channels.push(crate::model::ProjectChannel{
                id,project_id:project,title:"Report's original channel".into(),title_source:Default::default(),
                ordinal:0,shared:true,cards:Vec::new(),focused:None,
            });
            app.workspace_model.channels.push(crate::workspace_model::ModelChannel{
                id,repository:destination.into(),title:"Report's original channel".into(),title_source:Default::default(),ordinal:1,
            });
            scope=crate::agent_execution::Placement::Channel(id);
            app.workspace_model.views[0].scope=scope.clone();
        }
        let reference=chat::ArchiveReportReference{format:1,registry:app.workspace_model.identity,agent:owner,source_operation:uuid::Uuid::new_v4()};
        let mut issued=original.clone();issued.archive_report=Some(reference.clone());
        let report=crate::automations::ArchiveReport{
            reference,scope,repository:destination.into(),source_revision:1,
            presentation_scope:(!channel).then(||destination.to_owned()),
            session_digest:crate::automations::ArchiveReport::session_digest(&issued).unwrap(),archive:(*archive).clone(),
        };
        app.active_workspace=Some(old_scope.into());
        Arc::make_mut(&mut app.durable).active_channel=None;
        app.known_worktree_roots.extend([old_scope.to_owned(),destination.to_owned()]);
        app.worktree_protections.install_prepared(Some(
            crate::worktree_cleanup::fixture_protection(runtime,old_scope,false).unwrap()));
        let calls=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        app.force_local_selection_with_counter_for_test(Arc::clone(&calls));
        app.force_selection_guard_runtime_for_test(runtime.to_owned());
        (app,entry,report,calls)
    }

    fn report_scope_receipt(task:Task<Message>)->(Message,Vec<Message>) {
        let mut stream=iced::runtime::task::into_stream(task).expect("real preparation must emit a receipt");
        let executor=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        executor.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(10),async {
                let mut receipt=None;let mut lifecycle=Vec::new();
                while let Some(action)=stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(message))=action {
                        if matches!(&message,Message::WorktreeSelectionPrepared{..}) && receipt.is_none() {receipt=Some(message);}
                        else {lifecycle.push(message);}
                    }
                    // Focus Widget operations do not carry a navigation receipt.
                }
                (receipt.expect("actual worker WorktreeSelectionPrepared"),lifecycle)
            }).await.expect("bounded physical selection preparation")
        })
    }

    fn start_report_scope_receipt(app:&mut Cagents,entry:SavedConversation,report:crate::automations::ArchiveReport)->Message {
        use cosmic::app::Application;
        app.resume_ui.open=true;
        drop(app.finish_modal_update(Task::none()));
        drop(app.resume_saved_conversation(entry.clone())); // Drop the remote lookup, inject only its private typed reply.
        let nonce=app.resume_ui.report_query.as_ref().unwrap().nonce;
        let generation=app.resume_ui.generation;
        let task=app.update(Message::ResumeConversation(Action::ReportSource(generation,nonce,entry,Arc::new(Ok(Some(report))))));
        assert!(!app.resume_ui.open,"the exact submitting picker hands off before guard preparation");
        let (receipt,lifecycle)=report_scope_receipt(task);
        for message in lifecycle {drop(app.update(message));}
        receipt
    }

    fn report_presentations(app:&Cagents)->Vec<CardId> {
        app.durable.cards.iter().filter_map(|card|{
            (card.existence.is_open() && matches!(&card.kind,CardKind::Chat(session) if session.archive_report.is_some())).then_some(card.id)
        }).collect()
    }

    fn assert_report_only_insert(app:&Cagents,entry:&SavedConversation,report:&crate::automations::ArchiveReport) {
        let cards=report_presentations(app);assert_eq!(cards.len(),1);let card=cards[0];
        assert_ne!(card,entry.card,"permanent old-card tombstone must not be reused");
        assert!(!app.card_state(entry.card).unwrap().existence.is_open());
        let CardKind::Chat(session)=&app.card_state(card).unwrap().kind else {unreachable!()};
        assert_eq!(session.archive_report.as_ref(),Some(&report.reference));
        assert_eq!(session.id,entry.session.id);assert_eq!(session.proof,entry.session.proof);
        assert!(!app.chat_has_live_handle(card));
        assert!(app.pending_archive_read_for_test(card).is_some(),"only the private archive-body read phase may remain pending");
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        let changes=app.pending_shared_view_changes_for_test();
        assert!(changes.iter().any(|(scope,change)|scope==&report.scope && matches!(change,
            crate::workspace_model::views::Change::InsertArchiveReport{reference,pane,..}
            if reference==&report.reference && pane.id==card)),"publish only the original qualified report membership");
    }

    #[test]
    fn readonly_report_waits_for_real_scope_guard_and_upper_modal_before_exact_once_insertion() {
        use cosmic::app::Application;
        for channel in [false,true] {
            let old=tempfile::tempdir().unwrap();let destination=tempfile::tempdir().unwrap();let runtime=tempfile::tempdir().unwrap();
            let (mut app,entry,report,calls)=report_physical_fixture(destination.path(),old.path(),runtime.path(),channel);
            let receipt=start_report_scope_receipt(&mut app,entry.clone(),report.clone());
            assert_eq!(app.active_workspace.as_deref(),Some(old.path()));assert!(report_presentations(&app).is_empty());
            assert!(calls.load(std::sync::atomic::Ordering::Relaxed)>0,"actual physical preparation ran");
            assert!(matches!(crate::worktree_cleanup::fixture_protection(runtime.path(),destination.path(),true),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock));
            app.settings_reset_confirm=true;drop(app.finish_modal_update(Task::none()));
            drop(app.update(receipt));
            assert_eq!(app.deferred_navigation.len(),1);assert_eq!(app.active_workspace.as_deref(),Some(old.path()));
            assert!(report_presentations(&app).is_empty());
            let ready=app.update(Message::CancelResetSettings);
            let (receipt,lifecycle)=report_scope_receipt(ready);let repeated=receipt.clone();
            drop(app.update(receipt));for message in lifecycle {drop(app.update(message));}
            assert_report_only_insert(&app,&entry,&report);
            assert_eq!(app.active_workspace.as_deref(),Some(destination.path()));
            match &report.scope {
                crate::agent_execution::Placement::Channel(id)=>assert_eq!(app.durable.active_channel,Some(*id)),
                _=>assert!(app.durable.active_channel.is_none()),
            }
            drop(app.update(repeated));assert_report_only_insert(&app,&entry,&report);
            assert!(matches!(crate::worktree_cleanup::fixture_protection(runtime.path(),destination.path(),true),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock),"committed scope retains its real guard");
        }
    }

    #[test]
    fn readonly_report_prepared_update_retains_only_current_receipt_and_releases_it_once() {
        use cosmic::app::Application;
        let old=tempfile::tempdir().unwrap();let destination=tempfile::tempdir().unwrap();let runtime=tempfile::tempdir().unwrap();
        let (mut app,entry,report,_)=report_physical_fixture(destination.path(),old.path(),runtime.path(),false);
        let receipt=start_report_scope_receipt(&mut app,entry.clone(),report.clone());
        app.update_prepared=true;drop(app.update(receipt));
        for _ in 0..3 {
            drop(app.update(Message::ResumeConversation(Action::Indexed(app.resume_ui.generation,Progress::Counted(0)))));
            assert_eq!(app.deferred_navigation.len(),1);
            assert_eq!(app.active_workspace.as_deref(),Some(old.path()));assert!(report_presentations(&app).is_empty());
        }
        app.update_prepared=false;
        let (receipt,lifecycle)=report_scope_receipt(app.release_deferred_navigation_if_unblocked());
        let duplicate=receipt.clone();drop(app.update(receipt));for message in lifecycle {drop(app.update(message));}
        assert_report_only_insert(&app,&entry,&report);drop(app.update(duplicate));
        assert_report_only_insert(&app,&entry,&report);
    }

    #[test]
    fn readonly_report_stale_original_filing_or_nonce_retires_held_guard_without_scope_mutation() {
        use cosmic::app::Application;
        for scenario in 0..3 {
            let old=tempfile::tempdir().unwrap();let destination=tempfile::tempdir().unwrap();let runtime=tempfile::tempdir().unwrap();
            let (mut app,entry,report,_)=report_physical_fixture(destination.path(),old.path(),runtime.path(),scenario==2);
            let receipt=start_report_scope_receipt(&mut app,entry,report);
            let nonce=uuid::Uuid::new_v4();
            match scenario {
                0=>app.resume_ui.report_restore_nonce=Some(nonce),
                1=>app.sidebar_model.projects.clear(),
                _=>app.workspace_model.channels[0].repository=old.path().to_owned(),
            }
            app.update_prepared=true;drop(app.update(receipt));
            assert!(app.deferred_navigation.is_empty(),"stale receipt must retire instead of being deferred forever");
            assert_eq!(app.active_workspace.as_deref(),Some(old.path()));assert!(report_presentations(&app).is_empty());
            assert!(app.durable.active_channel.is_none());assert!(app.terminal_starts.lock().unwrap().is_empty());
            if scenario==0 {assert_eq!(app.resume_ui.report_restore_nonce,Some(nonce),"old abandonment cannot cancel a new intent");}
            drop(crate::worktree_cleanup::fixture_protection(runtime.path(),destination.path(),true).expect("stale receipt released the exact held lease"));
        }
    }

    #[test]
    fn readonly_report_real_guard_failure_never_changes_scope_or_inserts_membership() {
        use cosmic::app::Application;
        let old=tempfile::tempdir().unwrap();let destination=tempfile::tempdir().unwrap();let runtime=tempfile::tempdir().unwrap();
        let (mut app,entry,report,_)=report_physical_fixture(destination.path(),old.path(),runtime.path(),false);
        let invalid_runtime=runtime.path().join("not-a-directory");std::fs::write(&invalid_runtime,b"owned fixture file").unwrap();
        app.force_selection_guard_runtime_for_test(invalid_runtime);
        let receipt=start_report_scope_receipt(&mut app,entry,report);
        drop(app.update(receipt));
        assert!(app.resume_ui.report_restore_nonce.is_none());assert!(app.deferred_navigation.is_empty());
        assert_eq!(app.active_workspace.as_deref(),Some(old.path()));assert!(report_presentations(&app).is_empty());
        assert!(app.terminal_starts.lock().unwrap().is_empty());
    }

    #[test]
    fn a_private_report_restore_keeps_the_closed_card_and_native_owner_while_opening_a_new_read_only_pane()
     {
        let (mut app, old_card, agent, archive) =
            crate::app::chat_ui::qualified_closed_console_archive_for_test();
        let CardKind::Chat(original) = app.card_state(old_card).unwrap().kind.clone() else {
            unreachable!()
        };
        let entry = SavedConversation {
            card: old_card,
            title: "Retained report".into(),
            session: original.clone(),
            open: false,
            updated_at_unix_ms: 2,
        };
        app.workspace.request_close(old_card).unwrap();
        drop(app.commit_card_removal(old_card, None).unwrap());
        let closed = app.card_state(old_card).unwrap().clone();
        assert!(!closed.existence.is_open());
        app.workspace_model.views[0].panes.clear();
        app.workspace_model.views[0].retired.push(old_card);
        app.workspace_model.views[0].revision += 1;
        let reference = chat::ArchiveReportReference {
            format: 1,
            registry: app.workspace_model.identity,
            agent,
            source_operation: uuid::Uuid::new_v4(),
        };
        let mut issued = original.clone();
        issued.archive_report = Some(reference.clone());
        let report = crate::automations::ArchiveReport {
            reference,
            scope: app.workspace_model.views[0].scope.clone(),
            repository: archive.agent.spec.target.repository.clone(),
            source_revision: 1,
            presentation_scope: Some(original.launch.recipe.working_directory.clone()),
            session_digest: crate::automations::ArchiveReport::session_digest(&issued).unwrap(),
            archive: (*archive).clone(),
        };
        drop(app.resume_saved_conversation(entry.clone()));
        let query = app.resume_ui.report_query.as_ref().unwrap();
        let nonce = query.nonce;
        let generation = app.resume_ui.generation;
        drop(app.update_resume(Action::ReportSource(
            generation,
            nonce,
            entry,
            Arc::new(Ok(Some(report.clone()))),
        )));
        let card = app.workspace.focused().unwrap();
        assert_ne!(card, old_card);
        assert_eq!(
            app.card_state(old_card).unwrap(),
            &closed,
            "the permanent closed identity remains closed"
        );
        assert_eq!(
            app.card_state(card).unwrap().kind,
            CardKind::Chat(issued.clone())
        );
        assert_eq!(app.durable.automation_archives.get(&card), Some(&agent));
        assert_eq!(app.archived_pane_readiness(card), Ok(Some(false)));
        let target = app
            .pending_archive_read_for_test(card)
            .expect("private saved-body read, never provider start");
        drop(app.handle_chat_ui(chat_ui::Action::ArchiveReport(target, Arc::new(Ok(report)))));
        assert_eq!(app.archived_pane_readiness(card), Ok(Some(true)));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("saved report runtime")
        };
        assert!(!app.chat_has_live_handle(card));
        assert_eq!(runtime.session.id, original.id);
        assert_eq!(runtime.session.proof, original.proof);
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        assert!(!app.chat_start_pending(card));
    }

    #[test]
    fn a_report_continuation_cannot_insert_into_the_old_scope_or_outlive_its_request() {
        for scenario in 0..6 {
            let (mut app, card, agent, archive) =
                crate::app::chat_ui::qualified_closed_console_archive_for_test();
            let CardKind::Chat(session) = app.card_state(card).unwrap().kind.clone() else {
                unreachable!()
            };
            let reference = chat::ArchiveReportReference {
                format: 1,
                registry: app.workspace_model.identity,
                agent,
                source_operation: uuid::Uuid::new_v4(),
            };
            let mut issued = session.clone();
            issued.archive_report = Some(reference.clone());
            let report = crate::automations::ArchiveReport {
                reference,
                scope: app.workspace_model.views[0].scope.clone(),
                repository: archive.agent.spec.target.repository.clone(),
                source_revision: 1,
                presentation_scope: Some(session.launch.recipe.working_directory.clone()),
                session_digest: crate::automations::ArchiveReport::session_digest(&issued).unwrap(),
                archive: (*archive).clone(),
            };
            let nonce = uuid::Uuid::new_v4();
            app.resume_ui.report_restore_nonce = Some(nonce);
            let restore = ReportRestore {
                nonce,
                generation: app.resume_ui.generation,
                registry: app.workspace_model.identity,
                picker_origin: false,
                entry: SavedConversation {
                    card,
                    title: "Report".into(),
                    session,
                    open: false,
                    updated_at_unix_ms: 1,
                },
                report,
            };
            match scenario {
                0 => Arc::make_mut(&mut app.durable).active_channel = Some(ChannelId::new()),
                1 => app.resume_ui.report_restore_nonce = Some(uuid::Uuid::new_v4()),
                2 => app.workspace_model.identity = uuid::Uuid::new_v4(),
                3 => app.update_prepared = true,
                4 => app.sidebar_model.projects.clear(),
                _ => app.workspace_model.projects.clear(),
            }
            let cards = app.durable.cards.clone();
            let order = app.workspace.order().collect::<Vec<_>>();
            drop(app.finish_archive_report_restore(restore));
            assert_eq!(app.durable.cards, cards);
            assert_eq!(app.workspace.order().collect::<Vec<_>>(), order);
            assert!(app.terminal_starts.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn private_report_lookup_cancellation_registry_change_and_old_same_source_results_do_not_insert()
     {
        for scenario in 0..3 {
            let directory = tempfile::tempdir().unwrap();
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.workspace_model.identity = uuid::Uuid::new_v4();
            let mut entry = saved(directory.path());
            entry.session.archive_report = Some(chat::ArchiveReportReference {
                format: 1,
                registry: app.workspace_model.identity,
                agent: crate::model::AgentId::new(),
                source_operation: uuid::Uuid::new_v4(),
            });
            drop(app.resume_saved_conversation(entry.clone()));
            let old_nonce = app.resume_ui.report_query.as_ref().unwrap().nonce;
            let generation = app.resume_ui.generation;
            match scenario {
                0 => {
                    app.close_resume();
                }
                1 => {
                    app.workspace_model.identity = uuid::Uuid::new_v4();
                }
                _ => {
                    app.resume_ui.loading = false;
                    drop(app.lookup_archive_report_source(entry.clone()));
                }
            }
            drop(app.update_resume(Action::ReportSource(
                generation,
                old_nonce,
                entry,
                Arc::new(Ok(None)),
            )));
            assert!(app.durable.cards.is_empty());
            assert!(app.workspace.order().next().is_none());
            assert!(app.cards.is_empty());
            assert!(app.terminal_starts.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn report_source_completion_retires_without_navigation_under_another_modal_or_preparation() {
        for scenario in 0..4 {
            let directory = tempfile::tempdir().unwrap();
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.workspace_model.identity = uuid::Uuid::new_v4();
            let mut entry = saved(directory.path());
            entry.session.archive_report = Some(chat::ArchiveReportReference {
                format: 1,
                registry: app.workspace_model.identity,
                agent: crate::model::AgentId::new(),
                source_operation: uuid::Uuid::new_v4(),
            });
            app.resume_ui.open = scenario != 3;
            drop(app.resume_saved_conversation(entry.clone()));
            let nonce = app.resume_ui.report_query.as_ref().unwrap().nonce;
            let generation = app.resume_ui.generation;
            match scenario {
                0 => app.settings_reset_confirm = true,
                1 => app.update_prepared = true,
                2 => app.resume_ui.open = false,
                _ => app.settings_reset_confirm = true,
            }
            // Register the actual visible owner before the held completion.
            // Lower Resume retention never grants navigation ownership.
            drop(app.finish_modal_update(Task::none()));
            // An Err is deliberately supplied: even presenting a completion's
            // error must not move focus through the modal that took ownership.
            let task = app.update(Message::ResumeConversation(Action::ReportSource(
                generation,
                nonce,
                entry,
                Arc::new(Err("must not surface behind upper modal".into())),
            )));
            if app.settings_reset_confirm {
                let cancel = widget::Id::new("dialog.reset-settings.cancel");
                let preserve = Task::batch([
                    iced::widget::operation::focus(cancel.clone()).map(cosmic::Action::App),
                    task,
                ]);
                assert_eq!(
                    super::super::tests::focused_element(app.dialog().unwrap(), preserve),
                    Some(cancel)
                );
            } else {
                // Modal reconciliation may emit guarded Widget operations;
                // state assertions below prove the completion did not navigate.
                drop(task);
            }
            assert_eq!(app.resume_ui.deferred_report.is_some(), scenario != 2);
            assert_eq!(app.resume_ui.report_query.is_some(), scenario != 2);
            assert!(!app.resume_ui.loading);
            assert!(app.resume_ui.error.is_none());
            assert!(app.durable.cards.is_empty());
            assert!(app.terminal_starts.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn late_archive_classification_fences_the_actual_prepared_resume_callback() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut entry = saved(directory.path());
        entry.session.native_continuity = chat::NativeContinuity::Durable {
            provider_version: "0.162.0".into(),
        };
        let id = crate::model::AgentId::from_uuid(entry.card.as_uuid());
        app.resume_ui.open = true;
        app.resume_ui.loading = true;
        app.resume_ui.generation = 7;
        // This model edge arrives while a previous prepare RPC is held.
        Arc::make_mut(&mut app.durable)
            .automation_archives
            .insert(entry.card, id);
        let returned = entry.session.clone();
        let task = app.update_resume(Action::Prepared(7, entry, Ok(returned)));
        assert!(iced::runtime::task::into_stream(task).is_none());
        assert!(!app.resume_ui.loading);
        assert!(app.durable.cards.is_empty());
        assert!(app.workspace.order().next().is_none());
        assert!(app.cards.is_empty());
        assert!(app.terminal_starts.lock().unwrap().is_empty());
        assert!(
            app.resume_ui
                .error
                .as_deref()
                .unwrap()
                .contains("run history")
        );
    }

    #[test]
    fn closed_conversation_resumes_exact_identity_in_its_original_project() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let entry = saved(directory.path());
        let original = entry.session.clone();
        let prepare = app.resume_saved_conversation(entry);
        let prepared = crate::app::tests::task_app_message(prepare);
        drop(app.update(prepared));
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
            model: None,
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
                    models: Vec::new(),
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
                models: Vec::new(),
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
                    serde_json::json!({"type":"user","sessionId":id,"cwd":root.path(),"message":{"content":format!("Prompt {age}")}})
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
            ..
        } = &progress[2]
        else {
            panic!("the newest conversations come next")
        };
        assert_eq!(*read, FIRST_BATCH);
        assert_eq!(conversations.len(), FIRST_BATCH - 2);
        assert_eq!(conversations[0].id, ids[0]);
        assert_eq!(conversations[1].id, ids[2]);
        assert_eq!(titles, &[(entry.card, "Prompt 1".to_owned())]);
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
