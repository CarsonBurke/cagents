//! Local saved-conversation navigation shares the admitted selection worker.
use super::*;
use crate::persistence::SavedConversation;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Picker {
    generation: u64,
    open: bool,
    replace: Option<CardId>,
}

impl Picker {
    fn capture(app: &Cagents) -> Self {
        Self {
            generation: app.resume_ui.generation,
            open: app.resume_ui.open,
            replace: app.resume_ui.replace,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    remembered: Option<(ChannelId, Option<(ProjectId, PathBuf)>)>,
    existing: Option<(CardId, Option<(ChannelId, ProjectId)>, Option<PathBuf>)>,
}

impl Snapshot {
    fn capture(app: &Cagents, entry: &SavedConversation) -> Self {
        let existing = app.existing_conversation_card(&entry.session).map(|card| {
            let channel = app
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&card))
                .map(|channel| (channel.id, channel.project_id));
            let path = match channel {
                Some((channel, _)) => app.channel_directory(channel),
                None => app
                    .card_state(card)
                    .and_then(|card| app.card_worktree(card))
                    .map(Path::to_path_buf)
                    .or_else(|| Some(entry.session.launch.recipe.working_directory.clone())),
            };
            (card, channel, path)
        });
        Self {
            remembered: app.recorded_resume_channel(&entry.session),
            existing,
        }
    }
}

pub(super) struct Intent {
    entry: Arc<SavedConversation>,
    picker: Picker,
    snapshot: Snapshot,
    generation: u64,
    #[cfg(test)]
    pub(super) before_resolve: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
    #[cfg(test)]
    pub(super) owner_read: Option<Arc<dyn Fn() + Send + Sync>>,
    #[cfg(test)]
    before_missing: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
}

impl std::fmt::Debug for Intent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ResumeIntent")
            .field("picker", &self.picker)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Intent {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.entry, &other.entry)
            && self.picker == other.picker
            && self.snapshot == other.snapshot
            && self.generation == other.generation
    }
}
impl Eq for Intent {}

impl Intent {
    pub(super) fn launch_directory(&self) -> &Path {
        &self.entry.session.launch.recipe.working_directory
    }
    pub(super) fn read_only_history(&self) -> bool {
        self.entry.session.launch.needs_read_only_history()
    }
    pub(super) fn paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![self.entry.session.launch.recipe.working_directory.clone()];
        if let Some((_, Some((_, path)))) = &self.snapshot.remembered {
            paths.push(path.clone());
        }
        if let Some((_, _, Some(path))) = &self.snapshot.existing {
            paths.push(path.clone());
        }
        paths.sort();
        paths.dedup();
        paths
    }
}

#[derive(Clone)]
pub(super) enum Destination {
    Channel(ChannelId, PathBuf),
    Worktree,
    KeepScope,
}

/// Classification is metadata only. Stale resume receipts may always retire;
/// current receipts still require the caller's modal/save commit fences.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::app) enum Completion {
    Stale,
    SupersededPredecessor,
    CurrentPicker,
    CurrentWithoutPicker,
}

impl Receipt {
    pub(in crate::app) fn resume_completion(
        &self,
        app: &Cagents,
        ticket: u64,
    ) -> Option<Completion> {
        if let Some(intent) = &self.resume {
            return Some(if !current(app, ticket, intent) {
                Completion::Stale
            } else if intent.picker.open {
                Completion::CurrentPicker
            } else {
                Completion::CurrentWithoutPicker
            });
        }
        // A waiting resume must not be stranded behind an older ordinary
        // navigation receipt which its own request has already superseded.
        // This proves only cleanup eligibility, never ordinary commit rights.
        let pending = app.worktree_selection.pending.as_ref()?;
        let intent = pending.resume.as_ref()?;
        (owns(app, intent)
            && intent.picker == Picker::capture(app)
            && app.resume_ui.loading
            && intent.generation == app.worktree_selection.generation
            && pending.generation == intent.generation
            && self.request_generation != intent.generation
            && app.worktree_selection.active == Some((ticket, self.request_generation)))
        .then_some(Completion::SupersededPredecessor)
    }
}

