use super::*;

#[test]
fn an_ended_offstrip_sign_in_waits_for_completed_release_confirmation() {
    let mut app = app_in_repo();
    let (card, terminal) = open_sign_in(&mut app);
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        panic!("sign-in terminal expected");
    };
    // The existing off-strip fixture uses this detached hosted descriptor too.
    // It is fixture data, not a successful daemon start or attachment receipt.
    session.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: crate::model::HostSessionId::new("original-host"),
        boot_id: crate::model::BootId::new("original-boot"),
        host_incarnation: crate::model::HostIncarnation::new("original-incarnation"),
    };
    assert!(session.continuity.live_proof().is_some());

    app.retire_ended_terminal(card, 0, None, true);
    // Settle any held start bookkeeping through the actual late-completion
    // handler. The retired lifecycle refuses this result before installing it.
    drop(app.update(Message::TerminalStarted {
        card,
        terminal,
        result: Err("fixture: attachment completion after observed exit".into()),
    }));
    assert!(!app.terminal_start_in_flight(card));
    drop(app.select_worktree("/elsewhere".into(), false));
    assert!(!app.workspace.order().any(|id| id == card));
    assert!(!app.terminal_start_in_flight(card));

    let state = app.card_state(card).expect("the ended card still exists");
    assert!(state.existence.is_open());
    let CardKind::Terminal(source) = &state.kind else {
        panic!("the original terminal descriptor is retained");
    };
    let source = source.clone();
    assert_eq!(source.id, terminal);
    assert!(matches!(
        &source.continuity,
        TerminalContinuity::RelaunchAvailable {
            last_exit: Some(_),
            ..
        }
    ));
    assert!(source.continuity.live_proof().is_none());

    let close = app.close_offstrip_sign_in(card);
    // Deliberately do not poll this Task: no release confirmation has arrived.
    let current = app
        .card_state(card)
        .expect("closing keeps the original card");
    assert!(
        current.existence.is_open(),
        "an ended off-strip sign-in must remain open until completed-terminal release is confirmed"
    );
    let CardKind::Terminal(current) = &current.kind else {
        panic!("closing keeps the original terminal descriptor");
    };
    assert_eq!(current, &source);
    drop(close);
}

use crate::terminal_ipc::{
    ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage, ServerResponse,
    TerminalCloseResult, read_frame, write_frame,
};
use std::{os::unix::net::UnixStream, sync::mpsc, thread};

fn reply(
    peer: &mut UnixStream,
    request: &ClientEnvelope,
    proof: &HostSessionProof,
    response: ServerResponse,
) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof.clone(),
            message: ServerMessage::Response { response },
        },
    )
    .unwrap();
}

fn handshake(peer: &mut UnixStream, proof: &HostSessionProof) {
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let hello: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
    reply(
        peer,
        &hello,
        proof,
        ServerResponse::Connected {
            proof: proof.clone(),
            resumed: vec![],
        },
    );
    let auth: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(
        auth.request,
        ClientRequest::Authenticate {
            authority: crate::agent_execution::Authority::default()
        }
    );
    reply(peer, &auth, proof, ServerResponse::Accepted);
}

fn connected(
    stream: UnixStream,
    authority: crate::agent_execution::Authority,
) -> Result<ControlClient, String> {
    let mut client = ControlClient::from_stream(stream).map_err(|error| error.to_string())?;
    client
        .authenticate(authority)
        .map_err(|error| error.to_string())?;
    Ok(client)
}

fn ended_fixture(exited: bool) -> (Cagents, CardId, TerminalSession) {
    let (mut app, card, original) = offstrip_close_fixture();
    app.retire_ended_terminal(card, 0, None, false);
    drop(app.update(Message::TerminalStarted {
        card,
        terminal: original.id,
        result: Err("fixture attachment already ended".into()),
    }));
    assert!(!app.terminal_start_in_flight(card));
    if exited {
        let CardKind::Terminal(source) = &mut app.card_state_mut(card).unwrap().kind else {
            panic!()
        };
        let TerminalContinuity::RelaunchAvailable {
            last_exit: Some(last_exit),
            ..
        } = &source.continuity
        else {
            panic!()
        };
        source.continuity = TerminalContinuity::Exited {
            last_exit: last_exit.clone(),
        };
    }
    app.cards.insert(card, CardRuntime::Completed);
    let CardKind::Terminal(source) = &app.card_state(card).unwrap().kind else {
        panic!()
    };
    let source = source.clone();
    (app, card, source)
}

