//! Chat panes keep naming the processes this daemon serves.
//!
//! A second daemon over the same saved data (#4118) stamped panes with its
//! own proof. Startup gives those panes back to the chats this daemon serves,
//! and edits can't put another daemon's proof on a pane.

use super::*;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        host::state::{HostLock, HostState},
        model::{BootId, HostIncarnation, HostSessionId, HostSessionProof, LaunchRecipe},
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
        let empty_channel = crate::model::ChannelId::new();
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
                ModelEdit::CreateChannel {
                    id: empty_channel,
                    repository: directory.path().to_owned(),
                    title: "empty".into(),
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

        let draft_session = crate::chat::Session {
            id: TerminalId::new(),
            proof: None,
            launch: crate::chat::Launch {
                session_id: None,
                ..chat.launch.clone()
            },
            ..chat.clone()
        };
        let draft = Pane {
            id: CardId::new(),
            kind: CardKind::Chat(draft_session.clone()),
            ..stored.clone()
        };
        let new_foreign = Pane {
            kind: CardKind::Chat(crate::chat::Session {
                id: draft_session.id,
                proof: Some(proof("second")),
                ..chat.clone()
            }),
            ..draft.clone()
        };
        let edit_empty = |change| {
            let scope = Scope::Channel(empty_channel);
            let expected_revision = writer(&host)
                .unwrap()
                .lock()
                .unwrap()
                .view_revision(&scope)
                .unwrap();
            edit_delta(
                &host,
                1,
                ModelEdit::ChangeView {
                    scope,
                    expected_revision,
                    operation: uuid::Uuid::new_v4(),
                    change,
                },
            )
        };
        let assert_foreign = |error: ProtocolError| {
            assert_eq!(error.code, ProtocolErrorCode::InvalidRequest);
            assert_eq!(error.message, Registry::VIEW_FOREIGN_CHAT_PROOF);
            assert!(!error.retryable);
        };
        let seeded_foreign = Pane {
            id: CardId::new(),
            kind: CardKind::Chat(crate::chat::Session {
                id: TerminalId::new(),
                proof: Some(proof("second")),
                ..draft_session.clone()
            }),
            ..draft.clone()
        };
        assert_foreign(
            edit_delta(
                &host,
                1,
                ModelEdit::SeedView {
                    scope: Scope::Channel(empty_channel),
                    panes: vec![draft.clone(), seeded_foreign],
                    closed: vec![],
                },
            )
            .expect_err("foreign seed must refuse the whole batch"),
        );
        assert_foreign(
            edit_empty(Change::Insert {
                pane: new_foreign.clone(),
                before: None,
            })
            .expect_err("foreign insert must be refused"),
        );
        assert_foreign(
            edit_empty(Change::InsertIfEmpty {
                pane: new_foreign.clone(),
            })
            .expect_err("foreign conditional insert must be refused"),
        );
        edit_empty(Change::InsertIfEmpty {
            pane: draft.clone(),
        })
        .expect("refusals leave the channel empty");
        assert_foreign(
            edit_empty(Change::Update {
                before: draft.clone(),
                pane: new_foreign,
            })
            .expect_err("draft cannot acquire another daemon's proof"),
        );
        let persisted = writer(&host)
            .unwrap()
            .lock()
            .unwrap()
            .pane_view(draft.id)
            .unwrap()
            .unwrap();
        assert_eq!(persisted.panes, [draft]);

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
