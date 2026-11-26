//! A chat pane another daemon stamped with its own proof.
//!
//! A second daemon serving the same saved data from another runtime directory
//! (#4118) re-admitted chats the serving daemon still ran and saved their
//! panes under its own host session, dropping their Codex listeners. The
//! panes then named a process that never existed, and a managed agent's pane
//! was fenced because its agent record still named the serving daemon's.

use super::*;
use crate::{
    chat::{NativeContinuity, Session},
    model::{CardKind, HostSessionProof},
};

impl Registry {
    /// Gives the pane showing `live`'s chat back to the daemon serving it,
    /// as exactly the session it serves, when [`restamps_foreign_proof`]
    /// holds for them. `live` is a chat this daemon serves now, as its actor
    /// holds it. Returns whether the pane changed.
    pub(in crate::host) fn adopt_foreign_chat_pane(
        &mut self,
        live: &Session,
        serving: &HostSessionProof,
    ) -> Result<bool, String> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let Some(id) = store::chat_pane(&tx, live.id)? else {
            return Ok(false);
        };
        let Some(scope) = owner(&tx, id)? else {
            return Ok(false);
        };
        let mut view = read(&tx, &scope)?;
        let before = view.clone();
        if view.retired.contains(&id) || view.panes.iter().filter(|pane| pane.id == id).count() != 1
        {
            return Ok(false);
        }
        let pane = view.panes.iter_mut().find(|pane| pane.id == id).unwrap();
        let CardKind::Chat(current) = &pane.kind else {
            return Ok(false);
        };
        if !restamps_foreign_proof(current, live, serving) {
            return Ok(false);
        }
        // An open agent on this chat that names another process keeps its
        // pane as the agent knows it, rather than fenced from it.
        let actor = crate::agent_execution::HostedSession::Chat(live.clone());
        if super::super::execution::agent_for_session_in(&tx, live.id)?
            .and_then(|agent| agent.session)
            .is_some_and(|session| !session.same_process(&actor))
        {
            return Ok(false);
        }
        let adopted = crate::workspace_model::views::Pane {
            kind: CardKind::Chat(live.clone()),
            ..pane.clone()
        };
        if store::resource_holder(&tx, &adopted)?.is_some() {
            return Ok(false);
        }
        *pane = adopted;
        view.revision = store::next_revision(&tx)?;
        terminal_retention::sync_view_pins(&tx, &view.panes)?;
        store::store(&tx, &view)?;
        bindings::sync(&tx, &before, &view)?;
        tx.execute(
            "UPDATE metadata SET sequence = sequence + 1 WHERE key = 0",
            [],
        )
        .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(true)
    }
}