#[derive(Clone, Copy, Debug)]
enum Answer {
    Closed,
    Active,
    Unknown,
    WrongId,
    Eof,
}

type HeldRelease = (
    Task<Message>,
    tokio::sync::oneshot::Receiver<TerminalSession>,
    mpsc::Sender<()>,
    thread::JoinHandle<()>,
);

fn held_release(
    app: &mut Cagents,
    card: CardId,
    source: &TerminalSession,
    answer: Answer,
) -> HeldRelease {
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let proof = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("cleanup-framed"),
        boot_id: crate::model::BootId::new("cleanup-boot"),
        host_incarnation: crate::model::HostIncarnation::new("cleanup-incarnation"),
    };
    let original = source.clone();
    let (seen, observed) = tokio::sync::oneshot::channel();
    let (resume, held) = mpsc::channel();
    let server = thread::spawn(move || {
        handshake(&mut peer, &proof);
        let request: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
        assert_eq!(
            request.request,
            ClientRequest::ReleaseCompletedTerminal {
                session: original.clone(),
                allow_active: true
            }
        );
        seen.send(original.clone()).unwrap();
        held.recv_timeout(Duration::from_secs(3))
            .expect("finite held response");
        if matches!(answer, Answer::Eof) {
            return;
        }
        let outcome = match answer {
            Answer::Closed | Answer::WrongId => TerminalCloseResult::Closed,
            Answer::Active => TerminalCloseResult::Active,
            Answer::Unknown => TerminalCloseResult::Unknown("fixture cleanup unconfirmed".into()),
            Answer::Eof => unreachable!(),
        };
        reply(
            &mut peer,
            &request,
            &proof,
            ServerResponse::TerminalCloseChecked {
                terminal_id: if matches!(answer, Answer::WrongId) {
                    TerminalId::new()
                } else {
                    original.id
                },
                outcome,
            },
        );
        assert!(
            read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
            "no retry, LIST or replacement close"
        );
    });
    let task = app.close_offstrip_sign_in_via(card, move |authority| connected(stream, authority));
    (task, observed, resume, server)
}

#[tokio::test]
async fn completed_offstrip_task_waits_for_exact_release_and_keeps_refusals() {
    for exited in [false, true] {
        for answer in [
            Answer::Closed,
            Answer::Active,
            Answer::Unknown,
            Answer::WrongId,
            Answer::Eof,
        ] {
            let (mut app, card, source) = ended_fixture(exited);
            let original = app.card_state(card).unwrap().clone();
            let scopes = app.durable.card_scopes.clone();
            let (task, observed, resume, server) = held_release(&mut app, card, &source, answer);
            let completion = lifecycle_message(task);
            tokio::pin!(completion);
            let sent = tokio::select! {
                sent = observed => sent.unwrap(),
                _ = &mut completion => panic!("cleanup completed before its actual release response"),
            };
            assert_eq!(sent, source);
            assert_eq!(app.card_state(card), Some(&original));
            assert_eq!(app.durable.card_scopes, scopes);
            assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Waiting);
            resume.send(()).unwrap();
            let message = completion.await;
            drop(app.update(message));
            server.join().unwrap();
            if matches!(answer, Answer::Closed) {
                assert!(
                    app.card_state(card)
                        .is_none_or(|state| !state.existence.is_open())
                );
                assert!(!app.durable.card_scopes.contains_key(&card));
            } else {
                assert_eq!(app.card_state(card), Some(&original), "{answer:?}");
                assert_eq!(app.durable.card_scopes, scopes);
                assert!(
                    app.status
                        .as_ref()
                        .unwrap()
                        .message
                        .contains("card was kept")
                );
            }
        }
    }
}

#[tokio::test]
async fn old_completed_release_cannot_close_or_retire_a_public_retry() {
    let (mut app, card, source) = ended_fixture(false);
    let (task, observed, resume, server) = held_release(&mut app, card, &source, Answer::Closed);
    let completion = lifecycle_message(task);
    tokio::pin!(completion);
    tokio::select! {
        sent = observed => assert_eq!(sent.unwrap(), source),
        _ = &mut completion => panic!("release must be held"),
    }
    drop(app.select_worktree("/repo".into(), false));
    drop(app.update(Message::RetryCard(card)));
    let replacement = app.card_state(card).unwrap().clone();
    let CardKind::Terminal(next) = &replacement.kind else {
        panic!()
    };
    assert_ne!(next.id, source.id, "public Retry creates a different actor");
    let scopes = app.durable.card_scopes.clone();
    resume.send(()).unwrap();
    let message = completion.await;
    let result = app.update(message);
    assert_eq!(result.units(), 0, "no stale callback follow-up");
    drop(result);
    assert_eq!(app.card_state(card), Some(&replacement));
    assert_eq!(app.durable.card_scopes, scopes);
    server.join().unwrap(); // Server asserted only the original source was released.
}

