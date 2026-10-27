//! A user-requested observation, bound to one account, workspace and backend.
//! Closing the form does not cancel the daemon's bounded operation.
use super::*;
use crate::account::{
    readiness::{CheckReport, CheckRequest, CheckStatus, Measurement},
    workspace_probe::Cleanup,
};
use crate::model::HostSessionProof;

mod coding_config;

#[derive(Default)]
pub(super) struct State {
    next: u64,
    form: Option<Form>,
    // Independent of visibility: Close/reopen cannot dispatch a competing check.
    pending: Option<Pending>,
    coding_config: coding_config::State,
}
impl State {
    pub(super) fn is_open(&self) -> bool {
        self.form.is_some()
    }

    pub(super) fn close(&mut self) {
        self.form = None;
    }
}

#[derive(Debug, Clone)]
struct Fence {
    account: Account,
    owner: u64,
    registered: Option<Identity>,
}
impl Fence {
    fn capture(state: &super::State, id: &AccountId) -> Option<Self> {
        let row = state.account(id)?;
        (row.account.provider == Provider::Codex).then(|| Self {
            account: row.account.clone(),
            owner: row.owner,
            registered: row
                .account
                .registry_key()
                .and_then(|key| state.registry.find(key))
                .map(Registered::identity),
        })
    }
    fn matches(&self, state: &super::State) -> bool {
        Self::capture(state, &self.account.id()).is_some_and(|current| {
            current.account == self.account
                && current.owner == self.owner
                && current.registered == self.registered
        })
    }
}

#[derive(Debug, Clone)]
pub struct Target {
    socket: PathBuf,
    proof: HostSessionProof,
}
struct Form {
    sequence: u64,
    fence: Fence,
    name: String,
    device: String,
    target: Option<Target>,
    preparing: bool,
    workspace: String,
    email: String,
    known_email: bool,
    write: bool,
    github: bool,
    repository: String,
    problem: Option<&'static str>,
    report: Option<CheckReport>,
}
impl Form {
    fn request(&self) -> Result<CheckRequest, &'static str> {
        let request = CheckRequest {
            account: self.fence.account.id(),
            workspace: PathBuf::from(&self.workspace),
            expected_email: self.email.trim().to_owned(),
            write: self.write,
            github_repository: self.github.then(|| self.repository.clone()),
        };
        if self.workspace.chars().any(char::is_control) {
            return Err("Choose a workspace path without control characters");
        }
        let email = request.expected_email.as_str();
        if email.chars().any(char::is_whitespace)
            || !email.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty() && !domain.is_empty() && !domain.contains('@')
            })
        {
            return Err("Provide the expected provider email explicitly");
        }
        request.validate()?;
        Ok(request)
    }
}
#[derive(Clone)]
struct Pending {
    sequence: u64,
    form: u64,
    fence: Fence,
    target: Target,
    request: CheckRequest,
}

#[derive(Debug, Clone, Copy)]
pub enum Failure {
    DeviceUnavailable,
    DeviceChanged,
    CheckUnavailable,
    InvalidResponse,
}
impl Failure {
    fn description(self) -> &'static str {
        match self {
            Self::DeviceUnavailable => {
                "The selected device could not be reached; reopen this check"
            }
            Self::DeviceChanged => "The device changed; reopen this check",
            Self::CheckUnavailable => "The device did not return the check result",
            Self::InvalidResponse => {
                "The device returned a result for a different or inconsistent check"
            }
        }
    }
}
#[derive(Debug, Clone)]
pub enum Action {
    Open(AccountId),
    Close(u64),
    Workspace(u64, String),
    Email(u64, String),
    Write(u64, bool),
    GitHub(u64, bool),
    Repository(u64, String),
    Run(u64),
    Prepared {
        form: u64,
        result: Result<Target, Failure>,
    },
    Finished {
        sequence: u64,
        result: Result<CheckReport, Failure>,
    },
    CodingConfig(coding_config::Action),
}
impl Action {
    fn form_sequence(&self) -> Option<u64> {
        match self {
            Self::Close(sequence)
            | Self::Workspace(sequence, _)
            | Self::Email(sequence, _)
            | Self::Write(sequence, _)
            | Self::GitHub(sequence, _)
            | Self::Repository(sequence, _)
            | Self::Run(sequence) => Some(*sequence),
            Self::Open(_)
            | Self::Prepared { .. }
            | Self::Finished { .. }
            | Self::CodingConfig(_) => None,
        }
    }
    pub(super) fn blocked_while_modal(&self) -> bool {
        !matches!(
            self,
            Self::Close(_)
                | Self::Prepared { .. }
                | Self::Finished { .. }
                | Self::CodingConfig(
                    coding_config::Action::Inspected { .. } | coding_config::Action::Created { .. }
                )
        )
    }
}
fn message(action: Action) -> Message {
    Message::AccountUi(super::Action::Check(action))
}
fn prepare() -> Result<Target, Failure> {
    let (socket, client) = super::connection::current().map_err(|_| Failure::DeviceUnavailable)?;
    Ok(Target {
        socket,
        proof: client.proof().clone(),
    })
}
fn exchange(
    client: &mut crate::control::ControlClient,
    target: &Target,
    request: &CheckRequest,
) -> Result<CheckReport, Failure> {
    if client.proof() != &target.proof {
        return Err(Failure::DeviceChanged);
    }
    match client.device_operation(crate::device_service::Operation::AccountCheck(
        request.clone(),
    )) {
        Ok(crate::device_service::Reply::AccountCheck(report))
            if report.matches_request(request, &target.proof) =>
        {
            Ok(*report)
        }
        Ok(_) => Err(Failure::InvalidResponse),
        Err(_) => Err(Failure::CheckUnavailable),
    }
}
fn run(target: Target, request: CheckRequest) -> Result<CheckReport, Failure> {
    request.validate().map_err(|_| Failure::InvalidResponse)?;
    let mut client =
        super::connection::at(&target.socket).map_err(|_| Failure::DeviceUnavailable)?;
    exchange(&mut client, &target, &request)
}

