//! Explicit staged-file import. Accepted requests outlive a dismissed editor;
//! only the original operation can be observed or verified afterwards.
use super::*;
use crate::account::import::{
    CodingProfile, CopyConsent, ImportInspection, ImportOperation, ImportPlan, ImportPublication,
    ImportReply, ImportStage, ImportStatus,
};
use crate::model::HostSessionProof;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Profile(Option<(String, String, String)>);
impl Profile {
    fn current() -> Self {
        Self(crate::device::selected().map(|device| {
            (
                device.name.clone(),
                device.host.clone(),
                device.executable.clone(),
            )
        }))
    }
    fn label(&self) -> &str {
        self.0.as_ref().map_or("This device", |(name, _, _)| name)
    }
}
#[derive(Clone, Debug)]
pub struct Target {
    socket: PathBuf,
    proof: HostSessionProof,
    profile: Profile,
}
#[derive(Debug, Clone, Copy)]
pub enum Failure {
    Unavailable,
    Changed,
    /// Only Prepare's local target validation, before device_operation.
    /// A remote error, lost reply, Status or Verify can never produce this.
    PrepareNotSent,
    Invalid,
    Journal,
}
impl Failure {
    fn description(self) -> &'static str {
        match self {
            Self::Unavailable => {
                "No result returned. Preserve this operation and use Status; do not copy the cache again."
            }
            Self::Changed => {
                "The selected device or installation changed. No replacement operation was sent."
            }
            Self::PrepareNotSent => {
                "Preparation was not sent because the destination changed. Inspect the staged file again before preparing a new import."
            }
            Self::Invalid => {
                "The device returned inconsistent import metadata. Preserve the original operation."
            }
            Self::Journal => {
                "The journal is unavailable, conflicts, or has 16 retained operations (including completed imports). Completion does not free a slot."
            }
        }
    }
}
#[derive(Debug, Clone)]
pub enum Action {
    Open,
    Close(u64),
    Path(u64, String),
    Email(u64, String),
    CopyConsent(u64, bool),
    SessionConsent(u64, bool),
    WorkspaceWrite(u64, bool),
    Network(u64, bool),
    Inspect(u64),
    Prepare(u64),
    Status(u64),
    Verify(u64),
    Check(u64),
    Load(u64),
    Resume {
        owner: u64,
        discovery: u64,
        operation: Uuid,
    },
    New(u64),
    Finished {
        sequence: u64,
        result: Result<Response, Failure>,
    },
}
impl Action {
    fn owner(&self) -> Option<u64> {
        match self {
            Self::Open | Self::Finished { .. } => None,
            Self::Close(n)
            | Self::Path(n, _)
            | Self::Email(n, _)
            | Self::CopyConsent(n, _)
            | Self::SessionConsent(n, _)
            | Self::WorkspaceWrite(n, _)
            | Self::Network(n, _)
            | Self::Inspect(n)
            | Self::Prepare(n)
            | Self::Status(n)
            | Self::Verify(n)
            | Self::Check(n)
            | Self::Load(n)
            | Self::New(n) => Some(*n),
            Self::Resume { owner, .. } => Some(*owner),
        }
    }
    pub(super) fn blocked_while_modal(&self) -> bool {
        !matches!(self, Self::Close(_) | Self::Finished { .. })
    }
}
#[derive(Debug, Clone)]
pub struct Response {
    target: Target,
    reply: ImportReply,
}
#[derive(Debug, Clone)]
enum RequestKind {
    List,
    Inspect(PathBuf),
    Prepare(ImportPlan, Target),
    Status(ImportPlan),
    Verify(ImportPlan),
}
#[derive(Debug, Clone)]
struct Request {
    sequence: u64,
    profile: Profile,
    kind: RequestKind,
}
#[derive(Default)]
pub(super) struct State {
    next: u64,
    owner: u64,
    visible: bool,
    profile: Option<Profile>,
    path: String,
    email: String,
    copy: bool,
    session: bool,
    write: bool,
    network: bool,
    inspection: Option<(Target, ImportInspection)>,
    plan: Option<ImportPlan>,
    status: Option<ImportStatus>,
    retained: Vec<ImportStatus>,
    discovery: u64,
    // Unanswered/unacknowledged local dispatch, distinct from the journal's
    // historical uncertainty bit which survives a terminal registration ACK.
    local_uncertain: bool,
    pending: Option<Request>,
    problem: Option<&'static str>,
}
impl State {
    pub(super) fn is_open(&self) -> bool {
        self.visible
    }
    pub(super) fn close(&mut self) {
        self.visible = false;
    }
    fn editable(&self) -> bool {
        self.plan.is_none() && self.pending.is_none()
    }
    fn can_new(&self) -> bool {
        self.pending.is_none()
            && !self.local_uncertain
            && self
                .status
                .as_ref()
                .is_some_and(|status| status.stage == ImportStage::Registered)
    }
    fn can_resume(&self, status: &ImportStatus) -> bool {
        self.pending.is_none()
            && (self.plan.is_none()
                || self.can_new()
                || self.plan.as_ref().is_some_and(|plan| {
                    plan == &status.plan
                        && self
                            .status
                            .as_ref()
                            .and_then(|old| old.receipt.as_ref())
                            .is_none_or(|old| status.receipt.as_ref() == Some(old))
                }))
    }
    fn rotate_owner(&mut self) {
        self.next = self.next.saturating_add(1);
        self.owner = self.next;
    }
    fn registered_account(&self, accounts: &super::State) -> Option<AccountId> {
        let status = self.status.as_ref()?;
        let id = status.registered.as_ref()?;
        let plan = self.plan.as_ref()?;
        let row = accounts.account(id)?;
        let registered = accounts.registry.find(plan.operation)?;
        (row.account.id() == *id
            && row.account.registry_key() == Some(plan.operation)
            && !row.account.is_linked()
            && registered.provider == Provider::Codex
            && registered.linked_home.is_none()
            && !accounts.registry.is_archived(plan.operation)
            && registered
                .email
                .as_deref()
                .is_some_and(|email| email.eq_ignore_ascii_case(&plan.expected_email)))
        .then(|| id.clone())
    }
    fn can_prepare(&self) -> bool {
        self.editable()
            && self.copy
            && self.session
            && crate::account::import::expected_email_is_valid(self.email.trim())
            && self.inspection.is_some()
    }
    // Only an explicit, validated Verify completion may request a registry
    // reload. Metadata Status/List and stale replies never schedule probing.
    fn accept(&mut self, sequence: u64, result: Result<Response, Failure>) -> bool {
        let Some(request) = self
            .pending
            .as_ref()
            .filter(|request| request.sequence == sequence)
        else {
            return false;
        };
        let kind = request.kind.clone();
        let profile = request.profile.clone();
        self.pending = None;
        match result {
            Ok(response) if response.target.profile == profile => match (&kind, response.reply) {
                (RequestKind::List, ImportReply::Listed(statuses))
                    if valid_list(&statuses, &response.target.proof) =>
                {
                    self.retained = statuses;
                    self.discovery = sequence;
                    self.problem = if self.plan.as_ref().is_some_and(|plan| {
                        !self.retained.iter().any(|status| &status.plan == plan)
                    }) {
                        Some(
                            "The list did not confirm your local operation. Preserve its UUID and use Status; do not prepare it again.",
                        )
                    } else {
                        None
                    };
                }
                (RequestKind::Inspect(path), ImportReply::Inspected(inspection))
                    if inspection.destination == response.target.proof
                        && inspection.observation.validate().is_ok()
                        && (!path.is_absolute()
                            || inspection.observation.path() == path.as_path()) =>
                {
                    self.inspection = Some((response.target, inspection));
                    self.problem = None;
                }
                (
                    RequestKind::Prepare(plan, _)
                    | RequestKind::Status(plan)
                    | RequestKind::Verify(plan),
                    ImportReply::Status(status),
                ) if valid_status(&status, plan, &response.target.proof, self.status.as_ref()) => {
                    let reload =
                        matches!(kind, RequestKind::Verify(_)) && status.registered.is_some();
                    // A validated terminal ACK resolves local dispatch custody;
                    // the retained journal still records any earlier uncertainty.
                    self.local_uncertain =
                        status.uncertain && status.stage != ImportStage::Registered;
                    self.status = Some(status);
                    self.problem = None;
                    return reload;
                }
                _ => self.problem = Some(Failure::Invalid.description()),
            },
            Ok(_) => self.problem = Some(Failure::Changed.description()),
            Err(Failure::Unavailable) if matches!(kind, RequestKind::List) => {
                self.problem = Some(
                    "Retained imports could not be loaded. Existing local operation custody is unchanged.",
                );
            }
            Err(Failure::Unavailable) if self.plan.is_none() => {
                self.problem = Some(
                    "Inspection did not return a result. Inspect again explicitly; no import was prepared.",
                );
            }
            Err(Failure::PrepareNotSent)
                if matches!(&kind, RequestKind::Prepare(plan, _) if self.plan.as_ref() == Some(plan))
                    && self.status.is_none() =>
            {
                // This fresh operation never crossed the dispatch boundary.
                // Keep user input/consent, but require a new inspection and
                // invalidate held controls before another explicit Prepare.
                self.plan = None;
                self.inspection = None;
                self.local_uncertain = false;
                self.rotate_owner();
                self.problem = Some(Failure::PrepareNotSent.description());
            }
            Err(Failure::PrepareNotSent) => {
                self.problem = Some(Failure::Invalid.description());
            }
            Err(failure) => self.problem = Some(failure.description()),
        }
        false
    }
    fn update(&mut self, action: Action, profile: Profile) -> Option<Request> {
        if let Action::Finished { sequence, result } = action {
            self.accept(sequence, result);
            return None;
        }
        if matches!(action, Action::Open) {
            self.rotate_owner();
            self.visible = true;
            self.profile.get_or_insert(profile);
            return None;
        }
        if !self.visible || action.owner() != Some(self.owner) {
            return None;
        }
        if matches!(action, Action::Close(_)) {
            self.close();
            return None;
        }
        if self.profile.as_ref() != Some(&profile) {
            self.problem = Some(Failure::Changed.description());
            return None;
        }
        if self.pending.is_some() {
            return None;
        }
        if matches!(action, Action::New(_)) {
            if self.can_new() {
                self.path.clear();
                self.email.clear();
                self.copy = false;
                self.session = false;
                self.write = false;
                self.network = false;
                self.inspection = None;
                self.plan = None;
                self.status = None;
                self.problem = None;
                self.local_uncertain = false;
                self.rotate_owner();
            }
            return None;
        }
        if let Action::Resume {
            discovery,
            operation,
            ..
        } = action
        {
            if discovery == self.discovery
                && let Some(status) = self
                    .retained
                    .iter()
                    .find(|status| status.plan.operation == operation)
                    .cloned()
                && self.can_resume(&status)
            {
                self.path = status
                    .plan
                    .observation
                    .path()
                    .to_string_lossy()
                    .into_owned();
                self.email = status.plan.expected_email.clone();
                self.copy = status.plan.consent.staged_file_only;
                self.session = status.plan.consent.source_session_limit_acknowledged;
                (self.write, self.network) = match status.plan.profile {
                    CodingProfile::Unconfigured => (false, false),
                    CodingProfile::WorkspaceWrite { network_access } => (true, network_access),
                };
                self.inspection = None;
                self.plan = Some(status.plan.clone());
                // Discovery can be older than an unanswered local dispatch.
                // Resuming its cached record must not acknowledge that request.
                self.local_uncertain |= status.uncertain && status.stage != ImportStage::Registered;
                self.status = Some(status);
                self.problem = None;
                self.rotate_owner();
            }
            return None;
        }
        if self.editable() {
            self.problem = None;
            match &action {
                Action::Path(_, value) => {
                    self.path = value.clone();
                    self.inspection = None;
                }
                Action::Email(_, value) => self.email = value.clone(),
                Action::CopyConsent(_, value) => self.copy = *value,
                Action::SessionConsent(_, value) => self.session = *value,
                Action::WorkspaceWrite(_, value) => {
                    self.write = *value;
                    if !value {
                        self.network = false;
                    }
                }
                Action::Network(_, value) if self.write => self.network = *value,
                _ => {}
            }
        }
        let kind = match action {
            Action::Load(_) => RequestKind::List,
            Action::Inspect(_)
                if self.editable()
                    && !self.path.trim().is_empty()
                    && self.path.len() <= 4096
                    && !self.path.chars().any(char::is_control) =>
            {
                self.inspection = None;
                RequestKind::Inspect(PathBuf::from(self.path.trim()))
            }
            Action::Prepare(_) if self.can_prepare() => {
                let (target, inspection) = self.inspection.as_ref().unwrap();
                let plan = ImportPlan {
                    operation: Uuid::new_v4(),
                    destination: inspection.destination.clone(),
                    observation: inspection.observation.clone(),
                    expected_email: self.email.trim().into(),
                    consent: CopyConsent {
                        staged_file_only: self.copy,
                        source_session_limit_acknowledged: self.session,
                    },
                    profile: if self.write {
                        CodingProfile::WorkspaceWrite {
                            network_access: self.network,
                        }
                    } else {
                        CodingProfile::Unconfigured
                    },
                };
                if plan.validate().is_err() {
                    return None;
                }
                let kind = RequestKind::Prepare(plan.clone(), target.clone());
                // Retain before dispatch, even if the reply is lost or Close is next.
                self.plan = Some(plan);
                self.local_uncertain = true;
                kind
            }
            Action::Status(_) if self.plan.is_some() => {
                RequestKind::Status(self.plan.clone().unwrap())
            }
            Action::Verify(_)
                if self.plan.is_some()
                    && self.status.as_ref().is_some_and(|status| {
                        matches!(
                            status.stage,
                            ImportStage::Published | ImportStage::Registered
                        ) || (status.stage == ImportStage::Prepared
                            && status.publication == ImportPublication::Present)
                    }) =>
            {
                self.local_uncertain = true;
                RequestKind::Verify(self.plan.clone().unwrap())
            }
            _ => return None,
        };
        self.next = self.next.saturating_add(1);
        let request = Request {
            sequence: self.next,
            profile,
            kind,
        };
        self.problem = None;
        self.pending = Some(request.clone());
        Some(request)
    }
}
fn valid_status(
    status: &ImportStatus,
    plan: &ImportPlan,
    proof: &HostSessionProof,
    previous: Option<&ImportStatus>,
) -> bool {
    let id: AccountId = format!("codex/{}", plan.operation).parse().unwrap();
    status.plan == *plan && status.observed_destination == *proof
        && status.plan.validate().is_ok()
        && status.receipt.as_ref().is_none_or(|receipt| receipt.operation() == plan.operation && receipt.validate().is_ok() && receipt.size() == plan.observation.size())
        && previous.and_then(|old| old.receipt.as_ref()).is_none_or(|old| status.receipt.as_ref() == Some(old))
        && match status.stage {
            ImportStage::Reserved => status.receipt.is_none(),
            _ => status.receipt.is_some(),
        }
        && status.registered.as_ref().is_none_or(|account| status.stage == ImportStage::Registered && account == &id)
        && status.coding_profile.as_ref().is_none_or(|report| {
            status.registered.as_ref() == Some(&report.account) && report.host == *proof
                && status.receipt.as_ref().is_some_and(|receipt| {
                    report.home.device == receipt.home().device() && report.home.inode == receipt.home().inode()
                }) && matches!(plan.profile, CodingProfile::WorkspaceWrite { network_access } if network_access == report.network_access)
        })
}
fn valid_list(statuses: &[ImportStatus], proof: &HostSessionProof) -> bool {
    let mut operations = std::collections::HashSet::new();
    statuses.len() <= 16
        && statuses.iter().all(|status| {
            operations.insert(status.plan.operation)
                && status.plan.validate().is_ok()
                && valid_status(status, &status.plan, proof, None)
        })
}
fn wire_operation(request: &Request, proof: &HostSessionProof) -> Result<ImportOperation, Failure> {
    Ok(match &request.kind {
        RequestKind::List => ImportOperation::List,
        RequestKind::Inspect(path) => ImportOperation::Inspect { path: path.clone() },
        RequestKind::Prepare(plan, target) => {
            if *proof != target.proof
                || *proof != plan.destination
                || target.profile != request.profile
            {
                return Err(Failure::PrepareNotSent);
            }
            ImportOperation::Prepare { plan: plan.clone() }
        }
        RequestKind::Status(plan) => ImportOperation::Status {
            operation: plan.operation,
        },
        // A fresh connection proof is mandatory; original intent is never rebased.
        RequestKind::Verify(plan) => ImportOperation::Verify {
            operation: plan.operation,
            destination: proof.clone(),
        },
    })
}
fn perform(request: Request) -> Result<Response, Failure> {
    if Profile::current() != request.profile {
        return Err(Failure::Changed);
    }
    let socket = match &request.kind {
        RequestKind::Prepare(_, target) => target.socket.clone(),
        _ => crate::terminal::ensure_daemon_ready().map_err(|_| Failure::Unavailable)?,
    };
    let mut client =
        crate::control::ControlClient::connect(&socket).map_err(|_| Failure::Unavailable)?;
    let proof = client.proof().clone();
    let operation = wire_operation(&request, &proof)?;
    let reply = client.device_operation(crate::device_service::Operation::AccountImport(operation)).map_err(|error| {
        if matches!(&error, crate::control::ControlError::Remote { message, .. } if message == "The import journal is unavailable, conflicts, or contains 16 retained operations; existing evidence was preserved") { Failure::Journal } else { Failure::Unavailable }
    })?;
    match reply {
        crate::device_service::Reply::AccountImport(reply) => Ok(Response {
            target: Target {
                socket,
                proof,
                profile: request.profile,
            },
            reply: *reply,
        }),
        _ => Err(Failure::Invalid),
    }
}
fn message(action: Action) -> Message {
    Message::AccountUi(super::Action::Import(action))
}
impl Cagents {
    pub(super) fn update_account_import(&mut self, action: Action) -> Task<Message> {
        if let Action::Finished { sequence, result } = action {
            return if self.account_ui.import.accept(sequence, result) {
                self.reload_registry()
            } else {
                Task::none()
            };
        }
        if let Action::Check(owner) = action {
            let state = &self.account_ui.import;
            if state.visible
                && state.owner == owner
                && state.pending.is_none()
                && state.profile.as_ref() == Some(&Profile::current())
                && let Some(id) = state.registered_account(&self.account_ui)
            {
                self.account_ui.import.close();
                return self.update_account_check(check::Action::Open(id));
            }
            return Task::none();
        }
        if matches!(action, Action::Open) {
            self.account_ui.check.close();
            self.account_ui.table.close_popups();
        }
        let request = self.account_ui.import.update(action, Profile::current());
        if let Some(request) = request {
            let sequence = request.sequence;
            return Task::perform(
                async move {
                    let result = tokio::task::spawn_blocking(move || perform(request))
                        .await
                        .unwrap_or(Err(Failure::Unavailable));
                    Action::Finished { sequence, result }
                },
                |action| cosmic::Action::App(message(action)),
            );
        }
        Task::none()
    }
}

