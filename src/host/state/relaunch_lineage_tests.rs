//! An update relaunches a conversation that it cannot carry. Its agent follows
//! the relaunched descriptor only through the previous daemon's lineage.
use super::*;
use crate::{
    agent::Provider,
    agent_execution::{Destination, HostedSession, Origin, Spec, Submit, Surface, Target},
    model::{
        AgentId, BootId, HostIncarnation, HostSessionId, LaunchRecipe, ProjectKind, TerminalSession,
    },
    upgrade::handoff::ChatSource,
    workspace_model::ModelEdit,
};

const THREAD: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

fn proof(incarnation: &str) -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new(format!("{incarnation}-session")),
        boot_id: BootId::new("boot"),
        host_incarnation: HostIncarnation::new(incarnation),
    }
}

fn view(id: TerminalId, continuity: TerminalContinuity) -> TerminalSession {
    TerminalSession {
        id,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        launch: LaunchRecipe {
            program: "codex".into(),
            arguments: vec!["--remote".into(), "unix:///fixture/control.sock".into()],
            working_directory: "/project".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
        transcript_path: None,
        resume_hint: None,
        continuity,
    }
}

fn live(proof: &HostSessionProof) -> TerminalContinuity {
    TerminalContinuity::Live {
        host_session_id: proof.host_session_id.clone(),
        boot_id: proof.boot_id.clone(),
        host_incarnation: proof.host_incarnation.clone(),
    }
}

/// A Codex agent's issued descriptor before any view was paired with it.
fn issued(id: TerminalId, proof: &HostSessionProof) -> crate::chat::Session {
    crate::chat::Session {
        id,
        launch: crate::chat::Launch {
            provider: Provider::Codex,
            recipe: LaunchRecipe {
                program: "codex".into(),
                arguments: vec!["app-server".into()],
                working_directory: "/project".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
    }
}

fn agent_bound_to(
    registry: &mut super::super::model::Registry,
    root: &std::path::Path,
    session: &crate::chat::Session,
) -> AgentId {
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.into(),
            name: "Relaunch lineage".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let accepted = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::New(Spec {
                    provider: Provider::Codex,
                    account: None,
                    target: Target {
                        repository: root.into(),
                        directory: root.into(),
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "fixture".into(),
                idempotency_key: "fixture".into(),
            },
            1,
        )
        .unwrap();
    let owned = HostedSession::Chat(session.clone());
    registry
        .bind_agent(accepted.agent.id, None, owned.clone())
        .unwrap();
    registry.register_agent_authority(&owned).unwrap();
    accepted.agent.id
}

/// The agent was bound before its CLI view was paired, and the update relaunched
/// that view after the provider: the checkpoint and the new capability both
/// name a different terminal than the final descriptor does.
#[test]
fn agent_follows_a_relaunched_conversation_whose_view_was_paired_and_relaunched() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry =
        super::super::model::Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
    let (old, new) = (proof("previous"), proof("current"));
    let id = TerminalId::new();
    let bound = issued(id, &old);
    let agent = agent_bound_to(&mut registry, directory.path(), &bound);

    let mut checkpointed = bound.clone();
    checkpointed.terminal = Some(view(id, live(&old)));
    let mut confirmed = checkpointed.launch.clone();
    confirmed.session_id = Some(THREAD.into());
    let sources = [ChatSource {
        source: checkpointed.clone(),
        launch: confirmed,
    }];
    // `restore_upgrade` starts the provider before its view is relaunched.
    let mut started = checkpointed.clone();
    started.proof = Some(new.clone());
    started.launch.session_id = Some(THREAD.into());
    let relaunch = view(id, live(&old)).launch;
    started.terminal = Some(view(
        id,
        TerminalContinuity::RelaunchAvailable {
            recipe: relaunch,
            last_exit: None,
        },
    ));
    registry
        .fresh_agent_authority(&HostedSession::Chat(started.clone()))
        .unwrap();
    let mut successor = started.clone();
    successor.terminal = Some(view(id, live(&new)));

    assert!(adopt_relaunched_agent(&mut registry, &sources, successor.clone()).unwrap());
    assert_eq!(
        registry.agent(agent).unwrap().session,
        Some(HostedSession::Chat(successor.clone()))
    );
    assert_eq!(
        registry.retained_agent_authority_source(id).unwrap(),
        Some(HostedSession::Chat(successor))
    );
}

/// Only the checkpointed lineage moves an agent; one bound to another
/// descriptor under the same ID keeps it and refuses prompts.
#[test]
fn agent_outside_the_checkpointed_lineage_keeps_its_descriptor() {
    let directory = tempfile::tempdir().unwrap();
    let mut registry =
        super::super::model::Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
    let (old, new) = (proof("previous"), proof("current"));
    let id = TerminalId::new();
    let mut unrelated = issued(id, &old);
    unrelated.launch.recipe.arguments.push("--other".into());
    let agent = agent_bound_to(&mut registry, directory.path(), &unrelated);

    let checkpointed = issued(id, &old);
    let sources = [ChatSource {
        source: checkpointed.clone(),
        launch: checkpointed.launch.clone(),
    }];
    let mut successor = checkpointed;
    successor.proof = Some(new);
    registry
        .fresh_agent_authority(&HostedSession::Chat(successor.clone()))
        .unwrap();

    assert!(adopt_relaunched_agent(&mut registry, &sources, successor).is_err());
    assert_eq!(
        registry.agent(agent).unwrap().session,
        Some(HostedSession::Chat(unrelated))
    );
}