impl Cagents {
    pub(super) fn update_account_check(&mut self, action: Action) -> Task<Message> {
        if let Action::CodingConfig(action) = action {
            return coding_config::update(self, action);
        }
        // Native callbacks can remain queued across Close/reopen and a repaint.
        // They must act on the form that rendered them, including Run and Close.
        if let Some(sequence) = action.form_sequence()
            && self
                .account_ui
                .check
                .form
                .as_ref()
                .is_none_or(|form| form.sequence != sequence)
        {
            return Task::none();
        }
        match action {
            Action::Open(id) => {
                let Some(fence) = Fence::capture(&self.account_ui, &id) else {
                    return Task::none();
                };
                let name = self
                    .account_ui
                    .display_name(self.account_ui.account(&id).unwrap());
                let email = fence
                    .registered
                    .as_ref()
                    .and_then(|identity| identity.email.clone());
                let repository = self
                    .accounts_project()
                    .and_then(|(project, _, _)| self.github_availability.get(&project))
                    .and_then(|availability| availability.repository_key())
                    .filter(|key| key.host.eq_ignore_ascii_case("github.com") && key.port.is_none())
                    .map(|key| format!("{}/{}", key.owner, key.name))
                    .unwrap_or_default();
                let state = &mut self.account_ui.check;
                state.next = state.next.saturating_add(1);
                let sequence = state.next;
                state.form = Some(Form {
                    sequence,
                    fence,
                    name,
                    device: crate::device::selected()
                        .map(|device| device.name.clone())
                        .unwrap_or_else(|| "This device".into()),
                    target: None,
                    preparing: true,
                    workspace: self
                        .active_workspace
                        .as_ref()
                        .and_then(|path| path.to_str())
                        .unwrap_or_default()
                        .into(),
                    known_email: email.is_some(),
                    email: email.unwrap_or_default(),
                    write: false,
                    github: false,
                    repository,
                    problem: None,
                    report: None,
                });
                self.account_ui.table.close_popups();
                Task::perform(
                    async move {
                        let result = tokio::task::spawn_blocking(prepare)
                            .await
                            .unwrap_or(Err(Failure::DeviceUnavailable));
                        Action::Prepared {
                            form: sequence,
                            result,
                        }
                    },
                    |action| cosmic::Action::App(message(action)),
                )
            }
            Action::Close(_) => {
                self.account_ui.check.close();
                if !self.modal_is_open() && self.account_ui.table.link.is_some() {
                    widget::text_input::focus(table::link_id())
                } else {
                    Task::none()
                }
            }
            Action::Prepared { form, result } => {
                if let Some(current) = &mut self.account_ui.check.form
                    && current.sequence == form
                {
                    current.preparing = false;
                    match result {
                        Ok(target) => current.target = Some(target),
                        Err(error) => current.problem = Some(error.description()),
                    }
                }
                Task::none()
            }
            Action::Finished { sequence, result } => {
                let state = &mut self.account_ui.check;
                if state
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.sequence != sequence)
                {
                    return Task::none();
                }
                let pending = state.pending.take().unwrap();
                let current = pending.fence.matches(&self.account_ui);
                if let Some(form) = &mut self.account_ui.check.form
                    && form.sequence == pending.form
                {
                    if !current {
                        form.problem = Some("The selected account changed; reopen this check");
                    } else {
                        match result {
                            Ok(report)
                                if report
                                    .matches_request(&pending.request, &pending.target.proof) =>
                            {
                                form.report = Some(report)
                            }
                            Ok(_) => form.problem = Some(Failure::InvalidResponse.description()),
                            Err(Failure::CheckUnavailable) if pending.request.write => {
                                form.problem = Some(
                                    "The device did not return the check result. Scratch cleanup is not confirmed; inspect the scoped workspace before retrying",
                                );
                            }
                            Err(error) => form.problem = Some(error.description()),
                        }
                    }
                }
                Task::none()
            }
            Action::Run(_) => {
                if self.account_ui.check.pending.is_some()
                    || self.account_ui.check.coding_config.pending()
                {
                    return Task::none();
                }
                let Some(form) = &self.account_ui.check.form else {
                    return Task::none();
                };
                if !form.fence.matches(&self.account_ui) {
                    self.account_ui.check.form.as_mut().unwrap().problem =
                        Some("The selected account changed; reopen this check");
                    return Task::none();
                }
                let request = match form.request() {
                    Ok(request) => request,
                    Err(problem) => {
                        self.account_ui.check.form.as_mut().unwrap().problem = Some(problem);
                        return Task::none();
                    }
                };
                if self.project_ui.deleting_directory(&request.workspace) {
                    self.account_ui.check.form.as_mut().unwrap().problem =
                        Some("This workspace is being removed; choose another workspace");
                    return Task::none();
                }
                let Some(target) = form.target.clone() else {
                    return Task::none();
                };
                let pending = Pending {
                    sequence: self.account_ui.check.next.saturating_add(1),
                    form: form.sequence,
                    fence: form.fence.clone(),
                    target: target.clone(),
                    request: request.clone(),
                };
                let sequence = pending.sequence;
                let state = &mut self.account_ui.check;
                state.next = sequence;
                let form = state.form.as_mut().unwrap();
                form.problem = None;
                form.report = None;
                state.pending = Some(pending);
                Task::perform(
                    async move {
                        let result = tokio::task::spawn_blocking(move || run(target, request))
                            .await
                            .unwrap_or(Err(Failure::CheckUnavailable));
                        Action::Finished { sequence, result }
                    },
                    |action| cosmic::Action::App(message(action)),
                )
            }
            input => {
                if self.account_ui.check.pending.is_some()
                    || self.account_ui.check.coding_config.pending()
                {
                    return Task::none();
                }
                if let Some(form) = &mut self.account_ui.check.form {
                    match input {
                        Action::Workspace(_, value) => form.workspace = value,
                        Action::Email(_, value) if !form.known_email => form.email = value,
                        Action::Write(_, value) => form.write = value,
                        Action::GitHub(_, value) => form.github = value,
                        Action::Repository(_, value) => form.repository = value,
                        _ => return Task::none(),
                    }
                    form.problem = None;
                    form.report = None;
                }
                Task::none()
            }
        }
    }
}