fn owns(app: &Cagents, intent: &Arc<Intent>) -> bool {
    app.worktree_selection
        .resume
        .as_ref()
        .is_some_and(|active| Arc::ptr_eq(active, intent))
}

fn current(app: &Cagents, ticket: u64, intent: &Arc<Intent>) -> bool {
    owns(app, intent)
        && intent.picker == Picker::capture(app)
        && app.resume_ui.loading
        && intent.generation == app.worktree_selection.generation
        && app.worktree_selection.active == Some((ticket, intent.generation))
        && app.worktree_selection.pending.is_none()
}

pub(super) fn stale(app: &mut Cagents, intent: &Arc<Intent>) {
    if owns(app, intent) {
        app.worktree_selection.resume = None;
        // Never clear a reopened/replaced picker's loading or error state.
        if intent.picker == Picker::capture(app) {
            app.resume_ui.loading = false;
        }
    }
}

pub(super) fn refresh(app: &mut Cagents, request: &Request) -> Request {
    let previous = request.resume.as_ref().expect("resume request");
    let intent = Arc::new(Intent {
        entry: previous.entry.clone(),
        picker: previous.picker.clone(),
        snapshot: Snapshot::capture(app, &previous.entry),
        generation: previous.generation,
        #[cfg(test)]
        before_resolve: previous.before_resolve.clone(),
        #[cfg(test)]
        owner_read: previous.owner_read.clone(),
        #[cfg(test)]
        before_missing: previous.before_missing.clone(),
    });
    let mut request = request.clone();
    request.resume = Some(intent.clone());
    app.worktree_selection.resume = Some(intent);
    request
}

pub(super) fn snapshot_changed(app: &Cagents, intent: &Arc<Intent>) -> bool {
    intent.snapshot != Snapshot::capture(app, &intent.entry)
}

pub(super) fn failed(app: &mut Cagents, intent: &Arc<Intent>, error: &str) {
    if owns(app, intent) && intent.picker == Picker::capture(app) {
        app.worktree_selection.resume = None;
        app.resume_ui.loading = false;
        app.resume_ui.error = Some(error.into());
        if !app.resume_ui.open {
            app.status = Some(TransientNotice::error(error));
        }
    }
}

impl Cagents {
    /// A cancelled receipt or superseded ordinary navigation may already be
    /// parked behind the picker. Only cleanup is admitted here; its guards
    /// still retire on the original worker.
    pub(in crate::app) fn drain_stale_resume_navigation(&mut self) -> Task<Message> {
        let mut effects = Vec::new();
        let queued = std::mem::take(&mut self.deferred_navigation);
        for message in queued {
            let stale = matches!(&message,
                Message::WorktreeNavigationPrepared { generation, receipt }
                    if matches!(receipt.resume_completion(self, *generation),
                        Some(Completion::Stale | Completion::SupersededPredecessor)));
            if stale {
                if let Message::WorktreeNavigationPrepared {
                    generation,
                    receipt,
                } = message
                {
                    effects.push(self.worktree_navigation_prepared(generation, receipt));
                }
            } else {
                self.deferred_navigation.push_back(message);
            }
        }
        Task::batch(effects)
    }

    pub(in crate::app) fn cancel_local_resume_navigation(&mut self) {
        if self.worktree_selection.resume.take().is_some() {
            self.worktree_selection.cancel();
        }
    }

    pub(in crate::app) fn prepare_local_resume(
        &mut self,
        entry: SavedConversation,
    ) -> Task<Message> {
        if !self.resume_history_is_available() {
            return Task::none();
        }
        self.worktree_selection.cancel();
        let intent = Arc::new(Intent {
            snapshot: Snapshot::capture(self, &entry),
            entry: Arc::new(entry),
            picker: Picker::capture(self),
            generation: self.worktree_selection.generation,
            #[cfg(test)]
            before_resolve: self.worktree_selection.before_resume_resolve.clone(),
            #[cfg(test)]
            owner_read: self.worktree_selection.owner_read.clone(),
            #[cfg(test)]
            before_missing: self.worktree_selection.before_missing_resume.clone(),
        });
        self.resume_ui.loading = true;
        self.worktree_selection.resume = Some(intent.clone());
        self.worktree_selection.pending = Some(Request {
            generation: intent.generation,
            path: Some(intent.entry.session.launch.recipe.working_directory.clone()),
            create: false,
            origin: None,
            background: false,
            refresh_channel: None,
            resume: Some(intent),
        });
        let cleanup = self.drain_stale_resume_navigation();
        Task::batch([cleanup, self.dispatch_worktree_navigation()])
    }
}

