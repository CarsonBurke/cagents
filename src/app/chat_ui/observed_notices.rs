//! Durable copies of Capacity notices this reader actually observed. Ownership
//! follows the captured launch; provider history and other failures are untouched.
use super::*;
use crate::chat::failure::{Failure, FailureKind};
use crate::persistence::observed_notices::{Notice, Scope};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);
const READY_LOAD_DEADLINE: Duration = Duration::from_secs(5);
type SaveFailure = crate::session::ObservedNoticeSaveFailure;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    id: u64,
    target: Target,
}

impl Ticket {
    fn new(target: Target) -> Self {
        Self {
            id: NEXT_TICKET.fetch_add(1, Ordering::Relaxed),
            target,
        }
    }
}

pub(super) enum ReadyState {
    Loading {
        ticket: Ticket,
        launch: chat::Launch,
        actor: TerminalId,
        ready: Ready,
    },
    Complete(Arc<Loaded>),
}

impl ReadyState {
    pub(super) fn loaded(self) -> Option<Arc<Loaded>> {
        match self {
            Self::Complete(loaded) => Some(loaded),
            Self::Loading { .. } => None,
        }
    }
}

#[derive(Default)]
pub(super) struct RuntimeState {
    batch: Option<Batch>,
    retired: Vec<Capture>,
}

struct Pending {
    ticket: Ticket,
    offered: Option<Arc<[Notice]>>,
}

struct Batch {
    local: bool,
    launch: chat::Launch,
    actor: TerminalId,
    binding: Option<Result<Scope, String>>,
    pending: Option<Pending>,
    records: Vec<Notice>,
    // A parked write or a committed write whose readback failed must be
    // acknowledged again before a newer endpoint can use its minted revision.
    retry_offer: Option<Arc<[Notice]>>,
    dirty: bool,
    reported: bool,
    retryable: bool,
}

pub(super) struct Capture {
    target: Target,
    batch: Batch,
}

impl Batch {
    fn new(launch: chat::Launch, actor: TerminalId) -> Self {
        Self {
            local: !crate::device::is_remote(),
            launch,
            actor,
            binding: None,
            pending: None,
            records: Vec::new(),
            retry_offer: None,
            dirty: false,
            reported: false,
            retryable: true,
        }
    }

    fn owns(&self, ticket: Ticket, loaded: &Loaded) -> bool {
        self.launch == loaded.launch
            && self.actor == loaded.actor
            && self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.ticket == ticket)
    }

    fn adopt_revisions(&mut self, saved: &[Notice], offered: Option<&[Notice]>) {
        let provider = self.launch.provider;
        for current in &mut self.records {
            let Some(record) = saved
                .iter()
                .find(|record| record.message == current.message && record.text == current.text)
                .or_else(|| {
                    saved
                        .iter()
                        .find(|record| same_notice(provider, record, current))
                })
            else {
                continue;
            };
            // A save receipt can advance only the endpoint this operation
            // actually offered. A competing writer's different anchor is not
            // permission to overwrite it with an old local observation.
            let acknowledged = offered.is_some_and(|offered| {
                offered
                    .iter()
                    .any(|sent| same_notice(provider, sent, record) && sent.after == record.after)
            });
            if acknowledged || current.after == record.after {
                current.revision = record.revision;
                current.message.clone_from(&record.message);
            }
        }
    }
}

fn same_notice(provider: Provider, left: &Notice, right: &Notice) -> bool {
    failure::same_prompt(provider, left.message.as_deref(), right.message.as_deref())
        && left.text == right.text
}

fn same_endpoints(left: &[Notice], right: &[Notice]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(left, right)| {
            left.message == right.message
                && left.after == right.after
                && (Arc::ptr_eq(&left.text, &right.text) || left.text == right.text)
        })
}

fn retained_capture_bytes(capture: &Capture) -> usize {
    let batch = &capture.batch;
    let recipe = &batch.launch.recipe;
    let mut bytes = std::mem::size_of::<Capture>()
        .saturating_add(recipe.program.capacity())
        .saturating_add(recipe.working_directory.as_os_str().len())
        .saturating_add(batch.launch.session_id.as_ref().map_or(0, String::capacity));
    for value in recipe
        .arguments
        .iter()
        .chain(recipe.unset_environment.iter())
    {
        bytes = bytes.saturating_add(std::mem::size_of::<String>() + value.capacity() + 64);
    }
    for (key, value) in &recipe.environment {
        bytes = bytes.saturating_add(
            2 * std::mem::size_of::<String>() + key.capacity() + value.capacity() + 64,
        );
    }
    bytes = bytes.saturating_add(match &batch.binding {
        // Scope's independent encoded bound also covers its owned metadata.
        Some(Ok(_)) => 16 * 1024,
        Some(Err(error)) => error.capacity(),
        None => 0,
    });
    bytes = bytes.saturating_add(
        batch
            .records
            .capacity()
            .saturating_mul(std::mem::size_of::<Notice>()),
    );
    let mut charge = |records: &[Notice], headers: bool| {
        if headers {
            bytes = bytes.saturating_add(
                records.len().saturating_mul(std::mem::size_of::<Notice>())
                    + 2 * std::mem::size_of::<usize>(),
            );
        }
        for record in records {
            bytes = bytes
                .saturating_add(record.text.len())
                .saturating_add(record.message.as_ref().map_or(0, String::capacity))
                .saturating_add(record.after.as_ref().map_or(0, String::capacity));
        }
    };
    // Conservative duplicate charging is intentional. These arrays can retain
    // rotated bodies which no longer appear in the current 32-note window.
    charge(&batch.records, false);
    if let Some(offered) = &batch.retry_offer {
        charge(offered, true);
    }
    if let Some(offered) = batch
        .pending
        .as_ref()
        .and_then(|pending| pending.offered.as_ref())
    {
        charge(offered, true);
    }
    bytes
}

/// Compare captured observation owners without filesystem or inherited
/// environment fallback. An unavailable selector permits only this exact
/// actor/process context to retain its own local notes, never durable account
/// authority. Argument-only model/permission metadata changes do not change an
/// already-running actor's account; changing its captured environment does.
/// Scope::resolve_local still independently refuses an unattested account.
fn same_account(
    left: &chat::Launch,
    left_actor: TerminalId,
    right: &chat::Launch,
    right_actor: TerminalId,
) -> bool {
    if left.provider != right.provider {
        return false;
    }
    let home = |launch: &chat::Launch| {
        let variable = crate::account::home_variable(launch.provider)?;
        if launch.recipe.unset_environment.contains(variable) {
            return None;
        }
        launch
            .recipe
            .environment
            .get(variable)
            .filter(|home| !home.is_empty())
            .map(|home| {
                let path = PathBuf::from(home);
                if path.is_absolute() {
                    path
                } else {
                    launch.recipe.working_directory.join(path)
                }
            })
    };
    match (home(left), home(right)) {
        (Some(left), Some(right)) => left == right,
        (None, None) => {
            left_actor == right_actor
                && left.recipe.program == right.recipe.program
                && left.recipe.environment == right.recipe.environment
                && left.recipe.unset_environment == right.recipe.unset_environment
                && left.recipe.working_directory == right.recipe.working_directory
                && observation_identity_arguments(&left.recipe.arguments)
                    .eq(observation_identity_arguments(&right.recipe.arguments))
        }
        _ => false,
    }
}

/// Only these known presentation options may change within one actual actor.
/// Preserve every other argument, including profile/config/session selectors;
/// malformed value options cannot swallow a following ownership selector.
fn observation_identity_arguments(arguments: &[String]) -> impl Iterator<Item = &str> {
    let mut arguments = arguments.iter().map(String::as_str).peekable();
    let mut positional = false;
    std::iter::from_fn(move || {
        loop {
            let argument = arguments.next()?;
            if positional {
                return Some(argument);
            }
            if argument == "--" {
                positional = true;
                return Some(argument);
            }
            let metadata =
                |flag| matches!(flag, "--model" | "--permission-mode" | "--approval-mode");
            if metadata(argument)
                && arguments
                    .peek()
                    .is_some_and(|value| !value.starts_with('-'))
            {
                arguments.next();
                continue;
            }
            if argument
                .split_once('=')
                .is_some_and(|(flag, value)| metadata(flag) && !value.is_empty())
            {
                continue;
            }
            return Some(argument);
        }
    })
}

fn same_owner(
    left: &chat::Launch,
    left_actor: TerminalId,
    right: &chat::Launch,
    right_actor: TerminalId,
) -> bool {
    left.session_id == right.session_id && same_account(left, left_actor, right, right_actor)
}

fn capacity(provider: Provider, text: &str) -> bool {
    Failure::parse(provider, text).is_some_and(|failure| failure.kind == FailureKind::Capacity)
}

/// Raw String conversions happen once on the blocking reader. Messages clone
/// only this shared, single-use payload, including across recovery checkpoints.
pub struct Loaded {
    launch: chat::Launch,
    actor: TerminalId,
    scope: Result<Scope, String>,
    records: Arc<[Notice]>,
    notes: Mutex<Option<Vec<failure::Note>>>,
    retryable: bool,
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ObservedCapacityNotices")
    }
}

impl Loaded {
    fn from_records(
        launch: chat::Launch,
        actor: TerminalId,
        scope: Result<Scope, String>,
        records: Vec<Notice>,
    ) -> Self {
        let records: Vec<_> = records
            .into_iter()
            .filter(|record| capacity(launch.provider, &record.text))
            .collect();
        let notes = records
            .iter()
            .map(|record| failure::Note {
                message: record.message.clone(),
                after: record.after.clone(),
                text: record.text.to_string(),
            })
            .collect();
        Self {
            launch,
            actor,
            scope,
            records: records.into(),
            notes: Mutex::new(Some(notes)),
            retryable: true,
        }
    }

    fn failed(launch: chat::Launch, actor: TerminalId, error: String) -> Self {
        Self::from_records(launch, actor, Err(error), Vec::new())
    }

    fn unsupported(launch: chat::Launch, actor: TerminalId, error: String) -> Self {
        let mut loaded = Self::failed(launch, actor, error);
        loaded.retryable = false;
        loaded
    }
}