fn status(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::NotRequested => "Not requested",
        CheckStatus::Passed => "Passed",
        CheckStatus::Failed => "Failed",
        CheckStatus::Unsupported => "Unsupported",
        CheckStatus::Unknown => "Unknown",
    }
}
fn measurement(label: &'static str, value: Measurement) -> Element<'static, Message> {
    widget::column::with_children(vec![
        design::body(format!("{label} · {}", status(value.status))).into(),
        design::caption(value.reason.description())
            .wrapping(iced::widget::text::Wrapping::Word)
            .into(),
    ])
    .spacing(space::XXS)
    .into()
}
fn results(report: &CheckReport, now_ms: Millis) -> Element<'_, Message> {
    use crate::account::readiness::{ApprovalPolicy, CheckReason, SandboxMode};
    let observed = if report.observed_at == 0 {
        "No provider observation recorded".to_owned()
    } else {
        format!(
            "Observed {}",
            account::format_local_moment(unix_seconds(report.observed_at), unix_seconds(now_ms))
        )
    };
    let mut rows = vec![
        design::caption(format!(
            "Workspace: {} · {observed}",
            report.workspace.display(),
        ))
        .wrapping(iced::widget::text::Wrapping::Word)
        .into(),
        measurement("Authentication", report.authentication),
        measurement(
            "Default execution policy",
            report.execution_profile_configured,
        ),
        measurement("Workspace write, read and removal", report.workspace_write),
        measurement("GitHub repository read", report.github),
    ];
    if let Some(profile) = &report.profile {
        let sandbox = match profile.sandbox_mode {
            SandboxMode::ReadOnly => "read only",
            SandboxMode::WorkspaceWrite => "workspace write",
            SandboxMode::DangerFullAccess => "full access",
            SandboxMode::ExternalSandbox => "external sandbox",
            SandboxMode::Unknown => "unknown",
        };
        let approval = match profile.approval_policy {
            ApprovalPolicy::Untrusted => "untrusted commands",
            ApprovalPolicy::OnFailure => "on failure",
            ApprovalPolicy::OnRequest => "on request",
            ApprovalPolicy::Never => "never",
            ApprovalPolicy::Unknown => "unknown",
        };
        let network = match profile.network_access {
            Some(true) => "enabled",
            Some(false) => "disabled",
            None => "not reported",
        };
        rows.push(
            design::caption(format!(
                "Configured sandbox: {sandbox} · Approval: {approval} · Network: {network}"
            ))
            .wrapping(iced::widget::text::Wrapping::Word)
            .into(),
        );
        if profile.sandbox_mode == SandboxMode::ReadOnly
            && report.authentication.status == CheckStatus::Passed
            && matches!(
                report.execution_profile_configured.reason,
                CheckReason::PolicyReported | CheckReason::ImplicitPolicy
            )
        {
            rows.push(
                design::caption("For a new conversation, choose Permissions → Workspace write to request file edits. Account defaults stay unchanged; existing conversations and network access have separate policies.")
                    .wrapping(iced::widget::text::Wrapping::Word)
                    .into(),
            );
        }
    }
    if let Some(cleanup) = report.workspace_cleanup {
        rows.push(
            design::caption(match cleanup {
                Cleanup::NotCreated => "Scratch cleanup: no scratch created",
                Cleanup::Removed => "Scratch cleanup: removed",
                Cleanup::ReplacementPreserved => {
                    "Scratch cleanup: a replacement was preserved; inspect the scoped workspace"
                }
                Cleanup::Unconfirmed => {
                    "Scratch cleanup: not confirmed; inspect the scoped workspace"
                }
            })
            .wrapping(iced::widget::text::Wrapping::Word)
            .into(),
        );
    }
    rows.push(
        design::caption(if report.provider_cleanup_confirmed {
            "Provider cleanup: confirmed"
        } else {
            "Provider cleanup: not confirmed"
        })
        .wrapping(iced::widget::text::Wrapping::Word)
        .into(),
    );
    if let Some(push) = report.github_push_allowed {
        rows.push(
            design::caption(format!(
                "GitHub reported push permission: {}. No push performed.",
                if push { "allowed" } else { "not allowed" }
            ))
            .into(),
        );
    }
    widget::column::with_children(rows).spacing(space::S).into()
}
pub(super) fn view(app: &Cagents) -> Option<Element<'_, Message>> {
    let state = &app.account_ui.check;
    let form = state.form.as_ref()?;
    let sequence = form.sequence;
    let pending = state.pending.is_some();
    let configuring = state.coding_config.pending();
    let current = form.fence.matches(&app.account_ui);
    let editable = !pending && !configuring && current;
    let mut rows: Vec<Element<'_, Message>> = vec![
        design::heading("Check coding access").into(),
        design::body(format!("{} · {}", form.name, form.device)).wrapping(iced::widget::text::Wrapping::Word).into(),
        design::caption(format!("Account: {}", form.fence.account.id().as_str())).wrapping(iced::widget::text::Wrapping::Word).into(),
        design::caption("Observe the account's default policy. This does not verify another actor or a future launch.").wrapping(iced::widget::text::Wrapping::Word).into(),
        design::caption("Workspace on the selected device").into(),
        crate::ui_controls::field::text("Absolute path or ~/workspace", &form.workspace)
            .id(widget::Id::new("accounts.check.workspace"))
            .on_input_maybe(editable.then_some(move |value| message(Action::Workspace(sequence, value))))
            .width(Length::Fill).into(),
        design::caption("Expected Codex email").into(),
        crate::ui_controls::field::text("Expected provider email", &form.email)
            .id(widget::Id::new("accounts.check.email"))
            .on_input_maybe((editable && !form.known_email).then_some(move |value| message(Action::Email(sequence, value))))
            .width(Length::Fill).into(),
    ];
    let write = crate::ui_controls::field::checkbox(form.write)
        .label("Check workspace write, read and removal");
    rows.push(if editable {
        write
            .on_toggle(move |value| message(Action::Write(sequence, value)))
            .into()
    } else {
        write.into()
    });
    let github = crate::ui_controls::field::checkbox(form.github)
        .label("Check GitHub repository read access");
    rows.push(if editable {
        github
            .on_toggle(move |value| message(Action::GitHub(sequence, value)))
            .into()
    } else {
        github.into()
    });
    if form.github {
        rows.push(
            crate::ui_controls::field::text("GitHub.com OWNER/REPO", &form.repository)
                .id(widget::Id::new("accounts.check.repository"))
                .on_input_maybe(
                    editable.then_some(move |value| message(Action::Repository(sequence, value))),
                )
                .width(Length::Fill)
                .into(),
        );
    }
    if !current {
        rows.push(design::caption("The selected account changed; reopen this check").into());
    } else if let Some(problem) = form.problem {
        rows.push(
            design::caption(problem)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    }
    if pending {
        rows.push(
            design::caption("A check is finishing. Closing this form leaves the check running.")
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    } else if form.preparing {
        rows.push(design::caption("Connecting to the selected device…").into());
    }
    if current && let Some(report) = &form.report {
        rows.push(results(report, app.account_ui.now_ms));
    }
    if let Some(configuration) = coding_config::view(app) {
        rows.push(configuration);
    }
    rows.push(
        widget::row::with_children(vec![
            crate::ui_controls::button::secondary("Close")
                .on_press(message(Action::Close(sequence)))
                .into(),
            crate::ui_controls::button::primary(if pending { "Checking…" } else { "Check" })
                .id(widget::Id::new("accounts.check.run"))
                .on_press_maybe(
                    (editable
                        && form.target.is_some()
                        && form.request().is_ok()
                        && !app
                            .project_ui
                            .deleting_directory(Path::new(&form.workspace)))
                    .then_some(message(Action::Run(sequence))),
                )
                .into(),
        ])
        .spacing(space::S)
        .into(),
    );
    Some(
        widget::container(widget::column::with_children(rows).spacing(space::S))
            .padding(space::M)
            .class(design::surface(
                Some(design::raised),
                [design::RADIUS_M; 4],
                true,
            ))
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::readiness::{ApprovalPolicy, CheckReason, PolicySummary, SandboxMode};
    use crate::app::tests::context_inspector_test_app;
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    fn proof(name: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(name),
            boot_id: BootId::new("test-boot"),
            host_incarnation: HostIncarnation::new("test-incarnation"),
        }
    }
    fn target() -> Target {
        Target {
            socket: "/task-owned-fixture.sock".into(),
            proof: proof("device-a"),
        }
    }
    pub(super) fn fixture() -> (Cagents, AccountId, AccountId) {
        let mut app = context_inspector_test_app(false);
        let first = app.account_ui.add_managed_fixture(Provider::Codex, 145901);
        let second = app.account_ui.add_managed_fixture(Provider::Codex, 145902);
        app.active_workspace = Some("~/selected-worktree".into());
        (app, first, second)
    }
    pub(super) fn open(app: &mut Cagents, id: &AccountId) {
        drop(app.update_account_check(Action::Open(id.clone())));
        let form = app.account_ui.check.form.as_ref().unwrap().sequence;
        drop(app.update_account_check(Action::Prepared {
            form,
            result: Ok(target()),
        }));
    }
    pub(super) fn report(request: &CheckRequest) -> CheckReport {
        let mut report = CheckReport::new(request);
        report.workspace = "/destination/selected-worktree".into();
        report.host = Some(proof("device-a"));
        report.authentication =
            Measurement::new(CheckStatus::Passed, CheckReason::IdentityConfirmed);
        report.observed_identity = Some(Identity {
            email: Some(request.expected_email.clone()),
            ..Identity::default()
        });
        report.execution_profile_configured =
            Measurement::new(CheckStatus::Passed, CheckReason::PolicyReported);
        report.profile = Some(PolicySummary {
            sandbox_mode: SandboxMode::WorkspaceWrite,
            approval_policy: ApprovalPolicy::Never,
            network_access: Some(true),
        });
        if request.write {
            report.workspace_write =
                Measurement::new(CheckStatus::Failed, CheckReason::SandboxSetupFailed);
        }
        if request.github_repository.is_some() {
            report.github = Measurement::new(CheckStatus::Failed, CheckReason::SandboxSetupFailed);
        }
        report
    }

    // Exercise the real retained widget composition and pointer dispatch, rather
    // than call a button's reducer action and assume the control exists.
    pub(super) fn click(mut element: Element<'_, Message>, label: &str) -> Vec<Message> {
        use iced::advanced::{Layout, clipboard, layout, widget::Tree};
        use iced::runtime::{UserInterface, user_interface::Cache};
        use iced::{Event, Point, Size, mouse};
        let mut renderer = crate::app::tests::headless_renderer();
        let size = Size::new(1000.0, 1600.0);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let bounds = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .find(|node| node.node().label() == Some(label))
            .unwrap_or_else(|| panic!("missing actual control: {label}"))
            .node()
            .bounds()
            .unwrap();
        let point = Point::new(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        let mut ui = UserInterface::build(element, size, Cache::new(), &mut renderer);
        let mut messages = Vec::new();
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::CursorMoved { position: point }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(point),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        messages
    }
    pub(super) fn apply(app: &mut Cagents, messages: Vec<Message>) {
        for message in messages {
            if let Message::AccountUi(super::super::Action::Check(action)) = message {
                drop(app.update_account_check(action));
            }
        }
    }

    fn set_value(element: Element<'_, Message>, id: &'static str, value: &str) -> Vec<Message> {
        use iced::advanced::clipboard;
        use iced::runtime::{UserInterface, user_interface::Cache};
        use iced_accessibility::{
            A11yId,
            accesskit::{Action as AccessibleAction, ActionData, ActionRequest, TreeId},
        };
        let mut renderer = crate::app::tests::headless_renderer();
        let mut ui = UserInterface::build(
            element,
            iced::Size::new(1000.0, 1600.0),
            Cache::new(),
            &mut renderer,
        );
        let id = widget::Id::new(id);
        let mut messages = Vec::new();
        let _ = ui.update(
            &[iced::Event::A11y(
                id.clone(),
                ActionRequest {
                    action: AccessibleAction::SetValue,
                    target_tree: TreeId::ROOT,
                    target_node: A11yId::from(id).into(),
                    data: Some(ActionData::Value(value.into())),
                },
            )],
            iced::mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        messages
    }

    #[test]
    fn queued_rendered_form_actions_cannot_mutate_close_or_run_a_replacement_form() {
        let (mut app, second, _) = fixture();
        let first = Account::system(Provider::Codex).id();
        open(&mut app, &first);
        let mut queued = Vec::new();
        for (id, value) in [
            ("accounts.check.workspace", "~/queued-workspace"),
            ("accounts.check.email", "queued@example.test"),
        ] {
            let messages = set_value(view(&app).unwrap(), id, value);
            assert!(
                !messages.is_empty(),
                "the actual editable field must dispatch"
            );
            apply(&mut app, messages.clone());
            queued.extend(messages);
        }
        for label in [
            "Check workspace write, read and removal",
            "Check GitHub repository read access",
        ] {
            let messages = click(view(&app).unwrap(), label);
            apply(&mut app, messages.clone());
            queued.extend(messages);
        }
        let messages = set_value(
            view(&app).unwrap(),
            "accounts.check.repository",
            "owner/from-a",
        );
        assert!(!messages.is_empty());
        apply(&mut app, messages.clone());
        queued.extend(messages);
        queued.extend(click(view(&app).unwrap(), "Check"));
        queued.extend(click(view(&app).unwrap(), "Close"));
        open(&mut app, &second);
        let sequence = app.account_ui.check.form.as_ref().unwrap().sequence;
        apply(&mut app, queued);
        let form = app
            .account_ui
            .check
            .form
            .as_ref()
            .expect("old Close must not close B");
        assert_eq!(form.sequence, sequence);
        assert_eq!(form.fence.account.id(), second);
        assert_eq!(form.workspace, "~/selected-worktree");
        assert_ne!(form.email, "queued@example.test");
        assert!(!form.write && !form.github && form.repository.is_empty());
        assert!(
            app.account_ui.check.pending.is_none(),
            "old Run must not authorize B"
        );
        assert!(
            set_value(
                view(&app).unwrap(),
                "accounts.check.email",
                "replacement@example.test"
            )
            .is_empty(),
            "a known registered identity stays read-only"
        );
    }

    #[test]
    fn existing_home_editor_resumes_visible_focus_without_cancelling_pending_check() {
        use super::super::Action as AccountAction;
        use crate::app::tests::focused_element;
        let (mut app, first, second) = fixture();
        let home = "/selected/existing-codex-home";
        drop(app.update_account_ui(AccountAction::LinkStart(Provider::Codex)));
        let messages = set_value(
            super::super::table::table(&app, 1000.0),
            "accounts.link-home",
            home,
        );
        assert!(
            !messages.is_empty(),
            "the mounted home field must accept input"
        );
        for message in messages {
            if let Message::AccountUi(action) = message {
                drop(app.update_account_ui(action));
            }
        }
        open(&mut app, &first);
        assert!(view(&app).is_some());
        assert_eq!(app.account_ui.table.link.as_ref().unwrap().1, home);
        let messages = click(view(&app).unwrap(), "Check");
        apply(&mut app, messages);
        let first_pending = app.account_ui.check.pending.as_ref().unwrap().clone();
        let queued_close = click(view(&app).unwrap(), "Close");

        // Use the same rendered toolbar choice as the production popover.
        let messages = click(super::super::table::table(&app, 1000.0), "Add account");
        for message in messages {
            if let Message::AccountUi(action) = message {
                drop(app.update_account_ui(action));
            }
        }
        assert!(app.account_ui.table.add_menu);
        let choice = click(
            super::super::table::add_menu(&app.account_ui, false),
            "Existing Codex home",
        )
        .into_iter()
        .find_map(|message| match message {
            Message::AccountUi(action @ AccountAction::LinkStart(_)) => Some(action),
            _ => None,
        })
        .expect("the actual existing-home choice must dispatch");
        let focus = app.update_account_ui(choice);
        assert!(
            view(&app).is_none(),
            "the check editor must give way to the requested home editor"
        );
        assert_eq!(
            app.account_ui.table.link,
            Some((Provider::Codex, home.into()))
        );
        assert_eq!(
            app.account_ui.check.pending.as_ref().unwrap().sequence,
            first_pending.sequence
        );
        assert_eq!(
            focused_element(super::super::table::table(&app, 1000.0), focus),
            Some(super::super::table::link_id())
        );
        apply(&mut app, queued_close);
        assert!(app.account_ui.check.pending.is_some());

        // A completion for the hidden editor cannot be adopted by a new one.
        open(&mut app, &second);
        drop(app.update_account_check(Action::Finished {
            sequence: first_pending.sequence,
            result: Ok(report(&first_pending.request)),
        }));
        assert!(app.account_ui.check.form.as_ref().unwrap().report.is_none());
        let sequence = app.account_ui.check.form.as_ref().unwrap().sequence;
        drop(app.update_account_check(Action::Run(sequence)));
        let newer = app.account_ui.check.pending.as_ref().unwrap().clone();
        drop(app.update_account_check(Action::Finished {
            sequence: first_pending.sequence,
            result: Ok(report(&first_pending.request)),
        }));
        assert_eq!(
            app.account_ui.check.pending.as_ref().unwrap().sequence,
            newer.sequence
        );
        let close = click(view(&app).unwrap(), "Close")
            .into_iter()
            .find_map(|message| match message {
                Message::AccountUi(super::super::Action::Check(action @ Action::Close(_))) => {
                    Some(action)
                }
                _ => None,
            })
            .unwrap();
        let focus = app.update_account_check(close);
        assert_eq!(
            focused_element(super::super::table::table(&app, 1000.0), focus),
            Some(super::super::table::link_id())
        );
        assert_eq!(
            app.account_ui.check.pending.as_ref().unwrap().sequence,
            newer.sequence
        );
        assert_eq!(
            app.account_ui.table.link,
            Some((Provider::Codex, home.into()))
        );

        // An in-flight link cannot have its operand replaced by an old menu
        // callback, and the visible menu reflects that disabled action.
        open(&mut app, &first);
        let stale_choice = click(
            super::super::table::add_menu(&app.account_ui, false),
            "Existing Claude home",
        )
        .into_iter()
        .find_map(|message| match message {
            Message::AccountUi(action @ AccountAction::LinkStart(_)) => Some(action),
            _ => None,
        })
        .unwrap();
        app.account_ui.table.linking = true;
        assert!(
            click(
                super::super::table::add_menu(&app.account_ui, false),
                "Existing Codex home"
            )
            .is_empty()
        );
        drop(app.update_account_ui(stale_choice));
        assert!(view(&app).is_some());
        assert_eq!(
            app.account_ui.table.link,
            Some((Provider::Codex, home.into()))
        );
        assert_eq!(
            app.account_ui.check.pending.as_ref().unwrap().sequence,
            newer.sequence
        );
    }

    #[test]
    fn queued_check_close_does_not_focus_a_home_field_behind_a_new_modal() {
        let (mut app, first, _) = fixture();
        drop(app.update_account_ui(super::super::Action::LinkStart(Provider::Codex)));
        drop(app.update_account_ui(super::super::Action::LinkInput(
            "/selected/existing-home".into(),
        )));
        open(&mut app, &first);
        let sequence = app.account_ui.check.form.as_ref().unwrap().sequence;
        drop(app.update_account_check(Action::Run(sequence)));
        let pending = app.account_ui.check.pending.as_ref().unwrap().sequence;
        let close = click(view(&app).unwrap(), "Close")
            .into_iter()
            .find_map(|message| match message {
                Message::AccountUi(super::super::Action::Check(action @ Action::Close(_))) => {
                    Some(action)
                }
                _ => None,
            })
            .unwrap();
        app.device_ui.open = true;
        assert!(app.modal_is_open());
        assert!(!close.blocked_while_modal());
        let task = app.update_account_check(close);
        assert!(app.account_ui.check.form.is_none());
        assert_eq!(
            app.account_ui.check.pending.as_ref().unwrap().sequence,
            pending
        );
        assert_eq!(
            crate::app::tests::focused_element(super::super::table::table(&app, 1000.0), task),
            None,
            "an old Close callback must leave the new modal's keyboard focus alone"
        );
        assert!(app.modal_is_open());
        assert_eq!(
            app.account_ui.table.link.as_ref().unwrap().1,
            "/selected/existing-home"
        );
    }

    #[test]
    fn rendered_account_menu_and_opt_ins_dispatch_the_frozen_destination() {
        let (mut app, first, _) = fixture();
        let row = app.account_ui.account(&first).unwrap();
        let messages = click(
            super::super::table::menu(&app, row, None),
            "Check coding access",
        );
        assert!(messages.iter().any(|message| matches!(message, Message::AccountUi(super::super::Action::Check(Action::Open(id))) if id == &first)));
        apply(&mut app, messages);
        let form = app.account_ui.check.form.as_ref().unwrap();
        assert!(!form.write && !form.github);
        let sequence = form.sequence;
        drop(app.update_account_check(Action::Prepared {
            form: sequence,
            result: Ok(target()),
        }));
        let messages = click(
            view(&app).unwrap(),
            "Check workspace write, read and removal",
        );
        apply(&mut app, messages);
        let messages = click(view(&app).unwrap(), "Check GitHub repository read access");
        apply(&mut app, messages);
        drop(app.update_account_check(Action::Repository(
            app.account_ui.check.form.as_ref().unwrap().sequence,
            "owner/repository".into(),
        )));
        // Focus navigation must not rebase the already opened form.
        app.active_workspace = Some("/other-worktree".into());
        let messages = click(view(&app).unwrap(), "Check");
        assert!(messages.iter().any(|message| matches!(
            message,
            Message::AccountUi(super::super::Action::Check(Action::Run(_)))
        )));
        apply(&mut app, messages);
        let pending = app.account_ui.check.pending.as_ref().unwrap();
        assert_eq!(pending.request.account, first);
        assert_eq!(
            pending.request.workspace,
            PathBuf::from("~/selected-worktree")
        );
        assert!(pending.request.write);
        assert_eq!(
            pending.request.github_repository.as_deref(),
            Some("owner/repository")
        );
        assert_eq!(pending.target.proof, proof("device-a"));
        assert_eq!(
            pending.request.expected_email,
            "00000000-0000-0000-0000-0000000239ed@example.com"
        );
    }

    #[test]
    fn close_reopen_duplicate_run_and_late_completion_cannot_cross_accounts() {
        let (mut app, first, second) = fixture();
        open(&mut app, &first);
        drop(app.update_account_check(Action::Run(
            app.account_ui.check.form.as_ref().unwrap().sequence,
        )));
        let first_pending = app.account_ui.check.pending.as_ref().unwrap().clone();
        drop(app.update_account_check(Action::Run(
            app.account_ui.check.form.as_ref().unwrap().sequence,
        )));
        assert_eq!(
            app.account_ui.check.pending.as_ref().unwrap().sequence,
            first_pending.sequence
        );
        let messages = click(view(&app).unwrap(), "Close");
        apply(&mut app, messages);
        assert!(app.account_ui.check.form.is_none());
        assert!(
            app.account_ui.check.pending.is_some(),
            "Close does not claim server cancellation"
        );
        open(&mut app, &second);
        drop(app.update_account_check(Action::Run(
            app.account_ui.check.form.as_ref().unwrap().sequence,
        )));
        assert_eq!(
            app.account_ui
                .check
                .pending
                .as_ref()
                .unwrap()
                .request
                .account,
            first
        );
        drop(app.update_account_check(Action::Finished {
            sequence: first_pending.sequence,
            result: Ok(report(&first_pending.request)),
        }));
        assert!(app.account_ui.check.pending.is_none());
        assert!(app.account_ui.check.form.as_ref().unwrap().report.is_none());
        drop(app.update_account_check(Action::Run(
            app.account_ui.check.form.as_ref().unwrap().sequence,
        )));
        let newer = app.account_ui.check.pending.as_ref().unwrap().sequence;
        drop(app.update_account_check(Action::Finished {
            sequence: first_pending.sequence,
            result: Err(Failure::CheckUnavailable),
        }));
        assert_eq!(
            app.account_ui.check.pending.as_ref().unwrap().sequence,
            newer
        );
    }

    #[test]
    fn account_owner_identity_and_report_scope_are_checked_after_io() {
        for change in 0..4 {
            let (mut app, first, _) = fixture();
            open(&mut app, &first);
            drop(app.update_account_check(Action::Write(
                app.account_ui.check.form.as_ref().unwrap().sequence,
                true,
            )));
            drop(app.update_account_check(Action::Run(
                app.account_ui.check.form.as_ref().unwrap().sequence,
            )));
            let pending = app.account_ui.check.pending.as_ref().unwrap().clone();
            let mut returned = report(&pending.request);
            match change {
                0 => app
                    .account_ui
                    .account_mut(&first)
                    .unwrap()
                    .begin_sign_in(now_ms()),
                1 => {
                    let key = app
                        .account_ui
                        .account(&first)
                        .unwrap()
                        .account
                        .registry_key()
                        .unwrap();
                    let replacement = Identity {
                        email: Some("replacement@example.test".into()),
                        ..Identity::default()
                    };
                    // An observation of a different login is refused. Exercise
                    // the explicit replacement that actually changes this fence.
                    assert!(app.account_ui.registry.adopt_identity(key, &replacement));
                    assert_eq!(
                        app.account_ui.registry.find(key).unwrap().identity(),
                        replacement
                    );
                }
                2 => returned.requested_workspace = "/other-workspace".into(),
                3 => returned.host = Some(proof("replacement-daemon")),
                _ => unreachable!(),
            }
            drop(app.update_account_check(Action::Finished {
                sequence: pending.sequence,
                result: Ok(returned),
            }));
            let form = app.account_ui.check.form.as_ref().unwrap();
            assert!(form.report.is_none(), "account/report change {change}");
            assert!(form.problem.is_some(), "account/report change {change}");
            assert!(
                app.account_ui.check.pending.is_none(),
                "account/report change {change}"
            );
        }
    }

    #[test]
    fn independent_identity_and_policy_observations_survive_sandbox_failure() {
        let (mut app, first, _) = fixture();
        open(&mut app, &first);
        drop(app.update_account_check(Action::Write(
            app.account_ui.check.form.as_ref().unwrap().sequence,
            true,
        )));
        drop(app.update_account_check(Action::Run(
            app.account_ui.check.form.as_ref().unwrap().sequence,
        )));
        let pending = app.account_ui.check.pending.as_ref().unwrap().clone();
        let returned = report(&pending.request);
        drop(app.update_account_check(Action::Finished {
            sequence: pending.sequence,
            result: Ok(returned),
        }));
        let form = app.account_ui.check.form.as_ref().unwrap();
        let returned = form.report.as_ref().unwrap();
        assert_eq!(returned.authentication.status, CheckStatus::Passed);
        assert_eq!(
            returned.execution_profile_configured.status,
            CheckStatus::Passed
        );
        assert_eq!(
            returned.workspace_write.reason,
            CheckReason::SandboxSetupFailed
        );
        assert_eq!(returned.workspace_cleanup, Some(Cleanup::Unconfirmed));
        // Result composition stays usable; closing uses the actual rendered control.
        let messages = click(view(&app).unwrap(), "Close");
        apply(&mut app, messages);
        assert!(app.account_ui.check.form.is_none());
    }

    #[test]
    fn rendered_provider_cleanup_stays_separate_from_verified_scratch_removal() {
        use crate::account::workspace_probe::{Receipt, Status};
        use iced::advanced::{Layout, layout, widget::Tree};
        for confirmed in [false, true] {
            let (mut app, first, _) = fixture();
            open(&mut app, &first);
            let sequence = app.account_ui.check.form.as_ref().unwrap().sequence;
            drop(app.update_account_check(Action::Write(sequence, true)));
            drop(app.update_account_check(Action::Run(sequence)));
            let pending = app.account_ui.check.pending.as_ref().unwrap().clone();
            let mut returned = report(&pending.request);
            returned.workspace_write =
                Measurement::new(CheckStatus::Passed, CheckReason::WorkspaceRoundTrip);
            returned.workspace_cleanup = Some(Cleanup::Removed);
            returned.workspace_receipt = Some(Receipt {
                nonce: uuid::Uuid::new_v4(),
                created: true,
                read_match: true,
                removed: true,
                status: Status::Verified,
                cleanup: Cleanup::Removed,
            });
            returned.provider_cleanup_confirmed = confirmed;
            drop(app.update_account_check(Action::Finished {
                sequence: pending.sequence,
                result: Ok(returned),
            }));

            let renderer = crate::app::tests::headless_renderer();
            let mut element = view(&app).expect("the finished check remains visible");
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(1000.0, 1600.0)),
            );
            let nodes = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                iced::mouse::Cursor::Unavailable,
            );
            let labels: Vec<_> = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter_map(|node| node.node().label())
                .collect();
            assert!(labels.contains(&"Scratch cleanup: removed"));
            assert!(labels.contains(&"Workspace write, read and removal · Passed"));
            let confirmed_caption = "Provider cleanup: confirmed";
            let unconfirmed_caption = "Provider cleanup: not confirmed";
            assert!(labels.contains(&if confirmed {
                confirmed_caption
            } else {
                unconfirmed_caption
            }));
            assert!(!labels.contains(&if confirmed {
                unconfirmed_caption
            } else {
                confirmed_caption
            }));
        }
    }

    #[test]
    fn framed_exchange_checks_backend_before_dispatch_and_never_retries() {
        use crate::terminal_ipc::{
            ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
            ServerResponse, read_frame, write_frame,
        };
        use std::{os::unix::net::UnixStream, thread};
        for outcome in 0..3 {
            let (mut app, first, _) = fixture();
            open(&mut app, &first);
            let request = app
                .account_ui
                .check
                .form
                .as_ref()
                .unwrap()
                .request()
                .unwrap();
            let expected = request.clone();
            let (stream, mut peer) = UnixStream::pair().unwrap();
            peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            peer.set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let worker = thread::spawn(move || {
                let host = if outcome == 1 {
                    proof("changed-backend")
                } else {
                    proof("device-a")
                };
                let connect: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                assert!(matches!(connect.request, ClientRequest::Connect { .. }));
                write_frame(
                    &mut peer,
                    &ServerEnvelope {
                        version: PROTOCOL_VERSION,
                        request_id: Some(connect.request_id),
                        proof: host.clone(),
                        message: ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof: host.clone(),
                                resumed: vec![],
                            },
                        },
                    },
                )
                .unwrap();
                if outcome != 1 {
                    let check: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                    assert_eq!(check.proof, Some(host.clone()));
                    assert!(
                        matches!(check.request, ClientRequest::DeviceOperation { operation: crate::device_service::Operation::AccountCheck(actual) } if actual == expected)
                    );
                    let mut returned = report(&expected);
                    if outcome == 2 {
                        returned.expected_email = "wrong@example.test".into();
                    }
                    write_frame(
                        &mut peer,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id: Some(check.request_id),
                            proof: host,
                            message: ServerMessage::Response {
                                response: ServerResponse::DeviceOperation {
                                    reply: crate::device_service::Reply::AccountCheck(Box::new(
                                        returned,
                                    )),
                                },
                            },
                        },
                    )
                    .unwrap();
                }
                assert!(
                    read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                    "no retry, authentication fabrication or second dispatch"
                );
            });
            let mut client = crate::control::ControlClient::from_stream(stream).unwrap();
            let result = exchange(&mut client, &target(), &request);
            drop(client);
            worker.join().unwrap();
            assert_eq!(result.is_ok(), outcome == 0);
            if outcome == 1 {
                assert!(matches!(result, Err(Failure::DeviceChanged)));
            }
        }
    }
}
