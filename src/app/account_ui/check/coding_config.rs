//! Explicit creation of missing defaults, separate from capability observation.
//! Every filesystem operation runs on the selected daemon's blocking lane.
use super::*;
use crate::account::coding_config::{
    CreateReport, CreateRequest, InspectRequest, Observation, Outcome, Presence,
};

#[derive(Default)]
pub(super) struct State {
    next: u64,
    pending: Option<Pending>,
    draft: Option<Draft>,
    last: Option<Completion>,
}
impl State {
    pub(super) fn pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[derive(Clone)]
struct Context {
    form: u64,
    fence: Fence,
    target: Target,
    request: InspectRequest,
}
impl Context {
    fn capture(app: &Cagents) -> Option<Self> {
        let state = &app.account_ui.check;
        let form = state.form.as_ref()?;
        let report = form.report.as_ref()?;
        let target = form.target.as_ref()?;
        let key = form.fence.account.registry_key()?;
        if state.pending.is_some()
            || !form.fence.matches(&app.account_ui)
            || app.account_ui.registry.is_archived(key)
            || report.authentication.status != CheckStatus::Passed
            || !report.provider_cleanup_confirmed
            || !report.matches_request(&form.request().ok()?, &target.proof)
        {
            return None;
        }
        Some(Self {
            form: form.sequence,
            fence: form.fence.clone(),
            target: target.clone(),
            request: InspectRequest {
                account: form.fence.account.id(),
                expected_home: report.account_home_identity?,
                expected_email: report.expected_email.clone(),
            },
        })
    }

    fn current(&self, app: &Cagents) -> bool {
        Self::capture(app).is_some_and(|current| {
            current.form == self.form
                && current.request == self.request
                && current.target.proof == self.target.proof
                && current.target.socket == self.target.socket
                && current.target.authority == self.target.authority
                && self.fence.matches(&app.account_ui)
        })
    }

