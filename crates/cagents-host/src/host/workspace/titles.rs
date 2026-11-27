//! Publishes titles from the current primary chat, even with no open window.
use super::{apply_title, unavailable, writer};
use crate::{
    host::{service, state::SharedHost},
    model::{CardKind, ChannelId, ChannelTitleSource, TerminalId},
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
    workspace_model::{ModelEdit, views::Scope},
};
use ChannelTitleSource::{Manual, Prompt, Provider};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::host) enum Target {
    Chat(TerminalId),
    Channel(ChannelId),
}

pub(in crate::host) struct Updates {
    pending: Arc<Mutex<BTreeSet<Target>>>,
    wake: crossbeam_channel::Sender<()>,
}

impl Updates {
    pub(in crate::host) fn offer(&self, target: Target) {
        self.pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(target);
        let _ = self.wake.try_send(());
    }
}

pub(in crate::host) fn install(host: &SharedHost) {
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    if state.workspace_registry.is_none() {
        return;
    }
    let owner = Arc::downgrade(host);
    let pending = Arc::new(Mutex::new(BTreeSet::new()));
    let work = pending.clone();
    let (wake, receive) = crossbeam_channel::bounded(1);
    if let Err(error) = service::spawn(
        "cagents-channel-titles",
        Duration::from_secs(1),
        move || {
            if receive.recv().is_err() {
                return service::Next::Stop;
            }
            let Some(host) = owner.upgrade() else {
                return service::Next::Stop;
            };
            let targets = std::mem::take(&mut *work.lock().unwrap_or_else(|p| p.into_inner()));
            let mut retry = Vec::new();
            for target in targets {
                if let Err(error) = refresh(&host, target) {
                    match error.code {
                        ProtocolErrorCode::QueueFull => retry.push(target),
                        ProtocolErrorCode::InvalidRequest => {} // A newer source owns the name.
                        _ => {
                            tracing::warn!(?target, error = %error.message, "cannot refresh channel title")
                        }
                    }
                }
            }
            if !retry.is_empty() {
                std::thread::sleep(Duration::from_millis(100));
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if let Some(updates) = &state.channel_titles {
                    for target in retry {
                        updates.offer(target);
                    }
                }
            }
            service::Next::Continue
        },
    ) {
        tracing::error!(%error, "cannot start the channel title publisher");
        return;
    }
    state.channel_titles = Some(Updates { pending, wake });
}

pub(super) fn refresh(host: &SharedHost, target: Target) -> Result<(), ProtocolError> {
    let gate = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .control_gate
        .clone();
    let _control = gate.shared();
    let writer = writer(host)?;
    let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    let id = match target {
        Target::Channel(id) => id,
        Target::Chat(chat) => {
            let Some(id) = registry.title_channel_for_chat(chat).map_err(unavailable)? else {
                return Ok(());
            };
            id
        }
    };
    let Some(channel) = registry.channel(id).map_err(unavailable)? else {
        return Ok(());
    };
    if channel.title_source == Manual && !channel.title.is_empty() {
        return Ok(());
    }
    let (view, _) = registry
        .view_snapshot(&Scope::Channel(id))
        .map_err(unavailable)?;
    let Some(view) = view else {
        return Ok(());
    };
    let Some(card) = view.primary_chat() else {
        return Ok(());
    };
    let Some(CardKind::Chat(expected)) = view
        .panes
        .iter()
        .find(|pane| pane.id == card)
        .map(|pane| &pane.kind)
    else {
        return Ok(());
    };
    if let Target::Chat(chat) = target
        && expected.id != chat
    {
        return Ok(());
    }
    let naming = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.is_shutting_down() {
            return Ok(());
        }
        state.title_conversation(expected)?
    };
    let replacing_provider = channel.title_source == Provider && naming.provider_cleared;
    let (title, source) = match naming.title() {
        Some(title) => title,
        None if replacing_provider => (String::new(), Prompt),
        None => return Ok(()),
    };
    if (channel.title == title && channel.title_source == source)
        || (!replacing_provider && !channel.title_source.accepts_refresh(&channel.title, source))
        || (!replacing_provider
            && source == Prompt
            && naming
                .opening
                .as_ref()
                .is_some_and(crate::chat::OpeningTitle::provisional)
            && !channel.title.is_empty()
            && !crate::workspace_model::image_placeholder_prompt(&channel.title))
    {
        return Ok(());
    }
    let edit = ModelEdit::SuggestConversationTitle {
        id,
        card,
        expected_revision: view.revision,
        title,
        source,
    };
    let applied = apply_title(host, &mut registry, &edit)?;
    if applied.changed {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.announce_model_edit(0, applied.receipt.sequence, &edit, &applied.receipt.effects);
    }
    Ok(())
}
