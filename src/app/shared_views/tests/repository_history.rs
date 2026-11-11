use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Agent, HostedSession, Origin, Spec, Surface, Target},
    chat::{Intent, Launch, NativeContinuity, Session},
    model::{AgentId, HostIncarnation, HostSessionProof},
    upgrade::floor::{Ledger, Record},
};

#[test]
fn repository_refresh_projects_only_exact_paused_history_without_execution_authority() {
    for case in [
        "exact",
        "native-thread",
        "launch",
        "missing-proof",
        "incomplete-proof",
        "unissued-source-proof",
        "missing-ledger",
    ] {
        let (mut app, selected_channel, unrelated_channel, mut unrelated) = fixture();
        let selected = Scope::Channel(selected_channel);
        let unrelated_scope = Scope::Channel(unrelated_channel);
        let repository = PathBuf::from("/repo/.git");
        app.workspace_model.channels = [
            (selected_channel, repository.clone()),
            (unrelated_channel, PathBuf::from("/unrelated/.git")),
        ]
        .into_iter()
        .map(|(id, repository)| crate::workspace_model::ModelChannel {
            id,
            repository,
            title: "Channel".into(),
            title_source: Default::default(),
            ordinal: 1,
        })
        .collect();

        let terminal = attention_terminal_source();
        let mut recipe = terminal.launch;
        recipe.program = "never-execute-repository-history-provider".into();
        let source = Session {
            id: terminal.id,
            launch: Launch {
                provider: Provider::Codex,
                session_id: Some("accepted-parent-thread".into()),
                recipe,
            },
            proof: terminal.continuity.live_proof(),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: NativeContinuity::Durable {
                provider_version: "fixture".into(),
            },
        };
        let mut replacement = source.clone();
        replacement.proof = Some(HostSessionProof {
            host_incarnation: HostIncarnation::new("history-successor"),
            ..source.proof.clone().unwrap()
        });
        replacement.reset_owned_codex_listener();
        replacement.native_continuity = NativeContinuity::ReconnectOnly;
        let mut agent = Agent {
            id: AgentId::new(),
            name: None,
            origin: Origin::User,
            spec: Spec {
                provider: Provider::Codex,
                account: None,
                target: Target {
                    repository: repository.clone(),
                    directory: source.launch.recipe.working_directory.clone(),
                    placement: Some(selected.clone()),
                },
                surface: Surface::Chat,
                model: None,
                permission_mode: None,
                reasoning_effort: None,
            },
            placement: selected.clone(),
            session: Some(HostedSession::Chat(source.clone())),
            console: None,
            created_ms: 1,
            closed: false,
        };
        let ledger = Ledger {
            records: vec![Record::new(
                source.proof.clone().unwrap(),
                vec![source],
                vec![],
            )],
        };
        match case {
            "native-thread" => replacement.launch.session_id = Some("different-thread".into()),
            "launch" => replacement
                .launch
                .recipe
                .arguments
                .push("different-launch".into()),
            "missing-proof" => replacement.proof = None,
            "incomplete-proof" => {
                replacement.proof.as_mut().unwrap().host_incarnation = HostIncarnation::new("");
            }
            "unissued-source-proof" => {
                let Some(HostedSession::Chat(original)) = agent.session.as_mut() else {
                    unreachable!()
                };
                original.proof.as_mut().unwrap().host_incarnation =
                    HostIncarnation::new("not-the-stopped-source");
            }
            _ => {}
        }
        let original_agent = agent.clone();
        let pane = Pane {
            id: CardId::from_uuid(agent.id.as_uuid()),
            title: "Paused managed conversation".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.6,
            scope: Some("/repo".into()),
            kind: CardKind::Chat(replacement.clone()),
        };
        let mut view = View::empty(selected.clone());
        view.panes.push(pane.clone());
        app.workspace_model.agents = vec![agent];

        unrelated.title = "Local unrelated title".into();
        let unrelated_card = CardState {
            id: unrelated.id,
            generation: app.durable.generation,
            title: unrelated.title.clone(),
            width_fraction: unrelated.width_fraction,
            existence: Existence::Open,
            kind: unrelated.kind.clone(),
        };
        Arc::make_mut(&mut app.durable)
            .cards
            .push(unrelated_card.clone());
        Arc::make_mut(&mut app.durable)
            .channels
            .iter_mut()
            .find(|channel| channel.id == unrelated_channel)
            .unwrap()
            .cards = vec![unrelated.id];
        app.shared_views
            .baseline
            .insert(unrelated_scope.clone(), vec![unrelated.clone()]);
        let mut unrelated_view = View::empty(unrelated_scope.clone());
        let mut remote = unrelated.clone();
        remote.title = "Must wait for the unrelated refresh".into();
        unrelated_view.panes.push(remote);
        app.workspace_model.views = vec![view, unrelated_view];
        let pending = (
            uuid::Uuid::new_v4(),
            unrelated_scope.clone(),
            Change::Remove { id: CardId::new() },
        );
        app.shared_views.pending.push(pending.clone());

        app.startup_gate.initial_panes.capture(
            instance::WindowRestorePolicy::ExistingOnly,
            &app.durable.cards,
        );
        app.startup_gate
            .initial_panes
            .capture_views(vec![selected.clone()]);
        app.shared_views.view_projection_count.set(0);
        let history = (case != "missing-ledger").then_some(&ledger);
        // Do not poll attachment Tasks: this checks projection and scheduling,
        // while native rehearsal proves provider/run continuity end to end.
        drop(app.reconcile_shared_views_in(Some((&repository, &HashSet::new())), history));

        assert_eq!(app.workspace_model.agents, [original_agent], "{case}");
        assert_eq!(
            app.card_state(unrelated.id),
            Some(&unrelated_card),
            "{case}"
        );
        assert_eq!(
            app.shared_views.baseline[&unrelated_scope],
            [unrelated.clone()],
            "{case}"
        );
        assert!(app.shared_views.pending.contains(&pending), "{case}");
        if case == "exact" {
            assert!(!app.shared_views.blocked.contains(&selected));
            assert_eq!(app.card_state(pane.id).unwrap().kind, pane.kind);
            assert_eq!(app.shared_views.view_projection_count.get(), 1);
            assert!(app.chat_start_pending(pane.id));
            assert_eq!(
                app.startup_gate.initial_panes.chat_intent(
                    pane.id,
                    replacement.id,
                    Intent::Restore,
                ),
                Intent::RestoreExisting,
                "display membership must retain attachment-only startup intent"
            );
        } else {
            assert!(app.shared_views.blocked.contains(&selected), "{case}");
            assert!(app.card_state(pane.id).is_none(), "{case}");
            assert!(!app.cards.contains_key(&pane.id), "{case}");
            assert!(!app.chat_start_pending(pane.id), "{case}");
            assert_eq!(app.shared_views.view_projection_count.get(), 0, "{case}");
        }
    }
}