#[test]
fn live_source_does_not_rebase_or_close_a_replacement_daemon() {
    let (_, _, source) = offstrip_close_fixture();
    let (stream, mut peer) = UnixStream::pair().unwrap();
    let replacement = HostSessionProof {
        host_session_id: crate::model::HostSessionId::new("replacement-host"),
        boot_id: crate::model::BootId::new("replacement-boot"),
        host_incarnation: crate::model::HostIncarnation::new("replacement-incarnation"),
    };
    let server = thread::spawn(move || {
        handshake(&mut peer, &replacement);
        assert!(
            read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
            "no LIST or Close against a fresh proof"
        );
    });
    let mut client = connected(stream, crate::agent_execution::Authority::default()).unwrap();
    assert!(
        stop_hosted(&mut client, &source)
            .unwrap_err()
            .contains("daemon changed")
    );
    drop(client);
    server.join().unwrap();
}

fn successful_outcome() -> Outcome {
    Outcome::Answered {
        catalog: None,
        identity: Some(Identity {
            email: Some("cleanup-fixture@example.invalid".into()),
            ..Default::default()
        }),
        snapshot: UsageSnapshot::state(UsageStatus::Ok, "", now_ms()),
    }
}

#[tokio::test]
async fn original_verification_and_registration_cannot_close_a_newer_same_home_retry() {
    let (mut app, card, source) = ended_fixture(false);
    drop(app.select_worktree("/repo".into(), false));
    let key = app.account_ui.logins[0].key;
    let captured = app.sign_in_cleanup(key);
    let verification = app.update(Message::AccountUi(Action::LoginTick));
    assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Verifying);
    drop(verification); // Provider work is deliberately never polled.
    drop(app.update(Message::RetryCard(card)));
    let state = app.card_state_mut(card).unwrap();
    let CardKind::Terminal(next) = &mut state.kind else {
        panic!()
    };
    assert_ne!(next.id, source.id);
    // Complete the replacement's fixture custody using the same detached
    // descriptor as offstrip_close_fixture. This is not a real daemon start.
    next.continuity = TerminalContinuity::DetachedSameBoot {
        host_session_id: crate::model::HostSessionId::new("replacement-host"),
        boot_id: crate::model::BootId::new("replacement-boot"),
        host_incarnation: crate::model::HostIncarnation::new("replacement-incarnation"),
    };
    state.residency = Residency::Hot;
    drop(app.finish_terminal_start(card));
    assert!(!app.terminal_start_in_flight(card));
    assert!(!app.pending_terminal_spawn(card));
    let replacement = app.card_state(card).unwrap().clone();
    assert!(!matches!(
        replacement.residency,
        Residency::Hydrating { .. }
    ));
    let registration = app.update(Message::AccountUi(Action::LoginVerified {
        key,
        cleanup: captured,
        outcome: successful_outcome(),
    }));
    let settled = lifecycle_message(registration).await;
    let Message::AccountUi(Action::LoginSettled { cleanup, .. }) = &settled else {
        panic!("actual registration completion")
    };
    assert_eq!(cleanup.target.as_ref().unwrap().1, source);
    let close = app.update(settled);
    assert_eq!(
        close.units(),
        0,
        "late original settlement must not send a close for the retry"
    );
    drop(close);
    assert_eq!(app.card_state(card), Some(&replacement));
    assert!(!app.pane_close_owned(card));
    assert!(app.terminal_close.is_none());

    // Positive control: the otherwise identical current card is eligible
    // for guarded close. Pending hydration cannot explain the stale refusal.
    let close = app.close_card_unasked(card);
    assert!(close.units() > 0);
    let intent = app
        .terminal_close
        .as_ref()
        .expect("replacement is close-eligible");
    let CardKind::Terminal(next) = &replacement.kind else {
        panic!()
    };
    assert_eq!(intent.terminal, next.id);
    assert!(intent.pending);
    drop(close); // No daemon request is polled by this fixture control.
}