pub(super) fn prepare(
    inputs: Inputs,
    protections: crate::worktree_cleanup::WindowProtections,
    state: Arc<cache::WatchState>,
    #[cfg(test)] runtime: Option<PathBuf>,
    #[cfg(test)] after_acquire: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
) -> Result<Prepared, String> {
    let intent = inputs.request.resume.as_ref().expect("resume intent");
    let launch = &intent.entry.session.launch.recipe.working_directory;
    // All path observations, including availability, run on this worker.
    // prepare_navigation registers every lexical resume path before resolving.
    let mut choices = Vec::new();
    if let Some((_, channel, path)) = &intent.snapshot.existing {
        let path = path
            .clone()
            .ok_or("The conversation's current project is unavailable")?;
        choices.push((
            path.clone(),
            channel.map_or(Destination::Worktree, |(id, _)| {
                Destination::Channel(id, path)
            }),
        ));
    } else {
        if let Some((id, Some((_, path)))) = &intent.snapshot.remembered {
            choices.push((path.clone(), Destination::Channel(*id, path.clone())));
        }
        choices.push((launch.clone(), Destination::Worktree));
    }
    let mut last_error =
        "This conversation's project folder is unavailable. Restore the folder before resuming."
            .to_owned();
    for (path, destination) in choices {
        let mut working = inputs.clone();
        working.request.path = Some(path);
        // launch availability itself must follow lexical watch registration,
        // handled by prepare_navigation's resume path preflight below.
        working.protected.push(launch.clone());
        match prepare_navigation(
            working,
            protections.navigation_snapshot(),
            state.clone(),
            false,
            #[cfg(test)]
            runtime.clone(),
            #[cfg(test)]
            after_acquire.clone(),
        ) {
            Ok(mut prepared) => {
                prepared.inputs = inputs;
                prepared.resume = Some(destination);
                return Ok(prepared);
            }
            Err(PreparationError::Changed(error)) => return Err(error),
            Err(PreparationError::Unavailable(error)) => last_error = error,
        }
    }
    if intent.read_only_history() && intent.snapshot.existing.is_none() {
        #[cfg(test)]
        if let Some(before) = &intent.before_missing {
            before(launch);
        }
        // Exact saved read-only history may remain in the already-protected
        // browsing scope; it never grants launch authority for missing cwd.
        let mut working = inputs.clone();
        working.request.path = inputs.scope.0.clone();
        working.protected.push(launch.clone());
        let mut prepared = prepare_navigation(
            working,
            protections,
            state,
            true,
            #[cfg(test)]
            runtime,
            #[cfg(test)]
            after_acquire,
        )
        .map_err(PreparationError::message)?;
        prepared.inputs = inputs;
        prepared.resume = Some(Destination::KeepScope);
        return Ok(prepared);
    }
    Err(last_error)
}

pub(super) fn commit(
    app: &mut Cagents,
    intent: &Arc<Intent>,
    destination: &Destination,
    path: Option<&Path>,
) -> Task<Message> {
    app.worktree_selection.resume = None;
    app.resume_ui.loading = false;
    let scope = match destination {
        Destination::Channel(id, path) => app.commit_channel_selection(*id, path.clone()),
        Destination::Worktree => path.map_or_else(Task::none, |path| {
            app.commit_worktree_selection(path.to_owned(), false).1
        }),
        Destination::KeepScope => Task::none(),
    };
    let next = app.commit_prepared_resume((*intent.entry).clone(), intent.picker.replace);
    Task::batch([scope, next])
}