    fn accepts(&self, observation: &Observation) -> bool {
        observation.account == self.request.account
            && observation.home == self.request.expected_home
            && observation.expected_email == self.request.expected_email
            && observation.host == self.target.proof
    }
}
struct Draft {
    sequence: u64,
    context: Context,
    observation: Observation,
    network: bool,
}
enum Work {
    Inspect,
    Create(CreateRequest),
}
struct Pending {
    sequence: u64,
    context: Context,
    work: Work,
}
struct Completion {
    context: Context,
    text: String,
}
impl Completion {
    fn current(&self, app: &Cagents) -> bool {
        let Some(form) = app.account_ui.check.form.as_ref() else {
            return false;
        };
        let Some(target) = form.target.as_ref() else {
            return false;
        };
        if !self.context.fence.matches(&app.account_ui)
            || form.fence.account != self.context.fence.account
            || target.proof != self.context.target.proof
            || target.socket != self.context.target.socket
            || target.authority != self.context.target.authority
        {
            return false;
        }
        if form.sequence == self.context.form {
            return form.report.as_ref().is_none_or(|report| {
                report.account_home_identity == Some(self.context.request.expected_home)
            });
        }
        // A reopened form needs a fresh check of the same physical home.
        Context::capture(app).is_some_and(|current| current.request == self.context.request)
    }
}

#[derive(Debug, Clone)]
pub enum Action {
    Inspect(u64),
    Network(u64, bool),
    Create(u64),
    Dismiss(u64),
    Inspected {
        sequence: u64,
        result: Result<Observation, Failure>,
    },
    Created {
        sequence: u64,
        result: Result<CreateReport, Failure>,
    },
}
fn message(action: Action) -> Message {
    super::message(super::Action::CodingConfig(action))
}
fn inspect(context: Context) -> Result<Observation, Failure> {
    let mut client =
        super::super::connection::at(&context.target.socket, context.target.authority.clone())
            .map_err(|_| Failure::DeviceUnavailable)?;
    if client.proof() != &context.target.proof {
        return Err(Failure::DeviceChanged);
    }
    match client.device_operation(
        crate::device_service::Operation::AccountCodingConfigInspect(context.request.clone()),
    ) {
        Ok(crate::device_service::Reply::AccountCodingConfigInspect(observation))
            if context.accepts(&observation) =>
        {
            Ok(*observation)
        }
        Ok(_) => Err(Failure::InvalidResponse),
        Err(_) => Err(Failure::CheckUnavailable),
    }
}
fn create(context: Context, request: CreateRequest) -> Result<CreateReport, Failure> {
    let mut client =
        super::super::connection::at(&context.target.socket, context.target.authority.clone())
            .map_err(|_| Failure::DeviceUnavailable)?;
    if client.proof() != &context.target.proof {
        return Err(Failure::DeviceChanged);
    }
    match client.device_operation(crate::device_service::Operation::AccountCodingConfigCreate(
        request.clone(),
    )) {
        Ok(crate::device_service::Reply::AccountCodingConfigCreate(report))
            if report.account == request.observation.account
                && report.host == context.target.proof
                && report.home == request.observation.home
                && report.network_access == request.network_access =>
        {
            Ok(*report)
        }
        Ok(_) => Err(Failure::InvalidResponse),
        Err(_) => Err(Failure::CheckUnavailable),
    }
}

pub(super) fn update(app: &mut Cagents, action: Action) -> Task<Message> {
    match action {
        Action::Inspected { sequence, result } => {
            let state = &mut app.account_ui.check.coding_config;
            if !state.pending.as_ref().is_some_and(|pending| {
                pending.sequence == sequence && matches!(pending.work, Work::Inspect)
            }) {
                return Task::none();
            }
            let pending = state.pending.take().unwrap();
            if !pending.context.current(app) {
                return Task::none();
            }
            match result {
                Ok(observation) if pending.context.accepts(&observation) => {
                    app.account_ui.check.coding_config.draft = Some(Draft {
                        sequence,
                        context: pending.context,
                        observation,
                        network: false,
                    });
                }
                _ => {
                    app.account_ui.check.form.as_mut().unwrap().problem = Some(
                        "The device could not inspect this configuration; no creation was requested",
                    )
                }
            }
        }
        Action::Created { sequence, result } => {
            let state = &mut app.account_ui.check.coding_config;
            if !state.pending.as_ref().is_some_and(|pending| {
                pending.sequence == sequence && matches!(pending.work, Work::Create(_))
            }) {
                return Task::none();
            }
            let pending = state.pending.take().unwrap();
            let Work::Create(request) = pending.work else {
                unreachable!()
            };
            let created = result.as_ref().is_ok_and(|report| {
                report.account == request.observation.account
                    && report.host == pending.context.target.proof
                    && report.home == request.observation.home
                    && report.network_access == request.network_access
                    && report.outcome == Outcome::Created
            });
            let text = match &result {
                Ok(report)
                    if report.account == request.observation.account
                        && report.host == pending.context.target.proof
                        && report.home == request.observation.home
                        && report.network_access == request.network_access =>
                {
                    match report.outcome {
                        Outcome::Created => {
                            "Coding defaults created. Run Check again to measure actual access.".into()
                        }
                        Outcome::NotCreated(reason) => {
                            format!("Coding defaults were not created: {}.", reason.description())
                        }
                        Outcome::CreationUncertain(_) => {
                            "Configuration creation could not be fully confirmed. Inspect the account configuration; no retry was sent.".into()
                        }
                    }
                }
                _ => {
                    "The device did not confirm configuration creation. Inspect the account configuration before another request; no retry was sent.".into()
                }
            };
            // A mutation may finish after Close/reopen. Keep its receipt and
            // surface a notice without attaching it to a different form/report.
            app.account_ui.check.coding_config.last = Some(Completion {
                context: pending.context.clone(),
                text: text.clone(),
            });
            if pending.context.current(app) {
                let form = app.account_ui.check.form.as_mut().unwrap();
                form.report = None;
                form.problem = None;
            }
            let notice = format!("{}: {text}", pending.context.request.account);
            app.status = Some(if created {
                TransientNotice::success(notice)
            } else {
                TransientNotice::warning(notice)
            });
        }
        Action::Inspect(form) => {
            if app.account_ui.check.coding_config.pending() {
                return Task::none();
            }
            let Some(context) = Context::capture(app).filter(|context| context.form == form) else {
                return Task::none();
            };
            let state = &mut app.account_ui.check.coding_config;
            state.next = state.next.saturating_add(1);
            let sequence = state.next;
            state.draft = None;
            state.pending = Some(Pending {
                sequence,
                context: context.clone(),
                work: Work::Inspect,
            });
            return Task::perform(
                async move {
                    let result = tokio::task::spawn_blocking(move || inspect(context))
                        .await
                        .unwrap_or(Err(Failure::CheckUnavailable));
                    Action::Inspected { sequence, result }
                },
                |action| cosmic::Action::App(message(action)),
            );
        }
        Action::Network(sequence, value) => {
            if app.account_ui.check.coding_config.pending() {
                return Task::none();
            }
            let current = app
                .account_ui
                .check
                .coding_config
                .draft
                .as_ref()
                .is_some_and(|draft| draft.sequence == sequence && draft.context.current(app));
            if current && let Some(draft) = &mut app.account_ui.check.coding_config.draft {
                draft.network = value;
            }
        }
        Action::Create(sequence) => {
            if app.account_ui.check.coding_config.pending() {
                return Task::none();
            }
            let valid = app
                .account_ui
                .check
                .coding_config
                .draft
                .as_ref()
                .is_some_and(|draft| {
                    draft.sequence == sequence
                        && draft.context.current(app)
                        && draft.observation.presence == Presence::Absent
                        && draft.context.accepts(&draft.observation)
                });
            if !valid {
                return Task::none();
            }
            let state = &mut app.account_ui.check.coding_config;
            let draft = state.draft.take().unwrap();
            let request = CreateRequest {
                observation: draft.observation,
                network_access: draft.network,
            };
            state.next = state.next.saturating_add(1);
            let sequence = state.next;
            let context = draft.context;
            state.pending = Some(Pending {
                sequence,
                context: context.clone(),
                work: Work::Create(request.clone()),
            });
            return Task::perform(
                async move {
                    let result = tokio::task::spawn_blocking(move || create(context, request))
                        .await
                        .unwrap_or(Err(Failure::CheckUnavailable));
                    Action::Created { sequence, result }
                },
                |action| cosmic::Action::App(message(action)),
            );
        }
        Action::Dismiss(sequence) => {
            if app
                .account_ui
                .check
                .coding_config
                .draft
                .as_ref()
                .is_some_and(|draft| draft.sequence == sequence)
            {
                app.account_ui.check.coding_config.draft = None;
            }
        }
    }
    Task::none()
}

pub(super) fn view(app: &Cagents) -> Option<Element<'_, Message>> {
    let state = &app.account_ui.check.coding_config;
    let form = app.account_ui.check.form.as_ref()?;
    form.target.as_ref()?;
    let mut rows: Vec<Element<'_, Message>> = Vec::new();
    if let Some(last) = &state.last
        && last.current(app)
    {
        rows.push(
            design::caption(last.text.as_str())
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    }
    if let Some(pending) = &state.pending {
        let text = if matches!(pending.work, Work::Create(_)) {
            "A configuration request is finishing. Closing this form leaves it running; a notice will report its result."
        } else {
            "Inspecting the selected account configuration…"
        };
        rows.push(
            design::caption(text)
                .wrapping(iced::widget::text::Wrapping::Word)
                .into(),
        );
    } else if let Some(draft) = state
        .draft
        .as_ref()
        .filter(|draft| draft.context.current(app))
    {
        let sequence = draft.sequence;
        match draft.observation.presence {
            Presence::Absent => {
                rows.push(design::heading("Create missing coding defaults").into());
                rows.push(design::body("Create an independent config.toml for this account: workspace write with on-request approvals. Existing configuration is preserved. This takes effect in new Codex processes; run Check again afterward.")
                    .wrapping(iced::widget::text::Wrapping::Word).into());
                rows.push(crate::ui_controls::field::checkbox(draft.network)
                    .label("Allow network access from the workspace sandbox")
                    .on_toggle(move |value| message(Action::Network(sequence, value))).into());
                rows.push(widget::row::with_children(vec![
                    crate::ui_controls::button::secondary("Cancel setup").on_press(message(Action::Dismiss(sequence))).into(),
                    crate::ui_controls::button::primary("Create coding defaults").on_press(message(Action::Create(sequence))).into(),
                ]).spacing(space::S).into());
            }
            Presence::Present => rows.push(design::caption("This account already has a configuration entry. Review it with the provider's settings, or choose Permissions for a new conversation.").wrapping(iced::widget::text::Wrapping::Word).into()),
            Presence::Unknown => rows.push(design::caption("Configuration absence could not be confirmed. No creation is available.").wrapping(iced::widget::text::Wrapping::Word).into()),
        }
    } else if let Some(context) = Context::capture(app) {
        rows.push(
            crate::ui_controls::button::secondary("Inspect coding defaults")
                .on_press(message(Action::Inspect(context.form)))
                .into(),
        );
    }
    (!rows.is_empty()).then(|| widget::column::with_children(rows).spacing(space::S).into())
}

#[cfg(test)]
mod tests {
    use super::super::tests::{apply, click, fixture, open, report};
    use super::*;
    use crate::account::{coding_config::Uncertainty, readiness::PhysicalIdentity};

