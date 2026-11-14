use super::*;
use crate::agent::Provider;
use crate::chat;
use crate::persistence::SavedConversation;
use cosmic::iced::futures::StreamExt;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
mod catalog;
mod scan;

/// Conversations read before the first found ones are listed, so the newest
/// appear at once, and between later listings.
const FIRST_BATCH: usize = 64;
const BATCH: usize = 512;
/// Rows listed at a time; more are listed on request.
const PAGE: usize = 100;
/// A row holds two lines: what the conversation is, and where and when.
const ROW_HEIGHT: f32 = 54.0;
const ROW_SPACING: f32 = 2.0;
/// The picker's widest, and the width at which a preview of the chosen
/// conversation fits beside the list.
const DIALOG_WIDTH: f32 = 1080.0;
const PREVIEW_WIDTH: f32 = 360.0;
const PREVIEW_FITS: f32 = 820.0;
/// Rows Page Up and Page Down move the selection by.
const PAGE_STEP: usize = 8;
/// A conversation written to this recently may still be running elsewhere.
const RECENT_MS: i64 = 2 * 60 * 1000;

#[derive(Default)]
pub(super) struct State {
    pub open: bool,
    generation: u64,
    pub(super) query: String,
    rows: Vec<Row>,
    /// Indices of the rows the query and scope admit, best first.
    visible: Vec<usize>,
    /// How many rows the query admits at each reach, for the scope choice.
    counts: [usize; 3],
    /// The sessions found conversations resume with, and their accounts' names.
    seeds: Arc<[chat::Session]>,
    seed_accounts: Vec<String>,
    /// Where the picker was opened from, and how far beyond it it lists.
    pub(super) scope: Option<Scope>,
    /// Directories conversations ran in that no longer exist.
    missing: HashSet<PathBuf>,
    /// The untouched draft `/resume` was asked from, which the resumed
    /// conversation takes the place of.
    pub(super) replace: Option<CardId>,
    /// The conversation `/resume` was typed in. It is not listed: the reader
    /// is already in it. Its transcript, once found, is folded into this
    /// saved entry before the entry is dropped, so it is not listed either.
    pub(super) asked_from: Option<CardId>,
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
    /// The reader's Tab traversal setting while the picker owns Tab, which
    /// chooses where to look and must leave the keyboard in the search.
    restore_keyboard_nav: Option<bool>,
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
    /// Where it ran, as the reader knows it: its project, then its branch
    /// or worktree.
    place: String,
    /// What its transcript says, once read.
    summary: Option<Arc<scan::Summary>>,
    bytes: Option<u64>,
    /// What a query is matched against, lowercased once: the title and
    /// place first, each on its own so that where a word falls in one does
    /// not depend on the other, then everything else a reader may remember
    /// it by.
    title_key: String,
    place_key: String,
    content_key: String,
    source: Source,
}

enum Source {
    /// A conversation cagents saved, with the launch it was saved with.
    Saved(Box<SavedConversation>),
    /// A conversation found in a provider's history.
    Found(catalog::Found),
}

impl Row {
    fn saved(entry: SavedConversation, account: String, place: String) -> Self {
        let launch = &entry.session.launch;
        let mut row = Self {
            card: entry.card,
            title: entry.title.clone(),
            provider: launch.provider,
            directory: launch.recipe.working_directory.clone(),
            account,
            updated_at_unix_ms: entry.updated_at_unix_ms,
            open: entry.open,
            place,
            summary: None,
            bytes: None,
            title_key: String::new(),
            place_key: String::new(),
            content_key: String::new(),
            source: Source::Saved(Box::new(entry)),
        };
        row.index();
        row
    }

    fn found(found: catalog::Found, provider: Provider, account: String, place: String) -> Self {
        let mut row = Self {
            card: CardId::new(),
            title: found.title.clone(),
            provider,
            directory: found.directory().to_path_buf(),
            account,
            updated_at_unix_ms: found.updated_at_unix_ms,
            open: false,
            place,
            summary: Some(found.summary.clone()),
            bytes: Some(found.bytes),
            title_key: String::new(),
            place_key: String::new(),
            content_key: String::new(),
            source: Source::Found(found),
        };
        row.index();
        row
    }

    fn index(&mut self) {
        self.title_key = self.title.to_lowercase();
        self.place_key = self.place.to_lowercase();
        let summary = self.summary.as_deref();
        self.content_key = [
            self.provider.label(),
            summary
                .and_then(|summary| summary.model.as_deref())
                .unwrap_or_default(),
            summary
                .and_then(|summary| summary.branch.as_deref())
                .unwrap_or_default(),
            &self.account,
            &self.directory.to_string_lossy(),
            summary.map_or("", |summary| summary.asked.as_str()),
            summary
                .and_then(|summary| summary.last_reply.as_deref())
                .unwrap_or_default(),
        ]
        .join("\n")
        .to_lowercase();
    }

    fn model(&self) -> Option<&str> {
        self.summary.as_deref()?.model.as_deref()
    }
}

/// How far beyond where it was opened the picker lists conversations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    Worktree,
    Project,
    Everywhere,
}

impl Reach {
    const ALL: [Self; 3] = [Self::Worktree, Self::Project, Self::Everywhere];

    fn index(self) -> usize {
        self as usize
    }
}

/// Where the picker was opened from: a project, and the worktree in it.
#[derive(Debug, Clone)]
pub(super) struct Scope {
    pub(super) reach: Reach,
    pub(super) project: String,
    /// The project's repository and worktrees, as configured and as their
    /// providers resolve them.
    pub(super) roots: Vec<PathBuf>,
    pub(super) worktree: Option<WorktreeScope>,
}

#[derive(Debug, Clone)]
pub(super) struct WorktreeScope {
    pub(super) label: String,
    pub(super) roots: Vec<PathBuf>,
    /// Worktrees kept inside this one's directory, which are not it.
    pub(super) nested: Vec<PathBuf>,
}

impl Scope {
    fn admits(&self, reach: Reach, directory: &Path) -> bool {
        match reach {
            Reach::Everywhere => true,
            Reach::Project => self.roots.iter().any(|root| directory.starts_with(root)),
            Reach::Worktree => self.worktree.as_ref().is_some_and(|worktree| {
                worktree
                    .roots
                    .iter()
                    .any(|root| directory.starts_with(root))
                    && !worktree
                        .nested
                        .iter()
                        .any(|nested| directory.starts_with(nested))
            }),
        }
    }

    /// The reaches the reader can choose between.
    fn reaches(&self) -> impl Iterator<Item = Reach> + '_ {
        Reach::ALL
            .into_iter()
            .filter(|reach| *reach != Reach::Worktree || self.worktree.is_some())
    }

    fn label(&self, reach: Reach) -> String {
        match reach {
            Reach::Worktree => self
                .worktree
                .as_ref()
                .map_or_else(String::new, |worktree| worktree.label.clone()),
            Reach::Project => self.project.clone(),
            Reach::Everywhere => "Everywhere".into(),
        }
    }
}

