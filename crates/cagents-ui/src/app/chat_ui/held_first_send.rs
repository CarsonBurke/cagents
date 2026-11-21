//! A new chat's first message waits for its channel or project to load into
//! the shared workspace (#2697).
//!
//! The daemon starts a shared pane's first message only once the registry
//! lists the pane, and a scope that is still seeding or held back has not
//! published it. Rather than asking the daemon to start a pane it cannot
//! find, the accepted message waits on the transcript where it will appear,
//! and the reconcile that admits its scope starts it. A scope the registry
//! refused returns the message to the composer with a Retry.
use super::*;
use crate::workspace_model::views::Scope;

/// The start a held first message makes once its scope is admitted.
pub(super) struct Hold {
    start: crate::terminal_ipc::PaneChatStart,
    /// The channel or project the message was sent in. It starts there or
    /// not at all.
    scope: Scope,
}

impl Hold {
    pub(super) fn waiting(&self) -> &'static str {
        match self.scope {
            Scope::Channel(_) => "Waiting for this channel to load…",
            Scope::Worktree { .. } => "Waiting for this project to load…",
        }
    }
}

fn refusal(scope: &Scope) -> &'static str {
    match scope {
        Scope::Channel(_) => {
            "This channel couldn't load, so your message wasn't sent. It's kept here; Retry loads the channel again and sends it."
        }
        Scope::Worktree { .. } => {
            "This project couldn't load, so your message wasn't sent. It's kept here; Retry loads the project again and sends it."
        }
    }
}

impl Cagents {
    /// Whether `card`'s first message waits for its scope's admission.
    pub(in crate::app) fn first_send_held(&self, card: CardId) -> bool {
        matches!(
            self.cards.get(&card),
            Some(CardRuntime::Chat(runtime))
                if runtime.queued.as_ref().is_some_and(|queued| queued.admission.is_some())
        )
    }

    /// Holds an accepted first message, already queued on its runtime, until
    /// its scope is admitted. Nothing is asked of the daemon meanwhile.
    pub(super) fn hold_first_send(
        &mut self,
        target: Target,
        start: crate::terminal_ipc::PaneChatStart,
        scope: Scope,
    ) -> Task<Message> {
        let Some(queued) = self
            .chat_card_runtime_mut(target)
            .and_then(|runtime| runtime.queued.as_mut())
        else {
            return Task::none();
        };
        queued.admission = Some(Hold { start, scope });
        self.chat_ui.admission_holds.insert(target.card);
        Task::none()
    }

    /// Returns a first message to its composer because the registry refused
    /// its scope. Nothing was started for it.
    pub(super) fn refuse_first_send(&mut self, target: Target, scope: &Scope) -> Task<Message> {
        self.chat_ui.admission_holds.remove(&target.card);
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        runtime.unqueue();
        runtime.view_refused = Some(scope.clone());
        runtime.error = Some(refusal(scope).into());
        let shown = runtime.session.clone();
        self.chat_ui.view_refusals.insert(target.card);
        Task::batch([
            self.adopt_stored_draft_session(target.card),
            self.restore_failed_draft_start_focus(target, &shown),
        ])
    }