    fn ready() -> (Cagents, AccountId, AccountId) {
        let (mut app, first, second) = fixture();
        open(&mut app, &first);
        let form = app.account_ui.check.form.as_mut().unwrap();
        form.email = "fixture@example.test".into();
        let mut report = report(&form.request().unwrap());
        report.account_home_identity = Some(PhysicalIdentity {
            device: 19,
            inode: 1550,
        });
        report.provider_cleanup_confirmed = true;
        form.report = Some(report);
        assert!(Context::capture(&app).is_some());
        (app, first, second)
    }
    fn observed(app: &mut Cagents, presence: Presence) {
        let messages = click(view(app).unwrap(), "Inspect coding defaults");
        apply(app, messages);
        let pending = app.account_ui.check.coding_config.pending.as_ref().unwrap();
        let result = Observation {
            account: pending.context.request.account.clone(),
            host: pending.context.target.proof.clone(),
            home: pending.context.request.expected_home,
            presence,
            expected_email: pending.context.request.expected_email.clone(),
        };
        let sequence = pending.sequence;
        drop(update(
            app,
            Action::Inspected {
                sequence,
                result: Ok(result),
            },
        ));
    }
    fn dispatch_creation(app: &mut Cagents) -> (u64, CreateRequest) {
        let messages = click(view(app).unwrap(), "Create coding defaults");
        apply(app, messages);
        let pending = app.account_ui.check.coding_config.pending.as_ref().unwrap();
        let Work::Create(request) = &pending.work else {
            panic!("missing real creation intent")
        };
        (pending.sequence, request.clone())
    }

