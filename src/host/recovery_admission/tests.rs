use super::*;
use crate::{
    chat::{Command, PromptMode},
    host::{
        agent_execution::tests::{fixture, isolated},
        client::process_request,
    },
    model::{LaunchRecipe, TerminalContinuity},
    terminal_ipc::{ServerResponse, TerminalSpawnOptions},
};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const DETAIL: &str = "Upgrade restore retained original ownership: Codex server did not open its local socket within 15 seconds";

fn claude(root: &Path, session_id: Option<String>) -> Session {
    Session {
        id: TerminalId::new(),
        proof: None,
        terminal: None,
        native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
        owned_codex_listener: None,
        launch: crate::chat::Launch {
            provider: Provider::Claude,
            session_id,
            recipe: LaunchRecipe {
                program: root.join("bin/claude").to_str().unwrap().into(),
                arguments: Vec::new(),
                working_directory: root.join("project"),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
        },
    }
}

/// A Codex conversation the update checkpointed and the failed restore never
/// reached. Its provider and holder are outside this daemon's inventory.
fn unrestored_codex(
    root: &Path,
    proof: &crate::model::HostSessionProof,
) -> crate::upgrade::handoff::Chat {
    let launch = crate::chat::Launch {
        provider: Provider::Codex,
        session_id: Some("019a0000-0000-7000-8000-000000000001".into()),
        recipe: LaunchRecipe {
            program: root.join("bin/codex").to_str().unwrap().into(),
            arguments: Vec::new(),
            working_directory: root.join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        },
    };
    crate::upgrade::handoff::Chat {
        source: Session {
            id: TerminalId::new(),
            proof: Some(proof.clone()),
            terminal: None,
            native_continuity: crate::chat::NativeContinuity::ReconnectOnly,
            owned_codex_listener: None,
            launch: launch.clone(),
        },
        previous_sources: Vec::new(),
        launch,
    }
}

fn refused_by_hold(host: &SharedHost, client: u64, request: ClientRequest) {
    let description = format!("{request:?}");
    let refusal = process_request(host, client, request)
        .map(|_| ())
        .expect_err(&format!("the hold admitted {description}"));
    assert_eq!(
        refusal.code,
        ProtocolErrorCode::TerminalUnavailable,
        "{description}"
    );
    assert!(!refusal.retryable, "{description}");
    assert!(
        refusal
            .message
            .starts_with(crate::chat::STARTUP_RECOVERY_REFUSAL),
        "{description}: {refusal:?}"
    );
    assert!(refusal.message.ends_with(DETAIL), "{refusal:?}");
}

fn prompt(text: &str) -> Command {
    Command::Prompt {
        text: text.into(),
        attachments: Vec::new(),
        mode: PromptMode::Send,
    }
}

fn wait_for(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// #3082: an update stopped partway through its restore when one Codex server
/// missed its socket deadline, and its daemon kept the originals it had
/// adopted. A fresh Claude conversation was refused before its provider
/// started. A new conversation on another provider touches none of those
/// originals, so it starts, sends, closes and reopens; requests that name a
/// retained original still meet the hold.
#[test]
fn a_fresh_conversation_sends_and_reopens_while_recovery_holds_retained_owners() {
    if isolated(
        "host::recovery_admission::tests::a_fresh_conversation_sends_and_reopens_while_recovery_holds_retained_owners",
    )
    .is_some()
    {
        return;
    }
    let (host, _, registry) = fixture();
    let root = registry.parent().unwrap().to_owned();
    let user = 1;
    host.lock().unwrap().register_client(user);
    let proof = host.lock().unwrap().proof().clone();

    // An original the restore adopted before it failed: a live terminal.
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), "exec sleep 1000".into()],
        working_directory: root.join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    let original = TerminalSession {
        id: TerminalId::new(),
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
    };
    process_request(
        &host,
        user,
        ClientRequest::Spawn {
            session: original.clone(),
            options: TerminalSpawnOptions::default(),
        },
    )
    .unwrap();
    let codex = unrestored_codex(&root, &proof);
    {
        let mut state = host.lock().unwrap();
        state.retain_startup_checkpoint_for_test(crate::upgrade::handoff::Checkpoint::new(
            proof.clone(),
            Vec::new(),
            Vec::new(),
            vec![codex.clone()],
        ));
        state.retain_startup_recovery(DETAIL.into());
        assert!(state.recovery_owns_work());
    }

    // Status and the reads a window and a Close make are answered.
    assert!(matches!(
        process_request(&host, user, ClientRequest::BuildInfo)
            .unwrap()
            .0,
        ServerResponse::BuildInfo { .. }
    ));
    assert!(matches!(
        process_request(
            &host,
            user,
            ClientRequest::ReadWorkspaceMetadata {
                query: crate::workspace_model::metadata::Query::Agents,
            },
        )
        .unwrap()
        .0,
        ServerResponse::WorkspaceMetadata(_)
    ));

    // What names a retained original keeps the hold's refusal.
    refused_by_hold(
        &host,
        user,
        ClientRequest::Input {
            terminal_id: original.id,
            bytes: b"echo".to_vec(),
        },
    );
    refused_by_hold(
        &host,
        user,
        ClientRequest::ChatStart {
            session: codex.source.clone(),
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    );
    refused_by_hold(
        &host,
        user,
        ClientRequest::ChatAttach {
            session: codex.source.clone(),
        },
    );
    // An unstarted draft reusing the checkpointed allocation is no fresh start.
    let mut reused = claude(&root, None);
    reused.id = codex.source.id;
    refused_by_hold(
        &host,
        user,
        ClientRequest::ChatStart {
            session: reused,
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    );
    // Nor is a resume of a provider conversation nothing started during the hold.
    refused_by_hold(
        &host,
        user,
        ClientRequest::ChatStart {
            session: claude(&root, Some("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb".into())),
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    );
    refused_by_hold(&host, user, ClientRequest::ChatCapacitySet { limit: 1 });

    // A fresh conversation on another provider starts and sends.
    let fresh = claude(&root, None);
    let started = process_request(
        &host,
        user,
        ClientRequest::ChatStart {
            session: fresh.clone(),
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap()
    .0;
    assert!(
        matches!(&started, ServerResponse::ChatAttached { session, .. } if session.id == fresh.id),
        "{started:?}"
    );
    assert!(matches!(
        process_request(
            &host,
            user,
            ClientRequest::ChatCommand {
                chat_id: fresh.id,
                command: prompt("first"),
            },
        )
        .unwrap()
        .0,
        ServerResponse::Accepted
    ));
    let prompts = || std::fs::read_to_string(root.join("prompts")).unwrap_or_default();
    wait_for("the fresh conversation's prompt never arrived", || {
        prompts() == "prompt\n"
    });
    let conversation = {
        let mut conversation = None;
        wait_for(
            "the fresh conversation never confirmed its identity",
            || {
                conversation = host.lock().unwrap().chats[&fresh.id].handle.session_id();
                conversation.is_some()
            },
        );
        conversation.unwrap()
    };
    // The streaming provider fixture needs its saved history for a disk resume.
    let project: String = fresh
        .launch
        .recipe
        .working_directory
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let history = root.join("home/.claude/projects").join(project);
    std::fs::create_dir_all(&history).unwrap();
    std::fs::write(history.join(format!("{conversation}.jsonl")), b"").unwrap();
    assert!(matches!(
        process_request(
            &host,
            user,
            ClientRequest::ChatVisibility {
                chat_id: fresh.id,
                visible: true,
            },
        )
        .unwrap()
        .0,
        ServerResponse::Accepted
    ));
    process_request(
        &host,
        user,
        ClientRequest::ChatRead {
            chat_id: fresh.id,
            after_generation: None,
        },
    )
    .unwrap();

    // It closes, then reopens from its saved conversation.
    let handle = host.lock().unwrap().chats[&fresh.id].handle.clone();
    process_request(&host, user, ClientRequest::ChatClose { chat_id: fresh.id }).unwrap();
    wait_for("Close did not retire the fresh provider", || {
        handle.is_completed()
    });
    let mut reopened = claude(&root, Some(conversation.clone()));
    reopened.proof = Some(proof.clone());
    let attached = process_request(
        &host,
        user,
        ClientRequest::ChatStart {
            session: reopened.clone(),
            source: None,
            terminate_source: false,
            execution_environment: Default::default(),
        },
    )
    .unwrap()
    .0;
    assert!(
        matches!(&attached, ServerResponse::ChatAttached { session, .. } if session.id == reopened.id),
        "{attached:?}"
    );
    wait_for(
        "the reopened provider did not resume its conversation",
        || {
            std::fs::read_to_string(root.join("claude-arguments"))
                .is_ok_and(|arguments| {
                    arguments
                        .lines()
                        .any(|argument| argument == format!("--resume={conversation}"))
                })
        },
    );
    process_request(
        &host,
        user,
        ClientRequest::ChatCommand {
            chat_id: reopened.id,
            command: prompt("second"),
        },
    )
    .unwrap();
    wait_for("the reopened conversation's prompt never arrived", || {
        prompts() == "prompt\nprompt\n"
    });

    // None of this released the hold or touched the originals.
    {
        let state = host.lock().unwrap();
        assert_eq!(state.startup_recovery(), Some(DETAIL));
        assert!(!state.terminals[&original.id].handle.is_completed());
    }
    // The retained original's explicit Close stays available.
    process_request(
        &host,
        user,
        ClientRequest::CloseTerminalChecked {
            terminal_id: original.id,
            allow_active: true,
        },
    )
    .unwrap();
}

/// Only an unstarted session with no terminal, retained provider or listener
/// is a fresh start; and the checkpoint names each source, earlier source and
/// paired terminal it carries.
#[test]
fn recovery_holds_every_identity_its_checkpoint_names() {
    let root = Path::new("/nonexistent");
    let proof = crate::model::HostSessionProof {
        host_session_id: "recovery-admission".into(),
        boot_id: "boot".into(),
        host_incarnation: "incarnation".into(),
    };
    let mut codex = unrestored_codex(root, &proof);
    let earlier = unrestored_codex(root, &proof);
    codex
        .previous_sources
        .push(crate::upgrade::handoff::ChatSource {
            source: earlier.source.clone(),
            launch: earlier.launch.clone(),
        });
    let checkpoint = crate::upgrade::handoff::Checkpoint::new(
        proof.clone(),
        Vec::new(),
        Vec::new(),
        vec![codex.clone()],
    );
    assert!(checkpoint_names(&checkpoint, codex.source.id));
    assert!(checkpoint_names(&checkpoint, earlier.source.id));
    assert!(!checkpoint_names(&checkpoint, TerminalId::new()));

    let mut state = HostState::new(proof);
    state.retain_startup_checkpoint_for_test(checkpoint);
    state.retain_startup_recovery(DETAIL.into());
    assert!(held_by_recovery(&state, codex.source.id));
    assert!(!held_by_recovery(&state, TerminalId::new()));
    assert!(conversation_held_by_recovery(
        &state,
        Provider::Codex,
        codex.launch.session_id.as_deref().unwrap()
    ));
    assert!(!conversation_held_by_recovery(
        &state,
        Provider::Claude,
        codex.launch.session_id.as_deref().unwrap()
    ));
}