/// Whether `pane` shows the chat `live` is, under the proof of a daemon that
/// served this boot's data beside the `serving` one and no longer does, so
/// that it can show `live` itself. Every field that names the conversation
/// and how it runs must already be `live`'s; only the proof, a dropped Codex
/// listener, and continuity the actor knows better may differ. The listener
/// comes from `live`, which holds its own allocation, never from the pane.
pub(super) fn restamps_foreign_proof(
    pane: &Session,
    live: &Session,
    serving: &HostSessionProof,
) -> bool {
    let (Some(foreign), Some(issued)) = (pane.proof.as_ref(), live.proof.as_ref()) else {
        return false;
    };
    let conversation = |session: &Session| {
        session
            .launch
            .session_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    };
    issued == serving
        && serving.is_complete()
        && live.has_valid_native_continuity()
        && foreign.is_complete()
        && foreign.boot_id == serving.boot_id
        && foreign.host_session_id != serving.host_session_id
        && pane.id == live.id
        && pane.launch.provider == live.launch.provider
        && pane.launch.recipe == live.launch.recipe
        && conversation(pane).is_some()
        && conversation(pane) == conversation(live)
        && (pane.native_continuity == live.native_continuity
            || pane.native_continuity == NativeContinuity::ReconnectOnly)
        && (pane.launch.provider != crate::agent::Provider::Codex || live.owns_codex_listener())
        && (pane.owned_codex_listener.is_none()
            || pane.owned_codex_listener == live.owned_codex_listener)
        && match (&pane.terminal, &live.terminal) {
            (None, None) => true,
            (Some(shown), Some(served))
                if shown.owned_codex_listener.is_none()
                    || shown.owned_codex_listener == served.owned_codex_listener =>
            {
                let mut shown = shown.clone();
                shown.continuity = served.continuity.clone();
                shown.owned_codex_listener = served.owned_codex_listener.clone();
                shown.id == pane.id && &shown == served
            }
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_execution::HostedSession,
        chat::{Launch, OwnedCodexListener},
        model::{BootId, CardId, HostIncarnation, HostSessionId, LaunchRecipe, TerminalId},
        workspace_model::views::{Pane, Scope},
    };

    fn proof(host_session: &str, boot: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new(host_session),
            boot_id: BootId::new(boot),
            host_incarnation: HostIncarnation::new(format!("{host_session}-incarnation")),
        }
    }

    /// A Codex chat as the serving daemon's actor holds it, with the listener
    /// it allocated, and the pane a second daemon saved for it: stamped with
    /// that daemon's proof in the same boot, and without the listener.
    fn chat() -> (Session, Session) {
        let id = TerminalId::new();
        let socket =
            std::path::PathBuf::from(format!("/run/user/1000/cagents-codex-{id}/control.sock"));
        let live = Session {
            id,
            launch: Launch {
                provider: crate::agent::Provider::Codex,
                recipe: LaunchRecipe {
                    program: "/usr/bin/codex".into(),
                    arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
                    working_directory: "/repo".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: Some("thread".into()),
                draft_model: None,
            },
            proof: Some(proof("serving", "boot")),
            terminal: None,
            owned_codex_listener: Some(OwnedCodexListener {
                socket,
                allocator: proof("earlier", "boot"),
            }),
            native_continuity: Default::default(),
        };
        assert!(live.owns_codex_listener());
        let stamped = Session {
            proof: Some(proof("second", "boot")),
            owned_codex_listener: None,
            ..live.clone()
        };
        (live, stamped)
    }

    fn registry_with(session: &Session) -> (Registry, tempfile::TempDir, ChannelId, CardId) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: "/repo".into(),
                name: "repo".into(),
                kind: Default::default(),
            })
            .unwrap();
        let channel = ChannelId::new();
        registry
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository: "/repo".into(),
                title: "view".into(),
                title_source: Default::default(),
                checkout: None,
            })
            .unwrap();
        let pane = CardId::new();
        registry
            .apply(&ModelEdit::SeedView {
                scope: Scope::Channel(channel),
                panes: vec![Pane {
                    id: pane,
                    title: "chat".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 0.5,
                    scope: Some("/repo".into()),
                    kind: CardKind::Chat(session.clone()),
                }],
                closed: vec![],
            })
            .unwrap();
        (registry, directory, channel, pane)
    }

    fn shown(registry: &Registry, channel: ChannelId, pane: CardId) -> Pane {
        registry
            .model()
            .unwrap()
            .views
            .into_iter()
            .find(|view| view.scope == Scope::Channel(channel))
            .unwrap()
            .panes
            .into_iter()
            .find(|candidate| candidate.id == pane)
            .unwrap()
    }

    #[test]
    fn a_pane_another_daemon_stamped_shows_the_chat_this_daemon_serves_again() {
        let (live, stamped) = chat();
        let serving = live.proof.clone().unwrap();
        let (mut registry, _directory, channel, pane) = registry_with(&stamped);
        let agent = HostedSession::Chat(live.clone());
        // Fenced: the agent's chat is not the process the pane names.
        assert!(!agent.same_process(&HostedSession::Chat(stamped.clone())));

        assert!(registry.adopt_foreign_chat_pane(&live, &serving).unwrap());

        let adopted = shown(&registry, channel, pane);
        assert_eq!(adopted.kind, CardKind::Chat(live.clone()));
        let CardKind::Chat(adopted) = adopted.kind else {
            unreachable!()
        };
        assert!(agent.same_process(&HostedSession::Chat(adopted.clone())));
        assert_eq!(
            adopted.owned_codex_listener, live.owned_codex_listener,
            "the listener comes back from the actor's own allocation"
        );
        assert!(
            !registry.adopt_foreign_chat_pane(&live, &serving).unwrap(),
            "a pane already showing the chat is left alone"
        );
    }

    fn open_agent(registry: &Registry, channel: ChannelId, session: &Session) {
        let mut agent = crate::agent_execution::Agent::fixture_placed_in(channel);
        agent.session = Some(HostedSession::Chat(session.clone()));
        registry
            .db
            .execute(
                "INSERT INTO agents(id, origin, record, session_id, closed) \
                 VALUES (?1, '\"User\"', ?2, ?3, 0)",
                rusqlite::params![
                    agent.id.to_string(),
                    serde_json::to_string(&agent).unwrap(),
                    session.id.to_string()
                ],
            )
            .unwrap();
    }

    #[test]
    fn a_pane_whose_agent_names_another_process_stays_as_the_agent_knows_it() {
        let (live, stamped) = chat();
        let serving = live.proof.clone().unwrap();
        let (mut registry, _directory, channel, pane) = registry_with(&stamped);
        open_agent(&registry, channel, &stamped);

        assert!(!registry.adopt_foreign_chat_pane(&live, &serving).unwrap());
        assert_eq!(
            shown(&registry, channel, pane).kind,
            CardKind::Chat(stamped)
        );
    }

    /// As in #4118: restore has rebound the agent to the chat this daemon
    /// serves, and only its pane still names the other daemon.
    #[test]
    fn a_pane_whose_agent_names_the_served_chat_is_restamped() {
        let (live, stamped) = chat();
        let serving = live.proof.clone().unwrap();
        let (mut registry, _directory, channel, pane) = registry_with(&stamped);
        open_agent(&registry, channel, &live);

        assert!(registry.adopt_foreign_chat_pane(&live, &serving).unwrap());
        assert_eq!(shown(&registry, channel, pane).kind, CardKind::Chat(live));
    }

    #[test]
    fn a_pane_naming_another_conversation_stays_fenced() {
        let (live, mut stamped) = chat();
        stamped.launch.session_id = Some("another thread".into());
        let serving = live.proof.clone().unwrap();
        let (mut registry, _directory, channel, pane) = registry_with(&stamped);

        assert!(!registry.adopt_foreign_chat_pane(&live, &serving).unwrap());
        assert_eq!(
            shown(&registry, channel, pane).kind,
            CardKind::Chat(stamped)
        );
    }

    #[test]
    fn only_a_pane_with_nothing_but_its_proof_wrong_is_restamped() {
        let (live, stamped) = chat();
        let serving = live.proof.clone().unwrap();
        assert!(restamps_foreign_proof(&stamped, &live, &serving));
        let differing: Vec<(&str, Box<dyn Fn(&mut Session, &mut Session)>)> = vec![
            (
                "an earlier boot",
                Box::new(|pane, _| pane.proof = Some(proof("second", "earlier boot"))),
            ),
            (
                "the serving proof",
                Box::new(|pane, live| pane.proof = live.proof.clone()),
            ),
            (
                "an incomplete proof",
                Box::new(|pane, _| pane.proof = Some(proof("", "boot"))),
            ),
            ("no proof", Box::new(|pane, _| pane.proof = None)),
            (
                "another chat",
                Box::new(|pane, _| pane.id = TerminalId::new()),
            ),
            (
                "another provider",
                Box::new(|pane, _| pane.launch.provider = crate::agent::Provider::Claude),
            ),
            (
                "another recipe",
                Box::new(|pane, _| pane.launch.recipe.working_directory = "/elsewhere".into()),
            ),
            (
                "no conversation",
                Box::new(|pane, live| {
                    pane.launch.session_id = None;
                    live.launch.session_id = None;
                }),
            ),
            (
                "another listener",
                Box::new(|pane, _| {
                    pane.owned_codex_listener = Some(OwnedCodexListener {
                        socket: "/elsewhere/control.sock".into(),
                        allocator: proof("second", "boot"),
                    })
                }),
            ),
            (
                "an actor without its listener",
                Box::new(|_, live| live.owned_codex_listener = None),
            ),
            (
                "an actor another daemon serves",
                Box::new(|_, live| live.proof = Some(proof("second", "boot"))),
            ),
        ];
        for (case, change) in differing {
            let (mut live, mut stamped) = chat();
            change(&mut stamped, &mut live);
            assert!(!restamps_foreign_proof(&stamped, &live, &serving), "{case}");
        }
    }
}