    #[test]
    fn coding_defaults_context_and_completion_reject_changed_connection_scope() {
        let (mut app, _, _) = ready();
        let context = Context::capture(&app).unwrap();
        let completion = Completion {
            context: context.clone(),
            text: "fixture report".into(),
        };
        assert!(context.current(&app));
        assert!(completion.current(&app));
        app.account_ui
            .check
            .form
            .as_mut()
            .unwrap()
            .target
            .as_mut()
            .unwrap()
            .authority =
            crate::agent_execution::Authority::session("different-fixture-scope".into());
        assert!(!context.current(&app));
        assert!(!completion.current(&app));
    }

    #[test]
    fn coding_defaults_actual_controls_require_authenticated_absence_and_default_to_offline() {
        let (mut app, _, _) = ready();
        observed(&mut app, Presence::Absent);
        assert!(
            !app.account_ui
                .check
                .coding_config
                .draft
                .as_ref()
                .unwrap()
                .network
        );
        let (_, request) = dispatch_creation(&mut app);
        assert!(!request.network_access);
        assert_eq!(request.observation.presence, Presence::Absent);
        let sequence = app.account_ui.check.coding_config.next;
        drop(update(&mut app, Action::Create(1)));
        assert_eq!(app.account_ui.check.coding_config.next, sequence);
    }

    #[test]
    fn coding_defaults_authentication_and_cleanup_are_required_without_inferring_absence_from_policy()
     {
        let (mut app, _, _) = ready();
        assert!(app.account_ui.check.coding_config.draft.is_none());
        app.account_ui
            .check
            .form
            .as_mut()
            .unwrap()
            .report
            .as_mut()
            .unwrap()
            .provider_cleanup_confirmed = false;
        assert!(Context::capture(&app).is_none());
        app.account_ui
            .check
            .form
            .as_mut()
            .unwrap()
            .report
            .as_mut()
            .unwrap()
            .provider_cleanup_confirmed = true;
        app.account_ui
            .check
            .form
            .as_mut()
            .unwrap()
            .report
            .as_mut()
            .unwrap()
            .authentication
            .status = CheckStatus::Failed;
        assert!(Context::capture(&app).is_none());
        assert!(view(&app).is_none());
    }

