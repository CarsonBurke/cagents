//! Durable work intent, separate from checkout observations and execution authority.
//!
//! Requests never choose their author. The daemon resolves `Origin` from its
//! existing authenticated session/view proof before using these pure policies.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    agent_execution::Lineage,
    github_identity::{GitHubRepoKey, PullRequestKey},
    model::{AgentId, ChannelId, RunId, TerminalId},
};

pub const MAX_PAGE: u16 = 100;
pub const MAX_CLAIMS: usize = 128;
pub const MAX_CONTEXTS: usize = 128;
pub const MAX_CONTEXT_PATH_BYTES: usize = 4096;
/// Entry payload budget per independently paged current/history collection.
pub const MAX_BUCKET_BYTES: usize = 2 * 1024 * 1024 - 1024;
pub const MAX_SNAPSHOT_BYTES: usize = 4 * 1024 * 1024;

/// Measure the wire representation without allocating an encoded buffer.
pub fn encoded_size<T: Serialize + ?Sized>(value: &T) -> Result<usize, Error> {
    postcard::experimental::serialized_size(value).map_err(|_| Error::Capacity)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Home {
    pub registry: Uuid,
    pub channel: ChannelId,
    pub creation_ordinal: u32,
}

impl Home {
    pub fn validate(self) -> Result<(), Error> {
        if self.registry.is_nil()
            || self.channel.as_uuid().is_nil()
            || self.creation_ordinal == crate::workspace_model::PENDING_ORDINAL
        {
            return Err(Error::InvalidHome);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HomeLifecycle {
    Active,
    Archived,
    Deleted,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Target {
    PullRequest(PullRequestKey),
    /// A server-known local context, not a pathname or a guessed branch owner.
    Local(Uuid),
}

impl Target {
    pub fn canonicalized(&self) -> Result<Self, Error> {
        match self {
            Self::PullRequest(key) => Ok(Self::PullRequest(
                PullRequestKey::new(canonical_repository(&key.repository)?, key.number)
                    .map_err(|_| Error::InvalidTarget)?,
            )),
            Self::Local(id) if !id.is_nil() => Ok(Self::Local(*id)),
            Self::Local(_) => Err(Error::InvalidTarget),
        }
    }

    pub fn pull_request_url(&self) -> Result<Option<url::Url>, Error> {
        let Self::PullRequest(key) = self.canonicalized()? else {
            return Ok(None);
        };
        let mut url = url::Url::parse(&format!("https://{}/", key.repository.authority()))
            .map_err(|_| Error::InvalidTarget)?;
        url.path_segments_mut()
            .map_err(|_| Error::InvalidTarget)?
            .extend([
                key.repository.owner.as_str(),
                key.repository.name.as_str(),
                "pull",
                &key.number.to_string(),
            ]);
        Ok(Some(url))
    }
}

fn canonical_repository(repository: &GitHubRepoKey) -> Result<GitHubRepoKey, Error> {
    if repository.host.len() > 253 || repository.owner.len() > 256 || repository.name.len() > 256 {
        return Err(Error::InvalidTarget);
    }
    GitHubRepoKey::new(
        repository.host.to_ascii_lowercase(),
        repository.port,
        repository.owner.to_ascii_lowercase(),
        repository.name.to_ascii_lowercase(),
    )
    .map_err(|_| Error::InvalidTarget)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    Contribute,
    Steward,
    Review,
    Reference,
}

impl Role {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Contribute => "Contributing",
            Self::Steward => "Stewarding",
            Self::Review => "Review only",
            Self::Reference => "Reference",
        }
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Contribute => "Declared implementation work; not a verified filesystem write",
            Self::Steward => "Declared responsibility for progressing this work",
            Self::Review => "Reviewing this work without claiming implementation",
            Self::Reference => "Related context without claiming ownership",
        }
    }

    pub const fn establishes_work_intent(self) -> bool {
        matches!(self, Self::Contribute | Self::Steward)
    }
}

/// Descriptive provenance. Deserializing this does not authenticate a caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contributor {
    pub session: TerminalId,
    pub lineage: Lineage,
    pub agent: Option<AgentId>,
    pub run: Option<RunId>,
}

impl Contributor {
    fn validate(&self) -> Result<(), Error> {
        if self.session.as_uuid().is_nil()
            || self.lineage.0.as_uuid().is_nil()
            || self.agent.is_some_and(|id| id.as_uuid().is_nil())
            || self.run.is_some_and(|id| id.as_uuid().is_nil())
            || (self.run.is_some() && self.agent.is_none())
        {
            return Err(Error::InvalidMetadata);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Source {
    User,
    Session(Contributor),
}

impl Source {
    fn same_actor(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::User, Self::User) => true,
            (Self::Session(a), Self::Session(b)) => a.lineage == b.lineage,
            _ => false,
        }
    }
}

/// Constructed only after strict daemon authentication and exact view checks.
/// It deliberately has no Deserialize implementation or wire request variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Origin {
    User,
    Actor {
        home: Home,
        actor: Contributor,
        contexts: Vec<CheckoutContext>,
    },
}

impl Origin {
    pub(crate) fn source(&self) -> Source {
        match self {
            Self::User => Source::User,
            Self::Actor { actor, .. } => Source::Session(actor.clone()),
        }
    }

    fn contexts(&self, home: Home) -> Result<Vec<CheckoutContext>, Error> {
        let Self::Actor {
            home: actual,
            actor,
            contexts,
        } = self
        else {
            return Ok(Vec::new());
        };
        if *actual != home {
            return Err(Error::WrongHome);
        }
        actor.validate()?;
        if contexts.len() > MAX_CONTEXTS {
            return Err(Error::Capacity);
        }
        let mut result = contexts.clone();
        result.sort_by_key(|context| context.id);
        for context in &result {
            context.validate()?;
            if context.registry != home.registry {
                return Err(Error::WrongHome);
            }
        }
        if result.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err(Error::InvalidMetadata);
        }
        Ok(result)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutContext {
    pub id: Uuid,
    pub registry: Uuid,
    pub repository: PathBuf,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: Option<String>,
    pub generation: u64,
    pub dirty: bool,
}

impl CheckoutContext {
    pub fn validate(&self) -> Result<(), Error> {
        if self.id.is_nil()
            || self.registry.is_nil()
            || !context_path(&self.repository)
            || !context_path(&self.path)
            || self.branch.as_ref().is_some_and(|v| !text(v, 1024))
            || self.head.as_ref().is_some_and(|v| !oid(v))
        {
            return Err(Error::InvalidMetadata);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Change {
    Claim { target: Target, role: Role },
    End { target: Target },
    Suppress { target: Target },
}

impl Change {
    pub fn target(&self) -> &Target {
        match self {
            Self::Claim { target, .. } | Self::End { target } | Self::Suppress { target } => target,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mutation {
    pub home: Home,
    /// The target entry's revision, or zero for an absent target. This is not
    /// the snapshot/ledger revision: another PR must not invalidate this row.
    pub expected_revision: u64,
    pub operation: Uuid,
    pub change: Change,
}

impl Mutation {
    pub fn canonicalized(&self) -> Result<Self, Error> {
        self.home.validate()?;
        if self.operation.is_nil() {
            return Err(Error::InvalidOperation);
        }
        let target = self.change.target().canonicalized()?;
        let change = match self.change {
            Change::Claim { role, .. } => Change::Claim { target, role },
            Change::End { .. } => Change::End { target },
            Change::Suppress { .. } => Change::Suppress { target },
        };
        Ok(Self {
            change,
            ..self.clone()
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Bucket {
    Ongoing,
    NeedsConfirmation,
    History,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub source: Source,
    pub role: Role,
    pub active: bool,
    /// Last fresh nonterminal observation acknowledged by semantic intent.
    pub confirmed_generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrState {
    Unknown,
    Open { draft: bool },
    Closed,
    Merged,
}

impl PrState {
    pub const fn terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::Merged)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Freshness {
    Current,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Head {
    pub repository: GitHubRepoKey,
    pub branch: String,
    pub oid: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub generation: u64,
    pub state: PrState,
    pub freshness: Freshness,
    pub title: Option<String>,
    pub node_id: Option<String>,
    pub head: Option<Head>,
    pub observed_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: Uuid,
    pub target: Target,
    pub claims: Vec<Claim>,
    pub contexts: Vec<CheckoutContext>,
    pub suppressed: bool,
    pub user_ended: bool,
    pub observation: Option<Observation>,
    pub last_terminal_generation: u64,
    pub ever_claimed: bool,
    pub revision: u64,
}

impl Entry {
    pub fn validate(&self) -> Result<(), Error> {
        if self.id.is_nil() || self.target.canonicalized()? != self.target {
            return Err(Error::InvalidMetadata);
        }
        if self.claims.len() > MAX_CLAIMS || self.contexts.len() > MAX_CONTEXTS {
            return Err(Error::Capacity);
        }
        if let Some(observation) = &self.observation {
            if !matches!(self.target, Target::PullRequest(_))
                || observation.generation == 0
                || (observation.state == PrState::Unknown
                    && observation.freshness == Freshness::Current)
                || observation.title.as_ref().is_some_and(|v| !text(v, 512))
                || observation.node_id.as_ref().is_some_and(|v| !text(v, 256))
            {
                return Err(Error::InvalidMetadata);
            }
            if let Some(head) = &observation.head {
                if canonical_repository(&head.repository)? != head.repository
                    || !text(&head.branch, 1024)
                    || !oid(&head.oid)
                {
                    return Err(Error::InvalidMetadata);
                }
            }
        }
        let mut sources = std::collections::HashSet::new();
        for claim in &self.claims {
            let key = match &claim.source {
                Source::User => None,
                Source::Session(actor) => {
                    actor.validate()?;
                    Some(actor.lineage)
                }
            };
            if !sources.insert(key)
                || claim.confirmed_generation
                    > self.observation.as_ref().map_or(0, |o| o.generation)
            {
                return Err(Error::InvalidMetadata);
            }
        }
        let mut contexts = std::collections::HashSet::new();
        for context in &self.contexts {
            context.validate()?;
            if !contexts.insert(context.id) {
                return Err(Error::InvalidMetadata);
            }
        }
        if (!self.ever_claimed && !self.claims.is_empty())
            || self.last_terminal_generation > self.observation.as_ref().map_or(0, |o| o.generation)
        {
            return Err(Error::InvalidMetadata);
        }
        if encoded_size(self)? > MAX_BUCKET_BYTES {
            return Err(Error::Capacity);
        }
        Ok(())
    }

    pub fn bucket(&self) -> Bucket {
        if self.suppressed
            || self.user_ended
            || self
                .observation
                .as_ref()
                .is_some_and(|o| o.state.terminal())
        {
            return Bucket::History;
        }
        let mut active = self.claims.iter().filter(|claim| claim.active).peekable();
        if active.peek().is_none() {
            return if self.ever_claimed {
                Bucket::History
            } else {
                Bucket::NeedsConfirmation
            };
        }
        if self.last_terminal_generation != 0
            && !active.any(|claim| claim.confirmed_generation > self.last_terminal_generation)
        {
            Bucket::NeedsConfirmation
        } else {
            Bucket::Ongoing
        }
    }

    /// Active review/reference relations remain visible without counting as work.
    pub fn is_ongoing_work(&self) -> bool {
        if self.bucket() != Bucket::Ongoing
            || !self.role().is_some_and(Role::establishes_work_intent)
        {
            return false;
        }
        let fresh_work = |claim: &Claim| {
            claim.role.establishes_work_intent()
                && (self.last_terminal_generation == 0
                    || claim.confirmed_generation > self.last_terminal_generation)
        };
        if let Some(user) = self
            .claims
            .iter()
            .find(|c| c.active && c.source == Source::User)
        {
            return fresh_work(user);
        }
        self.claims.iter().filter(|c| c.active).any(fresh_work)
    }

    pub fn is_ongoing(&self) -> bool {
        self.is_ongoing_work()
    }

    /// A manual role overrides the display of inferred/self-declared roles.
    pub fn role(&self) -> Option<Role> {
        self.claims
            .iter()
            .find(|claim| claim.active && claim.source == Source::User)
            .map(|claim| claim.role)
            .or_else(|| {
                self.claims
                    .iter()
                    .filter(|claim| claim.active)
                    .map(|claim| claim.role)
                    .min_by_key(|role| match role {
                        Role::Contribute => 0,
                        Role::Steward => 1,
                        Role::Review => 2,
                        Role::Reference => 3,
                    })
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page {
    pub current_before: Option<u64>,
    pub before: Option<u64>,
    pub limit: u16,
}

impl Default for Page {
    fn default() -> Self {
        Self {
            current_before: None,
            before: None,
            limit: 25,
        }
    }
}

impl Page {
    pub fn validate(self) -> Result<(), Error> {
        if self.limit == 0 || self.limit > MAX_PAGE {
            return Err(Error::Capacity);
        }
        if self.before == Some(0) || self.current_before == Some(0) {
            return Err(Error::InvalidSnapshot);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub home: Home,
    pub lifecycle: HomeLifecycle,
    pub revision: u64,
    pub entries: Vec<Entry>,
    /// Latest historical relations, not a repeated snapshot per mutation.
    pub history: Vec<Entry>,
    pub next_current: Option<u64>,
    pub next_history: Option<u64>,
}

impl Snapshot {
    pub fn validate(&self) -> Result<(), Error> {
        self.home.validate()?;
        if self.entries.len() > usize::from(MAX_PAGE) || self.history.len() > usize::from(MAX_PAGE)
        {
            return Err(Error::Capacity);
        }
        for (entries, cursor) in [
            (&self.entries, self.next_current),
            (&self.history, self.next_history),
        ] {
            if cursor.is_some_and(|value| value == 0) || (cursor.is_some() && entries.is_empty()) {
                return Err(Error::InvalidSnapshot);
            }
        }
        let mut targets = std::collections::HashSet::new();
        let mut ids = std::collections::HashSet::new();
        for (entries, historical) in [(&self.entries, false), (&self.history, true)] {
            let mut bytes = 0usize;
            for entry in entries {
                bytes = bytes
                    .checked_add(encoded_size(entry)?)
                    .ok_or(Error::Capacity)?;
                if bytes > MAX_BUCKET_BYTES {
                    return Err(Error::Capacity);
                }
                entry.validate()?;
                if entry.revision > self.revision
                    || (self.lifecycle != HomeLifecycle::Active
                        && entry.bucket() != Bucket::History)
                    || entry
                        .contexts
                        .iter()
                        .any(|context| context.registry != self.home.registry)
                    || (entry.bucket() == Bucket::History) != historical
                    || !targets.insert(entry.target.clone())
                    || !ids.insert(entry.id)
                {
                    return Err(Error::InvalidSnapshot);
                }
            }
        }
        if encoded_size(self)? > MAX_SNAPSHOT_BYTES {
            return Err(Error::Capacity);
        }
        Ok(())
    }
}

/// Exact target lookup; absence is meaningful only in this validated home/target echo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetSnapshot {
    pub home: Home,
    pub lifecycle: HomeLifecycle,
    /// Home ledger ordering; the optional entry carries its own CAS revision.
    pub revision: u64,
    pub target: Target,
    pub entry: Option<Entry>,
}

impl TargetSnapshot {
    pub fn validate(&self) -> Result<(), Error> {
        self.home.validate()?;
        if self.target.canonicalized()? != self.target {
            return Err(Error::InvalidTarget);
        }
        if let Some(entry) = &self.entry {
            entry.validate()?;
            if entry.target != self.target
                || entry.revision > self.revision
                || (self.lifecycle != HomeLifecycle::Active && entry.bucket() != Bucket::History)
                || entry
                    .contexts
                    .iter()
                    .any(|context| context.registry != self.home.registry)
            {
                return Err(Error::InvalidSnapshot);
            }
        }
        // The row budget reserves room for the home, target and DTO framing.
        if encoded_size(self)? > MAX_BUCKET_BYTES + 1024 {
            return Err(Error::Capacity);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HomeState {
    pub home: Home,
    pub lifecycle: HomeLifecycle,
    /// Ledger ordering, not a row mutation precondition.
    pub revision: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub operation: Uuid,
    pub home: Home,
    /// The target entry revision at the original operation.
    pub revision: u64,
    pub sequence: u64,
    pub changed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reply {
    pub receipt: Receipt,
    pub snapshot: Snapshot,
}

/// Stored only by the daemon. No previous snapshot is replayed as current.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct StoredOperation {
    pub mutation: Mutation,
    pub source: Source,
    pub contexts: Vec<CheckoutContext>,
    pub receipt: Receipt,
}

impl StoredOperation {
    pub(crate) fn capture(
        mutation: &Mutation,
        origin: &Origin,
        receipt: Receipt,
    ) -> Result<Self, Error> {
        let mutation = mutation.canonicalized()?;
        let revision = mutation
            .expected_revision
            .checked_add(u64::from(receipt.changed))
            .ok_or(Error::RevisionExhausted)?;
        if receipt.home != mutation.home
            || receipt.operation != mutation.operation
            || receipt.revision != revision
        {
            return Err(Error::OperationConflict);
        }
        let contexts = origin.contexts(mutation.home)?;
        Ok(Self {
            mutation,
            source: origin.source(),
            contexts,
            receipt,
        })
    }
}

pub(crate) fn replay_response(
    stored: &StoredOperation,
    request: &Mutation,
    origin: &Origin,
    current: Snapshot,
) -> Result<Reply, Error> {
    let request = request.canonicalized()?;
    current.validate()?;
    // Contexts are retained original provenance, not a retry fingerprint:
    // ordinary dirty/head observations may change while the reply is lost.
    // The daemon still revalidates the caller's current source proof.
    origin.contexts(request.home)?;
    if stored.mutation != request
        || stored.source != origin.source()
        || current.home != request.home
        || stored.receipt.home != request.home
        || stored.receipt.operation != request.operation
        || stored.receipt.revision > current.revision
    {
        return Err(Error::OperationConflict);
    }
    Ok(Reply {
        receipt: stored.receipt.clone(),
        snapshot: current,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EntryTransition {
    pub entry: Entry,
    pub changed: bool,
}

pub(crate) fn reduce_entry(
    current: &HomeState,
    previous: Option<&Entry>,
    request: &Mutation,
    origin: &Origin,
) -> Result<EntryTransition, Error> {
    let request = request.canonicalized()?;
    if request.home != current.home {
        return Err(Error::WrongHome);
    }
    if current.lifecycle != HomeLifecycle::Active {
        return Err(Error::InactiveHome);
    }
    current.home.validate()?;
    let entry_revision = previous.map_or(0, |entry| entry.revision);
    if request.expected_revision != entry_revision {
        return Err(Error::StaleRevision);
    }
    if let Some(entry) = previous {
        entry.validate()?;
        if entry.revision > current.revision {
            return Err(Error::InvalidSnapshot);
        }
    }
    let contexts = origin.contexts(request.home)?;
    let source = origin.source();
    let target = request.change.target();
    if previous.is_none()
        && let Target::Local(id) = target
        && !contexts.iter().any(|context| context.id == *id)
    {
        return Err(Error::InvalidTarget);
    }
    if previous.is_some_and(|entry| &entry.target != target) {
        return Err(Error::InvalidTarget);
    }
    let mut entry = previous.cloned().unwrap_or_else(|| Entry {
        id: request.operation,
        target: target.clone(),
        claims: Vec::new(),
        contexts: Vec::new(),
        suppressed: false,
        user_ended: false,
        observation: None,
        last_terminal_generation: 0,
        ever_claimed: false,
        revision: 0,
    });
    match request.change {
        Change::Claim { role, .. } => {
            if source != Source::User && (entry.suppressed || entry.user_ended) {
                return Err(Error::UserDecision);
            }
            if source == Source::User {
                entry.suppressed = false;
                entry.user_ended = false;
            }
            let confirmed_generation = entry.observation.as_ref().map_or(0, |observation| {
                if observation.freshness == Freshness::Current && !observation.state.terminal() {
                    observation.generation
                } else {
                    0
                }
            });
            let claim = Claim {
                source: source.clone(),
                role,
                active: true,
                confirmed_generation,
            };
            if let Some(old) = entry
                .claims
                .iter_mut()
                .find(|old| old.source.same_actor(&source))
            {
                *old = claim;
            } else {
                if entry.claims.len() >= MAX_CLAIMS {
                    return Err(Error::Capacity);
                }
                entry.claims.push(claim);
            }
            for context in contexts {
                if let Some(old) = entry.contexts.iter_mut().find(|old| old.id == context.id) {
                    // A branch/head switch is a different context, not history retargeting.
                    if old.registry != context.registry
                        || old.repository != context.repository
                        || old.path != context.path
                        || old.branch != context.branch
                    {
                        return Err(Error::ContextChanged);
                    }
                    if context.generation >= old.generation {
                        *old = context;
                    }
                } else {
                    if entry.contexts.len() >= MAX_CONTEXTS {
                        return Err(Error::Capacity);
                    }
                    entry.contexts.push(context);
                }
            }
            entry.ever_claimed = true;
        }
        Change::End { .. } => {
            if previous.is_none() {
                return Err(Error::UnknownEntry);
            }
            for claim in &mut entry.claims {
                if source == Source::User || claim.source.same_actor(&source) {
                    claim.active = false;
                }
            }
            if source == Source::User {
                entry.user_ended = true;
            }
        }
        Change::Suppress { .. } => {
            if source != Source::User {
                return Err(Error::UserDecision);
            }
            entry.suppressed = true;
            for claim in &mut entry.claims {
                claim.active = false;
            }
        }
    }
    let changed = previous != Some(&entry);
    if changed {
        entry.revision = entry_revision
            .checked_add(1)
            .ok_or(Error::RevisionExhausted)?;
    }
    entry.validate()?;
    Ok(EntryTransition { entry, changed })
}

/// Internal forge input, never a client change or proof of work ownership.
pub(crate) enum PollOutcome {
    Success(Observation),
    Failure,
}

pub(crate) fn observe_entry(
    current: &HomeState,
    entry: &Entry,
    generation: u64,
    outcome: PollOutcome,
) -> Result<EntryTransition, Error> {
    current.home.validate()?;
    if current.lifecycle != HomeLifecycle::Active {
        return Err(Error::InactiveHome);
    }
    entry.validate()?;
    if entry.revision > current.revision {
        return Err(Error::InvalidSnapshot);
    }
    if !matches!(entry.target, Target::PullRequest(_)) {
        return Err(Error::InvalidTarget);
    }
    if generation == 0 {
        return Err(Error::InvalidMetadata);
    }
    if entry
        .observation
        .as_ref()
        .is_some_and(|old| old.generation >= generation)
    {
        return Ok(EntryTransition {
            entry: entry.clone(),
            changed: false,
        });
    }
    let mut updated = entry.clone();
    match outcome {
        PollOutcome::Success(mut observation) => {
            if observation.generation != generation
                || observation.state == PrState::Unknown
                || observation.freshness != Freshness::Current
                || observation.title.as_ref().is_some_and(|v| !text(v, 512))
                || observation.node_id.as_ref().is_some_and(|v| !text(v, 256))
            {
                return Err(Error::InvalidMetadata);
            }
            if let Some(head) = &mut observation.head {
                head.repository = canonical_repository(&head.repository)?;
                if !text(&head.branch, 1024) || !oid(&head.oid) {
                    return Err(Error::InvalidMetadata);
                }
            }
            if entry
                .observation
                .as_ref()
                .and_then(|o| o.node_id.as_ref())
                .zip(observation.node_id.as_ref())
                .is_some_and(|(a, b)| a != b)
            {
                return Err(Error::ContextChanged);
            }
            if observation.node_id.is_none() {
                observation.node_id = entry.observation.as_ref().and_then(|o| o.node_id.clone());
            }
            if observation.state.terminal() {
                updated.last_terminal_generation = generation;
            }
            updated.observation = Some(observation);
        }
        PollOutcome::Failure => {
            let mut observation = entry.observation.clone().unwrap_or(Observation {
                generation,
                state: PrState::Unknown,
                freshness: Freshness::Unavailable,
                title: None,
                node_id: None,
                head: None,
                observed_at_ms: 0,
            });
            observation.generation = generation;
            observation.freshness = Freshness::Unavailable;
            updated.observation = Some(observation);
        }
    }
    updated.revision = entry
        .revision
        .checked_add(1)
        .ok_or(Error::RevisionExhausted)?;
    updated.validate()?;
    Ok(EntryTransition {
        entry: updated,
        changed: true,
    })
}

fn text(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.chars().any(char::is_control)
}

fn context_path(path: &std::path::Path) -> bool {
    path.is_absolute()
        && path.as_os_str().as_encoded_bytes().len() <= MAX_CONTEXT_PATH_BYTES
        && !path.as_os_str().as_encoded_bytes().contains(&0)
        && !path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
}

fn oid(value: &str) -> bool {
    (7..=64).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Error {
    #[error("The channel home is not a confirmed registry incarnation")]
    InvalidHome,
    #[error("The work target is invalid")]
    InvalidTarget,
    #[error("The work metadata is invalid")]
    InvalidMetadata,
    #[error("The operation identity is invalid")]
    InvalidOperation,
    #[error("The channel home changed")]
    WrongHome,
    #[error("Refresh the channel work before changing it")]
    StaleRevision,
    #[error("The channel home is no longer active")]
    InactiveHome,
    #[error("An explicit user decision prevents this change")]
    UserDecision,
    #[error("This work relation does not exist")]
    UnknownEntry,
    #[error("The operation already identifies different work")]
    OperationConflict,
    #[error("The work page or relation exceeds its supported bound")]
    Capacity,
    #[error("The work revision cannot advance")]
    RevisionExhausted,
    #[error("The checkout or pull request identity changed")]
    ContextChanged,
    #[error("The channel work snapshot is inconsistent")]
    InvalidSnapshot,
}

#[cfg(test)]
mod tests;