#[test]
fn cleanup_keeps_uncertain_starts_and_scoped_sessions_but_retires_never_started() {
    for authority in [
        crate::agent_execution::Authority::session("fixture-only".into()),
        crate::agent_execution::Authority::session(String::new()),
    ] {
        let (mut app, card, _) = ended_fixture(false);
        app.account_ui.authority = authority;
        let original = app.card_state(card).unwrap().clone();
        let task =
            app.close_offstrip_sign_in_via(card, |_| panic!("scope refusal precedes connection"));
        assert_eq!(task.units(), 0);
        drop(task);
        assert_eq!(app.card_state(card), Some(&original));
    }
    let (mut app, card, _) = ended_fixture(false);
    drop(app.select_worktree("/repo".into(), false));
    drop(app.update(Message::RetryCard(card)));
    drop(app.select_worktree("/elsewhere".into(), false));
    let original = app.card_state(card).unwrap().clone();
    assert!(matches!(original.residency, Residency::Hydrating { .. }));
    let task = app.close_offstrip_sign_in_via(card, |_| panic!("uncertain start stays owned"));
    assert_eq!(task.units(), 0);
    drop(task);
    assert_eq!(app.card_state(card), Some(&original));

    // A separately proved process-free descriptor is not pending hydration.
    app.finish_terminal_start(card);
    let state = app.card_state_mut(card).unwrap();
    state.residency = Residency::Hibernated;
    let CardKind::Terminal(source) = &mut state.kind else {
        panic!()
    };
    source.continuity = TerminalContinuity::RelaunchAvailable {
        recipe: source.launch.clone(),
        last_exit: None,
    };
    let task = app.close_offstrip_sign_in_via(card, |_| {
        panic!("never-started needs no terminal cleanup connection")
    });
    assert!(
        app.card_state(card)
            .is_none_or(|state| !state.existence.is_open())
    );
    drop(task); // Subsequent check_logins work is not claimed to make no RPC.
}

#[test]
fn settled_original_live_sign_in_synchronously_acquires_the_guarded_close() {
    let (mut app, card, source) = offstrip_close_fixture();
    drop(app.select_worktree("/repo".into(), false));
    let key = app.account_ui.logins[0].key;
    let cleanup = app.sign_in_cleanup(key);
    assert_eq!(cleanup.target.as_ref().unwrap(), &(card, source.clone()));
    assert!(source.continuity.live_proof().is_some());
    assert!(!app.terminal_start_in_flight(card));
    let close = app.update(Message::AccountUi(Action::LoginSettled {
        key,
        cleanup,
        result: Ok(Settled::Abandoned(None)),
    }));
    let intent = app
        .terminal_close
        .as_ref()
        .expect("original live close is owned synchronously");
    assert_eq!(intent.card, card);
    assert_eq!(intent.terminal, source.id);
    assert_eq!(Some(&intent.proof), source.continuity.live_proof().as_ref());
    assert!(intent.pending);
    assert!(
        !intent.active,
        "successful sign-in cleanup does not open confirmation"
    );
    assert!(app.pane_close_owned(card));
    let before_retry = app.card_state(card).unwrap().clone();
    drop(app.update(Message::RetryCard(card)));
    assert_eq!(
        app.card_state(card),
        Some(&before_retry),
        "existing close custody blocks replacement"
    );
    drop(close); // Actual daemon close is unpolled; no backend completion is claimed.
}

#[test]
fn an_old_callback_blockage_releases_verification_without_blocking_the_retry() {
    let (mut app, card, source) = ended_fixture(false);
    drop(app.select_worktree("/repo".into(), false));
    let key = app.account_ui.logins[0].key;
    let cleanup = app.sign_in_cleanup(key);
    drop(app.update(Message::AccountUi(Action::LoginTick)));
    assert_eq!(app.account_ui.logins[0].phase, LoginPhase::Verifying);
    drop(app.update(Message::RetryCard(card)));
    let replacement = app.card_state(card).unwrap().clone();
    let CardKind::Terminal(next) = &replacement.kind else {
        panic!()
    };
    assert_ne!(next.id, source.id);
    let before_completion = now_ms();
    let result = app.update(Message::AccountUi(Action::LoginCallbackBlocked {
        key,
        cleanup,
    }));
    assert_eq!(
        result.units(),
        0,
        "old callback does not probe or close the retry"
    );
    drop(result);
    assert_eq!(app.card_state(card), Some(&replacement));
    assert_eq!(
        app.account_ui.logins[0].phase,
        LoginPhase::Waiting,
        "the old completed verification must not strand the new login in Verifying"
    );
    assert!(app.account_ui.logins[0].next_check >= before_completion + LOGIN_RETRY);
    assert!(!app.pane_close_owned(card));
}
