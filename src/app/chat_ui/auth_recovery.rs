//! Recovery belongs to the account this conversation actually launched with.
use super::*;

pub(super) struct Recovery {
    account: Account,
    label: String,
    signing_in: bool,
    login_change: Option<String>,
}
impl Runtime {
    pub(super) fn auth_error(&self) -> Option<&str> {
        self.snapshot.error
            .as_deref()
            .into_iter()
            .chain(self.error.as_deref())
            .find(|error| chat::requires_sign_in(self.session.launch.provider, error))
    }
    pub(super) fn auth_verification_detail(&self) -> Option<&str> {
        self.auth_error().and_then(chat::sign_in_verification_detail)
    }
    pub(super) fn active_auth_notice(&self, anchor: &Option<String>, message: &str) -> bool {
        self.auth_error().is_some()
            && anchor.as_deref() == self.turns.last().map(|turn| turn.key.as_str())
            && chat::requires_sign_in(self.session.launch.provider, message)
    }
    fn recovery_available(&self) -> bool {
        self.auth_error().is_some()
            && self.pending.is_none()
            && self.queued.is_none()
            && !self.snapshot.busy
            && !self.attachment_chooser_pending
    }
}
impl Cagents {
    fn auth_account(&self, runtime: &Runtime) -> Option<Account> {
        let launch = &runtime.session.launch;
        let recipe = &launch.recipe;
        let variable = account::home_variable(launch.provider)?;
        // ProcessSpec removes explicitly unset bindings before spawning.
        // A recipe with no bound provider home inherits HOME; it must name
        // the same home as this window's known system account.
        if recipe.unset_environment.contains(variable) {
            return None;
        }
        match recipe.environment.get(variable) {
            Some(home) if home.is_empty() => return None,
            Some(_) => {}
            None => {
                if recipe.unset_environment.contains("HOME")
                    || recipe.environment.get("HOME").is_some_and(|home| {
                        std::env::var_os("HOME")
                            .is_none_or(|inherited| Path::new(home) != Path::new(&inherited))
                    })
                {
                    return None;
                }
            }
        }
        let id = self.chat_account(launch.provider, recipe)?;
        let account = self.known_account(&id)?;
        account.resolved_home()?;
        Some(account)
    }
    pub(super) fn chat_auth_recovery(&self, runtime: &Runtime) -> Option<Recovery> {
        runtime.auth_error()?;
        let account = self.auth_account(runtime)?;
        Some(Recovery {
            label: self.resume_account_label(&runtime.session),
            signing_in: self.account_sign_in_running(&account),
            login_change: self.account_login_change(&account),
            account,
        })
    }
    pub(super) fn chat_auth_recovery_enabled(&self, target: Target) -> bool {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) else {
            return false;
        };
        runtime.target == target
            && runtime.recovery_available()
            && !self.chat_start_pending(target.card)
            && self
                .auth_account(runtime)
                .is_some_and(|account| !self.account_sign_in_running(&account))
    }
    pub(super) fn chat_auth_resume_enabled(&self, target: Target) -> bool {
        self.chat_auth_recovery_enabled(target)
            && matches!(self.cards.get(&target.card), Some(CardRuntime::Chat(runtime))
                if self.auth_account(runtime).is_some_and(|account| self.account_login_change(&account).is_none()))
    }
    pub(super) fn resume_after_sign_in(&mut self, target: Target) -> Task<Message> {
        if !self.chat_auth_resume_enabled(target) {
            return Task::none();
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) else {
            return Task::none();
        };
        if runtime.disconnected || runtime.snapshot.exited {
            return self.handle_chat_ui(Action::Reconnect(target));
        }
        let Some(failure) = runtime.auth_error().map(str::to_owned) else {
            return Task::none();
        };
        let Some(account) = self.auth_account(runtime) else {
            return Task::none();
        };
        let expected = self.expected_account_login(&account);
        self.chat_send(
            target,
            Command::ResumeAfterLogin { failure, expected },
            false,
        )
    }
    pub(super) fn sign_in_chat(&mut self, target: Target, account: Account) -> Task<Message> {
        if !self.chat_auth_recovery_enabled(target) {
            return Task::none();
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) else {
            return Task::none();
        };
        if self.auth_account(runtime).as_ref() != Some(&account)
            || self.known_account(&account.id()).as_ref() != Some(&account)
        {
            return Task::none();
        }
        self.update_account_ui(account_ui::Action::SignIn(account.id()))
    }
}
pub(super) fn view<'a>(
    runtime: &Runtime,
    recovery: Option<Recovery>,
    enabled: bool,
) -> Element<'a, Message> {
    let target = runtime.target;
    let mut controls = Vec::new();
    let known = recovery.is_some();
    let mut login_change = None;
    let label = if let Some(recovery) = recovery {
        let enabled = enabled && runtime.recovery_available() && !recovery.signing_in;
        let resume_enabled = enabled && recovery.login_change.is_none();
        login_change = recovery.login_change;
        controls.push(
            crate::ui_controls::button::primary(if recovery.signing_in {
                "Signing in…"
            } else {
                "Sign in"
            })
            .on_press_maybe(enabled.then_some(action(Action::SignIn(target, recovery.account))))
            .into(),
        );
        controls.push(
            crate::ui_controls::button::secondary("Resume chat")
                .on_press_maybe(resume_enabled.then_some(action(Action::ResumeAfterSignIn(target))))
                .into(),
        );
        controls.push(
            crate::ui_controls::button::secondary("Open accounts")
                .on_press(Message::AccountUi(account_ui::Action::OpenPane))
                .into(),
        );
        format!(
            "{} sign-in is no longer valid · {}",
            runtime.session.launch.provider.label(),
            recovery.label
        )
    } else {
        controls.push(
            crate::ui_controls::button::secondary("Open accounts")
                .on_press(Message::AccountUi(account_ui::Action::OpenPane))
                .into(),
        );
        "Sign-in required · Unregistered session account".into()
    };
    let mut rows = vec![
        design::body(label).into(),
        design::caption(if known { "Sign in, then resume this conversation. Your draft is retained." } else { "Open accounts to add or link this session’s account, then return here to sign in and resume. Your draft is retained." }).into(),
        widget::row::with_children(controls)
            .spacing(step::GAP_S)
            .wrap()
            .into(),
    ];
    if let Some(detail) = runtime.auth_verification_detail() {
        rows.insert(
            1,
            design::caption(format!("Sign-in verification: {detail}")).into(),
        );
    }
    if let Some(detail) = login_change {
        rows.insert(1, design::caption(format!("{detail} Open accounts to explicitly accept that login, or sign in to the expected account before resuming.")).into());
    }
    widget::column::with_children(rows)
        .spacing(step::GAP_XS)
        .into()
}