    #[test]
    fn coding_defaults_present_and_unknown_entries_never_dispatch_creation() {
        for presence in [Presence::Present, Presence::Unknown] {
            let (mut app, _, _) = ready();
            observed(&mut app, presence);
            let sequence = app.account_ui.check.form.as_ref().unwrap().sequence;
            drop(update(&mut app, Action::Create(sequence)));
            assert!(!app.account_ui.check.coding_config.pending());
            assert!(view(&app).is_some());
        }
    }

    #[test]
    fn coding_defaults_network_choice_is_explicit_and_stale_form_actions_are_ignored() {
        let (mut app, _, _) = ready();
        observed(&mut app, Presence::Absent);
        let form = app.account_ui.check.form.as_ref().unwrap().sequence;
        drop(update(
            &mut app,
            Action::Network(form.saturating_add(1), true),
        ));
        assert!(
            !app.account_ui
                .check
                .coding_config
                .draft
                .as_ref()
                .unwrap()
                .network
        );
        let messages = click(
            view(&app).unwrap(),
            "Allow network access from the workspace sandbox",
        );
        apply(&mut app, messages);
        assert!(dispatch_creation(&mut app).1.network_access);
    }

    #[test]
    fn coding_defaults_pending_mutation_blocks_check_and_keeps_receipt_after_close_reopen() {
        let (mut app, first, second) = ready();
        observed(&mut app, Presence::Absent);
        let (sequence, request) = dispatch_creation(&mut app);
        let form = app.account_ui.check.form.as_ref().unwrap().sequence;
        drop(app.update_account_check(super::super::Action::Run(form)));
        assert!(app.account_ui.check.pending.is_none());
        drop(app.update_account_check(super::super::Action::Close(form)));
        assert!(app.account_ui.check.coding_config.pending());
        open(&mut app, &second);
        let current_form = app.account_ui.check.form.as_ref().unwrap().sequence;
        let result = CreateReport {
            account: first,
            host: request.observation.host.clone(),
            home: request.observation.home,
            network_access: request.network_access,
            outcome: Outcome::CreationUncertain(Uncertainty::DirectorySyncFailed),
        };
        drop(update(
            &mut app,
            Action::Created {
                sequence,
                result: Ok(result),
            },
        ));
        assert!(!app.account_ui.check.coding_config.pending());
        assert!(
            app.account_ui
                .check
                .coding_config
                .last
                .as_ref()
                .unwrap()
                .text
                .contains("could not be fully confirmed")
        );
        assert_eq!(
            app.account_ui.check.form.as_ref().unwrap().sequence,
            current_form
        );
        assert_eq!(
            app.account_ui
                .check
                .form
                .as_ref()
                .unwrap()
                .fence
                .account
                .id(),
            second
        );
        assert!(
            view(&app).is_none(),
            "another account must not inherit the receipt"
        );
        assert!(app.status.is_some());
    }

    #[test]
    fn coding_defaults_invalid_receipt_is_uncertain_and_never_creates_an_automatic_retry() {
        let (mut app, _, second) = ready();
        observed(&mut app, Presence::Absent);
        let (sequence, request) = dispatch_creation(&mut app);
        let result = CreateReport {
            account: second,
            host: request.observation.host,
            home: request.observation.home,
            network_access: request.network_access,
            outcome: Outcome::Created,
        };
        drop(update(
            &mut app,
            Action::Created {
                sequence,
                result: Ok(result),
            },
        ));
        let state = &app.account_ui.check.coding_config;
        assert!(!state.pending());
        assert!(state.draft.is_none());
        assert!(
            state
                .last
                .as_ref()
                .unwrap()
                .text
                .starts_with("The device did not confirm")
        );
        assert!(app.account_ui.check.form.as_ref().unwrap().report.is_none());
        let next = state.next;
        drop(update(
            &mut app,
            Action::Created {
                sequence,
                result: Err(Failure::CheckUnavailable),
            },
        ));
        assert_eq!(app.account_ui.check.coding_config.next, next);
    }

