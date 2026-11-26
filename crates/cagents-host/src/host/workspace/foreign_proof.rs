//! Chat panes keep naming the processes this daemon serves.
//!
//! A second daemon over the same saved data (#4118) stamped panes with its
//! own proof. Startup gives those panes back to the chats this daemon serves,
//! and edits can't put another daemon's proof on a pane.

use super::*;
use crate::model::HostSessionProof;

/// Gives each chat this daemon serves back the pane another daemon stamped
/// with its own proof, as [`Registry::adopt_foreign_chat_pane`] allows. Runs
/// once restore has reattached every holder it could, before any client
/// connects. A pane it can't prove shows that chat keeps its proof, fenced.
pub(in crate::host) fn adopt_foreign_panes(host: &SharedHost) {
    let (serving, live) = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        let serving = state.proof().clone();
        let live: Vec<_> = state
            .chats
            .values()
            .filter(|record| !record.handle.is_completed() && !record.handle.is_read_only_history())
            .map(|record| record.current_session())
            .filter(|session| session.proof.as_ref() == Some(&serving))
            .collect();
        (serving, live)
    };
    if live.is_empty() {
        return;
    }
    let Ok(writer) = writer(host) else {
        return;
    };
    let mut registry = writer.lock().unwrap_or_else(|p| p.into_inner());
    let mut adopted = Vec::new();
    for session in &live {
        match registry.adopt_foreign_chat_pane(session, &serving) {
            Ok(true) => adopted.push(session.id),
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(chat = %session.id, %error, "could not check this chat's pane for another daemon's proof");
            }
        }
    }
    if !adopted.is_empty() {
        tracing::info!(chats = ?adopted, "gave chat panes another daemon had stamped back to the chats this daemon serves");
    }
}

