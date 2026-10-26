//! A channel-owned view of durable work intent. Checkout facts remain context;
//! queued messages and native child state retain the exact home and target.
pub(crate) mod journal;

use crate::{
    channel_work::{
        Bucket, Change, Entry, Freshness, Home, HomeLifecycle, Mutation, Page, PrState, Receipt,
        Reply, Role, Snapshot, Source, Target,
    },
    channel_work_api::LocalSelection,
    design::{self, space},
    github_identity::{GitHubRepoKey, PullRequestKey},
    ui_controls::{self, Icon, ViewTab, button, field},
};
use cosmic::{
    Element,
    iced::{Alignment, Length},
    widget,
};
use std::{collections::HashSet, path::PathBuf};
use uuid::Uuid;

const PAGE_SIZE: u16 = 50;
const VISIBLE_ROWS: usize = 30;
const MAX_PENDING: usize = 16;
// Sixteen original resident requests plus sixteen recovered disk records.
const MAX_RECOVERY: usize = 32;
const ROLES: [&str; 4] = ["Contributing", "Stewarding", "Review only", "Reference"];
const ROLE_VALUES: [Role; 4] = [
    Role::Contribute,
    Role::Steward,
    Role::Review,
    Role::Reference,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewOwner {
    pub home: Home,
    generation: Uuid,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ticket {
    pub home: Home,
    request: Uuid,
}
impl Ticket {
    fn new(home: Home) -> Self {
        Self {
            home,
            request: Uuid::new_v4(),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FormOwner {
    view: ViewOwner,
    draft: Uuid,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowOwner {
    view: ViewOwner,
    id: Uuid,
    target: Target,
    revision: u64,
    bucket: Bucket,
}
#[derive(Debug, Clone, Copy)]
pub enum Field {
    Host,
    Repository,
    Number,
}

/// Only the adapter can classify a confirmed pre-effect refusal. A lost or
/// malformed exchange is unavailable, never proof that a write did not occur.
#[derive(Debug, Clone)]
pub enum Failure {
    Unavailable,
    Refused(String),
}
impl Failure {
    fn read_message(&self) -> String {
        match self {
            Self::Unavailable => {
                "Could not refresh channel work. Previously loaded work is preserved.".into()
            }
            Self::Refused(reason) => format!("Channel work could not be read: {reason}"),
        }
    }
}

/// Safe presentation metadata. Routing and authority remain in the adapter.
#[derive(Debug, Clone)]
pub struct SavedOperation {
    pub mutation: Mutation,
    pub selection: Option<Uuid>,
    pub title: String,
    pub device_label: String,
    pub confirmed: Option<Receipt>,
}
impl SavedOperation {
    fn valid(&self) -> bool {
        self.mutation
            .canonicalized()
            .is_ok_and(|value| value == self.mutation)
            && self.selection.is_none_or(|selection| !selection.is_nil() && matches!(self.mutation.change, Change::Claim { target: Target::Local(id), .. } if !id.is_nil()))
            && self.title.len() <= 1024
            && !self.title.chars().any(char::is_control)
            && self.device_label.len() <= 4096
            && !self.device_label.chars().any(char::is_control)
            && self
                .confirmed
                .as_ref()
                .is_none_or(|receipt| State::receipt_matches(&self.mutation, receipt))
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    ShowSaved,
    SavedLoaded {
        generation: Uuid,
        result: Result<Vec<SavedOperation>, String>,
        problems: Vec<String>,
        occupied: usize,
        saturated: bool,
    },
    OpenSaved {
        generation: Uuid,
        operation: Uuid,
    },
    CloseSaved(Uuid),
    BeforeSendFailed {
        ticket: Ticket,
        reason: String,
        retained: bool,
    },
    AppliedWithSettlement {
        ticket: Ticket,
        result: Result<Reply, Failure>,
        settled: bool,
    },
    LookedUpWithSettlement {
        ticket: Ticket,
        operation: Uuid,
        result: Result<Option<Receipt>, Failure>,
        settled: bool,
    },
    Close(ViewOwner),
    Tab(ViewOwner, Bucket),
    Refresh(ViewOwner),
    NextCurrent(ViewOwner, u64),
    LatestCurrent(ViewOwner),
    Earlier(ViewOwner, u64),
    Latest(ViewOwner),
    More(ViewOwner),
    Attach(ViewOwner),
    AttachLocal(ViewOwner),
    LocalPath(FormOwner, String),
    InspectLocal {
        owner: FormOwner,
        path_generation: Uuid,
        directory: String,
        role: Role,
    },
    PreparedLocal {
        ticket: Ticket,
        result: Result<LocalSelection, Failure>,
    },
    SubmitLocal {
        owner: FormOwner,
        selection: Uuid,
        target: Uuid,
        role: Role,
        revision: u64,
    },
    Field(FormOwner, Field, String),
    Role(FormOwner, usize),
    Submit {
        owner: FormOwner,
        target: Target,
        role: Role,
        revision: u64,
    },
    Dismiss(FormOwner),
    Keep(ViewOwner, Uuid),
    Discard(ViewOwner, Uuid),
    Correct(RowOwner),
    End(RowOwner),
    Suppress(RowOwner),
    Open(RowOwner),
    Details(RowOwner, bool),
    Check(ViewOwner, Uuid),
    Retry(ViewOwner, Uuid),
    Loaded {
        ticket: Ticket,
        result: Result<Snapshot, Failure>,
    },
    Applied {
        ticket: Ticket,
        result: Result<Reply, Failure>,
    },
    LookedUp {
        ticket: Ticket,
        operation: Uuid,
        result: Result<Option<Receipt>, Failure>,
    },
}
impl Action {
    pub fn background(&self) -> bool {
        matches!(
            self,
            Self::Loaded { .. }
                | Self::Applied { .. }
                | Self::LookedUp { .. }
                | Self::PreparedLocal { .. }
                | Self::SavedLoaded { .. }
                | Self::BeforeSendFailed { .. }
                | Self::AppliedWithSettlement { .. }
                | Self::LookedUpWithSettlement { .. }
        )
    }
}

#[derive(Debug, Clone)]
pub enum Effect {
    LoadSaved {
        generation: Uuid,
    },
    OpenSaved {
        operation: Uuid,
    },
    CloseSaved,
    Refresh {
        ticket: Ticket,
        home: Home,
    },
    Read {
        ticket: Ticket,
        home: Home,
        page: Page,
    },
    PrepareLocal {
        ticket: Ticket,
        home: Home,
        directory: PathBuf,
    },
    Mutate {
        ticket: Ticket,
        mutation: Mutation,
        selection: Option<Uuid>,
    },
    Lookup {
        ticket: Ticket,
        home: Home,
        operation: Uuid,
    },
    OpenPullRequest {
        home: Home,
        key: PullRequestKey,
        url: url::Url,
    },
    Close {
        home: Home,
    },
    Focus {
        owner: ViewOwner,
        id: widget::Id,
    },
}

#[derive(Debug)]
struct Inspection {
    ticket: Ticket,
    path_generation: Uuid,
}
#[derive(Debug)]
struct LocalDraft {
    directory: String,
    path_generation: Uuid,
    inspecting: Option<Inspection>,
    selected: Option<LocalSelection>,
    error: Option<String>,
}
impl LocalDraft {
    fn directory(&self) -> Option<PathBuf> {
        (self.directory.trim() == self.directory
            && !self.directory.contains('\0')
            && self.directory.len() <= 4096
            && std::path::Path::new(&self.directory).is_absolute())
        .then(|| PathBuf::from(&self.directory))
    }
}
#[derive(Debug)]
struct Form {
    owner: FormOwner,
    fixed: Option<Target>,
    expected_revision: u64,
    host: String,
    repository: String,
    number: String,
    role: Role,
    initial_role: Role,
    local: Option<LocalDraft>,
}
impl Form {
    fn dirty(&self) -> bool {
        if let Some(local) = &self.local {
            return !local.directory.is_empty() || self.role != self.initial_role;
        }
        self.role != self.initial_role
            || (self.fixed.is_none()
                && (self.host != "github.com"
                    || !self.repository.is_empty()
                    || !self.number.is_empty()))
    }
    // Parsing must not rewrite the edited field: a typed /pull/1 can still
    // become /pull/1568. Only an explicit edit to a split identity field
    // switches a complete URL back to the split-field representation.
    fn repository_url(&self) -> Option<PullRequestKey> {
        if self.repository.trim() != self.repository
            || self.repository.chars().any(char::is_control)
        {
            return None;
        }
        let Target::PullRequest(key) =
            Target::PullRequest(PullRequestKey::from_url(&self.repository).ok()?)
                .canonicalized()
                .ok()?
        else {
            return None;
        };
        Some(key)
    }

    fn use_split_repository(&mut self) {
        if let Some(key) = self.repository_url() {
            self.repository = format!("{}/{}", key.repository.owner, key.repository.name);
        }
    }

    fn target(&self) -> Result<Target, &'static str> {
        if let Some(local) = &self.local {
            return local
                .selected
                .as_ref()
                .map(|selected| Target::Local(selected.context.id))
                .ok_or("Inspect a checkout before tracking local work.");
        }
        if let Some(target) = &self.fixed {
            return Ok(target.clone());
        }
        if self.host.trim() != self.host
            || self.repository.trim() != self.repository
            || self.number.trim() != self.number
        {
            return Err("Remove leading or trailing spaces from the PR identity.");
        }
        if self.repository.contains("://") {
            return Target::PullRequest(
                self.repository_url()
                    .ok_or("Enter a complete HTTPS pull request URL.")?,
            )
            .canonicalized()
            .map_err(|_| "Enter a valid PR identity.");
        }
        let (owner, name) = self
            .repository
            .split_once('/')
            .ok_or("Enter the repository as owner/name.")?;
        if name.contains('/') {
            return Err("Enter one repository as owner/name.");
        }
        let origin = url::Url::parse(&format!("https://{}/", self.host))
            .map_err(|_| "Enter a valid GitHub host.")?;
        if origin.username() != ""
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err("Enter only the GitHub host and optional port.");
        }
        let repository = GitHubRepoKey::new(
            origin.host_str().ok_or("Enter a GitHub host.")?,
            origin.port(),
            owner,
            name,
        )
        .map_err(|_| "Enter a valid host and repository.")?;
        let number = self
            .number
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or("Enter a positive PR number.")?;
        Target::PullRequest(
            PullRequestKey::new(repository, number).map_err(|_| "Enter a valid PR identity.")?,
        )
        .canonicalized()
        .map_err(|_| "Enter a valid PR identity.")
    }
}
#[derive(Debug)]
enum Leave {
    Close,
    Dismiss,
    Open {
        home: Home,
        title: String,
        read_only: bool,
    },
    OpenSaved {
        operation: Uuid,
    },
    ShowSaved,
}
#[derive(Debug)]
struct Confirmation {
    id: Uuid,
    intent: Leave,
}
#[derive(Debug)]
struct View {
    owner: ViewOwner,
    read_only: bool,
    title: String,
    snapshot: Option<Snapshot>,
    page: Page,
    tab: Bucket,
    loading: Option<Ticket>,
    read_page: Option<Page>,
    form: Option<Form>,
    error: Option<String>,
    notice: Option<String>,
    visible: usize,
    details: Option<Uuid>,
    confirmation: Option<Confirmation>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Sending,
    Unknown,
    Checking,
}
#[derive(Debug)]
struct Pending {
    mutation: Mutation,
    selection: Option<Uuid>,
    ticket: Ticket,
    lookup: Option<Ticket>,
    delivery: Delivery,
    ambiguous: bool,
    // Presentation admission only; the adapter independently proves the route.
    retry_ready: bool,
    // Reloaded records are receipt-only; their old selection is never renewed.
    receipt_only: bool,
    saved_record: bool,
    title: String,
    device_label: String,
    confirmed: Option<Receipt>,
    submitted_form: Option<FormOwner>,
}

/// One bounded set of unresolved changes across channel navigation. Close never
/// cancels a submitted change or evicts its immutable request. The limit refuses
/// additional changes, not reads, navigation, or host operations.
#[derive(Debug, Default)]
struct SavedView {
    generation: Uuid,
    shown: bool,
    loading: bool,
    ready: bool,
    occupied: usize,
    saturated: bool,
    problems: Vec<String>,
    error: Option<String>,
}

#[derive(Debug, Default)]
pub struct State {
    view: Option<View>,
    pending: Vec<Pending>,
    // One actual client inspection survives view/form dismissal. A stale reply
    // releases only its own slot and cannot populate another draft.
    local_inspection: Option<Ticket>,
    saved: SavedView,
}

impl State {
    pub fn is_open(&self) -> bool {
        self.saved.shown || self.view.is_some()
    }
    pub fn owner(&self) -> Option<ViewOwner> {
        self.view.as_ref().map(|view| view.owner)
    }
    pub fn refresh(&mut self) -> Option<Effect> {
        if self.saved.shown {
            return None;
        }
        self.read(self.owner()?)
    }
    /// Must complete before a newly opened channel permits fresh mutations.
    pub fn load_saved(&mut self) -> Effect {
        self.saved.generation = Uuid::new_v4();
        self.saved.loading = true;
        self.saved.ready = false;
        self.saved.error = None;
        Effect::LoadSaved {
            generation: self.saved.generation,
        }
    }
    /// The adapter must check this before adopting loaded records or routes.
    pub fn saved_load_is_current(&self, generation: Uuid) -> bool {
        self.saved.loading && self.saved.generation == generation
    }
    /// The adapter checks this before adopting the corresponding route map.
    pub fn can_restore_saved(&self, records: &[SavedOperation]) -> bool {
        if records.len() > journal::MAX_RECORDS {
            return false;
        }
        let mut ids = self
            .pending
            .iter()
            .map(|pending| pending.mutation.operation)
            .collect::<HashSet<_>>();
        let mut incoming = HashSet::new();
        for record in records {
            if !record.valid() || !incoming.insert(record.mutation.operation) {
                return false;
            }
            if self.pending.iter().any(|pending| {
                pending.mutation.operation == record.mutation.operation
                    && (pending.mutation != record.mutation
                        || pending.selection != record.selection
                        || matches!((&pending.confirmed, &record.confirmed), (Some(old), Some(new)) if old != new))
            }) {
                return false;
            }
            ids.insert(record.mutation.operation);
        }
        let recovered = self
            .pending
            .iter()
            .filter(|pending| pending.receipt_only)
            .map(|pending| pending.mutation.operation)
            .chain(
                records
                    .iter()
                    .filter(|record| {
                        !self.pending.iter().any(|pending| {
                            pending.mutation.operation == record.mutation.operation
                                && !pending.receipt_only
                        })
                    })
                    .map(|record| record.mutation.operation),
            )
            .collect::<HashSet<_>>();
        ids.len() <= MAX_RECOVERY && recovered.len() <= journal::MAX_RECORDS
    }
    fn submitted_form(&self, owner: FormOwner) -> bool {
        self.pending
            .iter()
            .any(|pending| pending.submitted_form == Some(owner))
    }
    fn unsent_draft(&self, view: &View) -> bool {
        view.form
            .as_ref()
            .is_some_and(|form| form.dirty() && !self.submitted_form(form.owner))
    }
    /// Original saved routing can reconcile saved operations, never start fresh intent.
    pub fn open_saved_home(&mut self, home: Home, title: String) -> Option<Effect> {
        self.open_mode(home, title, true)
    }
    fn open_mode(&mut self, home: Home, title: String, read_only: bool) -> Option<Effect> {
        self.saved.shown = false;
        if self.home() == Some(home)
            && self
                .view
                .as_ref()
                .is_some_and(|view| view.read_only == read_only)
        {
            self.view.as_mut()?.title = title;
            return self.read(self.owner()?);
        }
        if let Some(view) = &self.view {
            if view.confirmation.is_some() {
                return None;
            }
            if self.unsent_draft(view) {
                return self.leave(
                    view.owner,
                    Leave::Open {
                        home,
                        title,
                        read_only,
                    },
                );
            }
        }
        self.open_direct(home, title, read_only)
    }
    pub fn saved_is_shown(&self) -> bool {
        self.saved.shown
    }
    /// Complete, clean discovery is required before automatic Home retirement.
    /// Capacity prevents new intent but does not make the known Home set incomplete.
    pub fn saved_discovery_ready(&self) -> bool {
        self.saved.ready
            && !self.saved.loading
            && self.saved.error.is_none()
            && self.saved.problems.is_empty()
    }
    /// A pre-settlement load must not resurrect the operation it observed.
    pub fn invalidate_saved_load(&mut self) {
        if self.saved.loading {
            self.saved.generation = Uuid::new_v4();
            self.saved.loading = false;
            self.saved.ready = false;
            self.saved.error = Some(
                "Saved changes changed while loading. Reload saved changes to continue.".into(),
            );
        }
    }
    pub fn home(&self) -> Option<Home> {
        self.view.as_ref().map(|view| view.owner.home)
    }
    /// Refresh presentation metadata for the exact open channel incarnation.
    /// This does not open a view or change its draft, tickets or navigation.
    pub fn set_title(&mut self, home: Home, title: String) {
        if let Some(view) = self.view.as_mut().filter(|view| view.owner.home == home) {
            view.title = title;
        }
    }
    pub fn pending_homes(&self) -> impl Iterator<Item = Home> + '_ {
        self.pending.iter().map(|pending| pending.mutation.home)
    }
    pub fn has_confirmation(&self) -> bool {
        self.view
            .as_ref()
            .is_some_and(|view| view.confirmation.is_some())
    }
    pub fn open(&mut self, home: Home, title: String) -> Option<Effect> {
        self.open_mode(home, title, false)
    }
    fn open_direct(&mut self, home: Home, title: String, read_only: bool) -> Option<Effect> {
        let owner = ViewOwner {
            home,
            generation: Uuid::new_v4(),
        };
        self.view = Some(View {
            owner,
            read_only,
            title,
            snapshot: None,
            page: Page {
                current_before: None,
                before: None,
                limit: PAGE_SIZE,
            },
            tab: Bucket::Ongoing,
            loading: None,
            read_page: None,
            form: None,
            error: None,
            notice: None,
            visible: VISIBLE_ROWS,
            details: None,
            confirmation: None,
        });
        if home.validate().is_err() {
            self.view.as_mut()?.error = Some("This channel has not been confirmed by its workspace. Reconnect before changing work.".into());
            return None;
        }
        self.read(owner)
    }
    pub fn escape(&mut self) -> Option<Effect> {
        if self.saved.shown {
            return self.update(Action::CloseSaved(self.saved.generation));
        }
        let view = self.view.as_ref()?;
        if let Some(confirmation) = &view.confirmation {
            return self.update(Action::Keep(view.owner, confirmation.id));
        }
        self.update(match view.form.as_ref() {
            Some(form) if !self.submitted_form(form.owner) => Action::Dismiss(form.owner),
            _ => Action::Close(view.owner),
        })
    }
    fn leave(&mut self, owner: ViewOwner, intent: Leave) -> Option<Effect> {
        if !self.current(owner) || self.has_confirmation() {
            return None;
        }
        if self.unsent_draft(self.view.as_ref()?) {
            let id = Uuid::new_v4();
            self.view.as_mut()?.confirmation = Some(Confirmation { id, intent });
            return Some(Effect::Focus {
                owner,
                id: confirmation_id(id),
            });
        }
        self.finish_leave(owner, intent)
    }
    fn finish_leave(&mut self, owner: ViewOwner, intent: Leave) -> Option<Effect> {
        match intent {
            Leave::Close => {
                self.view = None;
                Some(Effect::Close { home: owner.home })
            }
            Leave::Dismiss => {
                self.view.as_mut()?.form = None;
                Some(Effect::Focus {
                    owner,
                    id: attach_id(owner),
                })
            }
            Leave::Open {
                home,
                title,
                read_only,
            } => self.open_direct(home, title, read_only),
            Leave::ShowSaved => {
                self.saved.shown = true;
                Some(self.load_saved())
            }
            Leave::OpenSaved { operation } => {
                if !self
                    .pending
                    .iter()
                    .any(|pending| pending.mutation.operation == operation)
                {
                    self.view.as_mut()?.error = Some(
                        "The saved change is no longer pending. No connection was changed.".into(),
                    );
                    return None;
                }
                self.view.as_mut()?.form = None;
                Some(Effect::OpenSaved { operation })
            }
        }
    }
    fn current(&self, owner: ViewOwner) -> bool {
        self.view.as_ref().is_some_and(|view| view.owner == owner)
    }
    /// Immutable resident intent; this does not reconstruct a saved record.
    pub fn original_mutation(&self, home: Home, operation: Uuid) -> Option<&Mutation> {
        self.pending
            .iter()
            .find(|pending| {
                pending.mutation.home == home && pending.mutation.operation == operation
            })
            .map(|pending| &pending.mutation)
    }
    fn pending_for(&self, home: Home) -> Option<&Pending> {
        self.pending
            .iter()
            .find(|pending| pending.mutation.home == home)
    }
    fn writable(&self, owner: ViewOwner) -> bool {
        self.current(owner)
            && self.view.as_ref().is_some_and(|view| !view.read_only)
            && !self.saved.shown
            && !self.has_confirmation()
            && self.pending_for(owner.home).is_none()
            && self
                .pending
                .iter()
                .filter(|pending| !pending.receipt_only)
                .count()
                < MAX_PENDING
            && self.pending.len() < MAX_RECOVERY
            && self
                .view
                .as_ref()
                .and_then(|view| view.snapshot.as_ref())
                .is_some_and(|snapshot| snapshot.lifecycle == HomeLifecycle::Active)
    }
    fn local_writable(&self, owner: ViewOwner) -> bool {
        self.writable(owner)
            && self.saved.ready
            && !self.saved.loading
            && !self.saved.saturated
            && self.saved.problems.is_empty()
            && self.saved.error.is_none()
    }
    fn row(&self, owner: &RowOwner, mutable: bool) -> Option<&Entry> {
        if !self.current(owner.view) || (mutable && !self.writable(owner.view)) {
            return None;
        }
        let view = self.view.as_ref()?;
        if mutable && view.form.is_some() {
            return None;
        }
        let snapshot = view.snapshot.as_ref()?;
        snapshot
            .entries
            .iter()
            .chain(&snapshot.history)
            .find(|entry| {
                entry.id == owner.id
                    && entry.target == owner.target
                    && (!mutable
                        || (entry.revision == owner.revision && entry.bucket() == owner.bucket))
            })
    }
    fn read(&mut self, owner: ViewOwner) -> Option<Effect> {
        if !self.current(owner) || owner.home.validate().is_err() {
            return None;
        }
        let view = self.view.as_mut()?;
        if view.loading.is_some() {
            return None;
        }
        let ticket = Ticket::new(owner.home);
        let page = view.read_page.unwrap_or(view.page);
        view.read_page = Some(page);
        view.loading = Some(ticket);
        view.error = None;
        Some(Effect::Read {
            ticket,
            home: owner.home,
            page,
        })
    }
    fn begin(&mut self, owner: ViewOwner, revision: u64, change: Change) -> Option<Effect> {
        self.begin_selected(owner, revision, change, None)
    }
    fn begin_selected(
        &mut self,
        owner: ViewOwner,
        revision: u64,
        change: Change,
        selection: Option<Uuid>,
    ) -> Option<Effect> {
        if !self.writable(owner) || (selection.is_some() && !self.local_writable(owner)) {
            return None;
        }
        let mutation = Mutation {
            home: owner.home,
            expected_revision: revision,
            operation: Uuid::new_v4(),
            change,
        }
        .canonicalized()
        .ok()?;
        let ticket = Ticket::new(owner.home);
        self.pending.push(Pending {
            mutation: mutation.clone(),
            selection,
            ticket,
            lookup: None,
            delivery: Delivery::Sending,
            ambiguous: false,
            retry_ready: false,
            receipt_only: false,
            saved_record: false,
            title: self
                .view
                .as_ref()
                .map_or_else(String::new, |view| view.title.clone()),
            device_label: "Original connection".into(),
            confirmed: None,
            submitted_form: self
                .view
                .as_ref()
                .and_then(|view| view.form.as_ref().map(|form| form.owner)),
        });
        if let Some(view) = &mut self.view {
            view.error = None;
            view.notice = None;
        }
        Some(Effect::Mutate {
            ticket,
            mutation,
            selection,
        })
    }
    fn adopt(&mut self, snapshot: Snapshot) {
        if let Some(view) = &mut self.view {
            if view.owner.home == snapshot.home
                && view
                    .snapshot
                    .as_ref()
                    .is_none_or(|old| snapshot.revision >= old.revision)
            {
                view.snapshot = Some(snapshot);
            }
        }
    }
    fn receipt_matches(mutation: &Mutation, receipt: &Receipt) -> bool {
        receipt.home == mutation.home
            && receipt.operation == mutation.operation
            && mutation
                .expected_revision
                .checked_add(u64::from(receipt.changed))
                == Some(receipt.revision)
    }
    fn valid_receipt(pending: &Pending, receipt: &Receipt) -> bool {
        Self::receipt_matches(&pending.mutation, receipt)
            && pending
                .confirmed
                .as_ref()
                .is_none_or(|confirmed| confirmed == receipt)
    }
    fn dismiss_submitted(view: &mut View, pending: &Pending) {
        if pending.submitted_form.is_some()
            && view.form.as_ref().map(|form| form.owner) == pending.submitted_form
        {
            view.form = None;
        }
    }
    fn confirmed_unsaved(&mut self, index: usize, receipt: Receipt) {
        if !Self::valid_receipt(&self.pending[index], &receipt) {
            return;
        }
        let pending = &mut self.pending[index];
        pending.confirmed = Some(receipt);
        pending.retry_ready = false;
        pending.delivery = Delivery::Unknown;
        pending.lookup = None;
        pending.ambiguous = true;
        if let Some(view) = self
            .view
            .as_mut()
            .filter(|view| view.owner.home == pending.mutation.home)
        {
            Self::dismiss_submitted(view, pending);
            view.notice = Some("The original change is confirmed. Its saved recovery record still needs to be updated.".into());
        }
    }
    fn valid_reply(pending: &Pending, reply: &Reply) -> bool {
        Self::valid_receipt(pending, &reply.receipt)
            && reply.snapshot.validate().is_ok()
            && reply.snapshot.home == pending.mutation.home
            && reply.receipt.revision <= reply.snapshot.revision
    }
    fn applied(&mut self, index: usize, result: Result<Reply, Failure>) {
        match result {
            Ok(reply) if Self::valid_reply(&self.pending[index], &reply) => {
                let pending = self.pending.remove(index);
                let reply_revision = reply.snapshot.revision;
                self.adopt(reply.snapshot);
                if let Some(view) = self
                    .view
                    .as_mut()
                    .filter(|view| view.owner.home == pending.mutation.home)
                {
                    if view
                        .snapshot
                        .as_ref()
                        .is_some_and(|snapshot| snapshot.revision == reply_revision)
                    {
                        view.page.current_before = None;
                        view.page.before = None;
                        view.read_page = None;
                        // The mutation reply contains a fresh latest page. A read
                        // launched for the old page must not replace it later.
                        view.loading = None;
                    }
                    Self::dismiss_submitted(view, &pending);
                    view.notice = Some(
                        if reply.receipt.changed {
                            "Channel work updated."
                        } else {
                            "Tracking was already up to date."
                        }
                        .into(),
                    );
                }
            }
            Err(Failure::Refused(reason)) if !self.pending[index].ambiguous => {
                let pending = self.pending.remove(index);
                if let Some(view) = self
                    .view
                    .as_mut()
                    .filter(|view| view.owner.home == pending.mutation.home)
                {
                    view.error = Some(format!("Tracking was not changed: {reason}"));
                    if pending.selection.is_some()
                        && let Some(local) = view.form.as_mut().and_then(|form| form.local.as_mut())
                    {
                        local.selected = None;
                        local.error = Some(
                            "Inspect the checkout again before submitting a new change.".into(),
                        );
                    }
                }
            }
            _ => {
                self.pending[index].delivery = Delivery::Unknown;
                self.pending[index].retry_ready =
                    !self.pending[index].receipt_only && self.pending[index].selection.is_none();
                self.pending[index].lookup = None;
                self.pending[index].ambiguous = true;
            }
        }
    }
    pub fn update(&mut self, action: Action) -> Option<Effect> {
        if let Action::PreparedLocal { ticket, .. } = &action {
            if self.local_inspection != Some(*ticket) {
                return None;
            }
            self.local_inspection = None;
        }
        if self.has_confirmation()
            && !action.background()
            && !matches!(&action, Action::Keep(..) | Action::Discard(..))
        {
            return None;
        }
        if self.saved.shown
            && !action.background()
            && !matches!(
                &action,
                Action::ShowSaved | Action::OpenSaved { .. } | Action::CloseSaved(..)
            )
        {
            return None;
        }
        match action {
            Action::ShowSaved => {
                if !self.saved.shown
                    && let Some(view) = &self.view
                {
                    if self.unsent_draft(view) {
                        return self.leave(view.owner, Leave::ShowSaved);
                    }
                }
                self.saved.shown = true;
                return Some(self.load_saved());
            }
            Action::CloseSaved(generation)
                if self.saved.shown && generation == self.saved.generation =>
            {
                self.saved.shown = false;
                self.view = None;
                return Some(Effect::CloseSaved);
            }
            Action::OpenSaved {
                generation,
                operation,
            } if self.saved.shown && generation == self.saved.generation => {
                if !self
                    .pending
                    .iter()
                    .any(|pending| pending.mutation.operation == operation && pending.saved_record)
                {
                    return None;
                }
                self.saved.shown = false;
                if let Some(owner) = self.owner() {
                    return self.leave(owner, Leave::OpenSaved { operation });
                }
                return Some(Effect::OpenSaved { operation });
            }
            Action::SavedLoaded {
                generation,
                result,
                problems,
                occupied,
                saturated,
            } => {
                if generation != self.saved.generation || !self.saved.loading {
                    return None;
                }
                self.saved.loading = false;
                self.saved.ready = false;
                match result {
                    Ok(records) if self.can_restore_saved(&records) => {
                        for record in records {
                            if let Some(pending) = self.pending.iter_mut().find(|pending| pending.mutation.operation == record.mutation.operation) {
                                pending.retry_ready = false;
                                pending.saved_record = true;
                                pending.title = record.title;
                                pending.device_label = record.device_label;
                                if record.confirmed.is_some() {
                                    pending.confirmed = record.confirmed;
                                    pending.delivery = Delivery::Unknown;
                                    pending.lookup = None;
                                    pending.ambiguous = true;
                                }
                            } else {
                                self.pending.push(Pending {
                                    ticket: Ticket::new(record.mutation.home),
                                    mutation: record.mutation,
                                    selection: record.selection,
                                    receipt_only: true,
                                    saved_record: true,
                                    lookup: None, delivery: Delivery::Unknown, ambiguous: true, retry_ready: false,
                                    title: record.title, device_label: record.device_label,
                                    confirmed: record.confirmed, submitted_form: None,
                                });
                            }
                        }
                        self.saved.problems = problems;
                        self.saved.occupied = occupied;
                        self.saved.saturated = saturated;
                        self.saved.ready = true;
                        self.saved.error = None;
                    }
                    Ok(_) => self.saved.error = Some("Saved changes could not be restored safely. Existing unconfirmed changes are preserved; check their results and reload.".into()),
                    Err(reason) => self.saved.error = Some(format!("Could not load saved changes: {reason}")),
                }
            }
            Action::BeforeSendFailed {
                ticket,
                reason,
                retained,
            } => {
                let index = self
                    .pending
                    .iter()
                    .position(|pending| pending.ticket == ticket)?;
                let confirmed = self.pending[index].confirmed.is_some();
                let keep = retained || self.pending[index].ambiguous || confirmed;
                if keep {
                    self.pending[index].retry_ready = false;
                    self.pending[index].delivery = Delivery::Unknown;
                    self.pending[index].ambiguous = true;
                } else {
                    self.pending.remove(index);
                }
                if let Some(view) = self
                    .view
                    .as_mut()
                    .filter(|view| view.owner.home == ticket.home)
                {
                    view.error = Some(format!(
                        "This attempt was not sent: {reason}{}",
                        if confirmed {
                            " The original change is confirmed; its recovery record still needs to be updated."
                        } else if keep {
                            " The original saved change remains unconfirmed."
                        } else {
                            " Your draft is preserved."
                        }
                    ));
                }
            }
            Action::AppliedWithSettlement {
                ticket,
                result,
                settled,
            } => {
                let index = self
                    .pending
                    .iter()
                    .position(|pending| pending.ticket == ticket)?;
                // A published request can have an earlier unconfirmed attempt.
                self.pending[index].ambiguous = true;
                self.pending[index].saved_record = true;
                if let Ok(reply) = &result {
                    if Self::valid_reply(&self.pending[index], reply) && !settled {
                        self.invalidate_saved_load();
                        self.confirmed_unsaved(index, reply.receipt.clone());
                        return None;
                    }
                }
                if result
                    .as_ref()
                    .is_ok_and(|reply| Self::valid_reply(&self.pending[index], reply))
                {
                    self.invalidate_saved_load();
                }
                self.applied(index, result);
            }
            Action::LookedUpWithSettlement {
                ticket,
                operation,
                result,
                settled,
            } => {
                let index = self.pending.iter().position(|pending| {
                    pending.lookup == Some(ticket) && pending.mutation.operation == operation
                })?;
                if let Ok(Some(receipt)) = &result {
                    if Self::valid_receipt(&self.pending[index], receipt) && !settled {
                        self.invalidate_saved_load();
                        self.confirmed_unsaved(index, receipt.clone());
                        return None;
                    }
                }
                if result.as_ref().is_ok_and(|receipt| {
                    receipt
                        .as_ref()
                        .is_some_and(|receipt| Self::valid_receipt(&self.pending[index], receipt))
                }) {
                    self.invalidate_saved_load();
                }
                // Only the adapter's successfully verified explicit Check can
                // make a missing receipt retry-ready. This is not route authority.
                let verified_missing = matches!(&result, Ok(None));
                let effect = self.update(Action::LookedUp {
                    ticket,
                    operation,
                    result,
                });
                if verified_missing
                    && let Some(pending) = self.pending.iter_mut().find(|pending| {
                        pending.mutation.operation == operation
                            && pending.ticket.home == ticket.home
                    })
                {
                    pending.retry_ready = !pending.receipt_only && pending.confirmed.is_none();
                }
                return effect;
            }
            Action::Keep(owner, id) if self.current(owner) => {
                if self.view.as_ref()?.confirmation.as_ref()?.id != id {
                    return None;
                }
                self.view.as_mut()?.confirmation = None;
                if let Some(form) = self.view.as_ref()?.form.as_ref() {
                    return Some(Effect::Focus {
                        owner,
                        id: role_id(form.owner, form.role),
                    });
                }
            }
            Action::Discard(owner, id) if self.current(owner) => {
                if self.view.as_ref()?.confirmation.as_ref()?.id != id {
                    return None;
                }
                let intent = self.view.as_mut()?.confirmation.take()?.intent;
                self.view.as_mut()?.form = None;
                return self.finish_leave(owner, intent);
            }
            Action::Loaded { ticket, result } => {
                let view = self.view.as_ref()?;
                if view.loading != Some(ticket) || view.owner.home != ticket.home {
                    return None;
                }
                let page = view.read_page.unwrap_or(view.page);
                self.view.as_mut()?.loading = None;
                self.view.as_mut()?.read_page = None;
                match result {
                    Ok(snapshot)
                        if snapshot.home == ticket.home
                            && snapshot.validate().is_ok()
                            && snapshot.entries.len() <= usize::from(page.limit)
                            && snapshot.history.len() <= usize::from(page.limit)
                            && page.current_before.is_none_or(|before| {
                                snapshot.next_current.is_none_or(|next| next < before)
                            })
                            && page.before.is_none_or(|before| {
                                snapshot.next_history.is_none_or(|next| next < before)
                            }) =>
                    {
                        if self
                            .view
                            .as_ref()?
                            .snapshot
                            .as_ref()
                            .is_some_and(|old| snapshot.revision < old.revision)
                        {
                            self.view.as_mut()?.error = Some("Received an older channel view. Previously loaded work is preserved.".into());
                        } else {
                            self.adopt(snapshot);
                            self.view.as_mut()?.page = page;
                        }
                    }
                    Ok(_) => {
                        self.view.as_mut()?.error = Some(
                            "The reply did not match this channel. No work was replaced.".into(),
                        )
                    }
                    Err(error) => self.view.as_mut()?.error = Some(error.read_message()),
                }
            }
            Action::Applied { ticket, result } => {
                let index = self
                    .pending
                    .iter()
                    .position(|pending| pending.ticket == ticket)?;
                self.applied(index, result);
            }
            Action::LookedUp {
                ticket,
                operation,
                result,
            } => {
                let index = self.pending.iter().position(|pending| {
                    pending.lookup == Some(ticket) && pending.mutation.operation == operation
                })?;
                match result {
                    Ok(Some(receipt)) if Self::valid_receipt(&self.pending[index], &receipt) => {
                        let pending = self.pending.remove(index);
                        if let Some(view) = self
                            .view
                            .as_mut()
                            .filter(|view| view.owner.home == pending.mutation.home)
                        {
                            Self::dismiss_submitted(view, &pending);
                            view.notice = Some("The original tracking change is confirmed. Refreshing the current view…".into());
                            // A receipt settles an operation, not its current state.
                            // Retire any older page read and request a separate latest
                            // projection. Failure of that read cannot undo confirmation.
                            view.loading = None;
                            view.read_page = Some(Page {
                                current_before: None,
                                before: None,
                                limit: PAGE_SIZE,
                            });
                            let owner = view.owner;
                            return self.read(owner);
                        }
                    }
                    result => {
                        self.pending[index].lookup = None;
                        self.pending[index].retry_ready = matches!(result, Ok(None))
                            && !self.pending[index].receipt_only
                            && self.pending[index].selection.is_none();
                        self.pending[index].delivery = Delivery::Unknown;
                    }
                }
            }
            Action::Close(owner) if self.current(owner) => return self.leave(owner, Leave::Close),
            Action::Tab(owner, tab) if self.current(owner) => {
                let view = self.view.as_mut()?;
                view.tab = tab;
                view.visible = VISIBLE_ROWS;
            }
            Action::Refresh(owner) if self.current(owner) => {
                if self.view.as_ref()?.loading.is_some() {
                    return None;
                }
                self.view.as_mut()?.read_page = Some(Page {
                    current_before: None,
                    before: None,
                    limit: PAGE_SIZE,
                });
                self.view.as_mut()?.visible = VISIBLE_ROWS;
                let Effect::Read { ticket, home, .. } = self.read(owner)? else {
                    unreachable!()
                };
                return Some(Effect::Refresh { ticket, home });
            }
            Action::NextCurrent(owner, before) if self.current(owner) => {
                let view = self.view.as_mut()?;
                if view.loading.is_some() || view.snapshot.as_ref()?.next_current != Some(before) {
                    return None;
                }
                view.read_page = Some(Page {
                    current_before: Some(before),
                    ..view.page
                });
                view.visible = VISIBLE_ROWS;
                return self.read(owner);
            }
            Action::LatestCurrent(owner) if self.current(owner) => {
                let view = self.view.as_mut()?;
                if view.loading.is_some() {
                    return None;
                }
                view.read_page = Some(Page {
                    current_before: None,
                    ..view.page
                });
                view.visible = VISIBLE_ROWS;
                return self.read(owner);
            }
            Action::Earlier(owner, before) if self.current(owner) => {
                let view = self.view.as_mut()?;
                if view.loading.is_some() || view.snapshot.as_ref()?.next_history != Some(before) {
                    return None;
                }
                view.read_page = Some(Page {
                    before: Some(before),
                    ..view.page
                });
                view.visible = VISIBLE_ROWS;
                return self.read(owner);
            }
            Action::Latest(owner) if self.current(owner) => {
                let view = self.view.as_mut()?;
                if view.loading.is_some() {
                    return None;
                }
                view.read_page = Some(Page {
                    before: None,
                    ..view.page
                });
                return self.read(owner);
            }
            Action::More(owner) if self.current(owner) => {
                let view = self.view.as_mut()?;
                view.visible = view.visible.saturating_add(VISIBLE_ROWS);
            }
            Action::AttachLocal(owner) if self.writable(owner) => {
                if let Some(form) = self.view.as_ref()?.form.as_ref() {
                    return Some(Effect::Focus {
                        owner,
                        id: if form.local.is_some() {
                            local_path_id(form.owner)
                        } else {
                            role_id(form.owner, form.role)
                        },
                    });
                }
                let form_owner = FormOwner {
                    view: owner,
                    draft: Uuid::new_v4(),
                };
                self.view.as_mut()?.notice = None;
                self.view.as_mut()?.error = None;
                self.view.as_mut()?.form = Some(Form {
                    owner: form_owner,
                    fixed: None,
                    expected_revision: 0,
                    host: String::new(),
                    repository: String::new(),
                    number: String::new(),
                    role: Role::Contribute,
                    initial_role: Role::Contribute,
                    local: Some(LocalDraft {
                        directory: String::new(),
                        path_generation: Uuid::new_v4(),
                        inspecting: None,
                        selected: None,
                        error: None,
                    }),
                });
                return Some(Effect::Focus {
                    owner,
                    id: local_path_id(form_owner),
                });
            }
            Action::LocalPath(owner, value) if self.writable(owner.view) => {
                let form = self.view.as_mut()?.form.as_mut()?;
                if form.owner != owner || value.len() > 4096 || value.contains('\0') {
                    return None;
                }
                let local = form.local.as_mut()?;
                if local.directory != value {
                    local.directory = value;
                    local.path_generation = Uuid::new_v4();
                    local.selected = None;
                    local.error = None;
                    // Keep a finite in-flight inspection owned until its reply;
                    // its old generation can no longer populate this path.
                }
            }
            Action::InspectLocal {
                owner,
                path_generation,
                directory,
                role,
            } if self.writable(owner.view) => {
                if self.local_inspection.is_some() {
                    return None;
                }
                let form = self.view.as_mut()?.form.as_mut()?;
                if form.owner != owner || form.role != role {
                    return None;
                }
                let local = form.local.as_mut()?;
                if local.path_generation != path_generation
                    || local.directory != directory
                    || local.inspecting.is_some()
                {
                    return None;
                }
                let directory = local.directory()?;
                let ticket = Ticket::new(owner.view.home);
                self.local_inspection = Some(ticket);
                local.inspecting = Some(Inspection {
                    ticket,
                    path_generation,
                });
                local.selected = None;
                local.error = None;
                return Some(Effect::PrepareLocal {
                    ticket,
                    home: owner.view.home,
                    directory,
                });
            }
            Action::PreparedLocal { ticket, result } => {
                let view = self.view.as_mut()?;
                if view.owner.home != ticket.home {
                    return None;
                }
                let form = view.form.as_mut()?;
                let local = form.local.as_mut()?;
                if local.inspecting.as_ref()?.ticket != ticket {
                    return None;
                }
                let inspection = local.inspecting.take()?;
                if local.path_generation != inspection.path_generation {
                    return None;
                }
                match result {
                    Ok(selected)
                        if selected.home == view.owner.home && selected.validate().is_ok() =>
                    {
                        local.selected = Some(selected);
                        local.error = None;
                    }
                    Ok(_) => {
                        local.error = Some(
                            "The selected checkout did not match this channel. Inspect it again."
                                .into(),
                        )
                    }
                    Err(Failure::Refused(reason)) => {
                        local.error = Some(format!("Checkout inspection was refused: {reason}"))
                    }
                    Err(Failure::Unavailable) => {
                        local.error = Some(
                            "The checkout could not be inspected. No tracking change was sent."
                                .into(),
                        )
                    }
                }
            }
            Action::SubmitLocal {
                owner,
                selection,
                target,
                role,
                revision,
            } if self.writable(owner.view) => {
                let form = self.view.as_ref()?.form.as_ref()?;
                if form.owner != owner || form.role != role {
                    return None;
                }
                let local = form.local.as_ref()?;
                let selected = local.selected.as_ref()?;
                if local.inspecting.is_some()
                    || selected.selection != selection
                    || selected.context.id != target
                    || selected.home != owner.view.home
                    || selected.expected_revision != revision
                    || selected.validate().is_err()
                {
                    return None;
                }
                // Exact selected-target point CAS comes from the daemon, even
                // when that target is absent from these bounded display pages.
                return self.begin_selected(
                    owner.view,
                    revision,
                    Change::Claim {
                        target: Target::Local(target),
                        role,
                    },
                    Some(selection),
                );
            }
            Action::Attach(owner) if self.writable(owner) => {
                if let Some(form) = self.view.as_ref()?.form.as_ref() {
                    return Some(Effect::Focus {
                        owner,
                        id: role_id(form.owner, form.role),
                    });
                }
                self.view.as_mut()?.notice = None;
                self.view.as_mut()?.error = None;
                self.view.as_mut()?.form = Some(Form {
                    owner: FormOwner {
                        view: owner,
                        draft: Uuid::new_v4(),
                    },
                    fixed: None,
                    expected_revision: 0,
                    host: "github.com".into(),
                    repository: String::new(),
                    number: String::new(),
                    role: Role::Contribute,
                    initial_role: Role::Contribute,
                    local: None,
                });
                return Some(Effect::Focus {
                    owner,
                    id: form_input_id(self.view.as_ref()?.form.as_ref()?.owner, Field::Host),
                });
            }
            Action::Field(owner, field, value) if self.writable(owner.view) => {
                let form = self.view.as_mut()?.form.as_mut()?;
                if form.owner != owner
                    || form.local.is_some()
                    || form.fixed.is_some()
                    || value.len() > 1024
                {
                    return None;
                }
                match field {
                    Field::Host => {
                        form.use_split_repository();
                        form.host = value;
                    }
                    Field::Repository => {
                        form.repository = value;
                        if let Some(key) = form.repository_url() {
                            form.host = key.repository.authority();
                            form.number = key.number.to_string();
                        }
                    }
                    Field::Number => {
                        form.use_split_repository();
                        form.number = value;
                    }
                }
            }
            Action::Role(owner, index) if self.writable(owner.view) => {
                let role = *ROLE_VALUES.get(index)?;
                let form = self.view.as_mut()?.form.as_mut()?;
                if form.owner == owner {
                    form.role = role;
                }
            }
            Action::Dismiss(owner) if self.current(owner.view) && !self.submitted_form(owner) => {
                if self.view.as_ref()?.form.as_ref()?.owner == owner {
                    return self.leave(owner.view, Leave::Dismiss);
                }
            }
            Action::Submit {
                owner,
                target,
                role,
                revision,
            } if self.writable(owner.view) => {
                let form = self.view.as_ref()?.form.as_ref()?;
                if form.owner != owner
                    || form.local.is_some()
                    || form.target().ok().as_ref() != Some(&target)
                    || form.role != role
                {
                    return None;
                }
                let expected = revision;
                if form.fixed.is_some() && form.expected_revision != revision {
                    return None;
                }
                let snapshot = self.view.as_ref()?.snapshot.as_ref()?;
                let actual = snapshot
                    .entries
                    .iter()
                    .chain(&snapshot.history)
                    .find(|entry| entry.target == target)
                    .map_or(0, |entry| entry.revision);
                if actual != expected {
                    self.view.as_mut()?.error = Some(
                        "This work changed while the form was open. Cancel and review it again."
                            .into(),
                    );
                    return None;
                }
                return self.begin(owner.view, expected, Change::Claim { target, role });
            }
            Action::Correct(owner) => {
                let entry = self.row(&owner, true)?;
                if entry.observation.as_ref().is_some_and(|observation| {
                    matches!(observation.state, PrState::Closed | PrState::Merged)
                }) {
                    return None;
                }
                let role = entry.role().unwrap_or(Role::Contribute);
                self.view.as_mut()?.notice = None;
                self.view.as_mut()?.error = None;
                self.view.as_mut()?.form = Some(Form {
                    owner: FormOwner {
                        view: owner.view,
                        draft: Uuid::new_v4(),
                    },
                    fixed: Some(owner.target),
                    expected_revision: owner.revision,
                    host: String::new(),
                    repository: String::new(),
                    number: String::new(),
                    role,
                    initial_role: role,
                    local: None,
                });
                return Some(Effect::Focus {
                    owner: owner.view,
                    id: role_id(self.view.as_ref()?.form.as_ref()?.owner, role),
                });
            }
            Action::End(owner) => {
                if self.row(&owner, true)?.bucket() != Bucket::Ongoing {
                    return None;
                }
                return self.begin(
                    owner.view,
                    owner.revision,
                    Change::End {
                        target: owner.target,
                    },
                );
            }
            Action::Suppress(owner) => {
                if self.row(&owner, true)?.bucket() != Bucket::NeedsConfirmation {
                    return None;
                }
                return self.begin(
                    owner.view,
                    owner.revision,
                    Change::Suppress {
                        target: owner.target,
                    },
                );
            }
            Action::Details(owner, expanded) => {
                self.row(&owner, false)?;
                self.view.as_mut()?.details = expanded.then_some(owner.id);
            }
            Action::Open(owner) => {
                self.row(&owner, false)?;
                let url = owner.target.pull_request_url().ok()??;
                if let Target::PullRequest(key) = owner.target {
                    return Some(Effect::OpenPullRequest {
                        home: owner.view.home,
                        key,
                        url,
                    });
                }
            }
            Action::Check(owner, operation) if self.current(owner) => {
                let pending = self.pending.iter_mut().find(|pending| {
                    pending.mutation.home == owner.home && pending.mutation.operation == operation
                })?;
                if pending.delivery != Delivery::Unknown {
                    return None;
                }
                let ticket = Ticket::new(owner.home);
                pending.lookup = Some(ticket);
                pending.retry_ready = false;
                pending.delivery = Delivery::Checking;
                return Some(Effect::Lookup {
                    ticket,
                    home: owner.home,
                    operation,
                });
            }
            Action::Retry(owner, operation) if self.current(owner) => {
                let pending = self.pending.iter_mut().find(|pending| {
                    pending.mutation.home == owner.home && pending.mutation.operation == operation
                })?;
                if pending.delivery != Delivery::Unknown
                    || pending.confirmed.is_some()
                    || !pending.retry_ready
                    || pending.receipt_only
                {
                    return None;
                }
                let ticket = Ticket::new(owner.home);
                pending.ticket = ticket;
                pending.lookup = None;
                pending.retry_ready = false;
                pending.delivery = Delivery::Sending;
                return Some(Effect::Mutate {
                    ticket,
                    mutation: pending.mutation.clone(),
                    selection: pending.selection,
                });
            }
            _ => {}
        }
        None
    }
}

impl State {
    pub fn view(&self, width: f32, height: f32) -> Element<'_, Action> {
        if self.saved.shown {
            return self.saved_view(width, height);
        }
        let Some(view) = &self.view else {
            return widget::Space::new().into();
        };
        let owner = view.owner;
        let narrow = width < 560.0;
        let close = ui_controls::owned(
            (owner, "close"),
            ui_controls::icon_button(Icon::Close, "Close channel work")
                .on_press(Action::Close(owner)),
        );
        let refresh = ui_controls::owned(
            (owner, "refresh"),
            ui_controls::icon_button(
                if view.loading.is_some() {
                    Icon::Loading
                } else {
                    Icon::Refresh
                },
                if view.loading.is_some() {
                    "Updating channel work…"
                } else {
                    "Refresh channel work"
                },
            )
            .on_press_maybe(view.loading.is_none().then_some(Action::Refresh(owner))),
        );
        let saved = ui_controls::owned(
            (owner, "saved-changes"),
            ui_controls::icon_button(Icon::History, "Saved channel changes")
                .on_press(Action::ShowSaved),
        );
        let title = widget::column::with_children(vec![
            design::caption("Channel work").into(),
            design::title(view.title.as_str()).into(),
        ])
        .spacing(space::XXS);
        let header = widget::row::with_children(vec![
            widget::container(title).width(Length::Fill).into(),
            saved,
            refresh,
            close,
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center);
        let mut status: Vec<Element<'_, Action>> = Vec::new();
        let mut body: Vec<Element<'_, Action>> = Vec::new();
        if let Some(confirmation) = &view.confirmation {
            status.push(panel(vec![design::heading("Discard this tracking draft?").into(),
                design::body("The unsaved target or role will be lost. Submitted changes are tracked separately and will not be cancelled.").into(),
                flow(vec![
                    ui_controls::owned((owner, confirmation.id, "keep"), button::primary("Keep editing").id(confirmation_id(confirmation.id)).on_press(Action::Keep(owner, confirmation.id))),
                    ui_controls::owned((owner, confirmation.id, "discard"), button::destructive("Discard draft").on_press(Action::Discard(owner, confirmation.id))),
                ], narrow),
            ]));
        }
        if let Some(error) = &view.error {
            status.push(message(error, true));
        }
        if let Some(notice) = &view.notice {
            status.push(message(notice, false));
        }
        self.saved_status(&mut status);
        if view.read_only {
            status.push(design::caption("Saved change recovery: check original results here. Restored requests are never resent. Open the channel normally to start a new change.").into());
        }
        for pending in self
            .pending
            .iter()
            .filter(|pending| pending.mutation.home == owner.home)
        {
            status.push(self.pending_view(owner, pending, narrow));
        }
        if self.pending_for(owner.home).is_none()
            && (self
                .pending
                .iter()
                .filter(|pending| !pending.receipt_only)
                .count()
                >= MAX_PENDING
                || self.pending.len() >= MAX_RECOVERY)
        {
            status.push(message("Other channels still have unconfirmed changes. Reads and navigation remain available; confirm a pending change before starting another.", true));
        }
        let Some(snapshot) = &view.snapshot else {
            body.push(
                ui_controls::placeholder(if view.loading.is_some() {
                    "Loading this channel’s work…"
                } else {
                    "Channel work is unavailable"
                })
                .icon(Icon::PullRequest.handle())
                .detail(
                    "PR tracking is separate from the conversations and checkouts in this channel.",
                )
                .into(),
            );
            return mount(header, status, body, height);
        };
        // Background reads must not move fields under the pointer while editing.
        // The fixed-size header control still announces progress during a form.
        if view.loading.is_some() && view.form.is_none() {
            status.push(design::caption("Updating the current view…").into());
        }
        if snapshot.lifecycle != HomeLifecycle::Active {
            status.push(message(match snapshot.lifecycle {
                HomeLifecycle::Archived => "This channel is archived. Its work history is preserved; reopen the channel to change tracking.",
                HomeLifecycle::Deleted => "This channel was deleted. Preserved work history is read-only.",
                HomeLifecycle::Active => unreachable!(),
            }, false));
        }
        let mut counts = [0usize; 3];
        for entry in &snapshot.entries {
            counts[match entry.bucket() {
                Bucket::Ongoing => 0,
                Bucket::NeedsConfirmation => 1,
                Bucket::History => 2,
            }] += 1;
        }
        counts[2] = snapshot.history.len();
        let tabs: Vec<_> = [
            (
                Bucket::Ongoing,
                "Ongoing",
                "Ongoing tracking on this page",
                counts[0],
            ),
            (
                Bucket::NeedsConfirmation,
                if narrow {
                    "Confirm"
                } else {
                    "Needs confirmation"
                },
                "Work needing confirmation on this page",
                counts[1],
            ),
            (Bucket::History, "History", "Loaded work history", counts[2]),
        ]
        .into_iter()
        .map(|(bucket, label, description, count)| ViewTab {
            label: label.into(),
            description: description.into(),
            count: Some((count.to_string().into(), design::muted_text)),
            selected: view.tab == bucket,
            id: None,
            on_press: Some(Action::Tab(owner, bucket)),
        })
        .collect();
        let navigation = if width < 360.0 {
            widget::column::with_children(
                tabs.into_iter()
                    .map(|tab| ui_controls::view_tab(tab, true, None))
                    .collect::<Vec<_>>(),
            )
            .spacing(space::XS)
            .into()
        } else {
            ui_controls::view_tabs(tabs, narrow)
        };
        body.push(ui_controls::owned((owner, "tabs"), navigation));
        body.push(flow(vec![design::caption("Counts describe the loaded pages, not all channel work. Tracked roles describe intent; PR state and checkout observations remain separate.").width(Length::Fill).into(),
            ui_controls::owned((owner, "attach", self.writable(owner)), button::secondary("Track PR…").icon(Icon::PullRequest).id(attach_id(owner))
                .on_press_maybe(self.writable(owner).then_some(Action::Attach(owner)))),
            ui_controls::owned((owner, "attach-local", self.writable(owner)), button::quiet("Track local work…")
                .on_press_maybe(self.writable(owner).then_some(Action::AttachLocal(owner))))], narrow));
        if let Some(form) = &view.form {
            body.push(self.form_view(view, form, narrow));
        }
        let entries: Vec<_> = if view.tab == Bucket::History {
            snapshot.history.iter().collect()
        } else {
            snapshot
                .entries
                .iter()
                .filter(|entry| entry.bucket() == view.tab)
                .collect()
        };
        if entries.is_empty() {
            let has_more_current =
                view.page.current_before.is_some() || snapshot.next_current.is_some();
            let (title, detail) = match view.tab {
                Bucket::Ongoing if has_more_current => (
                    "No ongoing work on this page",
                    "Other current pages may contain ongoing work. Use Next current page or Latest current work.",
                ),
                Bucket::NeedsConfirmation if has_more_current => (
                    "No confirmation needed on this page",
                    "Other current pages may contain relations that need confirmation.",
                ),
                Bucket::Ongoing => (
                    "No ongoing work is tracked here",
                    "Track a PR explicitly or review a suggested relation. A checkout alone is not a contribution.",
                ),
                Bucket::NeedsConfirmation => (
                    "No work needs confirmation",
                    "Related observations appear here when their connection to this channel needs a decision.",
                ),
                Bucket::History => (
                    "No history on this page",
                    "Ended, closed and merged work remains available after its conversations finish.",
                ),
            };
            body.push(
                ui_controls::placeholder(title)
                    .icon(Icon::History.handle())
                    .detail(detail)
                    .into(),
            );
        }
        if view.tab == Bucket::Ongoing {
            let work = entries
                .iter()
                .filter(|entry| entry.is_ongoing_work())
                .count();
            let awaiting = entries
                .iter()
                .filter(|entry| ongoing_group(entry) == OngoingGroup::ConfirmPrevious)
                .count();
            let related = entries.len() - work - awaiting;
            let summary = format!(
                "On this page: {work} work relations · {related} review or reference relations"
            );
            body.push(
                design::caption(if awaiting == 0 {
                    summary
                } else {
                    format!("{summary} · {awaiting} work declarations awaiting confirmation")
                })
                .into(),
            );
            let visible = [
                OngoingGroup::Working,
                OngoingGroup::ConfirmPrevious,
                OngoingGroup::Related,
            ]
            .into_iter()
            .flat_map(|group| {
                entries
                    .iter()
                    .filter(move |entry| ongoing_group(entry) == group)
            })
            .take(view.visible)
            .copied()
            .collect::<Vec<_>>();
            for (group, heading, detail) in [
                (
                    OngoingGroup::Working,
                    "Working on",
                    "Contributing and stewarding are explicit work declarations in this channel.",
                ),
                (
                    OngoingGroup::ConfirmPrevious,
                    "Confirm previous work",
                    "Confirm your role to continue work after the PR ended. A new review does not restart an earlier contribution.",
                ),
                (
                    OngoingGroup::Related,
                    "Reviews and references",
                    "Related to this channel, without declaring implementation or responsibility for progressing the work.",
                ),
            ] {
                let selected = visible
                    .iter()
                    .filter(|entry| ongoing_group(entry) == group)
                    .collect::<Vec<_>>();
                if !selected.is_empty() || (group == OngoingGroup::Working && !entries.is_empty()) {
                    body.push(design::heading(heading).into());
                    body.push(design::caption(detail).into());
                }
                if group == OngoingGroup::Working && work == 0 && !entries.is_empty() {
                    body.push(
                        design::body("No contributed or stewarded work on this page.").into(),
                    );
                }
                for entry in selected {
                    body.push(self.entry_view(view, entry, narrow));
                }
            }
        } else {
            for entry in entries.iter().take(view.visible) {
                body.push(self.entry_view(view, entry, narrow));
            }
        }
        if entries.len() > view.visible {
            body.push(ui_controls::owned(
                (owner, "more"),
                button::quiet(format!(
                    "Show more · {} remaining",
                    entries.len() - view.visible
                ))
                .on_press(Action::More(owner)),
            ));
        }
        if view.tab != Bucket::History {
            let mut navigation = Vec::new();
            if view.page.current_before.is_some() {
                navigation.push(ui_controls::owned(
                    (owner, "latest-current"),
                    button::secondary("Latest current work").on_press_maybe(
                        view.loading
                            .is_none()
                            .then_some(Action::LatestCurrent(owner)),
                    ),
                ));
            }
            if let Some(before) = snapshot.next_current {
                navigation.push(ui_controls::owned(
                    (owner, before, "next-current"),
                    button::secondary("Next current page").on_press_maybe(
                        view.loading
                            .is_none()
                            .then_some(Action::NextCurrent(owner, before)),
                    ),
                ));
            }
            if !navigation.is_empty() {
                body.push(flow(navigation, narrow));
            }
            body.push(design::caption("Ongoing and confirmation share a current page. Pages replace the loaded relations; counts describe this page only.").into());
        }
        if view.tab == Bucket::History {
            let mut navigation = Vec::new();
            if view.page.before.is_some() {
                navigation.push(ui_controls::owned(
                    (owner, "latest"),
                    button::secondary("Latest history")
                        .on_press_maybe(view.loading.is_none().then_some(Action::Latest(owner))),
                ));
            }
            if let Some(before) = snapshot.next_history {
                navigation.push(ui_controls::owned(
                    (owner, before, "earlier"),
                    button::secondary("Earlier history").on_press_maybe(
                        view.loading
                            .is_none()
                            .then_some(Action::Earlier(owner, before)),
                    ),
                ));
            }
            if !navigation.is_empty() {
                body.push(flow(navigation, narrow));
            }
            body.push(design::caption("History shows each relation’s latest ended state. Earlier pages replace this page; they do not load an unbounded timeline.").into());
        }
        mount(header, status, body, height)
    }

    fn form_view<'a>(
        &'a self,
        view: &'a View,
        form: &'a Form,
        narrow: bool,
    ) -> Element<'a, Action> {
        if let Some(local) = &form.local {
            return self.local_form_view(view, form, local, narrow);
        }
        let enabled = self.writable(view.owner);
        let owner = form.owner;
        let target = form.target();
        let revision = target
            .as_ref()
            .ok()
            .and_then(|target| {
                view.snapshot
                    .as_ref()?
                    .entries
                    .iter()
                    .chain(&view.snapshot.as_ref()?.history)
                    .find(|entry| &entry.target == target)
                    .map(|entry| entry.revision)
            })
            .unwrap_or(0);
        let submit = target
            .as_ref()
            .ok()
            .filter(|_| enabled)
            .map(|target| Action::Submit {
                owner,
                target: target.clone(),
                role: form.role,
                revision,
            });
        let mut rows = vec![
            design::heading(if form.fixed.is_some() {
                "Change tracking"
            } else {
                "Track a pull request"
            })
            .into(),
        ];
        if let Some(target) = &form.fixed {
            rows.push(design::body(target_label(target)).into());
        } else {
            for (label, placeholder, value, kind) in [
                ("GitHub host", "github.com", form.host.as_str(), Field::Host),
                (
                    "Repository",
                    "owner/repository or PR URL",
                    form.repository.as_str(),
                    Field::Repository,
                ),
                ("PR number", "123", form.number.as_str(), Field::Number),
            ] {
                rows.push(design::caption(label).into());
                let enter = submit.clone();
                rows.push(ui_controls::owned(
                    (owner, label),
                    field::text(placeholder, value)
                        .id(form_input_id(owner, kind))
                        .on_input_maybe(
                            enabled.then_some(move |value| Action::Field(owner, kind, value)),
                        )
                        .on_submit_maybe(enter.map(|action| move |_| action.clone())),
                ));
            }
        }
        rows.push(design::caption("Your role").into());
        rows.push(if enabled {
            let mut choices = ROLE_VALUES.iter().enumerate().map(|(index, role)| {
                let selected = *role == form.role;
                let action = Action::Role(owner, index);
                ui_controls::owned(
                    (owner, *role),
                    crate::interaction::focused_key(
                        widget::button::custom(design::body(ROLES[index]))
                            .name(ROLES[index])
                            .description(format!(
                                "{}{}",
                                if selected {
                                    "Selected role. "
                                } else {
                                    "Select role. "
                                },
                                role.description()
                            ))
                            .id(role_id(owner, *role))
                            .selected(selected)
                            .class(ui_controls::control_style(selected, None))
                            .padding([space::XS, space::S])
                            .width(Length::Fill)
                            .on_press(action.clone()),
                        // Handle Enter before Button sets a lingering native press.
                        move |event| {
                            matches!(
                                event,
                                cosmic::iced::keyboard::Event::KeyPressed {
                                    key: cosmic::iced::keyboard::Key::Named(
                                        cosmic::iced::keyboard::key::Named::Enter
                                    ),
                                    ..
                                }
                            )
                            .then(|| action.clone())
                        },
                    ),
                )
            });
            if narrow {
                widget::column::with_children((0..2).map(|_| {
                    widget::row::with_children(choices.by_ref().take(2).collect::<Vec<_>>())
                        .spacing(space::S)
                        .into()
                }))
                .spacing(space::S)
                .into()
            } else {
                widget::row::with_children(choices.collect::<Vec<_>>())
                    .spacing(space::S)
                    .into()
            }
        } else {
            design::body(form.role.label()).into()
        });
        rows.push(design::caption(form.role.description()).into());
        if let Err(error) = &target {
            if !form.repository.is_empty() || !form.number.is_empty() || form.fixed.is_some() {
                rows.push(message(*error, true));
            }
        }
        let mut controls = Vec::new();
        if let Ok(target) = target {
            let action = Action::Submit {
                owner,
                target: target.clone(),
                role: form.role,
                revision,
            };
            controls.push(ui_controls::owned(
                (owner, target, form.role, revision, enabled),
                button::primary(if form.fixed.is_some() {
                    "Apply tracking"
                } else {
                    "Track PR"
                })
                .on_press_maybe(enabled.then_some(action)),
            ));
        } else {
            controls.push(button::primary::<Action>("Track PR").into());
        }
        controls.push(ui_controls::owned(
            (owner, "cancel", !self.submitted_form(owner)),
            button::quiet("Cancel")
                .on_press_maybe((!self.submitted_form(owner)).then_some(Action::Dismiss(owner))),
        ));
        rows.push(flow(controls, narrow));
        panel(rows)
    }

    fn local_form_view<'a>(
        &'a self,
        view: &'a View,
        form: &'a Form,
        local: &'a LocalDraft,
        narrow: bool,
    ) -> Element<'a, Action> {
        let owner = form.owner;
        let enabled = self.writable(view.owner);
        let can_submit = self.local_writable(view.owner);
        let inspect = Action::InspectLocal {
            owner,
            path_generation: local.path_generation,
            directory: local.directory.clone(),
            role: form.role,
        };
        let can_inspect = enabled
            && self.local_inspection.is_none()
            && local.inspecting.is_none()
            && local.directory().is_some();
        let mut rows: Vec<Element<'a, Action>> = vec![
            design::heading("Track local work").into(),
            design::body("Choose a checkout explicitly. Branch and filesystem observations do not establish contribution or a pull request association.").into(),
            design::caption("Checkout directory on this device").into(),
            ui_controls::owned((owner, "local-path"), field::text("/absolute/path/to/checkout", &local.directory)
                .id(local_path_id(owner))
                .on_input_maybe(enabled.then_some(move |value| Action::LocalPath(owner, value)))
                .on_submit_maybe(can_inspect.then_some({let action = inspect.clone(); move |_| action.clone()}))),
            design::caption("Your role").into(),
        ];
        rows.push(if enabled {
            let mut choices = ROLE_VALUES.iter().enumerate().map(|(index, role)| {
                let selected = *role == form.role;
                let action = Action::Role(owner, index);
                ui_controls::owned(
                    (owner, *role),
                    crate::interaction::focused_key(
                        widget::button::custom(design::body(ROLES[index]))
                            .name(ROLES[index])
                            .description(format!(
                                "{}{}",
                                if selected {
                                    "Selected role. "
                                } else {
                                    "Select role. "
                                },
                                role.description()
                            ))
                            .id(role_id(owner, *role))
                            .selected(selected)
                            .class(ui_controls::control_style(selected, None))
                            .padding([space::XS, space::S])
                            .width(Length::Fill)
                            .on_press(action.clone()),
                        // Handle Enter before Button sets a lingering native press.
                        move |event| {
                            matches!(
                                event,
                                cosmic::iced::keyboard::Event::KeyPressed {
                                    key: cosmic::iced::keyboard::Key::Named(
                                        cosmic::iced::keyboard::key::Named::Enter
                                    ),
                                    ..
                                }
                            )
                            .then(|| action.clone())
                        },
                    ),
                )
            });
            if narrow {
                widget::column::with_children((0..2).map(|_| {
                    widget::row::with_children(choices.by_ref().take(2).collect::<Vec<_>>())
                        .spacing(space::S)
                        .into()
                }))
                .spacing(space::S)
                .into()
            } else {
                widget::row::with_children(choices.collect::<Vec<_>>())
                    .spacing(space::S)
                    .into()
            }
        } else {
            design::body(form.role.label()).into()
        });
        rows.push(design::caption(form.role.description()).into());
        if let Some(error) = &local.error {
            rows.push(message(error, true));
        }
        if !local.directory.is_empty() && local.directory().is_none() {
            rows.push(message(
                "Enter an absolute checkout directory without leading or trailing spaces.",
                true,
            ));
        }
        if self.local_inspection.is_some() && local.inspecting.is_none() {
            rows.push(design::caption("An earlier checkout inspection is finishing. Inspect this path explicitly when it settles.").into());
        }
        if local.inspecting.is_some() {
            rows.push(
                design::caption("Inspecting checkout… No tracking change has been sent.").into(),
            );
        }
        if let Some(selected) = &local.selected {
            rows.push(design::heading("Selected checkout").into());
            for text in [
                format!("Checkout: {}", selected.context.path.display()),
                format!("Repository: {}", selected.context.repository.display()),
                format!(
                    "Branch: {}",
                    selected
                        .context
                        .branch
                        .as_deref()
                        .unwrap_or("Detached or unavailable")
                ),
                format!(
                    "HEAD: {}",
                    selected.context.head.as_deref().unwrap_or("Unavailable")
                ),
                match selected.context.dirty {
                    Some(true) => "Uncommitted changes were observed.".into(),
                    Some(false) => "No uncommitted changes were observed.".into(),
                    None => "Working-tree changes not inspected.".into(),
                },
            ] {
                rows.push(design::body(text).width(Length::Fill).into());
            }
            rows.push(design::caption("This inspection is observational. Tracking declares your chosen role; it does not verify execution or create a pull request.").into());
        }
        let mut controls = vec![ui_controls::owned(
            (owner, local.path_generation, form.role, can_inspect),
            button::secondary(if local.selected.is_some() {
                "Inspect again"
            } else {
                "Inspect checkout"
            })
            .on_press_maybe(can_inspect.then_some(inspect)),
        )];
        if let Some(selected) = &local.selected {
            controls.push(ui_controls::owned(
                (
                    owner,
                    selected.selection,
                    selected.context.id,
                    form.role,
                    selected.expected_revision,
                    can_submit,
                ),
                button::primary("Track local work")
                    .name("Track selected local checkout")
                    .on_press_maybe(can_submit.then_some(Action::SubmitLocal {
                        owner,
                        selection: selected.selection,
                        target: selected.context.id,
                        role: form.role,
                        revision: selected.expected_revision,
                    })),
            ));
        }
        controls.push(ui_controls::owned(
            (
                owner,
                "cancel-local",
                self.pending_for(view.owner.home).is_none(),
            ),
            button::quiet("Cancel").on_press_maybe(
                self.pending_for(view.owner.home)
                    .is_none()
                    .then_some(Action::Dismiss(owner)),
            ),
        ));
        rows.push(flow(controls, narrow));
        panel(rows)
    }
    fn saved_status<'a>(&'a self, rows: &mut Vec<Element<'a, Action>>) {
        if self.saved.loading {
            rows.push(
                design::caption(
                    "Loading saved changes… New local tracking waits for recovery discovery.",
                )
                .into(),
            );
        }
        if let Some(error) = &self.saved.error {
            rows.push(message(error, true));
        }
        for problem in self.saved.problems.iter().take(MAX_PENDING + 1) {
            rows.push(message(problem, true));
        }
        if self.saved.saturated {
            rows.push(message("Saved local changes are at capacity. Check original results before starting another local change; ordinary PR tracking and reads remain available.", true));
        }
        if !self.saved.ready && !self.saved.loading && self.saved.error.is_none() {
            rows.push(
                design::caption(
                    "Saved changes have not been loaded. New local tracking waits for recovery discovery.",
                )
                .into(),
            );
        }
    }
    fn pending_view<'a>(
        &'a self,
        owner: ViewOwner,
        pending: &'a Pending,
        narrow: bool,
    ) -> Element<'a, Action> {
        let text = if pending.confirmed.is_some() {
            "The original change is confirmed. Its saved recovery record still needs to be updated."
        } else {
            match pending.delivery {
                Delivery::Sending => {
                    "Updating tracking… Closing this view does not cancel the change."
                }
                Delivery::Checking => {
                    "Checking the original change. Tracking controls remain paused until its result is confirmed."
                }
                Delivery::Unknown => {
                    "The change is not yet confirmed. A missing receipt does not mean it stopped."
                }
            }
        };
        let mut rows = vec![
            design::heading(operation_label(&pending.mutation)).into(),
            design::body(text).into(),
            design::caption(format!(
                "Original operation {} · {}",
                pending.mutation.operation, pending.device_label
            ))
            .into(),
        ];
        if pending.delivery == Delivery::Unknown {
            let mut actions = vec![ui_controls::owned(
                (
                    owner,
                    pending.mutation.operation,
                    pending.confirmed.is_some(),
                    "lookup",
                ),
                button::secondary(if pending.confirmed.is_some() {
                    "Save confirmation"
                } else {
                    "Check result"
                })
                .name(format!(
                    "Check original operation {}",
                    pending.mutation.operation
                ))
                .on_press(Action::Check(owner, pending.mutation.operation)),
            )];
            if pending.confirmed.is_none() && pending.retry_ready && !pending.receipt_only {
                actions.push(ui_controls::owned(
                    (owner, pending.mutation.operation, "retry"),
                    button::quiet("Retry original change")
                        .name(format!(
                            "Retry original operation {}",
                            pending.mutation.operation
                        ))
                        .on_press(Action::Retry(owner, pending.mutation.operation)),
                ));
            }
            rows.push(flow(actions, narrow));
            rows.push(design::caption(if pending.confirmed.is_some() {
                "Retry is disabled because the original result is already confirmed."
            } else if pending.receipt_only {
                "This saved request is read-only. Its selection is not restored. Check result never resends it; a missing receipt remains unknown."
            } else if !pending.retry_ready {
                "Check result first to verify the original destination before retrying."
            } else { "Retry sends the same target, role and operation. It does not apply a newly edited request." }).into());
        }
        panel(rows)
    }
    fn saved_view(&self, _width: f32, height: f32) -> Element<'_, Action> {
        let generation = self.saved.generation;
        let title = widget::column::with_children(vec![
            design::heading("Saved channel changes").into(),
            design::caption(format!("{} retained operations", self.pending.len())).into(),
        ])
        .spacing(space::XXS);
        let reload = ui_controls::owned(
            (generation, "saved-reload"),
            ui_controls::icon_button(Icon::Refresh, "Reload saved changes")
                .on_press_maybe((!self.saved.loading).then_some(Action::ShowSaved)),
        );
        let close = ui_controls::owned(
            (generation, "saved-close"),
            ui_controls::icon_button(Icon::Close, "Close saved changes")
                .on_press(Action::CloseSaved(generation)),
        );
        let header = widget::row::with_children(vec![
            widget::container(title).width(Length::Fill).into(),
            reload,
            close,
        ])
        .spacing(space::XS)
        .align_y(Alignment::Center);
        let mut rows: Vec<Element<'_, Action>> = Vec::new();
        self.saved_status(&mut rows);
        rows.push(design::body("These are the original tracking changes. Opening one does not resend it. A missing receipt never proves that it stopped.").into());
        for pending in &self.pending {
            let status = if pending.confirmed.is_some() {
                "Confirmed · recovery save pending"
            } else {
                match pending.delivery {
                    Delivery::Sending => "Sending",
                    Delivery::Checking => "Checking result",
                    Delivery::Unknown => "Result unknown",
                }
            };
            let home = pending.mutation.home;
            let content = vec![
                design::heading(operation_label(&pending.mutation)).into(),
                design::body(if pending.title.is_empty() {
                    format!("Channel {}", home.channel)
                } else {
                    pending.title.clone()
                })
                .into(),
                design::caption(format!("{} · {status}", pending.device_label)).into(),
                design::caption(format!(
                    "Original home {} / {} / {}",
                    home.registry, home.channel, home.creation_ordinal
                ))
                .into(),
                design::caption(format!("Operation {}", pending.mutation.operation)).into(),
                ui_controls::owned(
                    (generation, pending.mutation.operation, "open-saved"),
                    button::secondary("Open original change")
                        .name(format!(
                            "Open saved operation {}",
                            pending.mutation.operation
                        ))
                        .on_press_maybe(pending.saved_record.then_some(Action::OpenSaved {
                            generation,
                            operation: pending.mutation.operation,
                        })),
                )
                .into(),
            ];
            let mut content = content;
            if !pending.saved_record {
                content.push(design::caption("The original request is retained in this window. Reopen its original channel to check it; saved metadata has not been recovered.").into());
            }
            rows.push(panel(content));
        }
        if self.pending.is_empty()
            && self.saved.ready
            && self.saved.problems.is_empty()
            && !self.saved.loading
        {
            rows.push(design::body("No saved tracking changes need confirmation.").into());
        }
        mount(header, Vec::new(), rows, height)
    }

    fn entry_view<'a>(
        &'a self,
        view: &'a View,
        entry: &'a Entry,
        narrow: bool,
    ) -> Element<'a, Action> {
        let owner = RowOwner {
            view: view.owner,
            id: entry.id,
            target: entry.target.clone(),
            revision: entry.revision,
            bucket: entry.bucket(),
        };
        let title = entry
            .observation
            .as_ref()
            .and_then(|observation| observation.title.as_deref())
            .filter(|title| !title.is_empty())
            .map_or_else(|| target_label(&entry.target), str::to_owned);
        let role = entry.role();
        let mut rows = vec![
            design::heading(title).into(),
            design::caption(target_label(&entry.target))
                .class(cosmic::theme::Text::Custom(design::muted_text))
                .into(),
        ];
        let role_label = if entry.bucket() == Bucket::Ongoing
            && ongoing_group(entry) == OngoingGroup::ConfirmPrevious
        {
            match role {
                Some(Role::Contribute) => "Previously contributing",
                Some(Role::Steward) => "Previously stewarding",
                _ => "Work intent needs confirmation",
            }
        } else {
            role.map_or(
                if entry.bucket() == Bucket::History {
                    "Tracking ended"
                } else {
                    "Not confirmed"
                },
                Role::label,
            )
        };
        let status = entry_status(entry);
        rows.push(design::body(format!("{role_label} · {status}")).into());
        if entry.suppressed {
            rows.push(design::caption("Marked ‘not this channel’ by a user.").into());
        } else if entry.user_ended {
            rows.push(design::caption("Tracking ended by a user.").into());
        }
        if let Some(observation) = &entry.observation {
            rows.push(
                design::caption(format!(
                    "Last observed {}",
                    observed_time(observation.observed_at_ms)
                ))
                .into(),
            );
        }
        let mut controls = Vec::new();
        if matches!(entry.target, Target::PullRequest(_))
            && entry.target.pull_request_url().ok().flatten().is_some()
        {
            // Read-only navigation needs the exact key, not a mutable entry
            // revision: unrelated role/status updates must preserve this hold.
            controls.push(ui_controls::owned(
                (view.owner, entry.id, entry.target.clone(), "open"),
                button::secondary("Open PR")
                    .name(format!("Open {}", target_label(&entry.target)))
                    .on_press(Action::Open(owner.clone())),
            ));
        }
        let mutable = self.writable(view.owner) && view.form.is_none();
        let terminal = entry.observation.as_ref().is_some_and(|observation| {
            matches!(observation.state, PrState::Closed | PrState::Merged)
        });
        if !terminal {
            let label = match entry.bucket() {
                Bucket::Ongoing if ongoing_group(entry) == OngoingGroup::ConfirmPrevious => {
                    "Confirm work role…"
                }
                Bucket::Ongoing => "Change role",
                Bucket::NeedsConfirmation => "Confirm role…",
                Bucket::History => "Resume…",
            };
            controls.push(ui_controls::owned(
                (owner.clone(), "role", mutable),
                button::quiet(label)
                    .name(format!("{label} for {}", target_label(&entry.target)))
                    .on_press_maybe(mutable.then_some(Action::Correct(owner.clone()))),
            ));
        }
        if entry.bucket() == Bucket::Ongoing {
            controls.push(ui_controls::owned(
                (owner.clone(), "end", mutable),
                button::quiet("End tracking")
                    .name(format!("End tracking {} here", target_label(&entry.target)))
                    .on_press_maybe(mutable.then_some(Action::End(owner.clone()))),
            ));
        } else if entry.bucket() == Bucket::NeedsConfirmation {
            controls.push(ui_controls::owned(
                (owner.clone(), "suppress", mutable),
                button::quiet("Not this channel")
                    .name(format!(
                        "Mark {} as unrelated to this channel",
                        target_label(&entry.target)
                    ))
                    .on_press_maybe(mutable.then_some(Action::Suppress(owner.clone()))),
            ));
        }
        controls.push(ui_controls::owned(
            (
                view.owner,
                entry.id,
                entry.target.clone(),
                "details",
                view.details != Some(entry.id),
            ),
            button::quiet(if view.details == Some(entry.id) {
                "Hide context"
            } else {
                "Why here?"
            })
            .on_press(Action::Details(owner, view.details != Some(entry.id))),
        ));
        rows.push(flow(controls, narrow));
        if terminal {
            rows.push(
                design::caption(
                    "This PR ended. Further local work belongs to a fresh context or follow-up PR.",
                )
                .into(),
            );
        }
        if view.details == Some(entry.id) {
            if entry.claims.is_empty() {
                rows.push(
                    design::body("Observed context only; no contribution has been declared.")
                        .into(),
                );
            }
            for claim in entry.claims.iter().take(crate::channel_work::MAX_CLAIMS) {
                let source = match &claim.source {
                    Source::User => "User decision".into(),
                    Source::Session(actor) => {
                        let agent = actor
                            .agent
                            .map(|agent| format!(" · agent {}", short_id(&agent.to_string())))
                            .unwrap_or_default();
                        let run = actor
                            .run
                            .map(|run| format!(" · run {}", short_id(&run.to_string())))
                            .unwrap_or_default();
                        let lineage = if actor.lineage.0 != actor.session {
                            format!(" · lineage {}", short_id(&actor.lineage.0.to_string()))
                        } else {
                            String::new()
                        };
                        format!(
                            "Session {}{agent}{run}{lineage}",
                            short_id(&actor.session.to_string())
                        )
                    }
                };
                rows.push(
                    design::caption(format!(
                        "{source} · {}{}",
                        claim.role.label(),
                        if claim.active { "" } else { " · ended" }
                    ))
                    .into(),
                );
            }
            if !entry.contexts.is_empty() {
                rows.push(design::body("Observed checkouts").into());
                rows.push(design::caption("Checkout metadata describes an observed location; it does not identify who changed its files.").into());
                for context in entry
                    .contexts
                    .iter()
                    .take(crate::channel_work::MAX_CONTEXTS)
                {
                    rows.push(
                        design::caption(format!(
                            "{} · {}{}",
                            context.branch.as_deref().unwrap_or("Detached checkout"),
                            context.path.display(),
                            match context.dirty {
                                Some(true) => " · local changes observed",
                                Some(false) => " · no local changes observed",
                                None => " · changes not inspected",
                            }
                        ))
                        .into(),
                    );
                }
            }
        }
        widget::container(widget::column::with_children(rows).spacing(space::S))
            .padding(space::M)
            .width(Length::Fill)
            .class(design::surface(None, [design::RADIUS_S; 4], true))
            .into()
    }
}