pub(super) fn view(app: &Cagents) -> Option<Element<'_, Message>> {
    let state = &app.account_ui.import;
    if !state.visible {
        return None;
    }
    let owner = state.owner;
    let editable = state.editable() && state.profile.as_ref() == Some(&Profile::current());
    let profile = state.profile.as_ref().map_or("This device", Profile::label);
    let mut fields: Vec<Element<'_, Message>> = vec![
        design::heading("Import staged ChatGPT cache").into(),
        design::caption(format!("Destination: {profile} · New isolated Codex home")).into(),
        design::caption("Only an explicitly staged auth.json file is copied. No other home files or System preferences are imported. Registration does not verify execution access.").wrapping(iced::widget::text::Wrapping::Word).into(),
        crate::ui_controls::field::text("Staged auth.json path on this device", &state.path)
            .id(widget::Id::new("accounts.import.path"))
            .on_input_maybe(editable.then_some(move |value| message(Action::Path(owner, value))))
            .width(Length::Fill).into(),
        crate::ui_controls::field::text("Expected ChatGPT email", &state.email)
            .id(widget::Id::new("accounts.import.email"))
            .on_input_maybe(editable.then_some(move |value| message(Action::Email(owner, value))))
            .width(Length::Fill).into(),
    ];
    for (value, label, action, enabled) in [
        (
            state.copy,
            "Copy only this staged file into a new private home",
            Action::CopyConsent as fn(u64, bool) -> Action,
            editable,
        ),
        (
            state.session,
            "I understand copied sessions must not be used concurrently; refresh can invalidate another copy",
            Action::SessionConsent,
            editable,
        ),
        (
            state.write,
            "After identity verification, request a separate Workspace write profile",
            Action::WorkspaceWrite,
            editable,
        ),
        (
            state.network,
            "Also allow network access in that new profile",
            Action::Network,
            editable && state.write,
        ),
    ] {
        let checkbox = crate::ui_controls::field::checkbox(value).label(label);
        fields.push(if enabled {
            checkbox
                .on_toggle(move |value| message(action(owner, value)))
                .into()
        } else {
            checkbox.into()
        });
    }
    // Controls have stable positions; status and accepted completions are below
    // the entire editor, never inserted before a named input or role/control.
    fields.push(
        widget::column::with_children(vec![
            crate::ui_controls::button::secondary("Inspect file")
                .on_press_maybe(editable.then_some(message(Action::Inspect(owner))))
                .into(),
            crate::ui_controls::button::primary("Prepare import")
                .on_press_maybe(
                    state
                        .can_prepare()
                        .then_some(message(Action::Prepare(owner))),
                )
                .into(),
        ])
        .spacing(space::S)
        .into(),
    );
    fields.push(
        widget::column::with_children(vec![
            crate::ui_controls::button::secondary("Close")
                .on_press(message(Action::Close(owner)))
                .into(),
            crate::ui_controls::button::secondary("Status")
                .on_press_maybe(
                    (state.plan.is_some() && state.pending.is_none())
                        .then_some(message(Action::Status(owner))),
                )
                .into(),
            crate::ui_controls::button::primary("Verify identity")
                .on_press_maybe(
                    (state.plan.is_some()
                        && state.pending.is_none()
                        && state.status.as_ref().is_some_and(|status| {
                            matches!(
                                status.stage,
                                ImportStage::Published | ImportStage::Registered
                            ) || (status.stage == ImportStage::Prepared
                                && status.publication == ImportPublication::Present)
                        }))
                    .then_some(message(Action::Verify(owner))),
                )
                .into(),
        ])
        .spacing(space::S)
        .into(),
    );
    fields.push(
        crate::ui_controls::button::secondary("Load retained imports")
            .on_press_maybe(
                state
                    .pending
                    .is_none()
                    .then_some(message(Action::Load(owner))),
            )
            .into(),
    );
    fields.push(
        crate::ui_controls::button::secondary("New import")
            .on_press_maybe(state.can_new().then_some(message(Action::New(owner))))
            .into(),
    );
    let mut results: Vec<Element<'_, Message>> = vec![];
    if state.pending.is_some() {
        results.push(design::caption("Request in progress. Close only dismisses this form; it does not cancel an accepted import.").wrapping(iced::widget::text::Wrapping::Word).into());
    }
    if let Some(plan) = &state.plan {
        results.push(
            design::caption(format!("Original operation: {}", plan.operation))
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    }
    if let Some(problem) = state.problem {
        results.push(
            design::caption(problem)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    }
    if let Some(status) = &state.status {
        results.push(
            design::body(match status.stage {
                ImportStage::Reserved => "Intent retained; file publication not confirmed",
                ImportStage::Prepared => {
                    "Private home prepared; inspect publication before proceeding"
                }
                ImportStage::Published => "Cache published; identity not registered",
                ImportStage::Registered if status.registered.is_none() => {
                    "Historical registration; the current account is unavailable"
                }
                ImportStage::Registered => "Account registered; execution access not verified",
            })
            .into(),
        );
        if status.uncertain {
            let text = if status.stage == ImportStage::Registered {
                "Registration was acknowledged. The retained record still notes an earlier uncertain acknowledgement. New import starts a separate operation; it never retries this cache."
            } else {
                "An earlier acknowledgement was uncertain. Keep this operation; never reseed the cache or generate another key to retry."
            };
            results.push(
                design::caption(text)
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
            );
        }
        results.push(
            design::caption(match status.publication {
                ImportPublication::Missing => "Publication: not observed",
                ImportPublication::Present => {
                    "Publication: original file metadata observed (not identity validation)"
                }
                ImportPublication::Different => {
                    "Publication: metadata changed; a provider refresh may replace the cache"
                }
                ImportPublication::Unknown => "Publication: unconfirmed",
            })
            .wrapping(iced::widget::text::Wrapping::Word)
            .into(),
        );
        if let Some(report) = &status.coding_profile {
            results.push(design::caption(match report.outcome {
                crate::account::coding_config::Outcome::Created => "Separate coding profile created. Effective permissions still need an explicit check.",
                crate::account::coding_config::Outcome::NotCreated(crate::account::coding_config::Refusal::ExistingConfiguration) => "An existing configuration entry was preserved. Requested settings remain unverified; use Check coding access.",
                crate::account::coding_config::Outcome::NotCreated(_) => "Coding profile was not created; existing files were preserved.",
                crate::account::coding_config::Outcome::CreationUncertain(_) => "Coding profile publication is uncertain; inspect before any further configuration.",
            }).wrapping(iced::widget::text::Wrapping::Word).into());
        }
        if status.registered.is_some() && status.coding_profile.is_none() {
            results.push(design::caption("This response does not confirm coding-profile creation or effective permissions. Use an explicit coding-access check.").wrapping(iced::widget::text::Wrapping::Word).into());
        }
        if let Some(id) = &status.registered {
            results.push(
                crate::ui_controls::button::secondary("Check coding access")
                    .on_press_maybe(
                        (state.pending.is_none()
                            && state.registered_account(&app.account_ui).as_ref() == Some(id))
                        .then_some(message(Action::Check(owner))),
                    )
                    .into(),
            );
        }
    } else if state.inspection.is_some() {
        results.push(design::caption("File metadata inspected. Prepare copies it once; Verify identity is a separate provider request.").wrapping(iced::widget::text::Wrapping::Word).into());
    }
    if state.discovery != 0 && state.retained.is_empty() {
        results.push(design::caption("No retained records were returned. Any unconfirmed local operation remains preserved.").wrapping(iced::widget::text::Wrapping::Word).into());
    }
    if !state.retained.is_empty() {
        results.push(design::heading("Retained imports").into());
        results.push(design::caption("All retained records count toward the 16-operation limit, including completed imports. Loading and resuming do not copy a file or contact the provider.").wrapping(iced::widget::text::Wrapping::Word).into());
        for retained in &state.retained {
            let operation = retained.plan.operation;
            let stage = match retained.stage {
                ImportStage::Reserved => "Intent retained",
                ImportStage::Prepared => "Prepared",
                ImportStage::Published => "Published",
                ImportStage::Registered => "Historical registration",
            };
            let row =
                widget::column::with_children(vec![
                    design::body(format!(
                        "{} · {stage}{}",
                        retained.plan.expected_email,
                        if retained.uncertain {
                            " · acknowledgement uncertain"
                        } else {
                            ""
                        }
                    ))
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
                    design::caption(operation.to_string())
                        .wrapping(iced::widget::text::Wrapping::Word)
                        .into(),
                    crate::ui_controls::button::secondary("Resume import")
                        .on_press_maybe(state.can_resume(retained).then_some(message(
                            Action::Resume {
                                owner,
                                discovery: state.discovery,
                                operation,
                            },
                        )))
                        .into(),
                ])
                .spacing(space::XS);
            results.push(crate::ui_controls::owned(
                (owner, state.discovery, operation),
                row,
            ));
        }
    }
    Some(
        crate::ui_controls::owned(
            owner,
            widget::container(
                widget::column::with_children(vec![
                    widget::column::with_children(fields)
                        .spacing(space::S)
                        .into(),
                    widget::column::with_children(results)
                        .spacing(space::S)
                        .into(),
                ])
                .spacing(space::S),
            )
            .padding(space::M)
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_M; 4],
                true,
            )),
        )
        .into(),
    )
}

#[cfg(test)]
mod tests;