    #[test]
    fn coding_defaults_definite_refusal_keeps_its_actionable_reason_separate_from_uncertainty() {
        use crate::account::coding_config::Refusal;

        let (mut app, first, _) = ready();
        observed(&mut app, Presence::Absent);
        let (sequence, request) = dispatch_creation(&mut app);
        let result = CreateReport {
            account: first,
            host: request.observation.host,
            home: request.observation.home,
            network_access: request.network_access,
            outcome: Outcome::NotCreated(Refusal::RegistryBusy),
        };
        drop(update(
            &mut app,
            Action::Created {
                sequence,
                result: Ok(result),
            },
        ));
        let state = &app.account_ui.check.coding_config;
        assert!(!state.pending());
        assert!(state.draft.is_none());
        let text = &state.last.as_ref().unwrap().text;
        assert!(text.starts_with("Coding defaults were not created:"));
        assert!(text.contains(Refusal::RegistryBusy.description()));
        assert!(!text.contains("did not confirm"));
    }

    #[test]
    fn coding_defaults_cancel_and_reinspect_rejects_callbacks_from_the_previous_confirmation() {
        let (mut app, _, _) = ready();
        observed(&mut app, Presence::Absent);
        let stale_create = click(view(&app).unwrap(), "Create coding defaults");
        let stale_network = click(
            view(&app).unwrap(),
            "Allow network access from the workspace sandbox",
        );
        let stale_cancel = click(view(&app).unwrap(), "Cancel setup");
        let cancel = click(view(&app).unwrap(), "Cancel setup");
        apply(&mut app, cancel);
        assert!(app.account_ui.check.coding_config.draft.is_none());
        observed(&mut app, Presence::Absent);
        let sequence = app
            .account_ui
            .check
            .coding_config
            .draft
            .as_ref()
            .unwrap()
            .sequence;
        let next = app.account_ui.check.coding_config.next;
        apply(&mut app, stale_network);
        apply(&mut app, stale_create);
        apply(&mut app, stale_cancel);
        let state = &app.account_ui.check.coding_config;
        let draft = state.draft.as_ref().unwrap();
        assert_eq!(draft.sequence, sequence);
        assert!(!draft.network);
        assert!(!state.pending());
        assert_eq!(state.next, next);
    }

    #[test]
    fn coding_defaults_receipt_does_not_follow_account_id_to_a_replaced_physical_home() {
        let (mut app, first, _) = ready();
        observed(&mut app, Presence::Absent);
        let (sequence, request) = dispatch_creation(&mut app);
        let result = CreateReport {
            account: first.clone(),
            host: request.observation.host,
            home: request.observation.home,
            network_access: request.network_access,
            outcome: Outcome::Created,
        };
        drop(update(
            &mut app,
            Action::Created {
                sequence,
                result: Ok(result),
            },
        ));
        assert!(
            app.account_ui
                .check
                .coding_config
                .last
                .as_ref()
                .unwrap()
                .current(&app)
        );
        open(&mut app, &first);
        assert!(
            !app.account_ui
                .check
                .coding_config
                .last
                .as_ref()
                .unwrap()
                .current(&app)
        );
        let form = app.account_ui.check.form.as_mut().unwrap();
        form.email = "fixture@example.test".into();
        let mut report = report(&form.request().unwrap());
        report.provider_cleanup_confirmed = true;
        report.account_home_identity = Some(PhysicalIdentity {
            device: 19,
            inode: 1551,
        });
        form.report = Some(report);
        assert!(Context::capture(&app).is_some());
        assert!(
            !app.account_ui
                .check
                .coding_config
                .last
                .as_ref()
                .unwrap()
                .current(&app)
        );
        app.account_ui
            .check
            .form
            .as_mut()
            .unwrap()
            .report
            .as_mut()
            .unwrap()
            .account_home_identity = Some(request.observation.home);
        assert!(
            app.account_ui
                .check
                .coding_config
                .last
                .as_ref()
                .unwrap()
                .current(&app)
        );
    }
}

#[cfg(test)]
mod framed_tests;
