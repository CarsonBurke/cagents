//! Explicit switching keeps the original runtime until the normal durable rebind.
use super::*;

impl Runtime {
    pub(super) fn clear_account_switch_wait(&mut self) {
        self.pending_account_switch = None;
        self.account_switch_checking = false;
        self.account_switch_recheck = false;
        self.account_switch_observed = false;
        self.account_switch_checked_observation = None;
        self.account_switch_query = self.account_switch_query.wrapping_add(1);
        self.account_switch_choice = self.account_switch_choice.wrapping_add(1);
    }
}

impl Cagents {
    pub(super) fn probe_queued_account_switch(&mut self, target: Target) -> Task<Message> {
        if self.chat_ui.phases.contains_key(&target.card)
            || !matches!(
                self.card_state(target.card).map(|card| &card.kind),
                Some(CardKind::Chat(_))
            )
        {
            return Task::none();
        }
        let waiting = match self.cards.get(&target.card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.as_ref()),
            _ => self.chat_ui.parked.get(&target.card).map(Box::as_ref),
        };
        if waiting.is_none_or(|runtime| {
            runtime.target != target || runtime.pending_account_switch.is_none()
        }) {
            return Task::none();
        }
        let canonical = self
            .card_state(target.card)
            .filter(|card| card.existence.is_open())
            .and_then(|card| match &card.kind {
                CardKind::Chat(session) => Some(session.clone()),
                _ => None,
            });
        let runtime = match self.cards.get_mut(&target.card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime),
            _ => self.chat_ui.parked.get_mut(&target.card),
        };
        let Some(runtime) = runtime.filter(|runtime| runtime.target == target) else {
            return Task::none();
        };
        if canonical.as_ref() != Some(&runtime.session) {
            return Task::none();
        }
        if runtime.pending.is_some()
            || !runtime.followups.is_empty()
            || runtime.pastes_pending > 0
            || runtime.attachment_chooser_pending.is_some()
        {
            return Task::none();
        }
        let Some((id, _)) = &runtime.pending_account_switch else {
            runtime.account_switch_recheck = false;
            return Task::none();
        };
        let observation = runtime
            .handle()
            .and_then(|handle| handle.account_switch_readiness_observation());
        if runtime.account_switch_checking {
            // A successful old busy-read may arrive after newer readiness
            // metadata. Preserve one query hint, never close authority.
            if observation.is_some() && observation != runtime.account_switch_checked_observation {
                runtime.account_switch_recheck = true;
            }
            return Task::none();
        }
        if runtime.account_switch_observed
            && observation == runtime.account_switch_checked_observation
        {
            return Task::none();
        }
        let id = id.clone();
        let source = runtime.session.clone();
        runtime.account_switch_observed = true;
        runtime.account_switch_checked_observation = observation;
        runtime.account_switch_recheck = false;
        runtime.account_switch_checking = true;
        runtime.account_switch_query = runtime.account_switch_query.wrapping_add(1);
        let query = runtime.account_switch_query;
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let socket =
                    crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string())?;
                crate::control::ControlClient::connect(&socket)
                    .map_err(|error| error.to_string())?
                    .account_switch_generation(source)
                    .map_err(|error| error.to_string())
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ChatUi(Action::QueuedAccountGeneration(target, id, query, result))
        })
    }

    pub(super) fn continue_on_account(&mut self, target: Target, id: AccountId) -> Task<Message> {
        if self.chat_start_pending(target.card) {
            return Task::none();
        }
        let Some(account) = self.known_account(&id) else {
            return Task::none();
        };
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime
            .account_switch_fallback
            .as_ref()
            .map(|(account, _)| account)
            != Some(&id)
        {
            return Task::none();
        }
        if account.provider != runtime.session.launch.provider
            || runtime.attachment_chooser_pending.is_some()
            || runtime.pastes_pending > 0
        {
            return Task::none();
        }
        // This creates an independent conversation. Ongoing source work and
        // unconfirmed commands keep their exact original owner and may continue.
        let excerpt = continuation_excerpt(&runtime.snapshot);
        let seed = format!(
            "Continue this work in a new conversation. Exact provider history is not transferred to this new conversation. The following is a bounded recent transcript excerpt; refer to the original conversation for omitted history.\n\n{}\n\nUnsent request:\n{}",
            excerpt,
            runtime.unsent_text()
        );
        let attachments = runtime.unsent_attachments().to_vec();
        let directory = runtime.session.launch.recipe.working_directory.clone();
        let mut fresh = match fresh_chat_session(
            account.provider,
            TerminalId::new(),
            directory,
            &account,
            false,
        ) {
            Ok(session) => session,
            Err(error) => {
                runtime.error = Some(error);
                return Task::none();
            }
        };
        fresh
            .launch
            .recipe
            .environment
            .remove(account::routing::AUTOMATIC_MARKER);
        let fresh_id = fresh.id;
        let insert = self.insert_chat_card(fresh, Some(target.card), None);
        if let Some(card) = self.workspace.focused()
            && let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card)
            && runtime.session.id == fresh_id
        {
            runtime.replace_draft(text_editor::Content::with_text(&seed));
            runtime.replace_attachments(attachments);
        }
        insert
    }
}

/// Counts the actual rendered excerpt, including labels and separators. The
/// item cap also bounds work when the source contains many empty text records.
pub(super) fn continuation_excerpt(snapshot: &Snapshot) -> String {
    const LIMIT: usize = 64 * 1024;
    let mut parts = Vec::new();
    let mut bytes = 0;
    for item in snapshot.items.iter().rev().take(128) {
        let ItemContent::Text {
            text,
            thinking: false,
            ..
        } = &item.content
        else {
            continue;
        };
        let prefix = format!("{:?}: ", item.role);
        let overhead = prefix.len() + if parts.is_empty() { 0 } else { 2 };
        let Some(available) = LIMIT.checked_sub(bytes + overhead) else {
            break;
        };
        let mut start = text.len().saturating_sub(available);
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let part = format!("{prefix}{}", &text[start..]);
        bytes += part.len() + if parts.is_empty() { 0 } else { 2 };
        parts.push(part);
        if bytes == LIMIT {
            break;
        }
    }
    parts.reverse();
    parts.join("\n\n")
}