pub(super) fn load(
    store: &crate::persistence::StateStore,
    launch: chat::Launch,
    actor: TerminalId,
    local: bool,
) -> Loaded {
    if !local {
        return Loaded::unsupported(
            launch,
            actor,
            "Remote capacity history needs an attested remote account home".into(),
        );
    }
    let owner = (|| {
        let machine = std::fs::read_to_string("/etc/machine-id")
            .map_err(|error| format!("Cannot identify this device: {error}"))?;
        let machine = machine.trim();
        if machine.len() != 32 || !machine.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("This device has no stable owner domain".into());
        }
        Ok(format!("machine:{machine}"))
    })();
    let owner = match owner {
        Ok(owner) => owner,
        Err(error) => return Loaded::unsupported(launch, actor, error),
    };
    let scope = match Scope::resolve_local(&launch, owner) {
        Ok(scope) => scope,
        Err(error) => {
            let retryable = !error.refuses_content();
            let mut loaded = Loaded::failed(launch, actor, error.to_string());
            loaded.retryable = retryable;
            return loaded;
        }
    };
    match store.load_observed_notices(&scope) {
        Ok(records) => Loaded::from_records(launch, actor, Ok(scope), records),
        Err(error) => Loaded::failed(launch, actor, error.to_string()),
    }
}

impl Runtime {
    pub(super) fn prepare_observed_history(&mut self) {
        if self.session.launch.session_id.is_some() {
            self.observed_notices
                .batch
                .get_or_insert_with(|| Batch::new(self.session.launch.clone(), self.session.id));
        }
    }
    /// Called only after an actual note change. Share its body once, then use
    /// that Arc for every queue offer; ordinary frames never scan/copy bodies.
    pub(super) fn observe_capacity_notice(&mut self, index: usize) {
        let Some(note) = self.failures.get(index) else {
            return;
        };
        if !capacity(self.session.launch.provider, &note.text) {
            return;
        }
        let batch = self
            .observed_notices
            .batch
            .get_or_insert_with(|| Batch::new(self.session.launch.clone(), self.session.id));
        debug_assert!(same_owner(
            &batch.launch,
            batch.actor,
            &self.session.launch,
            self.session.id
        ));
        if let Some(record) = batch.records.iter_mut().find(|record| {
            failure::same_prompt(
                self.session.launch.provider,
                record.message.as_deref(),
                note.message.as_deref(),
            ) && record.text.as_ref() == note.text
        }) {
            record.after.clone_from(&note.after);
        } else {
            batch.records.push(Notice {
                message: note.message.clone(),
                after: note.after.clone(),
                text: Arc::from(note.text.as_str()),
                revision: 0,
            });
            if batch.records.len() > 32 {
                batch.records.remove(0);
            }
        }
        batch.dirty = true;
        batch.reported = false;
    }

    pub(super) fn prepare_observed_session(&mut self, next: &chat::Session) {
        if self.session.id == next.id {
            self.promote_observed_notices(&next.launch);
        } else if self.session.launch.session_id.is_none() {
            self.retire_observed_notices(None);
        }
        self.retire_observed_notices(Some((&next.launch, next.id)));
    }

    /// Only callers observing the same exact actor may promote pre-ID notes.
    pub(super) fn promote_observed_notices(&mut self, launch: &chat::Launch) {
        let Some(batch) = self.observed_notices.batch.as_mut() else {
            return;
        };
        if batch.launch.session_id.is_none()
            && launch.session_id.is_some()
            && same_account(&batch.launch, batch.actor, launch, self.session.id)
            && batch.pending.is_none()
        {
            batch.launch.session_id.clone_from(&launch.session_id);
            batch.binding = None;
            batch.reported = false;
            batch.dirty = true;
        }
    }

    /// Retire the old captured owner before a reset/rebind can reuse item IDs.
    /// Outstanding jobs and exact bytes move with it, rather than binding to
    /// the new account/conversation through this still-existing UI runtime.
    pub(super) fn retire_observed_notices(&mut self, launch: Option<(&chat::Launch, TerminalId)>) {
        if self.observed_notices.batch.as_ref().is_none_or(|batch| {
            launch.is_some_and(|(launch, actor)| {
                same_owner(&batch.launch, batch.actor, launch, actor)
            })
        }) {
            return;
        }
        let batch = self.observed_notices.batch.take().unwrap();
        let provider = batch.launch.provider;
        self.failures.retain(|note| !capacity(provider, &note.text));
        self.observed_notices.retired.push(Capture {
            target: self.target,
            batch,
        });
        self.regroup_observed_notices();
    }

    fn regroup_observed_notices(&mut self) {
        let turns = self.transcript_turns(&self.snapshot);
        carry_step_disclosures(
            &mut self.opened,
            &mut self.find_disclosure,
            &self.turns,
            &self.snapshot.items,
            &turns,
            &self.snapshot.items,
        );
        self.turns = turns;
        reconcile_disclosure(&mut self.opened, &self.turns);
        self.refresh_failure_ui();
        self.find_navigation.changed_document();
        self.refresh_find();
    }

    pub(super) fn restore_observed_notices(&mut self, loaded: Arc<Loaded>) {
        if !same_owner(
            &self.session.launch,
            self.session.id,
            &loaded.launch,
            loaded.actor,
        ) {
            return;
        }
        self.retire_observed_notices(Some((&loaded.launch, loaded.actor)));
        let batch = self
            .observed_notices
            .batch
            .get_or_insert_with(|| Batch::new(loaded.launch.clone(), loaded.actor));
        batch.binding = Some(loaded.scope.clone());
        batch.reported = false;
        batch.retryable = loaded.retryable;
        let offered = batch.retry_offer.clone();
        batch.adopt_revisions(&loaded.records, offered.as_deref());
        let current = std::mem::take(&mut batch.records);
        let mut records = loaded.records.to_vec();
        records.retain(|record| {
            !current
                .iter()
                .any(|note| same_notice(loaded.launch.provider, record, note))
        });
        records.extend(current);
        if records.len() > 32 {
            records.drain(..records.len() - 32);
        }
        batch.records = records;
        let Some(mut saved) = loaded.notes.lock().ok().and_then(|mut notes| notes.take()) else {
            self.regroup_observed_notices();
            return;
        };
        saved.retain(|record| {
            !self.failures.iter().any(|note| {
                failure::same_prompt(
                    self.session.launch.provider,
                    record.message.as_deref(),
                    note.message.as_deref(),
                ) && record.text == note.text
            })
        });
        saved.append(&mut self.failures);
        if saved.len() > 32 {
            saved.drain(..saved.len() - 32);
        }
        self.failures = saved;
        self.regroup_observed_notices();
    }
}

fn start_batch(
    writer: &crate::session::SessionRuntime,
    capture: &mut Capture,
) -> Result<Option<Task<Message>>, String> {
    let batch = &mut capture.batch;
    if batch.reported {
        return Ok(None);
    }
    if batch.pending.is_some() {
        return Ok(None);
    }
    if let Some(Err(error)) = &batch.binding {
        if !batch.retryable && batch.records.is_empty() && batch.retry_offer.is_none() {
            batch.reported = true;
            return Ok(None);
        }
        if batch.reported {
            return Ok(None);
        }
        batch.reported = true;
        return Err(error.clone());
    }
    if batch.binding.is_some() && !batch.dirty {
        return Ok(None);
    }
    let store = writer.operation_store();
    let launch = batch.launch.clone();
    let actor = batch.actor;
    let local = batch.local;
    let ticket = Ticket::new(capture.target);
    if batch.binding.is_none() {
        if batch.launch.session_id.is_none() {
            let error = "The provider has not confirmed an exact conversation ID".to_owned();
            batch.binding = Some(Err(error.clone()));
            batch.reported = true;
            batch.retryable = false;
            return Err(error);
        }
        batch.pending = Some(Pending {
            ticket,
            offered: None,
        });
        return Ok(Some(cosmic::task::future(async move {
            let worker_launch = launch.clone();
            let loaded =
                tokio::task::spawn_blocking(move || load(&store, worker_launch, actor, local))
                    .await
                    .unwrap_or_else(|error| {
                        Loaded::failed(
                            launch,
                            actor,
                            format!("Capacity history loader stopped: {error}"),
                        )
                    });
            Message::ChatUi(Action::ObservedNoticesBound(ticket, Arc::new(loaded)))
        })));
    }
    if !batch.dirty {
        return Ok(None);
    }
    if batch.records.is_empty() {
        batch.dirty = false;
        return Ok(None);
    }
    let scope = batch.binding.as_ref().unwrap().as_ref().unwrap().clone();
    let records: Arc<[Notice]> = batch
        .retry_offer
        .clone()
        .unwrap_or_else(|| batch.records.clone().into());
    let receipt = writer.preserve_observed_notices(capture.target.card, scope, records.clone())?;
    batch.dirty = !same_endpoints(&batch.records, &records);
    batch.pending = Some(Pending {
        ticket,
        offered: Some(records),
    });
    Ok(Some(cosmic::task::future(async move {
        let worker_launch = launch.clone();
        let result = tokio::task::spawn_blocking(move || {
            let result = receipt
                .recv()
                .map_err(|error| SaveFailure {
                    message: format!("Capacity history writer stopped: {error}"),
                    retryable: true,
                })
                .and_then(|result| result);
            (load(&store, worker_launch, actor, local), result)
        })
        .await;
        let (loaded, saved) = result.unwrap_or_else(|error| {
            (
                Loaded::failed(
                    launch,
                    actor,
                    format!("Capacity history writer stopped: {error}"),
                ),
                Err(SaveFailure {
                    message: error.to_string(),
                    retryable: true,
                }),
            )
        });
        Message::ChatUi(Action::ObservedNoticesSaved(
            ticket,
            Arc::new(loaded),
            saved,
        ))
    })))
}

fn ready_load_task(
    ticket: Ticket,
    ready: Ready,
    launch: chat::Launch,
    actor: TerminalId,
    deadline: Duration,
    loader: impl FnOnce() -> Loaded + Send + 'static,
) -> Task<Message> {
    cosmic::task::future(async move {
        let loaded = match tokio::time::timeout(deadline, tokio::task::spawn_blocking(loader)).await {
            Ok(Ok(loaded)) => loaded,
            Ok(Err(error)) => Loaded::failed(launch, actor, format!("Capacity history loader stopped: {error}")),
            Err(_) => Loaded::failed(launch, actor,
                "Earlier capacity history did not finish loading; the transcript may be incomplete. Open Notices to retry saving.".into()),
        };
        // A timed-out blocking read may finish later. Its detached result has
        // no callback and cannot replace this attempt or a newer owner.
        Message::ChatUi(Action::ObservedNoticesReady(
            ticket,
            ready,
            Arc::new(loaded),
        ))
    })
}