/// Refuses an edit that would leave a chat pane naming a different proof than
/// the complete one it has, unless it is this daemon's own. Only a daemon
/// stamps a pane with its proof as it serves the chat; a window that writes
/// another's is writing back a pane from before that daemon took it over.
/// Only pane updates can change a stored pane's chat, so only they are read.
pub(super) fn refuse_foreign_proof(
    host: &SharedHost,
    registry: &Registry,
    edit: &ModelEdit,
) -> Result<(), ProtocolError> {
    let ModelEdit::ChangeView {
        scope,
        expected_revision,
        change:
            change @ (Change::Update { before, pane }
            | Change::UpdateTerminalSource { before, pane, .. }),
        ..
    } = edit
    else {
        return Ok(());
    };
    let CardKind::Chat(written) = &pane.kind else {
        return Ok(());
    };
    let serving = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    // The write can leave only the stored proof or this one.
    if written.proof.as_ref().is_none_or(|proof| *proof == serving) {
        return Ok(());
    }
    let Some(mut view) = registry.pane_view(pane.id).map_err(unavailable)? else {
        return Ok(());
    };
    // A write against another view, an older revision or a changed source is
    // the store's to refuse as a conflict the window rebases and retries.
    if view.scope != *scope
        || view.revision != *expected_revision
        || view.source_update_conflicts(before, pane)
    {
        return Ok(());
    }
    let proof = |view: &crate::workspace_model::views::View| {
        view.panes
            .iter()
            .find(|candidate| candidate.id == pane.id)
            .and_then(|candidate| match &candidate.kind {
                CardKind::Chat(session) => session.proof.clone(),
                _ => None,
            })
    };
    let Some(stored) = proof(&view).filter(HostSessionProof::is_complete) else {
        return Ok(());
    };
    // What the write leaves, merged with the stored pane as the store merges
    // it. An edit the store would refuse anyway is its to refuse.
    if view.apply(change).is_err() {
        return Ok(());
    }
    match proof(&view) {
        Some(left) if left != stored && left != serving => Err(ProtocolError::new(
            ProtocolErrorCode::InvalidRequest,
            format!(
                "pane {} shows its chat as this daemon serves it; a copy from another daemon can't replace it",
                pane.id
            ),
            false,
        )),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host::state::{HostLock, HostState},
        model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
    };

    fn proof(host_session: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(host_session),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new(format!("{host_session}-incarnation")),
        }
    }

    #[test]
    fn a_window_cannot_put_another_daemons_proof_on_a_chat_pane() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = HostState::new(proof("serving"));
        state.install_model(Registry::at(&directory.path().join("model.sqlite3")).unwrap());
        state.register_client(1);
        let host: SharedHost = Arc::new(HostLock::new(state));
        let channel = crate::model::ChannelId::new();
        let chat = crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: LaunchRecipe {
                    program: "/usr/bin/claude".into(),
                    arguments: vec![],
                    working_directory: directory.path().to_owned(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("conversation".into()),
                draft_model: None,
            },
            proof: Some(proof("serving")),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let stored = Pane {
            id: CardId::new(),
            title: "chat".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: Some(directory.path().to_owned()),
            kind: CardKind::Chat(chat.clone()),
        };
        {
            let writer = writer(&host).unwrap();
            let mut registry = writer.lock().unwrap();
            for edit in [
                ModelEdit::OpenProject {
                    repository: directory.path().to_owned(),
                    name: "Fixture".into(),
                    kind: Default::default(),
                },
                ModelEdit::CreateChannel {
                    id: channel,
                    repository: directory.path().to_owned(),
                    title: "view".into(),
                    title_source: Default::default(),
                    checkout: None,
                },
                ModelEdit::SeedView {
                    scope: Scope::Channel(channel),
                    panes: vec![stored.clone()],
                    closed: vec![],
                },
            ] {
                registry.apply(&edit).unwrap();
            }
        }
        let update = |before: &Pane, pane: Pane| {
            let revision = writer(&host)
                .unwrap()
                .lock()
                .unwrap()
                .view_revision(&Scope::Channel(channel))
                .unwrap();
            edit_delta(
                &host,
                1,
                ModelEdit::ChangeView {
                    scope: Scope::Channel(channel),
                    expected_revision: revision,
                    operation: uuid::Uuid::new_v4(),
                    change: Change::Update {
                        before: before.clone(),
                        pane,
                    },
                },
            )
        };
        let saved = || {
            writer(&host)
                .unwrap()
                .lock()
                .unwrap()
                .pane_view(stored.id)
                .unwrap()
                .unwrap()
                .panes
                .into_iter()
                .find(|pane| pane.id == stored.id)
                .unwrap()
        };
        let foreign = Pane {
            kind: CardKind::Chat(crate::chat::Session {
                proof: Some(proof("second")),
                ..chat.clone()
            }),
            ..stored.clone()
        };

        let refused =
            update(&stored, foreign.clone()).expect_err("another daemon's proof is refused");
        assert_eq!(refused.code, ProtocolErrorCode::InvalidRequest);
        assert_eq!(saved(), stored, "the pane keeps the proof it had");

        // A window still holding that copy can rename the pane: the write
        // leaves the proof the pane has.
        let renamed = Pane {
            title: "renamed".into(),
            ..foreign.clone()
        };
        update(&foreign, renamed).unwrap();
        assert_eq!(saved().title, "renamed");
        assert_eq!(saved().kind, stored.kind);

        // A stale write is the store's conflict, which the window rebases.
        let stale = edit_delta(
            &host,
            1,
            ModelEdit::ChangeView {
                scope: Scope::Channel(channel),
                expected_revision: 0,
                operation: uuid::Uuid::new_v4(),
                change: Change::Update {
                    before: saved(),
                    pane: foreign.clone(),
                },
            },
        )
        .expect_err("a stale write is refused");
        assert_eq!(stale.code, ProtocolErrorCode::ViewConflict);
        assert!(stale.retryable);

        // A write carrying this daemon's own proof goes through.
        let current = saved();
        let restarted = Pane {
            kind: CardKind::Chat(crate::chat::Session {
                launch: crate::chat::Launch {
                    session_id: Some("restarted".into()),
                    ..chat.launch.clone()
                },
                ..chat.clone()
            }),
            ..current.clone()
        };
        update(&current, restarted.clone()).unwrap();
        assert_eq!(saved().kind, restarted.kind);
    }
}