// Effective work roles without a fresh work claim are not review/reference.
// A fresh review can keep the relation visible after another actor's old work ended.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OngoingGroup {
    Working,
    ConfirmPrevious,
    Related,
}

fn ongoing_group(entry: &Entry) -> OngoingGroup {
    if entry.is_ongoing_work() {
        OngoingGroup::Working
    } else if entry.role().is_some_and(Role::establishes_work_intent) {
        OngoingGroup::ConfirmPrevious
    } else {
        OngoingGroup::Related
    }
}

fn operation_label(mutation: &Mutation) -> String {
    match &mutation.change {
        Change::Claim { target, role } => format!("{} · {}", role.label(), target_label(target)),
        Change::End { target } => format!("End tracking · {}", target_label(target)),
        Change::Suppress { target } => format!("Not this channel · {}", target_label(target)),
    }
}
fn target_label(target: &Target) -> String {
    match target {
        Target::PullRequest(key) => format!(
            "{}/{} · #{} · {}",
            key.repository.owner,
            key.repository.name,
            key.number,
            key.repository.authority()
        ),
        Target::Local(id) => format!("Local work · {} · no PR linked", short_id(&id.to_string())),
    }
}
fn entry_status(entry: &Entry) -> String {
    let Some(observation) = &entry.observation else {
        return match entry.target {
            Target::Local(_) => "Local context; no PR identity".into(),
            Target::PullRequest(_) => "PR status not yet observed".into(),
        };
    };
    let state = match observation.state {
        PrState::Unknown => "PR state unknown",
        PrState::Open { draft: true } => "Draft PR",
        PrState::Open { draft: false } => "Open PR",
        PrState::Closed => "Closed PR",
        PrState::Merged => "Merged PR",
    };
    match observation.freshness {
        Freshness::Current => state.into(),
        Freshness::Stale => format!("Last known: {state}"),
        Freshness::Unavailable => format!("Unavailable now · last known: {state}"),
    }
}
fn observed_time(ms: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    crate::account::format_local_moment(
        (ms / 1000).min(i64::MAX as u64) as i64,
        now.min(i64::MAX as u64) as i64,
    )
}
fn short_id(value: &str) -> &str {
    value.get(..8).unwrap_or(value)
}
fn flow<'a>(items: Vec<Element<'a, Action>>, narrow: bool) -> Element<'a, Action> {
    if narrow {
        widget::column::with_children(items)
            .spacing(space::S)
            .into()
    } else {
        widget::row::with_children(items)
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into()
    }
}
fn panel<'a>(rows: Vec<Element<'a, Action>>) -> Element<'a, Action> {
    widget::container(widget::column::with_children(rows).spacing(space::S))
        .padding(space::M)
        .class(design::surface(None, [design::RADIUS_S; 4], true))
        .width(Length::Fill)
        .into()
}
fn message<'a>(text: &'a str, warning: bool) -> Element<'a, Action> {
    widget::container(design::body(text))
        .padding(space::S)
        .width(Length::Fill)
        .class(design::surface(None, [design::RADIUS_S; 4], warning))
        .into()
}
fn mount<'a>(
    header: impl Into<Element<'a, Action>>,
    status: Vec<Element<'a, Action>>,
    rows: Vec<Element<'a, Action>>,
    _height: f32,
) -> Element<'a, Action> {
    let region_spacing = if status.is_empty() || rows.is_empty() {
        0.0
    } else {
        space::M
    };
    // Keep content ancestry stable when transient status rows come and go.
    let body = widget::column::with_children(vec![
        widget::column::with_children(status)
            .spacing(space::M)
            .into(),
        widget::column::with_children(rows).spacing(space::M).into(),
    ])
    .spacing(region_spacing)
    .padding(space::M);
    widget::column::with_children(vec![
        widget::container(header)
            .padding(space::M)
            .width(Length::Fill)
            .into(),
        design::horizontal_rule(),
        widget::scrollable(body)
            .height(Length::Fill)
            .width(Length::Fill)
            .into(),
    ])
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests;

fn attach_id(owner: ViewOwner) -> widget::Id {
    widget::Id::new(format!("channel-work-attach-{}", owner.generation))
}
fn role_id(owner: FormOwner, role: Role) -> widget::Id {
    widget::Id::new(format!("channel-work-role-{}-{role:?}", owner.draft))
}
fn form_input_id(owner: FormOwner, field: Field) -> widget::Id {
    widget::Id::new(format!(
        "channel-work-input-{}-{}",
        owner.draft,
        match field {
            Field::Host => "host",
            Field::Repository => "repo",
            Field::Number => "number",
        }
    ))
}

fn confirmation_id(id: Uuid) -> widget::Id {
    widget::Id::new(format!("channel-work-keep-{id}"))
}

fn local_path_id(owner: FormOwner) -> widget::Id {
    widget::Id::new(format!("channel-work-local-path-{}", owner.draft))
}
