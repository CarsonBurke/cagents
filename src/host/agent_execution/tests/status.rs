//! Actual service/run/mailbox identities over the existing offline provider.
//! This covers a refused competing submission, not a commercial capacity error.
use super::*;
use crate::agent_execution::{
    directory::Scope,
    mailbox::{Inbox, Outgoing, State as MessageState, To},
};

#[test]
fn directory_keeps_a_live_untracked_turn_failed_followup_and_queued_mail_distinct() {
    let name = "host::agent_execution::tests::status::directory_keeps_a_live_untracked_turn_failed_followup_and_queued_mail_distinct";
    if isolated(name).is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let root = path.parent().unwrap();
    let accepted = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let agent = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap();
    let HostedSession::Chat(source) = agent.session.unwrap() else {
        panic!("native fixture")
    };
    let handle = host.lock().unwrap().chats[&source.id].handle.clone();
    let authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(source.id)
        .unwrap();
    host.lock().unwrap().register_client(17);
    crate::host::client::process_request(
        &host,
        17,
        ClientRequest::ChatAttach {
            session: source.clone(),
        },
    )
    .unwrap();
    struct Release(std::path::PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = std::fs::write(&self.0, b"release");
        }
    }
    let release = Release(root.join("cleanup-release"));
    crate::host::client::process_request(
        &host,
        17,
        ClientRequest::ChatCommand {
            chat_id: source.id,
            command: Command::Prompt {
                text: "HOLD_CLEANUP_BUSY".into(),
                attachments: Vec::new(),
                mode: PromptMode::Send,
            },
        },
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("cleanup-busy").exists() {
        assert!(
            Instant::now() < deadline,
            "original fixture prompt never arrived"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let mut followup = request;
    followup.destination = Destination::Existing(accepted.agent.id);
    followup.idempotency_key = "competing-followup".into();
    let second = submit(&host, &Authority::default(), followup).unwrap();
    let failed = settled(&host, second.run.id);
    assert_eq!(failed.state, State::Failed);
    assert!(
        failed
            .result
            .diagnostic
            .as_ref()
            .unwrap()
            .contains("not idle")
    );
    assert!(failed.started_ms.is_none());
    let sent = mailbox::send(
        &host,
        &Authority::default(),
        Outgoing {
            to: To::Session(source.id.to_string()),
            body: "Queued instruction, not executed".into(),
            wait_ms: None,
        },
    )
    .unwrap();
    assert_eq!(sent.message.state, MessageState::Pending);
    let read = |authority: &Authority| {
        let Reply::Directory(directory) =
            process(&host, authority.clone(), Operation::Directory(Scope::All)).unwrap()
        else {
            panic!("directory response")
        };
        directory
            .caller
            .into_iter()
            .chain(directory.sessions)
            .find(|entry| entry.id == source.id)
            .unwrap()
    };
    let own = read(&Authority::default());
    assert_eq!(own.state, crate::agent::ActivityState::Working);
    let status = own.status.unwrap();
    let observed = status.observed.unwrap();
    assert!(observed.parent_busy);
    assert_eq!(observed.activity, crate::agent::ActivityState::Working);
    assert_eq!(
        observed.active_run, None,
        "interactive prompt is not the historical failed managed run"
    );
    let latest = status.latest_run.unwrap();
    assert_eq!(latest.id, failed.id);
    assert_eq!(latest.state, State::Failed);
    assert!(!latest.request_recorded);
    assert_eq!(latest.diagnostic, failed.result.diagnostic);
    assert_eq!(status.pending_message, Some(true));
    let peer = read(&Authority::session("invalid-directory-reader".into()));
    assert_eq!(peer.state, crate::agent::ActivityState::Working);
    let peer = peer.status.unwrap();
    assert!(peer.latest_run.unwrap().diagnostic.is_none());
    assert_eq!(peer.pending_message, None);
    let stored = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .message(sent.message.id)
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.state,
        MessageState::Pending,
        "directory reads must not consume mail"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        2,
        "failed followup and queued message must not submit another prompt"
    );

    let inbox = mailbox::inbox(
        &host,
        &authority,
        Inbox {
            all: false,
            replies_to: None,
        },
    )
    .unwrap();
    assert!(inbox.iter().any(|message| message.id == sent.message.id));
    let after_read = read(&Authority::default());
    assert_eq!(after_read.state, crate::agent::ActivityState::Working);
    assert_eq!(after_read.status.unwrap().pending_message, Some(false));
    assert_eq!(
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .message(sent.message.id)
            .unwrap()
            .unwrap()
            .state,
        MessageState::Read
    );
    assert_eq!(
        std::fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        2,
        "Read is an inbox receipt, not provider execution"
    );
    std::fs::write(&release.0, b"release").unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_idle() {
        assert!(
            Instant::now() < deadline,
            "original interactive turn did not settle"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    close(&host, &Authority::default(), accepted.agent.id).unwrap();
}