impl Cagents {
    /// Admission has already finished. A reconstructed named transcript waits
    /// for hydration before its first grouping. An existing same-owner runtime
    /// retains its notes and can rebind asynchronously.
    pub(super) fn load_ready_observed_notices(
        &self,
        attempt: &mut Attempt,
        handle: &Arc<ClientHandle>,
    ) -> Option<Task<Message>> {
        let session = handle.session();
        let launch = session.launch;
        let actor = session.id;
        launch.session_id.as_ref()?;
        let fresh = attempt.intent == chat::Intent::Start
            && attempt.session.launch.session_id.is_none()
            && attempt.session.terminal.is_none()
            && attempt.recovery.is_none()
            && !attempt.upgrade_committed
            && attempt.upgrade_admission.is_none();
        let retained = matches!(self.cards.get(&attempt.target.card), Some(CardRuntime::Chat(runtime))
            if matches!(runtime.connection, Connection::Live(_)) && same_owner(&runtime.session.launch, runtime.session.id, &launch, actor));
        if fresh || retained {
            return None;
        }
        match &attempt.observed_notices {
            Some(ReadyState::Loading {
                launch: pending,
                actor: pending_actor,
                ..
            }) if same_owner(pending, *pending_actor, &launch, actor) => return Some(Task::none()),
            Some(ReadyState::Complete(loaded))
                if same_owner(&loaded.launch, loaded.actor, &launch, actor) =>
            {
                return None;
            }
            _ => {}
        }
        let store = self.session.as_ref()?.operation_store();
        let local = !crate::device::is_remote();
        let target = attempt.target;
        let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle.clone())))));
        let ticket = Ticket::new(target);
        attempt.observed_notices = Some(ReadyState::Loading {
            ticket,
            launch: launch.clone(),
            actor,
            ready: ready.clone(),
        });
        let worker_launch = launch.clone();
        Some(ready_load_task(
            ticket,
            ready,
            launch,
            actor,
            READY_LOAD_DEADLINE,
            move || load(&store, worker_launch, actor, local),
        ))
    }

    pub(super) fn finish_ready_observed_notices(
        &mut self,
        ticket: Ticket,
        ready: Ready,
        loaded: Arc<Loaded>,
    ) -> Task<Message> {
        let target = ticket.target;
        if let Some(Phase::Starting(attempt)) = self.chat_ui.phases.get_mut(&target.card)
            && attempt.target == target
        {
            if !matches!(&attempt.observed_notices, Some(ReadyState::Loading { ticket: pending, launch, actor, .. }) if *pending == ticket && *actor == loaded.actor && *launch == loaded.launch)
            {
                return Task::none();
            }
            attempt.observed_notices = Some(ReadyState::Complete(loaded));
        }
        // This remains the sole consumer of Ready, including abandoned-start
        // retirement and exact descriptor/account/upgrade admission checks.
        self.handle_chat_ui(Action::Ready(target, ready))
    }

    pub(super) fn capture_closing_observed_notices(&mut self, card: CardId) {
        let runtime = match self.cards.get_mut(&card) {
            Some(CardRuntime::Chat(runtime)) => runtime.as_mut(),
            _ => match self.chat_ui.parked.get_mut(&card) {
                Some(runtime) => runtime.as_mut(),
                None => return,
            },
        };
        self.chat_ui
            .closing_notices
            .append(&mut runtime.observed_notices.retired);
        if let Some(batch) = runtime.observed_notices.batch.take() {
            self.chat_ui.closing_notices.push(Capture {
                target: runtime.target,
                batch,
            });
        }
    }

    pub(super) fn finish_bound_observed_notices(&mut self, ticket: Ticket, loaded: Arc<Loaded>) {
        for runtime in self
            .cards
            .values_mut()
            .filter_map(|card| match card {
                CardRuntime::Chat(runtime) => Some(runtime.as_mut()),
                _ => None,
            })
            .chain(self.chat_ui.parked.values_mut().map(Box::as_mut))
        {
            if runtime
                .observed_notices
                .batch
                .as_ref()
                .is_some_and(|batch| batch.owns(ticket, &loaded))
            {
                runtime.observed_notices.batch.as_mut().unwrap().pending = None;
                if same_owner(
                    &runtime.session.launch,
                    runtime.session.id,
                    &loaded.launch,
                    loaded.actor,
                ) {
                    runtime.restore_observed_notices(loaded);
                }
                return;
            }
        }
        if let Some(capture) = self
            .chat_ui
            .closing_notices
            .iter_mut()
            .find(|capture| capture.batch.owns(ticket, &loaded))
        {
            capture.batch.pending = None;
            capture.batch.binding = Some(loaded.scope.clone());
            capture.batch.retryable = loaded.retryable;
            capture.batch.reported = false;
            let offered = capture.batch.retry_offer.clone();
            capture
                .batch
                .adopt_revisions(&loaded.records, offered.as_deref());
        }
    }

    pub(super) fn finish_saved_observed_notices(
        &mut self,
        ticket: Ticket,
        loaded: Arc<Loaded>,
        saved: Result<(), SaveFailure>,
    ) {
        let batches = self
            .cards
            .values_mut()
            .filter_map(|card| match card {
                CardRuntime::Chat(runtime) => runtime.observed_notices.batch.as_mut(),
                _ => None,
            })
            .chain(
                self.chat_ui
                    .parked
                    .values_mut()
                    .filter_map(|runtime| runtime.observed_notices.batch.as_mut()),
            )
            .chain(
                self.chat_ui
                    .closing_notices
                    .iter_mut()
                    .map(|capture| &mut capture.batch),
            );
        for batch in batches {
            if !batch.owns(ticket, &loaded) {
                continue;
            }
            let pending = batch.pending.take().unwrap();
            match (&saved, &loaded.scope) {
                (Ok(()), Ok(_)) => {
                    batch.binding = Some(loaded.scope.clone());
                    batch.adopt_revisions(&loaded.records, pending.offered.as_deref());
                    batch.retry_offer = None;
                    batch.reported = false;
                    batch.retryable = true;
                }
                _ => {
                    // A successful write can mint a token even when its readback
                    // fails. Drop old authority, retain the exact offered bytes,
                    // and obtain another durable receipt before the next anchor.
                    if saved.is_ok() {
                        for record in &mut batch.records {
                            record.revision = 0;
                        }
                    }
                    let conflicting_proof = saved.is_err()
                        && pending.offered.as_ref().is_some_and(|offered| {
                            offered.iter().any(|offered| {
                                loaded.records.iter().any(|stored| {
                                    same_notice(batch.launch.provider, offered, stored)
                                        && offered.after != stored.after
                                        && (offered.revision == 0
                                            || offered.revision != stored.revision)
                                })
                            })
                        });
                    batch.retry_offer = pending.offered;
                    batch.binding = Some(Err(saved.as_ref().err().map(|error| error.message.clone()).unwrap_or_else(|| {
                        format!("Capacity details were saved, but their saved revision could not be verified: {}", loaded.scope.as_ref().unwrap_err())
                    })));
                    batch.retryable = loaded.retryable
                        && saved.as_ref().err().is_none_or(|error| error.retryable)
                        && !conflicting_proof;
                    batch.dirty = true;
                    batch.reported = false;
                }
            }
            return;
        }
    }

    pub(in crate::app) fn retry_observed_chat_notices(&mut self) {
        let batches = self
            .cards
            .values_mut()
            .filter_map(|card| match card {
                CardRuntime::Chat(runtime) => runtime.observed_notices.batch.as_mut(),
                _ => None,
            })
            .chain(
                self.chat_ui
                    .parked
                    .values_mut()
                    .filter_map(|runtime| runtime.observed_notices.batch.as_mut()),
            )
            .chain(
                self.chat_ui
                    .closing_notices
                    .iter_mut()
                    .map(|capture| &mut capture.batch),
            );
        for batch in batches {
            if batch.reported && batch.retryable {
                batch.reported = false;
                if matches!(batch.binding, Some(Err(_))) {
                    batch.binding = None;
                }
            }
        }
    }

    /// Persistent Notices entry, independent of writer health: a failed read or
    /// scope attestation may never have offered a write to the session writer.
    pub(in crate::app) fn observed_capacity_notice_problem(&self) -> Option<(&'static str, bool)> {
        let batches = self
            .cards
            .values()
            .filter_map(|card| match card {
                CardRuntime::Chat(runtime) => runtime.observed_notices.batch.as_ref(),
                _ => None,
            })
            .chain(
                self.chat_ui
                    .parked
                    .values()
                    .filter_map(|runtime| runtime.observed_notices.batch.as_ref()),
            )
            .chain(
                self.chat_ui
                    .closing_notices
                    .iter()
                    .map(|capture| &capture.batch),
            );
        let mut unsupported = false;
        for batch in batches {
            let has_details = !batch.records.is_empty() || batch.retry_offer.is_some();
            if self.observed_notice_storage_unavailable()
                && (has_details || (matches!(batch.binding, Some(Err(_))) && batch.retryable))
            {
                return Some((
                    "Earlier capacity-error details cannot be loaded or saved because session storage is unavailable. Keep this window open to retain any current full details.",
                    false,
                ));
            }
            if matches!(batch.binding, Some(Err(_))) || batch.reported {
                if batch.retryable {
                    return Some((
                        "Earlier capacity-error details could not be loaded, saved, or their saved revision verified. Retry save if storage is available again. Keep this window open while details are pending.",
                        true,
                    ));
                }
                unsupported |= has_details;
            } else if batch.retry_offer.is_some() && batch.pending.is_some() {
                return Some((
                    "Retrying the earlier capacity-error details. Keep this window open until saving finishes.",
                    false,
                ));
            }
        }
        unsupported.then_some(("Earlier capacity-error details cannot be restored or saved safely for this conversation. The retained full details remain available while this window stays open.", false))
    }

    /// The global update postlude calls this. Idle frames inspect only small
    /// flags; filesystem work, journal reads and String conversion run on workers.
    pub(in crate::app) fn persist_observed_chat_notices(&mut self) -> Task<Message> {
        let mut tasks = self
            .chat_ui
            .notice_ready_cleanup
            .drain(..)
            .map(|(target, ready)| {
                cosmic::task::message(Message::ChatUi(Action::Ready(target, ready)))
            })
            .collect::<Vec<_>>();
        // Retired owners need the same visibility and aggregate byte bound
        // even when no writer is available to accept their pending details.
        for runtime in self
            .cards
            .values_mut()
            .filter_map(|card| match card {
                CardRuntime::Chat(runtime) => Some(runtime.as_mut()),
                _ => None,
            })
            .chain(self.chat_ui.parked.values_mut().map(Box::as_mut))
        {
            self.chat_ui
                .closing_notices
                .append(&mut runtime.observed_notices.retired);
        }
        if self.observed_notice_storage_unavailable() {
            if self.bound_closed_observed_notices() {
                self.status = Some(TransientNotice::warning(
                    "Earlier capacity details exceeded the pending-save limit and could not all be retained for retry.",
                ));
            }
            return Task::batch(tasks);
        }
        let writer = self.session.as_ref().unwrap();
        let mut failed = false;
        for runtime in self
            .cards
            .values_mut()
            .filter_map(|card| match card {
                CardRuntime::Chat(runtime) => Some(runtime.as_mut()),
                _ => None,
            })
            .chain(self.chat_ui.parked.values_mut().map(Box::as_mut))
        {
            let Some(batch) = runtime.observed_notices.batch.take() else {
                continue;
            };
            let mut capture = Capture {
                target: runtime.target,
                batch,
            };
            match start_batch(writer, &mut capture) {
                Ok(Some(task)) => tasks.push(task),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "cannot save observed capacity notices");
                    failed = true;
                    capture.batch.reported = true;
                }
            }
            runtime.observed_notices.batch = Some(capture.batch);
        }
        for capture in &mut self.chat_ui.closing_notices {
            match start_batch(writer, capture) {
                Ok(Some(task)) => tasks.push(task),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%error, "cannot save closed conversation capacity notices");
                    failed = true;
                    capture.batch.reported = true;
                }
            }
        }
        let over_budget = self.bound_closed_observed_notices();
        if over_budget {
            self.status = Some(TransientNotice::warning(
                "Earlier capacity details exceeded the pending-save limit and could not all be retained for retry.",
            ));
        } else if failed {
            self.status = Some(TransientNotice::warning(
                "Earlier capacity details could not be loaded or saved. Open Notices for recovery information.",
            ));
        }
        Task::batch(tasks)
    }

    fn bound_closed_observed_notices(&mut self) -> bool {
        let mut over_budget = false;
        let mut remaining = crate::persistence::observed_notices::MAX_GLOBAL_BYTES;
        self.chat_ui.closing_notices.retain(|capture| {
            if capture.batch.pending.is_none() && !capture.batch.dirty {
                return false;
            }
            let bytes = retained_capture_bytes(capture);
            if bytes > remaining {
                over_budget = true;
                return false;
            }
            remaining -= bytes;
            true
        });
        over_budget
    }

    fn observed_notice_storage_unavailable(&self) -> bool {
        self.session.is_none()
            || self.save_health.as_ref().is_some_and(|health| {
                matches!(
                    health.health,
                    crate::session::SessionSaveHealth::WriterUnavailable
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FIRST: &str = "11111111-1111-4111-8111-111111111111";
    const SECOND: &str = "22222222-2222-4222-8222-222222222222";
    const CAPACITY: &str = "Selected model is at capacity. Please try a different model.";

    fn text(id: &str, role: chat::Role, body: &str) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: id.into(),
            role,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: chat::Phase::Commentary,
            },
        })
    }

    fn fixture() -> (Runtime, Arc<ClientHandle>, tempfile::TempDir) {
        fixture_for(Provider::Codex)
    }

    fn fixture_for(provider: Provider) -> (Runtime, Arc<ClientHandle>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let mut session = fresh_chat_session(
            provider,
            TerminalId::new(),
            directory.path().into(),
            &Account::system(provider),
            false,
        )
        .unwrap();
        session.launch.session_id = Some(FIRST.into());
        if let Some(variable) = crate::account::home_variable(provider) {
            session.launch.recipe.environment.insert(
                variable.into(),
                directory.path().to_string_lossy().into_owned(),
            );
        }
        let (first, next) = if provider == Provider::Codex {
            ("first/input/0".into(), "next/input/0".into())
        } else {
            (format!("{FIRST}:0"), format!("{SECOND}:0"))
        };
        let handle = Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                session_id: Some(FIRST.into()),
                items: vec![
                    text(&first, chat::Role::User, "Investigate"),
                    text(&next, chat::Role::User, "."),
                ],
                ..Snapshot::default()
            },
        ));
        let runtime = Runtime::new(
            Target {
                card: CardId::new(),
                generation: 1,
            },
            session,
            Connection::Live(handle.clone()),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        (runtime, handle, directory)
    }

    fn scope(launch: &chat::Launch) -> Scope {
        Scope::resolve_local(launch, "test-owner-domain".into()).unwrap()
    }

    fn note(after: &str, revision: u64) -> Notice {
        Notice {
            message: Some("first/input/0".into()),
            after: Some(after.into()),
            text: CAPACITY.into(),
            revision,
        }
    }

    fn loaded(runtime: &Runtime, records: Vec<Notice>) -> Arc<Loaded> {
        Arc::new(Loaded::from_records(
            runtime.session.launch.clone(),
            runtime.session.id,
            Ok(scope(&runtime.session.launch)),
            records,
        ))
    }

    fn put(app: &mut Cagents, runtime: Runtime) -> Target {
        let target = runtime.target;
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        target
    }

    fn assert_retry_is_reachable(app: &mut Cagents) {
        assert_eq!(app.notice_control_severity(), Some(NoticeSeverity::Error));
        app.core.set_keyboard_nav(true);
        drop(app.update_chrome(super::super::chrome_ui::Action::ToggleNotices));
        let tab = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Tab),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Tab),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        for _ in 0..12 {
            let task = app.chrome_key_event(&tab).unwrap();
            if crate::app::tests::focused_element(
                app.observed_capacity_notice_view().unwrap(),
                task,
            ) == Some(widget::Id::new("capacity-notices.retry"))
            {
                return;
            }
        }
        panic!("the rendered capacity notice must offer keyboard-reachable Retry saving");
    }

    fn assert_before_next(runtime: &mut Runtime) {
        preparation::drain_fixture(runtime);
        let key = runtime
            .failure_ui
            .incidents
            .iter()
            .find(|incident| incident.kind == Some(FailureKind::Capacity))
            .unwrap()
            .key
            .clone();
        runtime.failure_ui.expanded.insert(key.clone());
        runtime.cache_failure_preview(&key);
        let (catalog, _) = runtime.selection_document();
        assert!(
            catalog
                .position(failure::diagnostic_preview_identity(&key))
                .unwrap()
                < catalog
                    .position(selection::logical_identity("next/input/0", &[2]))
                    .unwrap()
        );
    }

    #[test]
    fn only_capacity_is_recorded_and_anchor_changes_reuse_the_body_and_revision() {
        let (mut runtime, _, _directory) = fixture();
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        let record = runtime.observed_notices.batch.as_ref().unwrap().records[0].clone();
        runtime.restore_observed_notices(loaded(&runtime, vec![note("first/input/0", 7)]));
        runtime.note_at(
            Some("first/input/0".into()),
            Some("later-work".into()),
            CAPACITY.into(),
        );
        let batch = runtime.observed_notices.batch.as_ref().unwrap();
        assert_eq!(batch.records[0].revision, 7);
        assert_eq!(batch.records[0].after.as_deref(), Some("later-work"));
        assert!(Arc::ptr_eq(&record.text, &batch.records[0].text));
        runtime.note("Authentication required. Please sign in again.".into());
        runtime.note("You've hit your limit".into());
        runtime.note("Connection refused".into());
        assert_eq!(
            runtime
                .observed_notices
                .batch
                .as_ref()
                .unwrap()
                .records
                .len(),
            1
        );
        assert_eq!(
            runtime.failures.len(),
            4,
            "other failure presentation remains intact"
        );
    }

    #[test]
    fn restored_notes_regroup_unchanged_history_and_keep_tool_disclosure_and_find_current() {
        let (mut runtime, handle, _directory) = fixture();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.items.push(Arc::new(chat::Item {
            id: "tool".into(),
            role: chat::Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Tool {
                name: "Bash".into(),
                input: "needle".into(),
                output: "needle".into(),
                status: chat::ToolStatus::Succeeded,
            },
        }));
        handle.publish_snapshot_for_test(snapshot);
        runtime.refresh();
        preparation::drain_fixture(&mut runtime);
        assert_eq!(runtime.turns.len(), 1);
        runtime.opened.insert("first/input/0".into(), true);
        runtime.find = Some("needle".into());
        let epoch = runtime.find_navigation.document_epoch();
        runtime.restore_observed_notices(loaded(&runtime, vec![note("first/input/0", 1)]));
        assert_eq!(runtime.turns.len(), 2);
        assert_eq!(runtime.opened.get("next/input/0"), Some(&true));
        assert_ne!(epoch, runtime.find_navigation.document_epoch());
        assert_eq!(runtime.find.as_deref(), Some("needle"));
        assert_before_next(&mut runtime);
    }

    #[test]
    fn changed_session_and_account_retire_capacity_ownership_before_reused_item_ids() {
        for account_change in [false, true] {
            let (mut runtime, _, directory) = fixture();
            runtime.note_at(
                Some("first/input/0".into()),
                Some("first/input/0".into()),
                CAPACITY.into(),
            );
            runtime.restore_observed_notices(loaded(&runtime, vec![note("first/input/0", 1)]));
            runtime.note("Authentication required. Please sign in again.".into());
            let original = runtime.session.launch.clone();
            let mut next = original.clone();
            if account_change {
                let home = directory.path().join("other-account");
                std::fs::create_dir(&home).unwrap();
                next.recipe
                    .environment
                    .insert("CODEX_HOME".into(), home.to_string_lossy().into_owned());
            } else {
                next.session_id = Some(SECOND.into());
            }
            runtime.retire_observed_notices(Some((&next, runtime.session.id)));
            runtime.session.launch = next;
            runtime.restore_observed_notices(loaded(&runtime, Vec::new()));
            assert!(
                !runtime
                    .failures
                    .iter()
                    .any(|note| capacity(Provider::Codex, &note.text))
            );
            assert!(
                runtime
                    .failures
                    .iter()
                    .any(|note| note.text.contains("Authentication"))
            );
            assert_eq!(runtime.observed_notices.retired.len(), 1);
            assert_eq!(runtime.observed_notices.retired[0].batch.launch, original);
            assert!(
                runtime
                    .observed_notices
                    .batch
                    .as_ref()
                    .unwrap()
                    .records
                    .is_empty()
            );
        }
    }

    #[test]
    fn binding_completion_follows_its_batch_across_retarget_and_close() {
        let (mut runtime, _, _directory) = fixture();
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        let ticket = Ticket::new(runtime.target);
        runtime.observed_notices.batch.as_mut().unwrap().pending = Some(Pending {
            ticket,
            offered: None,
        });
        let answer = loaded(&runtime, Vec::new());
        runtime.retarget(Target {
            generation: 99,
            ..runtime.target
        });
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = put(&mut app, runtime);
        app.capture_closing_observed_notices(target.card);
        app.cards.remove(&target.card);
        app.finish_bound_observed_notices(ticket, answer);
        let capture = &app.chat_ui.closing_notices[0];
        assert!(capture.batch.pending.is_none());
        assert!(matches!(capture.batch.binding, Some(Ok(_))));
        assert_eq!(capture.batch.records[0].text.as_ref(), CAPACITY);
        assert!(capture.batch.dirty);
        assert!(!app.cards.contains_key(&target.card));
    }

    #[test]
    fn stale_load_cannot_bind_new_owner_and_clears_the_retired_pending_job() {
        let (mut runtime, _, _directory) = fixture();
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        let old_ticket = Ticket::new(runtime.target);
        runtime.observed_notices.batch.as_mut().unwrap().pending = Some(Pending {
            ticket: old_ticket,
            offered: None,
        });
        let answer = loaded(&runtime, Vec::new());
        let mut next = runtime.session.launch.clone();
        next.session_id = Some(SECOND.into());
        runtime.retire_observed_notices(Some((&next, runtime.session.id)));
        runtime.session.launch = next;
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        let new_ticket = Ticket::new(runtime.target);
        runtime.observed_notices.batch.as_mut().unwrap().pending = Some(Pending {
            ticket: new_ticket,
            offered: None,
        });
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = put(&mut app, runtime);
        let old = app
            .chat_runtime_mut(target)
            .unwrap()
            .observed_notices
            .retired
            .pop()
            .unwrap();
        app.chat_ui.closing_notices.push(old);
        app.finish_bound_observed_notices(old_ticket, answer);
        assert!(app.chat_ui.closing_notices[0].batch.pending.is_none());
        assert_eq!(
            app.chat_runtime_mut(target)
                .unwrap()
                .observed_notices
                .batch
                .as_ref()
                .unwrap()
                .pending
                .as_ref()
                .unwrap()
                .ticket,
            new_ticket
        );
    }

    #[test]
    fn save_receipt_advances_only_its_own_anchor_before_a_closed_followup() {
        let (mut runtime, _, _directory) = fixture();
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        runtime.restore_observed_notices(loaded(&runtime, vec![note("first/input/0", 3)]));
        let ticket = Ticket::new(runtime.target);
        let batch = runtime.observed_notices.batch.as_mut().unwrap();
        batch.pending = Some(Pending {
            ticket,
            offered: Some(batch.records.clone().into()),
        });
        batch.dirty = false;
        runtime.note_at(
            Some("first/input/0".into()),
            Some("later-work".into()),
            CAPACITY.into(),
        );
        let answer = loaded(&runtime, vec![note("first/input/0", 4)]);
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = put(&mut app, runtime);
        app.capture_closing_observed_notices(target.card);
        app.cards.remove(&target.card);
        app.finish_saved_observed_notices(ticket, answer, Ok(()));
        let batch = &app.chat_ui.closing_notices[0].batch;
        assert_eq!(batch.records[0].revision, 4);
        assert_eq!(batch.records[0].after.as_deref(), Some("later-work"));
        assert!(batch.dirty);
        assert!(batch.pending.is_none());
    }

    #[test]
    fn unreadable_journal_reports_even_when_there_are_no_current_failures() {
        let (mut runtime, _, directory) = fixture();
        runtime.restore_observed_notices(Arc::new(Loaded::failed(
            runtime.session.launch.clone(),
            runtime.session.id,
            "fixture read refusal".into(),
        )));
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("notice-test"),
        )
        .unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.session = Some(writer);
        let target = put(&mut app, runtime);
        drop(app.persist_observed_chat_notices());
        assert!(app.status.is_some());
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .observed_notices
                .batch
                .as_ref()
                .unwrap()
                .reported
        );
        assert_retry_is_reachable(&mut app);
        let retry = app.update(Message::RetrySessionSave);
        let batch = app
            .chat_runtime_mut(target)
            .unwrap()
            .observed_notices
            .batch
            .as_ref()
            .unwrap();
        assert!(!batch.reported);
        assert!(batch.binding.is_none());
        assert!(batch.pending.is_some());
        drop(retry);
    }
    fn first_output(task: Task<Message>) -> Message {
        use cosmic::iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let mut stream =
                        cosmic::iced::runtime::task::into_stream(task).expect("worker task");
                    while let Some(action) = stream.next().await {
                        if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) =
                            action
                        {
                            return message;
                        }
                    }
                    panic!("worker emitted no completion");
                })
                .await
                .expect("bounded journal task completion")
            })
    }

    fn assert_no_task_actions(task: Task<Message>) {
        use cosmic::iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                if let Some(mut stream) = cosmic::iced::runtime::task::into_stream(task) {
                    let action = tokio::time::timeout(Duration::from_secs(1), stream.next())
                        .await
                        .expect("ignored action must finish promptly");
                    assert!(action.is_none(), "ignored action must not emit work");
                }
            });
    }

    #[test]
    fn model_and_permission_options_keep_the_capacity_owner_and_pending_revision() {
        let (mut runtime, _, _directory) = fixture();
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        runtime.restore_observed_notices(loaded(&runtime, vec![note("first/input/0", 4)]));
        let ticket = Ticket::new(runtime.target);
        runtime.observed_notices.batch.as_mut().unwrap().pending = Some(Pending {
            ticket,
            offered: None,
        });
        let answer = loaded(&runtime, vec![note("first/input/0", 4)]);
        let original = runtime.session.launch.clone();
        let mut changed = runtime.session.clone();
        changed.launch.recipe.arguments.extend([
            "--model".into(),
            "another-model".into(),
            "--permission-mode=default".into(),
        ]);
        runtime.prepare_observed_session(&changed);
        runtime.session = changed;
        assert!(runtime.observed_notices.retired.is_empty());
        assert_eq!(
            runtime.observed_notices.batch.as_ref().unwrap().launch,
            original
        );
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = put(&mut app, runtime);
        app.finish_bound_observed_notices(ticket, answer);
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(
            runtime
                .observed_notices
                .batch
                .as_ref()
                .unwrap()
                .pending
                .is_none()
        );
        assert_eq!(
            runtime.observed_notices.batch.as_ref().unwrap().records[0].revision,
            4
        );
        assert_before_next(runtime);
    }

    #[test]
    fn only_the_same_actor_can_promote_unnamed_observations_to_a_confirmed_thread() {
        for same_actor in [false, true] {
            let (mut runtime, _, _directory) = fixture();
            runtime.session.launch.session_id = None;
            runtime.note_at(
                Some("first/input/0".into()),
                Some("first/input/0".into()),
                CAPACITY.into(),
            );
            let mut confirmed = runtime.session.clone();
            confirmed.launch.session_id = Some(SECOND.into());
            if !same_actor {
                confirmed.id = TerminalId::new();
            }
            runtime.prepare_observed_session(&confirmed);
            runtime.session = confirmed;
            if same_actor {
                assert!(runtime.observed_notices.retired.is_empty());
                assert_eq!(
                    runtime
                        .observed_notices
                        .batch
                        .as_ref()
                        .unwrap()
                        .launch
                        .session_id
                        .as_deref(),
                    Some(SECOND)
                );
                assert!(runtime.failures.iter().any(|note| note.text == CAPACITY));
            } else {
                assert_eq!(runtime.observed_notices.retired.len(), 1);
                assert!(runtime.observed_notices.batch.is_none());
                assert!(!runtime.failures.iter().any(|note| note.text == CAPACITY));
            }
        }
    }

    #[test]
    fn real_writer_receipt_roundtrip_retains_full_capacity_details_before_reopened_first_grouping()
    {
        let (mut runtime, handle, directory) = fixture();
        let raw = format!(
            "{CAPACITY}\n{}RAW_DIAGNOSTIC_END",
            "exact λ diagnostic bytes ".repeat(8192)
        );
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            raw.clone(),
        );
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("roundtrip"),
        )
        .unwrap();
        app.session = Some(writer);
        let source = runtime.session.clone();
        let mut source_snapshot = (*handle.snapshot()).clone();
        source_snapshot.exited = true;
        source_snapshot.read_only_reason = Some("Saved history; no provider was started".into());
        let old_target = put(&mut app, runtime);
        let task = app.persist_observed_chat_notices();
        let Message::ChatUi(Action::ObservedNoticesBound(ticket, loaded)) = first_output(task)
        else {
            panic!("binding completion")
        };
        app.finish_bound_observed_notices(ticket, loaded);
        let task = app.persist_observed_chat_notices();
        let Message::ChatUi(Action::ObservedNoticesSaved(ticket, loaded, saved)) =
            first_output(task)
        else {
            panic!("durable completion")
        };
        assert_eq!(saved, Ok(()));
        app.finish_saved_observed_notices(ticket, loaded, saved);
        assert_eq!(
            app.chat_runtime_mut(old_target)
                .unwrap()
                .observed_notices
                .batch
                .as_ref()
                .unwrap()
                .records[0]
                .revision,
            1
        );
        app.capture_closing_observed_notices(old_target.card);
        app.cards.remove(&old_target.card);
        drop(app.persist_observed_chat_notices());
        assert!(app.chat_ui.closing_notices.is_empty());

        let mut resumed = source;
        resumed.id = TerminalId::new();
        resumed.proof = None;
        drop(app.insert_chat_card(resumed.clone(), None, None));
        let card = app.workspace.focused().unwrap();
        assert_ne!(card, old_target.card);
        // Ready suppresses transcript bodies for offscreen panes. Establish
        // the viewport before attaching so this checks the first visible
        // grouping, rather than an intentionally unpresented hidden runtime.
        app.workspace.resize_viewport(1_600.0);
        assert_eq!(
            app.workspace.visibility(card),
            Some(VisibilityClass::Visible)
        );
        assert!(app.workspace_surfaces_visible());
        let target = app.chat_ui.attempt(card).unwrap().target;
        let handle = Arc::new(ClientHandle::fixture(resumed, source_snapshot));
        let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle.clone())))));
        let task = app.handle_chat_ui(Action::Ready(target, ready.clone()));
        assert!(matches!(
            app.chat_ui.attempt(card).unwrap().observed_notices,
            Some(ReadyState::Loading { .. })
        ));
        let pending = match &app.chat_ui.attempt(card).unwrap().observed_notices {
            Some(ReadyState::Loading { ready, .. }) => ready.clone(),
            _ => unreachable!(),
        };
        let duplicate = app.handle_chat_ui(Action::Ready(target, pending.clone()));
        assert_no_task_actions(duplicate);
        assert!(
            pending.0.lock().unwrap().is_some(),
            "duplicate cannot bypass hydration"
        );
        let Message::ChatUi(Action::ObservedNoticesReady(ticket, ready, loaded)) =
            first_output(task)
        else {
            panic!("Ready journal completion")
        };
        drop(app.finish_ready_observed_notices(ticket, ready, loaded));
        assert!(app.chat_ui.attempt(card).is_none());
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(Arc::ptr_eq(runtime.handle().unwrap(), &handle));
        assert!(runtime.visible.load(Ordering::Relaxed));
        assert_eq!(runtime.snapshot.items.len(), 2);
        assert_eq!(runtime.turns.len(), 2);
        assert_before_next(runtime);
        let key = runtime.failure_ui.incidents[0].key.clone();
        assert_eq!(runtime.failure_details(&key).as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn closed_ready_drops_the_phase_payload_and_keeps_original_abandoned_cleanup() {
        let (runtime, handle, directory) = fixture();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.exited = true;
        snapshot.read_only_reason = Some("Saved history; no provider was started".into());
        handle.publish_snapshot_for_test(snapshot);
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("closed-ready"),
        )
        .unwrap();
        app.session = Some(writer);
        let session = runtime.session.clone();
        drop(app.insert_chat_card(session, None, None));
        let card = app.workspace.focused().unwrap();
        let target = app.chat_ui.attempt(card).unwrap().target;
        let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle)))));
        let task = app.handle_chat_ui(Action::Ready(target, ready.clone()));
        let pending = match &app.chat_ui.attempt(card).unwrap().observed_notices {
            Some(ReadyState::Loading { ready, .. }) => ready.clone(),
            _ => panic!("saved-history hydration"),
        };
        assert!(
            app.chat_ui
                .attempt(card)
                .unwrap()
                .observed_notices
                .is_some()
        );
        app.chat_discard(card);
        app.cards.remove(&card);
        assert!(app.chat_ui.attempt(card).is_none());
        assert!(app.chat_ui.abandoned.contains_key(&target));
        let cleanup = first_output(app.persist_observed_chat_notices());
        assert!(
            matches!(&cleanup, Message::ChatUi(Action::Ready(cleanup_target, _)) if *cleanup_target == target)
        );
        drop(app.handle_chat_ui(match cleanup {
            Message::ChatUi(action) => action,
            _ => unreachable!(),
        }));
        assert!(
            pending.0.lock().unwrap().is_none(),
            "Close cannot wait on optional journal IO to retire Ready"
        );
        assert!(!app.chat_ui.abandoned.contains_key(&target));
        let Message::ChatUi(Action::ObservedNoticesReady(ticket, ready, loaded)) =
            first_output(task)
        else {
            panic!("late Ready completion")
        };
        drop(app.finish_ready_observed_notices(ticket, ready.clone(), loaded));
        assert!(ready.0.lock().unwrap().is_none());
        assert!(!app.chat_ui.abandoned.contains_key(&target));
        assert!(!app.cards.contains_key(&card));
        assert!(app.chat_ui.closing_notices.is_empty());
    }
    #[test]
    fn failed_binding_after_close_keeps_exact_bytes_for_explicit_retry() {
        let (mut runtime, _, directory) = fixture();
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        let ticket = Ticket::new(runtime.target);
        runtime.observed_notices.batch.as_mut().unwrap().pending = Some(Pending {
            ticket,
            offered: None,
        });
        let answer = Arc::new(Loaded::failed(
            runtime.session.launch.clone(),
            runtime.session.id,
            "fixture unavailable".into(),
        ));
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("state.sqlite3"),
            crate::model::BootId::new("closed-failure"),
        )
        .unwrap();
        app.session = Some(writer);
        let target = put(&mut app, runtime);
        app.capture_closing_observed_notices(target.card);
        app.cards.remove(&target.card);
        app.finish_bound_observed_notices(ticket, answer);
        drop(app.persist_observed_chat_notices());
        assert!(app.status.is_some());
        assert_eq!(app.chat_ui.closing_notices.len(), 1);
        assert_eq!(
            app.chat_ui.closing_notices[0].batch.records[0]
                .text
                .as_ref(),
            CAPACITY
        );
        assert!(app.chat_ui.closing_notices[0].batch.reported);
        assert_retry_is_reachable(&mut app);
        let retry = app.update(Message::RetrySessionSave);
        assert!(app.chat_ui.closing_notices[0].batch.binding.is_none());
        assert!(!app.chat_ui.closing_notices[0].batch.reported);
        assert!(app.chat_ui.closing_notices[0].batch.pending.is_some());
        drop(retry);
    }
    #[test]
    fn paged_message_part_adopts_the_durable_carrier_and_revision_without_duplicate_history() {
        let (mut runtime, _, _directory) = fixture();
        runtime.note_at(
            Some("first/input/7".into()),
            Some("first/input/7".into()),
            CAPACITY.into(),
        );
        runtime.restore_observed_notices(loaded(&runtime, vec![note("first/input/7", 8)]));
        let records = &runtime.observed_notices.batch.as_ref().unwrap().records;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].message.as_deref(), Some("first/input/0"));
        assert_eq!(records[0].after.as_deref(), Some("first/input/7"));
        assert_eq!(records[0].revision, 8);
        assert_eq!(runtime.failures.len(), 1);
    }

    #[test]
    fn durable_retry_and_failed_readback_recover_before_a_newer_anchor_can_save() {
        let (mut runtime, _, directory) = fixture();
        let path = directory.path().join("receipt-recovery.sqlite3");
        let (writer, _) = crate::session::SessionRuntime::open_with_options(
            &path,
            crate::model::BootId::new("receipt-recovery"),
            crate::persistence::StoreOptions {
                busy_timeout: Duration::from_millis(1),
            },
        )
        .unwrap();
        let store = writer.operation_store();
        let initial = Arc::new(load(
            &store,
            runtime.session.launch.clone(),
            runtime.session.id,
            true,
        ));
        let scope = initial.scope.clone().unwrap();
        runtime.restore_observed_notices(initial);
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.save_health = Some(crate::app::SaveHealthState::new(path.clone()));
        let health = writer.health_receiver();
        app.session = Some(writer);
        let target = put(&mut app, runtime);
        let blocker = rusqlite::Connection::open(&path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let task = app.persist_observed_chat_notices();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(event) = health.try_recv()
                && event.quarantined.iter().any(|write| {
                    matches!(write.subject, crate::session::WriteSubject::ObservedNotices { card } if card == target.card)
                        && write.category == crate::session::SaveFailureCategory::Busy
                })
            {
                assert!(matches!(event.health, crate::session::SessionSaveHealth::Healthy | crate::session::SessionSaveHealth::Recovered { .. }),
                    "optional journal IO cannot block ordinary custody admission");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "actual writer must encounter the SQLite lock"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        blocker.execute_batch("ROLLBACK").unwrap();
        loop {
            if store
                .load_observed_notices(&scope)
                .unwrap()
                .first()
                .is_some_and(|note| note.revision == 1)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "automatic retry must commit the original receipt"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        // The writer has committed, but the receipt worker has not been polled.
        // Remove its captured home so the real readback fails after success.
        let launch = app.chat_runtime_mut(target).unwrap().session.launch.clone();
        let home = PathBuf::from(&launch.recipe.environment["CODEX_HOME"]);
        let held_home = home.with_extension("temporarily-unavailable");
        std::fs::rename(&home, &held_home).unwrap();
        app.chat_runtime_mut(target).unwrap().note_at(
            Some("first/input/0".into()),
            Some("later-work".into()),
            CAPACITY.into(),
        );
        let Message::ChatUi(Action::ObservedNoticesSaved(ticket, loaded, saved)) =
            first_output(task)
        else {
            panic!("durable receipt")
        };
        assert_eq!(
            saved,
            Ok(()),
            "a transient Busy must not consume the receipt"
        );
        assert!(loaded.scope.is_err(), "actual readback must fail");
        app.finish_saved_observed_notices(ticket, loaded, saved);
        drop(app.persist_observed_chat_notices());
        let batch = app
            .chat_runtime_mut(target)
            .unwrap()
            .observed_notices
            .batch
            .as_ref()
            .unwrap();
        assert_eq!(batch.records[0].revision, 0);
        assert_eq!(batch.records[0].after.as_deref(), Some("later-work"));
        assert!(batch.retry_offer.is_some());
        assert!(batch.reported);
        assert_retry_is_reachable(&mut app);
        std::fs::rename(&held_home, &home).unwrap();
        let retry = app.update(Message::RetrySessionSave);
        let Message::ChatUi(Action::ObservedNoticesBound(ticket, loaded)) = first_output(retry)
        else {
            panic!("retry scope binding")
        };
        app.finish_bound_observed_notices(ticket, loaded);
        // Reoffer the original endpoint idempotently, then use its verified
        // revision for the change which arrived while the receipt was pending.
        for expected in [1, 2] {
            let task = app.persist_observed_chat_notices();
            let Message::ChatUi(Action::ObservedNoticesSaved(ticket, loaded, saved)) =
                first_output(task)
            else {
                panic!("retry durable receipt")
            };
            assert_eq!(saved, Ok(()));
            app.finish_saved_observed_notices(ticket, loaded, saved);
            assert_eq!(
                app.chat_runtime_mut(target)
                    .unwrap()
                    .observed_notices
                    .batch
                    .as_ref()
                    .unwrap()
                    .records[0]
                    .revision,
                expected
            );
        }
        let saved = store.load_observed_notices(&scope).unwrap();
        assert_eq!(saved[0].after.as_deref(), Some("later-work"));
        assert_eq!(saved[0].revision, 2);
        assert!(app.observed_capacity_notice_problem().is_none());
    }

    #[test]
    fn stale_unversioned_local_anchor_cannot_borrow_a_newer_durable_revision() {
        let (mut runtime, _, directory) = fixture();
        let (writer, _) = crate::session::SessionRuntime::open(
            directory.path().join("stale.sqlite3"),
            crate::model::BootId::new("stale"),
        )
        .unwrap();
        let store = writer.operation_store();
        let empty = load(
            &store,
            runtime.session.launch.clone(),
            runtime.session.id,
            true,
        );
        let scope = empty.scope.unwrap();
        let newer = note("newer-work", 0);
        store.preserve_observed_notices(&scope, &[newer]).unwrap();
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        runtime.restore_observed_notices(Arc::new(load(
            &store,
            runtime.session.launch.clone(),
            runtime.session.id,
            true,
        )));
        assert_eq!(
            runtime.observed_notices.batch.as_ref().unwrap().records[0].revision,
            0
        );
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.session = Some(writer);
        put(&mut app, runtime);
        let task = app.persist_observed_chat_notices();
        let Message::ChatUi(Action::ObservedNoticesSaved(ticket, loaded, saved)) =
            first_output(task)
        else {
            panic!("stale anchor refusal")
        };
        assert!(saved.is_err());
        assert!(!saved.as_ref().unwrap_err().retryable);
        app.finish_saved_observed_notices(ticket, loaded, saved);
        drop(app.persist_observed_chat_notices());
        assert!(app.observed_capacity_notice_problem().is_some());
        assert!(
            app.observed_capacity_notice_problem()
                .is_some_and(|(_, retry)| !retry)
        );
        let saved = store.load_observed_notices(&scope).unwrap();
        assert_eq!(saved[0].after.as_deref(), Some("newer-work"));
        assert_eq!(saved[0].revision, 1);
    }

    #[test]
    fn unsupported_discovery_is_quiet_but_retained_details_and_missing_storage_are_explicit() {
        let (mut runtime, _, _directory) = fixture();
        let launch = runtime.session.launch.clone();
        runtime.restore_observed_notices(Arc::new(Loaded::unsupported(
            launch,
            runtime.session.id,
            "unattested".into(),
        )));
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = put(&mut app, runtime);
        assert!(app.observed_capacity_notice_problem().is_none());
        app.chat_runtime_mut(target).unwrap().note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        assert!(
            app.observed_capacity_notice_problem().is_some_and(
                |(guidance, retry)| !retry && guidance.contains("storage is unavailable")
            )
        );
        app.capture_closing_observed_notices(target.card);
        app.cards.remove(&target.card);
        drop(app.persist_observed_chat_notices());
        assert_eq!(app.chat_ui.closing_notices.len(), 1);
        assert_eq!(
            app.chat_ui.closing_notices[0].batch.records[0]
                .text
                .as_ref(),
            CAPACITY
        );
        assert!(app.observed_capacity_notice_view().is_some());
    }

    #[test]
    fn reconstructed_named_reader_waits_but_fresh_start_and_retained_reader_do_not() {
        for mode in ["named", "fresh", "retained", "different-owner"] {
            let (mut runtime, handle, directory) = fixture();
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (writer, _) = crate::session::SessionRuntime::open(
                directory.path().join("admission.sqlite3"),
                crate::model::BootId::new(mode),
            )
            .unwrap();
            app.session = Some(writer);
            let source = runtime.session.clone();
            drop(app.insert_chat_card(source.clone(), None, None));
            let card = app.workspace.focused().unwrap();
            let target = app.chat_ui.attempt(card).unwrap().target;
            if mode == "fresh" {
                let mut unnamed = source;
                unnamed.launch.session_id = None;
                app.card_state_mut(card).unwrap().kind = CardKind::Chat(unnamed.clone());
                let Some(Phase::Starting(attempt)) = app.chat_ui.phases.get_mut(&card) else {
                    unreachable!()
                };
                attempt.session = unnamed;
                attempt.intent = chat::Intent::Start;
            } else if mode == "retained" || mode == "different-owner" {
                runtime.retarget(target);
                if mode == "different-owner" {
                    runtime.session.launch.session_id = Some(SECOND.into());
                }
                put(&mut app, runtime);
            }
            let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle)))));
            let task = app.handle_chat_ui(Action::Ready(target, ready));
            if mode == "named" || mode == "different-owner" {
                assert!(app.chat_ui.attempt(card).is_some_and(|attempt| matches!(
                    attempt.observed_notices,
                    Some(ReadyState::Loading { .. })
                )));
                let Message::ChatUi(Action::ObservedNoticesReady(ticket, ready, loaded)) =
                    first_output(task)
                else {
                    panic!("named hydration")
                };
                drop(app.finish_ready_observed_notices(ticket, ready, loaded));
            } else {
                assert!(
                    app.chat_ui.attempt(card).is_none(),
                    "{mode} must not wait for optional history IO"
                );
                drop(task);
            }
            assert!(app.chat_runtime_mut(target).unwrap().handle().is_some());
        }
    }

    #[test]
    fn ready_history_deadline_is_explicit_and_late_results_cannot_reinstall_after_close() {
        use cosmic::iced::futures::StreamExt;
        for close in [false, true] {
            let (runtime, handle, directory) = fixture();
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (writer, _) = crate::session::SessionRuntime::open(
                directory.path().join("deadline.sqlite3"),
                crate::model::BootId::new("deadline"),
            )
            .unwrap();
            app.session = Some(writer);
            let launch = runtime.session.launch.clone();
            let actor = runtime.session.id;
            drop(app.insert_chat_card(runtime.session.clone(), None, None));
            let card = app.workspace.focused().unwrap();
            let target = app.chat_ui.attempt(card).unwrap().target;
            let ticket = Ticket::new(target);
            let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle)))));
            let Some(Phase::Starting(attempt)) = app.chat_ui.phases.get_mut(&card) else {
                unreachable!()
            };
            attempt.observed_notices = Some(ReadyState::Loading {
                ticket,
                launch: launch.clone(),
                actor,
                ready: ready.clone(),
            });
            let (release, blocked) = std::sync::mpsc::sync_channel(1);
            let (finished, done) = std::sync::mpsc::sync_channel(1);
            let late_scope = scope(&launch);
            let worker_launch = launch.clone();
            let task = ready_load_task(
                ticket,
                ready.clone(),
                launch,
                actor,
                Duration::from_millis(5),
                move || {
                    blocked.recv().unwrap();
                    let loaded = Loaded::from_records(
                        worker_launch,
                        actor,
                        Ok(late_scope),
                        vec![note("first/input/0", 1)],
                    );
                    finished.send(()).unwrap();
                    loaded
                },
            );
            if close {
                app.chat_discard(card);
                app.cards.remove(&card);
                let Message::ChatUi(cleanup) = first_output(app.persist_observed_chat_notices())
                else {
                    panic!("eager close cleanup")
                };
                drop(app.handle_chat_ui(cleanup));
                assert!(ready.0.lock().unwrap().is_none());
                assert!(!app.chat_ui.abandoned.contains_key(&target));
            }
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut stream = cosmic::iced::runtime::task::into_stream(task).unwrap();
            let message = executor.block_on(async {
                tokio::time::timeout(Duration::from_secs(1), async {
                    loop {
                        if let Some(cosmic::iced::runtime::Action::Output(cosmic::Action::App(
                            message,
                        ))) = stream.next().await
                        {
                            return message;
                        }
                    }
                })
                .await
                .unwrap()
            });
            let Message::ChatUi(Action::ObservedNoticesReady(ticket, ready, loaded)) = message
            else {
                panic!("deadline completion")
            };
            assert!(loaded.scope.as_ref().unwrap_err().contains("incomplete"));
            assert!(loaded.retryable);
            drop(app.finish_ready_observed_notices(ticket, ready, loaded));
            if !close {
                assert!(app.chat_ui.attempt(card).is_none());
                assert!(
                    app.observed_capacity_notice_problem()
                        .is_some_and(|(_, retry)| retry)
                );
                assert!(app.chat_runtime_mut(target).unwrap().failures.is_empty());
            }
            release.send(()).unwrap();
            done.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(
                executor.block_on(stream.next()).is_none(),
                "late blocking completion is inert"
            );
            assert_eq!(app.cards.contains_key(&card), !close);
        }
    }

    #[test]
    fn closed_capture_budget_charges_rotated_retry_and_pending_payloads_and_identity_bytes() {
        let (mut runtime, _, _directory) = fixture();
        runtime.note_at(
            Some("first/input/0".into()),
            Some("first/input/0".into()),
            CAPACITY.into(),
        );
        let mut capture = Capture {
            target: runtime.target,
            batch: runtime.observed_notices.batch.take().unwrap(),
        };
        let baseline = retained_capture_bytes(&capture);
        let mut old = note(&"old-endpoint".repeat(20), 0);
        old.message = Some("old-identity".repeat(30));
        old.text = format!("{CAPACITY}{}", "retained old body".repeat(100)).into();
        let minimum = old.text.len()
            + old.message.as_ref().unwrap().len()
            + old.after.as_ref().unwrap().len()
            + std::mem::size_of::<Notice>();
        capture.batch.retry_offer = Some(vec![old.clone()].into());
        capture.batch.pending = Some(Pending {
            ticket: Ticket::new(capture.target),
            offered: Some(vec![old].into()),
        });
        assert!(retained_capture_bytes(&capture) >= baseline + 2 * minimum);
    }

    #[test]
    fn unavailable_storage_drains_and_bounds_retired_owners_without_applying_them_to_current_owner()
    {
        for parked in [false, true] {
            let (mut runtime, _, directory) = fixture();
            let raw = format!("{CAPACITY}\n{}", "x".repeat(1024 * 1024));
            runtime.note_at(
                Some("first/input/0".into()),
                Some("first/input/0".into()),
                raw,
            );
            let mut next = runtime.session.launch.clone();
            next.session_id = Some(SECOND.into());
            runtime.retire_observed_notices(Some((&next, runtime.session.id)));
            runtime.session.launch = next;
            let old_records = runtime.observed_notices.retired[0].batch.records.clone();
            let old_launch = runtime.observed_notices.retired[0].batch.launch.clone();
            // Share one raw allocation across many retired scopes so the test
            // exercises the conservative 128 MiB accounting cheaply.
            for index in 0..139 {
                let mut launch = old_launch.clone();
                launch.session_id = Some(format!("77777777-7777-4777-8777-{index:012x}"));
                let mut batch = Batch::new(launch, runtime.session.id);
                batch.records = old_records.clone();
                batch.dirty = true;
                runtime.observed_notices.retired.push(Capture {
                    target: runtime.target,
                    batch,
                });
            }
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let target = runtime.target;
            if parked {
                let path = directory.path().join("unavailable.sqlite3");
                let (writer, _) = crate::session::SessionRuntime::open(
                    &path,
                    crate::model::BootId::new("unavailable"),
                )
                .unwrap();
                app.session = Some(writer);
                app.save_health = Some(crate::app::SaveHealthState::writer_unavailable(Some(path)));
                app.chat_ui.parked.insert(target.card, Box::new(runtime));
            } else {
                app.session = None;
                put(&mut app, runtime);
            }
            drop(app.persist_observed_chat_notices());
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert!(runtime.observed_notices.retired.is_empty());
            assert!(runtime.observed_notices.batch.is_none());
            assert!(runtime.failures.is_empty());
            assert_eq!(runtime.session.launch.session_id.as_deref(), Some(SECOND));
            assert!(!app.chat_ui.closing_notices.is_empty());
            assert!(app.chat_ui.closing_notices.len() < 140);
            assert!(
                app.chat_ui
                    .closing_notices
                    .iter()
                    .map(retained_capture_bytes)
                    .sum::<usize>()
                    <= crate::persistence::observed_notices::MAX_GLOBAL_BYTES
            );
            assert!(app.chat_ui.closing_notices.iter().all(|capture| {
                capture.batch.launch.session_id.as_deref() != Some(SECOND)
                    && capture.batch.pending.is_none()
            }));
            assert!(app.observed_capacity_notice_problem().is_some_and(
                |(guidance, retry)| !retry && guidance.contains("storage is unavailable")
            ));
            assert!(app.observed_capacity_notice_view().is_some());
            assert!(
                app.status
                    .as_ref()
                    .is_some_and(|notice| notice.message.contains("could not all be retained"))
            );
        }
    }

    #[test]
    fn unattested_omp_notes_stay_with_the_same_actor_options_but_not_new_process_contexts() {
        for change in [
            "clone",
            "options",
            "environment",
            "directory",
            "program",
            "actor",
            "profile",
            "profile-equals",
            "session-dir",
            "session-dir-equals",
            "config",
            "config-equals",
            "missing-model-value",
        ] {
            let (mut runtime, _, directory) = fixture_for(Provider::Omp);
            let anchor = format!("{FIRST}:0");
            runtime.note_at(Some(anchor.clone()), Some(anchor.clone()), CAPACITY.into());
            let original = runtime.session.clone();
            let body = runtime.observed_notices.batch.as_ref().unwrap().records[0]
                .text
                .clone();
            let ticket = Ticket::new(runtime.target);
            runtime.observed_notices.batch.as_mut().unwrap().pending = Some(Pending {
                ticket,
                offered: None,
            });
            let mut next = original.clone();
            match change {
                "options" => next.launch.recipe.arguments.extend([
                    "--model".into(),
                    "another-model".into(),
                    "--permission-mode=default".into(),
                ]),
                "environment" => {
                    next.launch
                        .recipe
                        .environment
                        .insert("OMP_PROFILE".into(), "other-account".into());
                }
                "directory" => {
                    next.launch.recipe.working_directory = directory.path().join("other-directory")
                }
                "program" => next.launch.recipe.program = "another-omp".into(),
                "actor" => next.id = TerminalId::new(),
                "profile" => next
                    .launch
                    .recipe
                    .arguments
                    .extend(["--profile".into(), "another-profile".into()]),
                "profile-equals" => next
                    .launch
                    .recipe
                    .arguments
                    .push("--profile=another-profile".into()),
                "session-dir" => next
                    .launch
                    .recipe
                    .arguments
                    .extend(["--session-dir".into(), "/other-sessions".into()]),
                "session-dir-equals" => next
                    .launch
                    .recipe
                    .arguments
                    .push("--session-dir=/other-sessions".into()),
                "config" => next
                    .launch
                    .recipe
                    .arguments
                    .extend(["--config".into(), "/other-config".into()]),
                "config-equals" => next
                    .launch
                    .recipe
                    .arguments
                    .push("--config=/other-config".into()),
                "missing-model-value" => next.launch.recipe.arguments.extend([
                    "--model".into(),
                    "--profile".into(),
                    "another-profile".into(),
                ]),
                _ => {}
            }
            runtime.prepare_observed_session(&next);
            runtime.session = next;
            let (writer, _) = crate::session::SessionRuntime::open(
                directory.path().join("omp-owner.sqlite3"),
                crate::model::BootId::new(change),
            )
            .unwrap();
            let answer = Arc::new(load(
                &writer.operation_store(),
                original.launch.clone(),
                original.id,
                true,
            ));
            assert!(answer.scope.is_err());
            assert!(
                !answer.retryable,
                "actor continuity cannot grant an attested account home"
            );
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.session = Some(writer);
            let target = put(&mut app, runtime);
            drop(app.persist_observed_chat_notices());
            let wrong_actor = Arc::new(Loaded::unsupported(
                original.launch.clone(),
                TerminalId::new(),
                "unattested actor".into(),
            ));
            app.finish_bound_observed_notices(ticket, wrong_actor);
            let same_actor = change == "clone" || change == "options";
            if same_actor {
                assert!(
                    app.chat_runtime_mut(target)
                        .unwrap()
                        .observed_notices
                        .batch
                        .as_ref()
                        .unwrap()
                        .pending
                        .is_some()
                );
            } else {
                assert!(app.chat_ui.closing_notices[0].batch.pending.is_some());
            }
            app.finish_bound_observed_notices(ticket, answer);
            let runtime = app.chat_runtime_mut(target).unwrap();
            if same_actor {
                let batch = runtime.observed_notices.batch.as_ref().unwrap();
                assert_eq!(
                    batch.launch, original.launch,
                    "pending work retains its exact captured recipe"
                );
                assert_eq!(batch.actor, original.id);
                assert!(batch.pending.is_none());
                assert!(Arc::ptr_eq(&body, &batch.records[0].text));
                assert!(runtime.failures.iter().any(|note| note.text == CAPACITY));
                runtime.note_at(Some(anchor), Some("later-work".into()), CAPACITY.into());
            } else {
                assert!(runtime.observed_notices.batch.is_none());
                assert!(
                    runtime.failures.is_empty(),
                    "an old unattested owner cannot bind to a replacement context"
                );
                let retired = &app.chat_ui.closing_notices[0].batch;
                assert_eq!(retired.launch, original.launch);
                assert_eq!(retired.actor, original.id);
                assert!(Arc::ptr_eq(&body, &retired.records[0].text));
                assert!(retired.pending.is_none());
            }
            drop(app.persist_observed_chat_notices());
            assert!(
                app.observed_capacity_notice_problem()
                    .is_some_and(|(_, retry)| !retry)
            );
        }
    }

    #[test]
    fn named_omp_ready_finishes_unsupported_or_timed_out_hydration_for_only_its_actual_actor() {
        for timeout in [false, true] {
            let (runtime, handle, directory) = fixture_for(Provider::Omp);
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.error = Some(CAPACITY.into());
            handle.publish_snapshot_for_test(snapshot);
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (writer, _) = crate::session::SessionRuntime::open(
                directory.path().join("omp-ready.sqlite3"),
                crate::model::BootId::new("omp-ready"),
            )
            .unwrap();
            app.session = Some(writer);
            drop(app.insert_chat_card(runtime.session.clone(), None, None));
            let card = app.workspace.focused().unwrap();
            // The current snapshot error is observed when its transcript is
            // presented. A zero-width fixture is offscreen by construction.
            app.workspace.resize_viewport(1_600.0);
            assert_eq!(
                app.workspace.visibility(card),
                Some(VisibilityClass::Visible)
            );
            assert!(app.workspace_surfaces_visible());
            let target = app.chat_ui.attempt(card).unwrap().target;
            let ready = Ready(Arc::new(Mutex::new(Some(Ok(handle.clone())))));
            let initial = app.handle_chat_ui(Action::Ready(target, ready));
            let (ticket, launch, actor, pending) =
                match &app.chat_ui.attempt(card).unwrap().observed_notices {
                    Some(ReadyState::Loading {
                        ticket,
                        launch,
                        actor,
                        ready,
                    }) => (*ticket, launch.clone(), *actor, ready.clone()),
                    _ => panic!("named Omp must start exactly one hydration"),
                };
            let wrong = Arc::new(Loaded::unsupported(
                launch.clone(),
                TerminalId::new(),
                "unattested actor".into(),
            ));
            assert_no_task_actions(app.finish_ready_observed_notices(
                ticket,
                pending.clone(),
                wrong,
            ));
            assert!(
                matches!(app.chat_ui.attempt(card).unwrap().observed_notices, Some(ReadyState::Loading { ticket: current, .. }) if current == ticket)
            );
            assert!(pending.0.lock().unwrap().is_some());
            let task = if timeout {
                drop(initial);
                let late_launch = launch.clone();
                ready_load_task(
                    ticket,
                    pending,
                    launch,
                    actor,
                    Duration::from_millis(1),
                    move || {
                        std::thread::sleep(Duration::from_millis(25));
                        Loaded::unsupported(
                            late_launch,
                            actor,
                            "provider has no account-home selector".into(),
                        )
                    },
                )
            } else {
                initial
            };
            let Message::ChatUi(Action::ObservedNoticesReady(ticket, ready, loaded)) =
                first_output(task)
            else {
                panic!("Omp hydration completion")
            };
            assert_eq!(loaded.actor, handle.session().id);
            assert!(loaded.scope.is_err());
            assert_eq!(loaded.retryable, timeout);
            drop(app.finish_ready_observed_notices(ticket, ready, loaded));
            assert!(
                app.chat_ui.attempt(card).is_none(),
                "unattested account must not strand named Omp in Starting"
            );
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert!(Arc::ptr_eq(runtime.handle().unwrap(), &handle));
            assert!(runtime.visible.load(Ordering::Relaxed));
            assert_eq!(runtime.snapshot.items.len(), 2);
            assert!(runtime.failures.iter().any(|note| note.text == CAPACITY));
            assert!(
                runtime
                    .observed_notices
                    .batch
                    .as_ref()
                    .unwrap()
                    .binding
                    .as_ref()
                    .unwrap()
                    .is_err()
            );
            drop(app.persist_observed_chat_notices());
            assert!(
                app.observed_capacity_notice_problem()
                    .is_some_and(|(_, retry)| retry == timeout)
            );
        }
    }
}