/// A directory's own name, or the whole path when it has none.
fn folder_name(directory: &Path) -> String {
    directory.file_name().map_or_else(
        || directory.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Each path, and the path its provider would have recorded when that
/// differs, such as through a symbolic link.
fn with_resolved(paths: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .flat_map(|path| {
            let resolved = path
                .canonicalize()
                .ok()
                .filter(|resolved| *resolved != path);
            std::iter::once(path).chain(resolved)
        })
        .collect()
}

#[derive(Debug, Clone)]
pub enum Action {
    Open,
    Close,
    Query(String),
    /// Lists conversations from this far beyond where the picker was opened.
    Reach(Reach),
    Indexed(u64, Progress),
    Resume(CardId),
    More,
    Submit,
    /// The pointer entered the row at this position among the matches, which
    /// becomes the selection.
    Point(usize, CardId),
    /// The service's answer to preparing a resume. `Ok(None)` means it could
    /// not be reached to prepare an unstarted successor, which then resumes as
    /// a conversation of its own, as it would without one.
    Prepared(
        u64,
        SavedConversation,
        Result<Option<chat::Session>, String>,
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
    /// Directories listed conversations ran in that no longer exist.
    Missing(Vec<PathBuf>),
    /// Nothing more is coming.
    Finished,
    /// Conversations found among the next transcripts, and the transcripts
    /// of saved conversations found among them, with better titles for
    /// those that have one.
    Found {
        conversations: Vec<catalog::Found>,
        titles: Vec<(CardId, String)>,
        matched: Vec<(CardId, catalog::Found)>,
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
        && conversation_home(a.launch.provider, &a.launch.recipe)
            == conversation_home(b.launch.provider, &b.launch.recipe)
}

/// What makes two listings the same conversation: its provider, its ID, and
/// the account home its history is kept in.
type Identity = (Provider, Option<String>, Option<PathBuf>);

fn identity(provider: Provider, id: Option<&str>, recipe: &LaunchRecipe) -> Identity {
    (
        provider,
        id.map(str::to_owned),
        conversation_home(provider, recipe),
    )
}

/// The directory a launch keeps its provider's history in, however the
/// launch names it: a recipe that exports the provider's default home and one
/// that leaves the CLI to resolve that default keep the same history.
fn conversation_home(provider: Provider, recipe: &LaunchRecipe) -> Option<PathBuf> {
    let root = provider.session_root(recipe).or_else(|| {
        let variable = crate::account::home_variable(provider)?;
        recipe
            .environment
            .get(variable)
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
    })?;
    Some(root.canonicalize().unwrap_or(root))
}

/// Which of `directories` no longer exist, each looked at once.
fn missing_directories<'a>(
    directories: impl IntoIterator<Item = &'a Path>,
    checked: &mut HashSet<PathBuf>,
) -> Vec<PathBuf> {
    directories
        .into_iter()
        .filter(|directory| checked.insert(directory.to_path_buf()))
        .filter(|directory| {
            matches!(std::fs::metadata(directory), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
        })
        .map(Path::to_path_buf)
        .collect()
}

/// Lists saved conversations, then every conversation the seeds' providers
/// keep, newest first, until `send` reports that nobody is listening or
/// `cancel` is set. What was read is saved to `store` for the next run.
fn index(
    saved: Result<Vec<SavedConversation>, String>,
    seeds: Vec<chat::Session>,
    cache: &catalog::Cache,
    store: Option<&Path>,
    cancel: &AtomicBool,
    mut send: impl FnMut(Progress) -> bool,
) {
    if let Some(store) = store {
        cache.restore(store);
    }
    let mut candidates = Vec::new();
    let complete = list(saved, seeds, cache, cancel, &mut candidates, &mut send);
    if let Some(store) = store {
        // Only a reading of every transcript knows which are gone.
        let live = complete.then(|| {
            candidates
                .iter()
                .map(|candidate| candidate.path.as_path())
                .collect::<HashSet<_>>()
        });
        if let Err(error) = cache.save(store, live.as_ref()) {
            tracing::warn!(%error, path = %store.display(), "Could not save the conversation catalog");
        }
    }
}

/// [`index`]'s listing, which answers whether it read every transcript.
fn list(
    saved: Result<Vec<SavedConversation>, String>,
    seeds: Vec<chat::Session>,
    cache: &catalog::Cache,
    cancel: &AtomicBool,
    candidates: &mut Vec<catalog::Candidate>,
    send: &mut impl FnMut(Progress) -> bool,
) -> bool {
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
    let mut checked = HashSet::new();
    let missing = missing_directories(
        entries
            .iter()
            .map(|entry| entry.session.launch.recipe.working_directory.as_path()),
        &mut checked,
    );
    let seeds: Arc<[chat::Session]> = seeds.into();
    if !send(Progress::Saved {
        entries,
        seeds: seeds.clone(),
        error,
    }) || !missing.is_empty() && !send(Progress::Missing(missing))
    {
        return false;
    }
    *candidates = catalog::candidates(&seeds, cancel);
    if !send(Progress::Counted(candidates.len())) {
        return false;
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
    while read < candidates.len() {
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        let batch = &candidates[read..candidates
            .len()
            .min(read + if read == 0 { FIRST_BATCH } else { BATCH })];
        read += batch.len();
        let mut conversations = Vec::new();
        let mut titles = Vec::new();
        let mut matched = Vec::new();
        for mut found in catalog::find_all(cache, &seeds, batch, workers, cancel) {
            if let Some(name) = names
                .get(found.seed)
                .and_then(|names| names.get(found.id()))
            {
                found.title = name.clone();
                found.titled = true;
                found.named = true;
            }
            let provider = seeds[found.seed].launch.provider;
            match seen.entry((
                provider,
                Some(found.id().to_owned()),
                homes[found.seed].clone(),
            )) {
                std::collections::hash_map::Entry::Vacant(vacant) => {
                    vacant.insert(None);
                    conversations.push(found);
                }
                // A saved conversation goes by the name its provider gives
                // it, and one cagents never named by its first message,
                // unless the reader renamed it here.
                std::collections::hash_map::Entry::Occupied(mut occupied) => {
                    if let Some((card, untitled)) = occupied.get_mut().take() {
                        if found.named || found.titled && untitled {
                            titles.push((card, found.title.clone()));
                        }
                        matched.push((card, found));
                    }
                }
            }
        }
        let missing = missing_directories(
            conversations.iter().map(catalog::Found::directory),
            &mut checked,
        );
        if !missing.is_empty() && !send(Progress::Missing(missing)) {
            return false;
        }
        if !send(Progress::Found {
            conversations,
            titles,
            matched,
            read,
        }) {
            return false;
        }
    }
    !cancel.load(Ordering::Relaxed)
}

/// How long ago `at` was, as a reader scanning a list wants it: minutes and
/// hours today, days this month, then the date.
fn age(now_ms: i64, at_ms: i64) -> String {
    let seconds = (now_ms - at_ms).max(0) / 1000;
    match seconds {
        0..60 => "Just now".into(),
        60..3_600 => format!("{} min ago", seconds / 60),
        3_600..86_400 => format!("{} hr ago", seconds / 3_600),
        86_400..172_800 => "Yesterday".into(),
        172_800..2_592_000 => format!("{} days ago", seconds / 86_400),
        _ => {
            const MONTHS: [&str; 12] = [
                "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
            ];
            let (Some(then), Some(today)) = (
                chat::local_time(at_ms / 1000),
                chat::local_time(now_ms / 1000),
            ) else {
                return String::new();
            };
            if then.year == today.year {
                format!("{} {}", MONTHS[then.month], then.day)
            } else {
                format!("{} {}, {}", MONTHS[then.month], then.day, then.year + 1900)
            }
        }
    }
}

/// A transcript's size, rounded for reading at a glance.
fn size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn messages(count: u32) -> String {
    match count {
        1 => "1 message".into(),
        count => format!("{count} messages"),
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
        let keyboard::Key::Named(key) = key else {
            return None;
        };
        use keyboard::key::Named;
        if *key == Named::Tab && !modifiers.control() && !modifiers.alt() && !modifiers.logo() {
            return Some(self.step_resume_reach(modifiers.shift()));
        }
        if !modifiers.is_empty() {
            return None;
        }
        let state = &mut self.resume_ui;
        let target = match key {
            Named::ArrowDown => state.selected + 1,
            Named::ArrowUp => state.selected.saturating_sub(1),
            Named::PageDown => state.selected + PAGE_STEP,
            Named::PageUp => state.selected.saturating_sub(PAGE_STEP),
            _ => return None,
        };
        // Moving past the last row listed lists more, as "Show more" does,
        // which the keyboard cannot reach while the picker owns Tab.
        if target >= state.limit {
            state.limit = (target + 1).next_multiple_of(PAGE);
        }
        state.selected = target.min(state.visible.len().min(state.limit).saturating_sub(1));
        Some(self.reveal_resume_selection())
    }

    /// Moves to the next reach, or the previous one, wrapping around.
    fn step_resume_reach(&mut self, backwards: bool) -> Task<Message> {
        let Some(scope) = &self.resume_ui.scope else {
            return Task::none();
        };
        let reaches = scope.reaches().collect::<Vec<_>>();
        let at = reaches
            .iter()
            .position(|reach| *reach == scope.reach)
            .unwrap_or(0);
        let next = if backwards {
            (at + reaches.len() - 1) % reaches.len()
        } else {
            (at + 1) % reaches.len()
        };
        self.update_resume(Action::Reach(reaches[next]))
    }

    fn reveal_resume_selection(&self) -> Task<Message> {
        iced::advanced::widget::operate(RevealSelection {
            selected: self.resume_ui.selected,
        })
        .map(cosmic::Action::App)
    }

    /// Where `directory` is, as the picker narrows to it: its project, and
    /// the worktree within it. `None` outside every open project.
    fn resume_scope(&self, directory: &Path) -> Option<Scope> {
        let (project_name, roots) = self.project_roots(directory)?;
        let project = self.resume_project(directory)?;
        let worktree = project
            .worktrees
            .iter()
            .filter(|worktree| directory.starts_with(&worktree.path))
            .max_by_key(|worktree| worktree.path.components().count())
            .map(|worktree| WorktreeScope {
                label: worktree
                    .branch
                    .clone()
                    .unwrap_or_else(|| folder_name(&worktree.path)),
                roots: with_resolved([worktree.path.clone()]),
                nested: with_resolved(
                    project
                        .worktrees
                        .iter()
                        .filter(|other| {
                            other.path != worktree.path && other.path.starts_with(&worktree.path)
                        })
                        .map(|other| other.path.clone())
                        .chain([worktree.path.join(".worktrees")]),
                ),
            });
        Some(Scope {
            reach: Reach::Project,
            project: project_name,
            roots: with_resolved(roots),
            worktree,
        })
    }

    /// Where the reader is working: the selected channel's directory, else
    /// the selected worktree, else the focused pane's.
    fn resume_scope_here(&self) -> Option<Scope> {
        let directory = self
            .durable
            .active_channel
            .and_then(|channel| self.channel_directory(channel))
            .or_else(|| self.active_workspace.clone())
            .or_else(|| {
                let card = self.card_state(self.workspace.focused()?)?;
                match &card.kind {
                    CardKind::Chat(session) => {
                        Some(session.launch.recipe.working_directory.clone())
                    }
                    _ => self.card_worktree(card).map(Path::to_path_buf),
                }
            })?;
        self.resume_scope(&directory)
    }

    /// Where a conversation ran, as the reader knows it: the project, then
    /// the branch it was on or the worktree it ran in; or its folder's name
    /// outside every open project.
    fn resume_place(&self, directory: &Path, branch: Option<&str>) -> String {
        let Some(project) = self.resume_project(directory) else {
            return folder_name(directory);
        };
        let worktree = project
            .worktrees
            .iter()
            .filter(|worktree| directory.starts_with(&worktree.path))
            .max_by_key(|worktree| worktree.path.components().count());
        // A worktree that was removed is no longer listed, but its
        // directory still says which one it was.
        let removed = directory
            .strip_prefix(worktree.map_or(&project.repository, |worktree| &worktree.path))
            .ok()
            .and_then(|rest| {
                let mut parts = rest.components();
                (parts.next()?.as_os_str() == ".worktrees")
                    .then(|| parts.next())
                    .flatten()
                    .map(|part| part.as_os_str().to_string_lossy().into_owned())
            });
        let name = &project.name;
        let label = branch
            .map(str::to_owned)
            .or(removed)
            .or_else(|| worktree.and_then(|worktree| worktree.branch.clone()))
            .or_else(|| worktree.map(|worktree| folder_name(&worktree.path)));
        match label {
            Some(label) => format!("{name} › {label}"),
            None => name.clone(),
        }
    }

    /// The open project `directory` belongs to.
    fn resume_project(&self, directory: &Path) -> Option<&sidebar::SidebarProject> {
        let (id, _, _) = self.project_for_directory(directory)?;
        self.sidebar_model
            .projects
            .iter()
            .find(|project| project.id == id)
    }

    /// Opens the picker from `card`'s composer, narrowed to its project and
    /// searching for `query`, unless something else holds the window.
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
        let directory = runtime.session.launch.recipe.working_directory.clone();
        let scope = self.resume_scope(&directory).or_else(|| {
            // A folder outside every project is a scope of its own.
            Some(Scope {
                reach: Reach::Project,
                project: folder_name(&directory),
                roots: with_resolved([directory.clone()]),
                worktree: None,
            })
        });
        let task = self.open_resume(scope)?;
        self.resume_ui.asked_from = Some(card);
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
        // Tab chooses where to look. COSMIC's own traversal would also move
        // the keyboard out of the search, onto a scope button that Enter
        // then presses instead of resuming.
        if self.resume_ui.restore_keyboard_nav.is_none() {
            self.resume_ui.restore_keyboard_nav = Some(self.core.keyboard_nav());
            self.core.set_keyboard_nav(false);
        }
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
        state.missing.clear();
        state.counts = [0; 3];
        state.replace = None;
        state.asked_from = None;
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
        // Tests never read or write the reader's own catalog.
        let catalog_store = (!cfg!(test))
            .then(crate::instance::project_dirs)
            .flatten()
            .map(|dirs| dirs.cache_dir().join("resume-catalog"));
        let (sender, receiver) = async_channel::bounded(2);
        let spawned = std::thread::Builder::new()
            .name("resume-index".into())
            .spawn(move || {
                let mut saved = store
                    .map_or(Ok(Vec::new()), |store| store.saved_conversations())
                    .map_err(|error| error.to_string());
                if let Ok(entries) = &mut saved {
                    for card in &current {
                        if let CardKind::Chat(session) = &card.kind
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
                                title: card.title.clone(),
                                session: session.clone(),
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
                index(
                    saved,
                    seeds,
                    &cache,
                    catalog_store.as_deref(),
                    &cancel,
                    |progress| sender.send_blocking(progress).is_ok(),
                );
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
            Action::Open => {
                let scope = self.resume_scope_here();
                self.open_resume(scope).unwrap_or_else(Task::none)
            }
            Action::Close => {
                self.close_resume();
                // Whatever was being prepared is abandoned with the picker.
                self.resume_ui.loading = false;
                self.resume_ui.generation = self.resume_ui.generation.wrapping_add(1);
                self.workspace
                    .focused()
                    .map_or_else(Task::none, |card| self.focus_pane_keyboard(card))
            }
            Action::Query(query) => {
                self.resume_ui.query = query;
                self.refilter_resume();
                self.reveal_resume_selection()
            }
            Action::Reach(reach) => {
                if let Some(scope) = &mut self.resume_ui.scope {
                    scope.reach = reach;
                }
                self.refilter_resume();
                // The search keeps the keyboard wherever the choice was made.
                Task::batch([
                    self.reveal_resume_selection(),
                    iced::widget::operation::focus(focus_id()).map(cosmic::Action::App),
                ])
            }
            Action::Point(index, card) => {
                // Only the row drawn there: the matches may have changed since.
                let state = &mut self.resume_ui;
                if state
                    .visible
                    .get(index)
                    .is_some_and(|&row| state.rows[row].card == card)
                {
                    state.selected = index;
                }
                Task::none()
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
                    Ok(Some(session)) => {
                        if !prepared_resume_matches(&entry.session, &session) {
                            self.resume_error("The service did not prepare this exact saved conversation. Its original identity was retained.");
                            return Task::none();
                        }
                        entry.session = session;
                        if self.existing_conversation_card(&entry.session).is_some() {
                            return self.resume_saved_conversation(entry);
                        }
                        self.insert_resumed_conversation(entry, true)
                    }
                    Ok(None) => self.insert_resumed_conversation(entry, false),
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
                let asked_from = self.resume_ui.asked_from;
                let rows = entries
                    .into_iter()
                    .filter(|entry| Some(entry.card) != asked_from)
                    .map(|entry| {
                        let account = self.resume_account_label(&entry.session);
                        let place =
                            self.resume_place(&entry.session.launch.recipe.working_directory, None);
                        Row::saved(entry, account, place)
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
            Progress::Missing(directories) => {
                self.resume_ui.missing.extend(directories);
                return;
            }
            Progress::Found {
                conversations,
                titles,
                matched,
                read,
            } => {
                let user_titles = &self.chrome_ui.user_titles;
                let titles = titles
                    .into_iter()
                    .filter(|(card, _)| !user_titles.contains_key(card))
                    .collect::<HashMap<_, _>>();
                // Saved conversations learn what their transcripts say.
                let matched = matched
                    .into_iter()
                    .map(|(card, found)| {
                        let place =
                            self.resume_place(found.directory(), found.summary.branch.as_deref());
                        (card, (found, place))
                    })
                    .collect::<HashMap<_, _>>();
                for row in &mut self.resume_ui.rows {
                    let Some((found, place)) = matched.get(&row.card) else {
                        continue;
                    };
                    if let Some(title) = titles.get(&row.card) {
                        row.title.clone_from(title);
                    }
                    row.summary = Some(found.summary.clone());
                    row.bytes = Some(found.bytes);
                    row.place.clone_from(place);
                    row.index();
                }
                let mut rows = Vec::with_capacity(conversations.len());
                for found in conversations {
                    let Some(seed) = self.resume_ui.seeds.get(found.seed) else {
                        continue;
                    };
                    let provider = seed.launch.provider;
                    let account = self.resume_ui.seed_accounts[found.seed].clone();
                    let place =
                        self.resume_place(found.directory(), found.summary.branch.as_deref());
                    rows.push(Row::found(found, provider, account, place));
                }
                let state = &mut self.resume_ui;
                state.rows.extend(rows);
                state.indexing = state
                    .indexing
                    .map(|(_, total)| (read, total))
                    .filter(|(read, total)| read < total);
            }
        }
        let state = &mut self.resume_ui;
        // A row the reader moved to stays selected wherever more rows land;
        // otherwise the best match stays first.
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
        let (visible, counts) = resume_matches(&state.rows, &state.query, state.scope.as_ref());
        state.visible = visible;
        state.counts = counts;
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
        let (visible, counts) = resume_matches(&state.rows, &state.query, state.scope.as_ref());
        state.visible = visible;
        state.counts = counts;
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
        if let Some(task) = self.defer_retiring_conversation(&entry, false) {
            return task;
        }
        let session = &entry.session;
        if session.native_continuity.retains_provider() || unstarted_successor(session) {
            let generation = self.resume_ui.generation;
            self.resume_ui.loading = true;
            let unstarted = unstarted_successor(session);
            return cosmic::task::future(async move {
                let source = entry.session.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let socket = crate::terminal::ipc::daemon_socket_path()
                        .map_err(|error| error.to_string())?;
                    let mut client = match crate::control::ControlClient::connect(&socket) {
                        Ok(client) => client,
                        // Nothing runs to vouch for it, or to refuse it.
                        Err(crate::control::ControlError::Connect(_)) if unstarted => {
                            return Ok(None);
                        }
                        Err(error) => return Err(error.to_string()),
                    };
                    client
                        .prepare_chat_resume(source)
                        .map(Some)
                        .map_err(|error| error.to_string())
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()));
                Message::ResumeConversation(Action::Prepared(generation, entry, result))
            });
        }
        self.insert_resumed_conversation(entry, false)
    }

    fn insert_resumed_conversation(
        &mut self,
        entry: SavedConversation,
        prepared: bool,
    ) -> Task<Message> {
        if let Some(task) = self.defer_retiring_conversation(&entry, prepared) {
            return task;
        }
        let user_title = self.chrome_ui.user_titles.get(&entry.card).cloned();
        let mut session = entry.session;
        if !session.launch.recipe.working_directory.is_dir()
            && !session.launch.needs_read_only_history()
        {
            let error = format!(
                "This conversation ran in {}, which no longer exists. Restore that folder to resume it.",
                session.launch.recipe.working_directory.display()
            );
            self.resume_error(&error);
            return Task::none();
        }
        // Preparation may either retain a live actor or allocate its exact
        // successor after proved retirement. Both answers must survive intact;
        // a second locally allocated ID loses the daemon's issued lineage.
        if !prepared {
            session.reset_owned_codex_listener();
            session.id = TerminalId::new();
            session.proof = None;
            session.terminal = None;
        }
        let replace = self.resume_ui.replace;
        self.close_resume();
        // Read before forgetting the closed card, which records where it was.
        let home = self.closed_card_home(entry.card);
        self.recently_closed
            .retain(|(card, _)| card.id != entry.card);
        let scope = self.enter_resumed_conversation_home(home, &session);
        // The draft `/resume` was asked from gives its place to the
        // conversation, where both belong to the same strip.
        let replace = replace.filter(|card| {
            self.chat_is_unused(*card) && self.workspace.order().any(|id| id == *card)
        });
        let insert = self.insert_resumed_chat_card(session, replace, user_title);
        // Manual names were installed before insertion. Otherwise publish
        // the saved automatic title instead of the provider's generic name.
        if let Some(card) = self.durable.focused
            && self.apply_reported_title(card, entry.title)
        {
            self.persist();
        }
        let discard = replace.map_or_else(Task::none, |card| self.close_card_unasked(card));
        Task::batch([scope, insert, discard])
    }

    fn defer_retiring_conversation(
        &mut self,
        entry: &SavedConversation,
        prepared: bool,
    ) -> Option<Task<Message>> {
        let home @ channel_ui::ClosedCardHome::Pending(_) = self.closed_card_home(entry.card)?
        else {
            return None;
        };
        let task = self.defer_channel_reopen(
            home,
            channel_ui::Reopen::Conversation {
                entry: entry.clone(),
                prepared,
                generation: self.resume_ui.generation,
                replace: self.resume_ui.replace,
            },
        )?;
        self.resume_ui.loading = true;
        Some(task)
    }

    pub(super) fn finish_deferred_conversation(
        &mut self,
        entry: SavedConversation,
        prepared: bool,
        generation: u64,
        replace: Option<CardId>,
        same_home: bool,
    ) -> Task<Message> {
        if generation != self.resume_ui.generation {
            return Task::none();
        }
        self.resume_ui.loading = false;
        if !same_home {
            self.resume_error(
                "The channel changed while reopening. Select the conversation again.",
            );
            return Task::none();
        }
        self.resume_ui.replace = replace;
        if prepared && self.existing_conversation_card(&entry.session).is_none() {
            self.insert_resumed_conversation(entry, true)
        } else {
            self.resume_saved_conversation(entry)
        }
    }

    pub(super) fn fail_deferred_conversation(&mut self, generation: u64, error: &str) -> bool {
        if generation == self.resume_ui.generation {
            self.resume_ui.loading = false;
            self.resume_error(error);
            return true;
        }
        false
    }

    /// Enters the place a resumed conversation belongs: the channel it was
    /// closed in, brought back if it retired with its last card; else the
    /// channel `/resume` was asked from, when the conversation runs in that
    /// channel's directory; else the conversation's own worktree.
    fn enter_resumed_conversation_home(
        &mut self,
        home: Option<channel_ui::ClosedCardHome>,
        session: &chat::Session,
    ) -> Task<Message> {
        let directory = &session.launch.recipe.working_directory;
        if let Some(home) = home
            && let Some(scope) = self.enter_closed_card_home(home)
        {
            return scope;
        }
        if home.is_none()
            && let Some(channel) = self.durable.active_channel
            && !self.channel_ui.is_deleting(channel)
            && self.channel_directory(channel).as_ref() == Some(directory)
        {
            return self
                .enter_closed_card_home(channel_ui::ClosedCardHome::Channel(channel))
                .unwrap_or_else(Task::none);
        }
        self.select_worktree(directory.clone(), false)
    }

    /// Closes the picker and stops indexing. Focus is left to whatever the
    /// caller shows next.
    fn close_resume(&mut self) {
        if let Some(restore) = self.resume_ui.restore_keyboard_nav.take() {
            self.core.set_keyboard_nav(restore);
        }
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

    /// Takes over Tab again from a popup that closed under the picker and
    /// recorded its suppressed traversal: `restore` is the reader's choice.
    pub(super) fn adopt_resume_keyboard_nav(&mut self, restore: bool) -> bool {
        if !self.resume_ui.open {
            return false;
        }
        self.resume_ui.restore_keyboard_nav = Some(restore);
        self.core.set_keyboard_nav(false);
        true
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
        let body = crate::ui_controls::responsive(move |size| {
            dialog_ui::surface(self.resume_body(size.width >= PREVIEW_FITS), DIALOG_WIDTH)
        });
        Some(dialog_ui::overlay(
            widget::container(body)
                .width(Length::Fill)
                .max_width(DIALOG_WIDTH)
                .height(Length::Fill)
                .max_height(680.0)
                .into(),
            Some(Message::ResumeConversation(Action::Close)),
            1.0,
        ))
    }

    /// The picker's contents: the search, where it looks, the matches and,
    /// when there is room, the chosen one's preview beside them.
    fn resume_body(&self, wide: bool) -> Element<'_, Message> {
        let state = &self.resume_ui;
        let now = chat::now_ms().map_or(0, |now| now as i64);
        let progress = match state.indexing {
            Some((_, 0)) => "Looking for conversations…".to_owned(),
            Some((read, total)) => format!("Reading {read} of {total} transcripts…"),
            None => match state.visible.len() {
                1 => "1 conversation".to_owned(),
                count => format!("{count} conversations"),
            },
        };
        let header = widget::row::with_children(vec![
            design::title("Resume conversation")
                .width(Length::Fill)
                .into(),
            design::caption(progress).into(),
        ])
        .align_y(Alignment::Center)
        .spacing(space::S);
        let search = crate::ui_controls::field::search(
            "Search titles, prompts, replies, branches…",
            &state.query,
        )
        .id(focus_id())
        .on_input(|query| Message::ResumeConversation(Action::Query(query)))
        .on_submit(|_| Message::ResumeConversation(Action::Submit));
        let mut content = widget::column::with_children(vec![header.into(), search.into()])
            .spacing(space::M)
            .height(Length::Fill);
        if let Some(scope) = &state.scope {
            content = content.push(resume_reaches(scope, &state.counts));
        }
        let list = self.resume_list(now);
        let chosen = state
            .visible
            .get(state.selected)
            .map(|&index| &state.rows[index]);
        content = content.push(if wide {
            let preview: Element<'_, Message> = match chosen {
                Some(row) => self.resume_preview(row, now),
                None => iced::widget::Space::new().into(),
            };
            Element::from(
                widget::row::with_children(vec![
                    list,
                    widget::container(
                        widget::scrollable(
                            widget::container(preview)
                                .padding(design::with_vertical_rail(Padding::ZERO))
                                .width(Length::Fill),
                        )
                        .direction(
                            iced::widget::scrollable::Direction::Vertical(design::scrollbar()),
                        ),
                    )
                    .width(Length::Fixed(PREVIEW_WIDTH))
                    .height(Length::Fill)
                    .into(),
                ])
                .spacing(space::L)
                .height(Length::Fill),
            )
        } else {
            // Without room beside the list, the last exchange shows under it.
            let mut column = widget::column::with_children(vec![list]).spacing(space::S);
            if let Some(exchange) = chosen.and_then(resume_exchange) {
                column = column.push(exchange);
            }
            column.height(Length::Fill).into()
        });
        let status: Element<'_, Message> = if state.loading {
            design::caption("Opening the conversation…").into()
        } else if let Some(error) = &state.error {
            design::caption(error.as_str())
                .class(cosmic::theme::Text::Custom(design::warning_text))
                .into()
        } else {
            design::caption("↑↓ choose · Enter resume · Tab where to look · Esc close").into()
        };
        content
            .push(
                widget::row::with_children(vec![
                    widget::container(status).width(Length::Fill).into(),
                    crate::ui_controls::button::secondary("Cancel")
                        .id(widget::Id::new("conversation-resume.cancel"))
                        .on_press(Message::ResumeConversation(Action::Close))
                        .into(),
                ])
                .align_y(Alignment::Center)
                .spacing(space::M),
            )
            .into()
    }

    fn resume_list(&self, now: i64) -> Element<'_, Message> {
        let state = &self.resume_ui;
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        for (index, row) in state
            .visible
            .iter()
            .take(state.limit)
            .map(|&index| &state.rows[index])
            .enumerate()
        {
            let line = |text: String| {
                design::caption(text)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
            };
            let mut title: Vec<Element<'_, Message>> = vec![
                design::heading(&row.title)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            ];
            if row.open {
                title.push(design::caption("Open").into());
            } else if state.missing.contains(&row.directory) {
                title.push(
                    design::caption("Folder missing")
                        .class(cosmic::theme::Text::Custom(design::warning_text))
                        .into(),
                );
            }
            title.push(design::caption(age(now, row.updated_at_unix_ms)).into());
            let contents = widget::column::with_children(vec![
                widget::row::with_children(title)
                    .align_y(Alignment::Center)
                    .spacing(space::S)
                    .into(),
                line(resume_details(row)).into(),
            ])
            .spacing(space::XXS);
            let button = widget::button::custom(contents)
                .name(format!("Resume {}", row.title))
                .width(Length::Fill)
                .height(Length::Fixed(ROW_HEIGHT))
                .class(cosmic::theme::Button::ListItem([8.0; 4]))
                .selected(index == state.selected)
                .on_press_maybe(
                    (!state.loading)
                        .then_some(Message::ResumeConversation(Action::Resume(row.card))),
                );
            // Pointing at a row selects it, so the preview always describes
            // the highlighted row. A row does not hear the pointer leave the
            // list, so a pointer-only preview could outlive the pointer.
            rows.push(
                iced::widget::mouse_area(button)
                    .on_enter(Message::ResumeConversation(Action::Point(index, row.card)))
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
            rows.push(self.resume_empty());
        }
        widget::scrollable(
            widget::column::with_children(rows)
                .spacing(ROW_SPACING)
                .padding(design::with_vertical_rail(Padding::ZERO)),
        )
        .direction(iced::widget::scrollable::Direction::Vertical(
            design::scrollbar(),
        ))
        .id(widget::Id::new("conversation-resume.results"))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// Why nothing is listed, and where else to look.
    fn resume_empty(&self) -> Element<'_, Message> {
        let state = &self.resume_ui;
        // The nearest place, in the order Tab visits them, that has matches.
        let elsewhere = state.scope.as_ref().and_then(|scope| {
            let order: Vec<Reach> = scope.reaches().collect();
            let at = order.iter().position(|reach| *reach == scope.reach)?;
            (1..order.len()).find_map(|presses| {
                let reach = order[(at + presses) % order.len()];
                let count = state.counts[reach.index()];
                (count > 0).then(|| {
                    let place = match reach {
                        Reach::Everywhere => "other projects".to_owned(),
                        reach => scope.label(reach),
                    };
                    (place, count, presses)
                })
            })
        });
        let here = state.scope.as_ref().map(|scope| scope.label(scope.reach));
        let mut lines = vec![if state.rows.is_empty() && state.indexing.is_some() {
            "Looking for conversations…".to_owned()
        } else if state.rows.is_empty() {
            "No conversations yet. Conversations with Claude Code and Codex appear here once they have started.".to_owned()
        } else if !state.query.trim().is_empty() {
            format!("No conversations match “{}”.", state.query.trim())
        } else if let Some(here) = here {
            format!("No conversations in {here} yet.")
        } else {
            "No conversations yet.".to_owned()
        }];
        if let Some((place, count, presses)) = elsewhere {
            lines.push(format!(
                "{} in {place}. Press Tab{} to look there.",
                match count {
                    1 => "1 conversation".to_owned(),
                    count => format!("{count} conversations"),
                },
                if presses == 1 { "" } else { " twice" }
            ));
        }
        widget::column::with_children(
            lines
                .into_iter()
                .map(|line| design::body(line).into())
                .collect::<Vec<_>>(),
        )
        .spacing(space::XS)
        .padding(space::M)
        .into()
    }

    /// The chosen conversation: what it is, where and when it ran, and how
    /// it last went.
    fn resume_preview<'a>(&'a self, row: &'a Row, now: i64) -> Element<'a, Message> {
        let state = &self.resume_ui;
        let summary = row.summary.as_deref();
        let mut column = widget::column::with_children(vec![
            design::heading(&row.title)
                .ellipsize(iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(3),
                ))
                .into(),
        ])
        .spacing(space::XS)
        .width(Length::Fill);
        let mut identity = vec![row.provider.label().to_owned()];
        identity.extend(row.model().map(str::to_owned));
        if !row.account.is_empty() {
            identity.push(row.account.clone());
        }
        column = column.push(design::caption(identity.join(" · ")));
        column = column.push(design::caption(row.place.clone()));
        let when = age(now, row.updated_at_unix_ms);
        let when = match when.as_str() {
            "Just now" => "just now".to_owned(),
            "Yesterday" => "yesterday".to_owned(),
            _ => when,
        };
        let mut activity = vec![format!("Last active {when}")];
        if let Some(summary) = summary {
            activity.push(messages(summary.messages()));
        }
        activity.extend(row.bytes.map(size));
        column = column.push(design::caption(activity.join(" · ")));
        let warning = |text: String| {
            design::caption(text).class(cosmic::theme::Text::Custom(design::warning_text))
        };
        if row.open {
            column = column.push(design::caption("Already open. Resuming switches to it."));
        } else if state.missing.contains(&row.directory) {
            column = column.push(warning(if row.provider == Provider::Claude {
                "Its folder no longer exists, so it opens read-only. Restore the folder to continue it.".to_owned()
            } else {
                "Its folder no longer exists. Restore the folder to resume it.".to_owned()
            }));
        } else if now - row.updated_at_unix_ms < RECENT_MS {
            column = column.push(warning(
                "Active moments ago. It may still be running in another window or terminal."
                    .to_owned(),
            ));
        }
        let section = |label: &'static str, text: &'a str, lines: usize| {
            widget::column::with_children(vec![
                design::caption(label).into(),
                design::body(text)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(lines),
                    ))
                    .into(),
            ])
            .spacing(space::XXS)
        };
        match summary {
            Some(summary) => {
                let first = summary.first_prompt.as_deref();
                let last = summary.last_prompt.as_deref();
                if let Some(first) = first.filter(|first| Some(*first) != last) {
                    column = column.push(section("First asked", first, 3));
                }
                if let Some(last) = last {
                    column = column.push(section("Last asked", last, 4));
                }
                if let Some(reply) = summary.last_reply.as_deref() {
                    column = column.push(section("Last reply", reply, 8));
                }
            }
            None if state.indexing.is_some() => {
                column = column.push(design::caption("Reading its transcript…"));
            }
            None => {
                column = column.push(design::caption(
                    "Its transcript was not found, so only its saved details are shown.",
                ));
            }
        }
        column = column.push(design::caption(row.directory.display().to_string()));
        column.spacing(space::M).into()
    }
}

/// A row's second line: who answered, where, and how much was said.
fn resume_details(row: &Row) -> String {
    let mut details = vec![row.provider.label().to_owned()];
    details.extend(row.model().map(str::to_owned));
    details.push(row.place.clone());
    if let Some(summary) = &row.summary {
        details.push(messages(summary.messages()));
    }
    details.extend(row.bytes.map(size));
    details.join(" · ")
}

/// The last exchange, in a line each, for a picker too narrow for a preview.
fn resume_exchange(row: &Row) -> Option<Element<'_, Message>> {
    let summary = row.summary.as_deref()?;
    let line = |label: &str, text: &str| -> Element<'static, Message> {
        design::caption(format!("{label}: {text}"))
            .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
            .ellipsize(iced::widget::text::Ellipsize::End(
                EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into()
    };
    let lines = [
        summary
            .last_prompt
            .as_deref()
            .map(|text| line("Asked", text)),
        summary
            .last_reply
            .as_deref()
            .map(|text| line("Replied", text)),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<Element<'_, Message>>>();
    (!lines.is_empty()).then(|| {
        widget::column::with_children(lines)
            .spacing(space::XXS)
            .into()
    })
}

/// Where the picker looks, with how many conversations each place holds.
fn resume_reaches<'a>(scope: &Scope, counts: &[usize; 3]) -> Element<'a, Message> {
    crate::ui_controls::segmented_controls(
        scope
            .reaches()
            .map(|reach| {
                let label = format!("{} {}", scope.label(reach), counts[reach.index()]);
                // The sidebar's icons: a branch for a worktree, a folder for
                // a project.
                let icon = match reach {
                    Reach::Worktree => Some(crate::ui_controls::Icon::Branch),
                    Reach::Project => Some(crate::ui_controls::Icon::Folder),
                    Reach::Everywhere => None,
                };
                let mut contents: Vec<Element<'_, Message>> = Vec::new();
                if let Some(icon) = icon {
                    contents.push(crate::ui_controls::passive_icon(
                        icon,
                        design::COMPACT_ICON_SIZE + 2,
                    ));
                }
                contents.push(design::body(label).into());
                widget::button::custom(
                    widget::row::with_children(contents)
                        .spacing(space::XXS)
                        .align_y(Alignment::Center),
                )
                .name(format!("Show conversations from {}", scope.label(reach)))
                .padding([space::XXS, space::S])
                .class(crate::ui_controls::control_style(
                    reach == scope.reach,
                    None,
                ))
                .on_press(Message::ResumeConversation(Action::Reach(reach)))
                .into()
            })
            .collect(),
    )
}

/// A resume successor the daemon prepared but that never started. Resuming it
/// again through the daemon keeps the lineage, and the agent, it was prepared
/// to take over.
fn unstarted_successor(session: &chat::Session) -> bool {
    session.launch.provider == Provider::Claude
        && session.native_continuity == chat::NativeContinuity::ReconnectOnly
        && session.proof.is_none()
        && session.terminal.is_none()
        && session.launch.session_id.is_some()
}

/// Shape validation of the authenticated preparation reply. It grants no
/// provider ownership; Start/Attach still runs normal HOST admission.
fn prepared_resume_matches(source: &chat::Session, prepared: &chat::Session) -> bool {
    // An ended retained Claude actor resumes as a reconnect-only successor,
    // on the conversation the daemon proved it confirmed.
    if source == prepared || chat::is_retained_resume_successor(source, prepared) {
        return true;
    }
    if !source.has_valid_native_continuity()
        || source.launch.session_id.is_none()
        || prepared.id == source.id
    {
        return false;
    }
    let mut expected = source.clone();
    expected.reset_owned_codex_listener();
    expected.id = prepared.id;
    expected.proof = None;
    expected.terminal = None;
    &expected == prepared
}

/// Indices of the rows matching `query` within `scope`.
/// The rows `query` matches, best first, among those `scope` admits, and how
/// many it matches at each reach. Every word is looked for on its own: in
/// the title and place, where a loose match counts, or anywhere in what was
/// said, where it must appear whole.
fn resume_matches(rows: &[Row], query: &str, scope: Option<&Scope>) -> (Vec<usize>, [usize; 3]) {
    let query = query.trim().to_lowercase();
    let words = query.split_whitespace().collect::<Vec<_>>();
    let mut counts = [0; 3];
    let mut ranked = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| {
            let score = words.iter().try_fold(0, |total, word| {
                let named = [&row.title_key, &row.place_key]
                    .into_iter()
                    .filter_map(|key| crate::search_match::score(key, word))
                    .min();
                let score = match named {
                    // A loose title match is weaker than the word itself
                    // anywhere in the conversation.
                    Some(score) if score < 4_000 => score,
                    loose => {
                        if row.content_key.contains(word) {
                            3_999
                        } else {
                            loose? + 1_000
                        }
                    }
                };
                Some(total + score)
            })?;
            let Some(scope) = scope else {
                counts[Reach::Everywhere.index()] += 1;
                return Some((score, index));
            };
            for reach in Reach::ALL {
                if scope.admits(reach, &row.directory) {
                    counts[reach.index()] += 1;
                }
            }
            scope
                .admits(scope.reach, &row.directory)
                .then_some((score, index))
        })
        .collect::<Vec<_>>();
    // Stable, so equally good matches stay newest first.
    ranked.sort_by_key(|(score, _)| *score);
    (ranked.into_iter().map(|(_, index)| index).collect(), counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A conversation saved while cagents exported Claude's default home and
    /// the same conversation found under a launch that leaves the CLI to
    /// resolve it are one listing, kept in one history.
    #[test]
    fn exporting_the_default_home_or_not_lists_one_conversation() {
        let mut exported = LaunchRecipe::shell("/w".into()).unwrap();
        exported.environment.insert("HOME".into(), "/home/a".into());
        let mut resolved = exported.clone();
        exported
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "/home/a/.claude".into());
        resolved.environment.remove("CLAUDE_CONFIG_DIR");
        assert_eq!(
            identity(Provider::Claude, Some("id"), &exported),
            identity(Provider::Claude, Some("id"), &resolved)
        );
        let mut other = exported.clone();
        other
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), "/home/a/work".into());
        assert_ne!(
            identity(Provider::Claude, Some("id"), &other),
            identity(Provider::Claude, Some("id"), &resolved)
        );
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
        // A saved conversation that ran carries the proof of its last start.
        session.proof = Some(crate::model::HostSessionProof {
            host_session_id: "saved-owner".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "saved-incarnation".into(),
        });
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
    fn retained_claude_resume_waits_for_preparation_and_keeps_its_exact_allocation() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let mut entry = saved(directory.path());
        entry.session.native_continuity =
            chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        assert!(entry.session.has_valid_native_continuity());
        let source = entry.session.clone();
        // Do not execute this UI task: HOST preparation/retirement is covered
        // by the real-helper tests. This tests dispatch and reply admission.
        drop(app.resume_saved_conversation(entry.clone()));
        assert!(app.resume_ui.loading);
        assert!(app.durable.cards.is_empty());
        let mut prepared = source.clone();
        prepared.reset_owned_codex_listener();
        prepared.id = TerminalId::new();
        prepared.proof = None;
        prepared.terminal = None;
        // The daemon answers for an ended actor with its reconnect-only
        // successor, never another retained actor.
        prepared.native_continuity = chat::NativeContinuity::ReconnectOnly;
        let generation = app.resume_ui.generation;
        drop(app.update_resume(Action::Prepared(
            generation,
            entry,
            Ok(Some(prepared.clone())),
        )));
        let card = app.card_state(app.workspace.focused().unwrap()).unwrap();
        assert_eq!(card.kind, CardKind::Chat(prepared));
        assert_eq!(card.title, "Review peripheral Rust modules");
        assert!(!app.resume_ui.loading);
    }

    #[test]
    fn prepared_resume_transfers_a_manual_title_before_publication() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.settings_path = Some(directory.path().join("settings.toml"));
        app.settings_write_blocked = false;
        let mut entry = saved(directory.path());
        entry.title = "Parser recovery".into();
        app.chrome_ui
            .user_titles
            .insert(entry.card, entry.title.clone());
        entry.session.proof = Some(crate::model::HostSessionProof {
            host_session_id: "saved-owner".into(),
            boot_id: "fixture-boot".into(),
            host_incarnation: "saved-incarnation".into(),
        });
        entry.session.native_continuity =
            chat::NativeContinuity::RetainedClaudeActor { protocol: 2 };
        let mut prepared = entry.session.clone();
        prepared.reset_owned_codex_listener();
        prepared.id = TerminalId::new();
        prepared.proof = None;
        prepared.terminal = None;
        drop(app.resume_saved_conversation(entry.clone()));
        assert!(app.resume_ui.loading);
        let generation = app.resume_ui.generation;
        let before_settings = app.settings_save_generation;
        drop(app.update_resume(Action::Prepared(
            generation,
            entry,
            Ok(Some(prepared.clone())),
        )));

        let card = app
            .card_state(app.workspace.focused().unwrap())
            .unwrap()
            .clone();
        assert_eq!(card.kind, CardKind::Chat(prepared));
        assert_eq!(card.title, "Parser recovery");
        assert_eq!(app.chrome_ui.user_titles[&card.id], "Parser recovery");
        assert!(app.settings_save_generation > before_settings);
        assert!(!app.apply_reported_title(card.id, "Original provider prompt".into()));
        assert_eq!(app.card_state(card.id).unwrap().title, "Parser recovery");
        assert_eq!(
            app.capture_settings_save().unwrap().view.user_titles[&card.id],
            "Parser recovery"
        );

        // The saved list uses the resumed card's title, and later provider
        // indexing must not replace the manual name on that new identity.
        let current = app.card_state(card.id).unwrap();
        let CardKind::Chat(session) = &current.kind else {
            panic!("a resumed chat");
        };
        let saved = SavedConversation {
            card: card.id,
            title: current.title.clone(),
            session: session.clone(),
            open: false,
            updated_at_unix_ms: 2,
        };
        app.resume_ui.open = true;
        list(&mut app, generation, vec![saved]);
        drop(app.update_resume(Action::Indexed(
            generation,
            Progress::Found {
                conversations: Vec::new(),
                titles: vec![(card.id, "Original provider prompt".into())],
                matched: Vec::new(),
                read: 1,
            },
        )));
        let row = app
            .resume_ui
            .rows
            .iter()
            .find(|row| row.card == card.id)
            .unwrap();
        assert_eq!(row.title, "Parser recovery");
        assert!(matches!(&row.source, Source::Saved(saved) if saved.title == "Parser recovery"));
    }

    #[test]
    fn unstarted_successor_resumes_through_the_daemon_which_may_refuse_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        // A prepared successor the card closed before it ever started.
        let mut entry = saved(directory.path());
        entry.session.proof = None;
        assert!(unstarted_successor(&entry.session));
        let source = entry.session.clone();
        drop(app.resume_saved_conversation(entry.clone()));
        assert!(app.resume_ui.loading, "the daemon keeps its lineage");
        assert!(app.durable.cards.is_empty());
        let mut prepared = source.clone();
        prepared.id = TerminalId::new();
        let generation = app.resume_ui.generation;
        drop(app.update_resume(Action::Prepared(
            generation,
            entry.clone(),
            Ok(Some(prepared.clone())),
        )));
        let card = app.card_state(app.workspace.focused().unwrap()).unwrap();
        assert_eq!(card.kind, CardKind::Chat(prepared));

        // A refusal, such as another pane still writing to the conversation,
        // is shown and starts nothing.
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        drop(app.resume_saved_conversation(entry.clone()));
        let generation = app.resume_ui.generation;
        drop(app.update_resume(Action::Prepared(
            generation,
            entry.clone(),
            Err("Another pane may still be writing to this Claude conversation".into()),
        )));
        assert!(app.durable.cards.is_empty());
        assert!(app.resume_ui.error.is_some());

        // With no daemon to vouch for it or refuse it, it resumes on its own.
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.resume_saved_conversation(entry.clone()));
        let generation = app.resume_ui.generation;
        drop(app.update_resume(Action::Prepared(generation, entry, Ok(None))));
        let card = app.card_state(app.workspace.focused().unwrap()).unwrap();
        let CardKind::Chat(session) = &card.kind else {
            panic!("the conversation resumes as a chat");
        };
        assert_ne!(session.id, source.id);
        assert_eq!(session.launch, source.launch);
        assert!(app.resume_ui.error.is_none());
    }

    #[test]
    fn preparation_cannot_change_the_saved_thread_account_or_launch() {
        let directory = tempfile::tempdir().unwrap();
        let source = saved(directory.path()).session;
        let mut prepared = source.clone();
        prepared.id = TerminalId::new();
        prepared.proof = None;
        assert!(prepared_resume_matches(&source, &prepared));
        for mutation in 0..4 {
            let mut altered = prepared.clone();
            match mutation {
                0 => altered.launch.session_id = Some("another-thread".into()),
                1 => {
                    altered
                        .launch
                        .recipe
                        .environment
                        .insert("CLAUDE_CONFIG_DIR".into(), "/other-account".into());
                }
                2 => altered.launch.recipe.arguments.push("--different".into()),
                _ => altered.launch.provider = Provider::Codex,
            }
            assert!(!prepared_resume_matches(&source, &altered));
        }
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
                "fixture".into(),
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
        // Rows are 54 px apart by 2: the fourth ends at 222.
        assert_eq!(resume_selection_offset(3, 160.0, 1040.0, 0.0), 62.0);
        assert_eq!(resume_selection_offset(3, 420.0, 1040.0, 0.0), 0.0);
        assert_eq!(resume_selection_offset(0, 160.0, 1040.0, 248.0), 0.0);
        assert_eq!(resume_selection_offset(9, 160.0, 1040.0, 248.0), 398.0);
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
            Ok(Some(entry.session.clone())),
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
            Ok(Some(entry.session)),
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
        let found = |updated_at_unix_ms| found(directory.path(), "Newer", updated_at_unix_ms);
        let arrive = |app: &mut Cagents, updated_at_unix_ms| {
            let _ = app.update_resume(Action::Indexed(
                generation,
                Progress::Found {
                    conversations: vec![found(updated_at_unix_ms)],
                    titles: Vec::new(),
                    matched: Vec::new(),
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
                matched: Vec::new(),
                read: 1,
            },
        ));
        assert_eq!(selected(&app), older.card);
        assert!(
            app.resume_ui.selected >= PAGE && app.resume_ui.selected < app.resume_ui.limit,
            "a page of newer rows leaves it listed"
        );
    }

    fn press(app: &mut Cagents, key: keyboard::key::Named) {
        let _ = app.resume_key_event(&keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
    }

    /// "Show more" is a button the keyboard cannot reach while the picker
    /// owns Tab, so moving past the last row listed lists more.
    #[test]
    fn the_keyboard_reaches_past_the_first_page() {
        use keyboard::key::Named;
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.limit = PAGE;
        let generation = app.resume_ui.generation;
        list(
            &mut app,
            generation,
            (0..PAGE + 20).map(|_| saved(directory.path())).collect(),
        );
        assert_eq!(app.resume_ui.visible.len(), PAGE + 20);
        assert_eq!(app.resume_ui.limit, PAGE);
        for _ in 0..PAGE / PAGE_STEP {
            press(&mut app, Named::PageDown);
        }
        assert_eq!(app.resume_ui.selected, PAGE / PAGE_STEP * PAGE_STEP);
        while app.resume_ui.selected < PAGE - 1 {
            press(&mut app, Named::ArrowDown);
        }
        press(&mut app, Named::ArrowDown);
        assert_eq!(app.resume_ui.selected, PAGE, "the next page is listed");
        assert!(app.resume_ui.limit > PAGE);
        for _ in 0..10 {
            press(&mut app, Named::PageDown);
        }
        assert_eq!(app.resume_ui.selected, PAGE + 19, "and stops at the last");
        press(&mut app, Named::PageUp);
        assert_eq!(app.resume_ui.selected, PAGE + 19 - PAGE_STEP);
    }

    /// `/resume` lists conversations to go to, so the one it was typed in,
    /// where the reader already is, is not among them.
    #[test]
    fn resume_does_not_offer_the_conversation_it_was_typed_in() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.limit = PAGE;
        let generation = app.resume_ui.generation;
        let here = SavedConversation {
            open: true,
            updated_at_unix_ms: 2,
            ..saved(directory.path())
        };
        let earlier = saved(directory.path());
        app.resume_ui.asked_from = Some(here.card);
        list(&mut app, generation, vec![here, earlier.clone()]);
        let listed = app
            .resume_ui
            .visible
            .iter()
            .map(|&index| app.resume_ui.rows[index].card)
            .collect::<Vec<_>>();
        assert_eq!(listed, [earlier.card]);
    }

    /// The picker opens on its project, with its worktree and every
    /// project a keystroke away, each counted.
    /// Tab chooses where to look, so COSMIC's traversal is off while the
    /// picker is open: otherwise the keyboard leaves the search for a scope
    /// button, and Enter presses it instead of resuming.
    #[test]
    fn the_picker_owns_tab_while_open() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.core.set_keyboard_nav(true);
        drop(app.update_resume(Action::Open));
        assert!(app.resume_ui.open);
        assert!(!app.core.keyboard_nav());
        drop(app.update_resume(Action::Close));
        assert!(app.core.keyboard_nav(), "the reader's setting comes back");

        app.core.set_keyboard_nav(false);
        drop(app.update_resume(Action::Open));
        drop(app.update_resume(Action::Close));
        assert!(!app.core.keyboard_nav(), "an existing choice is kept");
    }

    #[test]
    fn the_picker_looks_in_its_project_until_told_to_look_wider() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let worktree = project.path().join(".worktrees/fix-auth");
        std::fs::create_dir_all(&worktree).unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.scope = Some(Scope {
            reach: Reach::Project,
            project: "project".into(),
            roots: vec![project.path().into()],
            worktree: Some(WorktreeScope {
                label: "fix-auth".into(),
                roots: vec![worktree.clone()],
                nested: Vec::new(),
            }),
        });
        let entry = |directory: &Path| {
            let mut entry = saved(directory);
            entry.card = CardId::new();
            entry.session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
            entry
        };
        let inside = entry(&project.path().join("src"));
        let branch = entry(&worktree);
        let outside = entry(elsewhere.path());
        let generation = app.resume_ui.generation;
        list(
            &mut app,
            generation,
            vec![inside.clone(), branch.clone(), outside.clone()],
        );
        let listed = |app: &Cagents| {
            let mut cards = app
                .resume_ui
                .visible
                .iter()
                .map(|&index| app.resume_ui.rows[index].card)
                .collect::<Vec<_>>();
            cards.sort();
            cards
        };
        let sorted = |mut cards: Vec<CardId>| {
            cards.sort();
            cards
        };
        assert_eq!(listed(&app), sorted(vec![inside.card, branch.card]));
        assert_eq!(app.resume_ui.counts, [1, 2, 3]);
        let tab = |app: &mut Cagents, shift: bool| {
            let mut event = crate::app::dialog_ui::tests::key_press(keyboard::key::Named::Tab);
            if let keyboard::Event::KeyPressed { modifiers, .. } = &mut event {
                *modifiers = if shift {
                    keyboard::Modifiers::SHIFT
                } else {
                    keyboard::Modifiers::empty()
                };
            }
            assert!(
                app.resume_key_event(&event).is_some(),
                "Tab chooses where to look"
            );
        };
        tab(&mut app, false);
        assert_eq!(listed(&app).len(), 3, "everywhere");
        tab(&mut app, false);
        assert_eq!(listed(&app), [branch.card], "wraps around to the worktree");
        tab(&mut app, true);
        assert_eq!(listed(&app).len(), 3, "Shift+Tab goes back");
        let _ = app.update_resume(Action::Reach(Reach::Project));
        let _ = app.update_resume(Action::Query("zzzz".into()));
        assert!(listed(&app).is_empty());
        assert_eq!(app.resume_ui.counts, [0, 0, 0]);
    }

    /// A word in the title ranks above the same word elsewhere in the
    /// conversation, which ranks above a loose match of the title.
    #[test]
    fn search_ranks_the_title_over_what_was_said_over_a_loose_match() {
        let directory = tempfile::tempdir().unwrap();
        let row = |title: &str, asked: &str, updated_at_unix_ms| {
            let mut found = found(directory.path(), title, updated_at_unix_ms);
            found.summary = Arc::new(scan::Summary {
                asked: asked.into(),
                ..scan::Summary::clone(&found.summary)
            });
            Row::found(found, Provider::Claude, String::new(), "project".into())
        };
        let rows = vec![
            row("Rename the parser", "", 4),
            row("Sweep warnings", "the parser panics on empty input", 3),
            row("Port a rewrite", "", 2),
            row("Parser recovery", "", 1),
            row("Unrelated", "", 0),
        ];
        let titles = |rows: &[Row], query: &str| {
            let (visible, _) = resume_matches(rows, query, None);
            visible
                .into_iter()
                .map(|index| rows[index].title.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            titles(&rows, "parser"),
            ["Parser recovery", "Rename the parser", "Sweep warnings"]
        );
        assert_eq!(titles(&rows, "prt rwrt"), ["Port a rewrite"]);
        assert_eq!(titles(&rows, "empty parser"), ["Sweep warnings"]);
        assert_eq!(
            titles(&rows, "").len(),
            5,
            "an empty search lists everything, newest first"
        );

        // A word that names only where conversations ran matches them all
        // alike, so they stay newest first rather than shortest title first.
        let place = |title: &str, place: &str, updated_at_unix_ms| {
            let found = found(directory.path(), title, updated_at_unix_ms);
            Row::found(found, Provider::Claude, String::new(), place.into())
        };
        let rows = vec![
            place(
                "Move the billing exports to the new queue",
                "old-migration",
                3,
            ),
            place("Profile", "old-migration", 2),
            place("Draft the release notes", "main", 1),
        ];
        assert_eq!(
            titles(&rows, "old-migration"),
            ["Move the billing exports to the new queue", "Profile"]
        );
    }

    #[test]
    fn ages_and_sizes_read_at_a_glance() {
        let now = 1_800_000_000_000;
        let minute = 60_000;
        assert_eq!(age(now, now - 20_000), "Just now");
        assert_eq!(age(now, now - 5 * minute), "5 min ago");
        assert_eq!(age(now, now - 3 * 60 * minute), "3 hr ago");
        assert_eq!(age(now, now - 30 * 60 * minute), "Yesterday");
        assert_eq!(age(now, now - 4 * 24 * 60 * minute), "4 days ago");
        assert_eq!(
            age(now, now + minute),
            "Just now",
            "a clock behind the file"
        );
        assert_eq!(size(512), "512 B");
        assert_eq!(size(12_300), "12.3 KB");
        assert_eq!(size(220_000_000), "220 MB");
        assert_eq!(messages(1), "1 message");
        assert_eq!(messages(42), "42 messages");
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
            None,
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
            matched,
            read,
        } = &progress[2]
        else {
            panic!("the newest conversations come next")
        };
        assert_eq!(*read, FIRST_BATCH);
        assert_eq!(conversations.len(), FIRST_BATCH - 2);
        assert_eq!(conversations[0].id(), ids[0]);
        assert_eq!(conversations[1].id(), ids[2]);
        assert_eq!(titles, &[(entry.card, "Review prompt 1".to_owned())]);
        // Both saved conversations learn what their transcripts say.
        assert_eq!(matched.len(), 2);
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
            None,
            &AtomicBool::new(false),
            |_| {
                steps += 1;
                false
            },
        );
        assert_eq!(steps, 1);
    }

    fn found(directory: &Path, title: &str, updated_at_unix_ms: i64) -> catalog::Found {
        catalog::Found {
            seed: 0,
            path: directory.join("transcript.jsonl"),
            summary: Arc::new(scan::Summary {
                id: uuid::Uuid::new_v4().to_string(),
                directory: directory.into(),
                ..Default::default()
            }),
            title: title.into(),
            bytes: 0,
            titled: true,
            named: true,
            updated_at_unix_ms,
        }
    }

    fn provider_root(seed: &chat::Session) -> PathBuf {
        seed.launch
            .provider
            .session_root(&seed.launch.recipe)
            .unwrap()
    }

    #[test]
    fn pointing_at_a_row_selects_it_and_leaving_keeps_preview_and_highlight_together() {
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Size, mouse};
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.limit = PAGE;
        for index in 0..20 {
            let mut entry = saved(std::path::Path::new("/fixture"));
            entry.title = format!("Row {index}");
            app.resume_ui
                .rows
                .push(Row::saved(entry, "Fixture".into(), "fixture".into()));
        }
        app.resume_ui.visible = (0..20).collect();
        let mut renderer = crate::app::tests::headless_renderer();
        let size = Size::new(1396.0, 873.0);
        // Moves the real pointer to `to` over the mounted picker and applies
        // what it reports, as the application would.
        let mut point = |app: &mut Cagents, cache: Cache, to: Point| {
            let mut ui =
                UserInterface::build(app.resume_dialog().unwrap(), size, cache, &mut renderer);
            let mut messages = Vec::new();
            let _ = ui.update(
                &[Event::Mouse(mouse::Event::CursorMoved { position: to })],
                mouse::Cursor::Available(to),
                &mut renderer,
                &mut cosmic::iced::advanced::clipboard::Null,
                &mut messages,
            );
            let cache = ui.into_cache();
            for message in messages {
                if let Message::ResumeConversation(action) = message {
                    let _ = app.update_resume(action);
                }
            }
            cache
        };
        let row = |app: &Cagents, title: &str| {
            let ui = UserInterface::build(
                app.resume_dialog().unwrap(),
                size,
                Cache::new(),
                &mut crate::app::tests::headless_renderer(),
            );
            let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
            let name = format!("Resume {title}");
            let rect = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|node| node.node().label() == Some(name.as_str()))
                .unwrap_or_else(|| panic!("mounted {name}"))
                .node()
                .bounds()
                .unwrap();
            Point::new(
                ((rect.x0 + rect.x1) / 2.0) as f32,
                ((rect.y0 + rect.y1) / 2.0) as f32,
            )
        };
        let selected = |app: &Cagents| {
            app.resume_ui.rows[app.resume_ui.visible[app.resume_ui.selected]]
                .title
                .clone()
        };
        let mut cache = Cache::new();
        let second = row(&app, "Row 2");
        cache = point(&mut app, cache, second);
        assert_eq!(selected(&app), "Row 2");
        // Leaving by any side, including below the dialog where the list
        // hears nothing, leaves the selection where it was.
        for outside in [
            Point::new(1300.0, 860.0),
            Point::new(1300.0, 500.0),
            Point::new(600.0, 30.0),
        ] {
            cache = point(&mut app, cache, outside);
            assert_eq!(selected(&app), "Row 2", "{outside:?}");
        }
        let fourth = row(&app, "Row 4");
        cache = point(&mut app, cache, fourth);
        assert_eq!(selected(&app), "Row 4");
        drop(cache);
        // A row reported after the matches changed under it selects nothing.
        let stale = app.resume_ui.rows[app.resume_ui.visible[7]].card;
        let _ = app.update_resume(Action::Point(3, stale));
        assert_eq!(selected(&app), "Row 4");
    }
}