    /// Starts each held first message whose scope this reconcile admitted,
    /// after the reconcile queued its pane's Insert; returns to the composer
    /// one whose scope was refused, or whose pane was closed or refused
    /// elsewhere. The daemon still retries a start that reaches it before the
    /// Insert does, so ordering between the two connections is not needed.
    pub(in crate::app) fn release_admitted_first_sends(&mut self) -> Task<Message> {
        self.withdraw_settled_view_refusals();
        if self.chat_ui.admission_holds.is_empty() {
            return Task::none();
        }
        let mut tasks = Vec::new();
        for card in self.chat_ui.admission_holds.clone() {
            let held = match self.cards.get(&card) {
                Some(CardRuntime::Chat(runtime)) if runtime.is_draft() => runtime
                    .queued
                    .as_ref()
                    .and_then(|queued| queued.admission.as_ref())
                    .map(|hold| (runtime.target, hold.start.registry, hold.scope.clone())),
                _ => None,
            };
            // Taken back, closed or replaced since: nothing to start.
            let Some((target, registry, held_scope)) = held else {
                self.chat_ui.admission_holds.remove(&card);
                continue;
            };
            // A message sent while the registry was still connecting was
            // bound to none, and starts in whichever registry this window
            // then joined; one bound to a registry this window has since left
            // must not start in another. Nor does one whose pane has left the
            // channel or project it was sent in start in another.
            let identity = self.workspace_model.identity;
            let Some(scope) = self.draft_start_scope(card).filter(|scope| {
                *scope == held_scope
                    && (registry == identity || registry == crate::workspace_model::NO_REGISTRY)
            }) else {
                tasks.push(self.unhold_first_send(
                    target,
                    "The workspace changed before this message was sent. Nothing was started; your message is kept, so send it again when ready.",
                ));
                continue;
            };
            // No session starts in a project or worktree being removed, as
            // none starts for a first message sent there. Held, the message
            // goes with its pane, or starts once a removal that fails ends.
            if self.deleting_card_reason(card).is_some() {
                continue;
            }
            // The reconcile publishes a scope's new panes; this only covers a
            // pane whose scope was admitted without its projection changing.
            if !self.shared_views.registry_unavailable
                && !self.shared_views.attached.contains(&card)
            {
                self.publish_new_managed_pane(scope.clone(), card);
            }
            match self.first_send_admission(card, &scope) {
                super::super::shared_views::FirstSendAdmission::Waiting => continue,
                super::super::shared_views::FirstSendAdmission::Refused => {
                    tasks.push(self.refuse_first_send(target, &scope));
                    continue;
                }
                super::super::shared_views::FirstSendAdmission::Admitted => {}
            }
            if self.shared_chat_creation_revoked(card, Some(&scope)) {
                tasks.push(self.unhold_first_send(
                    target,
                    "This pane was closed or its creation was refused. Nothing was started. Your draft is retained; copy it before closing this pane.",
                ));
                continue;
            }
            self.chat_ui.admission_holds.remove(&card);
            let Some((session, mut start)) =
                self.chat_card_runtime_mut(target).and_then(|runtime| {
                    let queued = runtime.queued.as_mut()?;
                    Some((queued.draft.clone(), queued.admission.take()?.start))
                })
            else {
                continue;
            };
            start.registry = identity;
            // A pane whose publication the registry refused while it waited
            // is this window's alone now, and starts here.
            start.allow_local |= self.durable.local_only_cards.contains(&card);
            tasks.push(self.start_chat_with_upgrade(
                card,
                session,
                chat::Intent::Start,
                None,
                false,
                None,
                Some(start),
            ));
        }
        Task::batch(tasks)
    }

    /// Returns a held first message to its composer with `error`.
    fn unhold_first_send(&mut self, target: Target, error: &str) -> Task<Message> {
        self.chat_ui.admission_holds.remove(&target.card);
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        runtime.unqueue();
        runtime.error = Some(error.into());
        let shown = runtime.session.clone();
        Task::batch([
            self.adopt_stored_draft_session(target.card),
            self.restore_failed_draft_start_focus(target, &shown),
        ])
    }

    /// Offers the refused scope to the shared workspace again and sends the
    /// message that came back. Its start then waits for the scope as any
    /// first message does, so a second refusal returns it here again.
    pub(super) fn retry_refused_view_send(&mut self, target: Target) -> Task<Message> {
        let Some(runtime) = self.chat_card_runtime_mut(target) else {
            return Task::none();
        };
        let Some(scope) = runtime.view_refused.take() else {
            return Task::none();
        };
        runtime.error = None;
        self.chat_ui.view_refusals.remove(&target.card);
        self.retry_view_seed(&scope);
        self.handle_chat_ui(Action::Submit(target, PromptMode::Send))
    }

    /// Takes the refusal notice and its Retry off each pane whose channel or
    /// project has loaded since, as by the workspace notice's Retry. The
    /// message stays in the composer for the user to send.
    fn withdraw_settled_view_refusals(&mut self) {
        if self.chat_ui.view_refusals.is_empty() {
            return;
        }
        for card in self.chat_ui.view_refusals.clone() {
            let scope = match self.cards.get(&card) {
                Some(CardRuntime::Chat(runtime)) => runtime.view_refused.clone(),
                _ => None,
            };
            let Some(scope) = scope else {
                self.chat_ui.view_refusals.remove(&card);
                continue;
            };
            // A pane moved to another channel since is no longer refused:
            // Retry would load the one it left.
            if self.draft_start_scope(card).as_ref() == Some(&scope)
                && !self.view_seed_loaded(&scope)
            {
                continue;
            }
            self.chat_ui.view_refusals.remove(&card);
            if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
                runtime.view_refused = None;
                if runtime.error.as_deref() == Some(refusal(&scope)) {
                    runtime.error = None;
                }
            }
        }
    }
}
