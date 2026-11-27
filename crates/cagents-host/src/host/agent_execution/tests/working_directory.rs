//! An agent's provider starts only in the directory the daemon opened and
//! judged for it (#4327).
use super::*;

/// How many providers the fixture has started.
fn spawns(root: &Path) -> usize {
    std::fs::read_to_string(root.join("spawns")).map_or(0, |text| text.lines().count())
}

/// Puts a link to a directory outside the project where `directory` was, and
/// returns where the directory went.
fn swap_for_a_link_out(root: &Path, directory: &Path) -> std::path::PathBuf {
    let outside = root.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let kept = root.join("kept");
    std::fs::rename(directory, &kept).unwrap();
    std::os::unix::fs::symlink(&outside, directory).unwrap();
    kept
}

/// #4327: the directory checked when an agent is accepted is recorded with
/// it, and a later prompt does not go once its path leads somewhere else.
#[test]
fn an_agent_keeps_the_directory_it_was_accepted_in() {
    if isolated("host::agent_execution::tests::working_directory::an_agent_keeps_the_directory_it_was_accepted_in").is_some() {
        return;
    }
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap().to_path_buf();
    let nested = root.join("project").join("nested");
    std::fs::create_dir(&nested).unwrap();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.target.directory = nested.clone();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    let recorded = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent_directory(accepted.agent.id)
        .unwrap();
    assert_eq!(
        recorded,
        Some(crate::agent_execution::DirectoryIdentity::from_metadata(
            &std::fs::metadata(&nested).unwrap()
        )),
        "acceptance records the directory it checked"
    );
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let prompts = || std::fs::read_to_string(root.join("prompts")).unwrap();
    let sent = prompts();

    let kept = swap_for_a_link_out(&root, &nested);
    let follow_up = |key: &str| Submit {
        name: None,
        destination: Destination::Existing(accepted.agent.id),
        prompt: "Carry on".into(),
        idempotency_key: key.into(),
    };
    let (refused, _) =
        super::super::prompt(&host, &Authority::default(), follow_up("swapped"), false).unwrap();
    let run = settled(&host, refused.run.id);
    assert_eq!(run.state, State::Failed, "{run:?}");
    let diagnostic = run.result.diagnostic.unwrap();
    assert!(
        diagnostic.contains(nested.to_str().unwrap()),
        "{diagnostic}"
    );
    assert_eq!(prompts(), sent, "the provider was given nothing");

    // Back where it was, it is the approved directory again.
    std::fs::remove_file(&nested).unwrap();
    std::fs::rename(kept, &nested).unwrap();
    let (resumed, _) =
        super::super::prompt(&host, &Authority::default(), follow_up("restored"), false).unwrap();
    assert_eq!(settled(&host, resumed.run.id).state, State::Succeeded);
}

/// #4327: an agent's conversation is not only reopened by the agent service.
/// A window that resumes it starts the agent's provider too, and starts it
/// only in the agent's directory: not where a link left at that path leads.
#[test]
fn a_window_reopens_an_agents_conversation_only_in_its_directory() {
    if isolated("host::agent_execution::tests::working_directory::a_window_reopens_an_agents_conversation_only_in_its_directory").is_some() {
        return;
    }
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap().to_path_buf();
    let nested = root.join("project").join("nested");
    std::fs::create_dir(&nested).unwrap();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.target.directory = nested.clone();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    super::super::super::finish_chat_shutdown(&host).unwrap();
    drop(host);
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("restarted-directory-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("restarted-directory"),
    });
    state.install_model(Registry::at(&path).unwrap());
    let host = Arc::new(crate::host::state::HostLock::new(state));
    // The conversation the stop saved, as a window holds it.
    let (agent, _) = status(&host, &Authority::default(), accepted.agent.id).unwrap();
    let Some(HostedSession::Chat(stopped)) = agent.session else {
        panic!("the agent did not bind a native conversation");
    };
    let directories = || {
        std::fs::read_to_string(root.join("directories"))
            .unwrap()
            .lines()
            .map(std::path::PathBuf::from)
            .collect::<Vec<_>>()
    };
    assert_eq!(directories(), [nested.as_path()]);
    let reopen = || {
        crate::host::chat::process(
            &host,
            7,
            ClientRequest::ChatStart {
                session: stopped.clone(),
                source: None,
                terminate_source: false,
                execution_environment: Default::default(),
            },
        )
    };

    // The prompt itself reopens nothing: this provider's conversation does
    // not reopen by itself, whatever its directory has become.
    let kept = swap_for_a_link_out(&root, &nested);
    let error = super::super::prompt(
        &host,
        &Authority::default(),
        Submit {
            name: None,
            destination: Destination::Existing(accepted.agent.id),
            prompt: "Carry on".into(),
            idempotency_key: "swapped".into(),
        },
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("Resume it from its pane"), "{error}");

    let error = reopen().unwrap_err().message;
    assert!(
        error.contains("symbolic link") && error.contains(nested.to_str().unwrap()),
        "{error}"
    );
    assert_eq!(directories().len(), 1, "nothing started: {error}");
    assert!(host.lock().unwrap().chats.is_empty());

    // With its directory back, the window's start runs the provider there and
    // the agent takes it over.
    std::fs::remove_file(&nested).unwrap();
    std::fs::rename(kept, &nested).unwrap();
    let ServerResponse::ChatAttached { session, .. } = reopen().unwrap() else {
        panic!("the conversation did not reopen");
    };
    assert_eq!(directories(), [nested.as_path(), nested.as_path()]);
    let (agent, _) = status(&host, &Authority::default(), accepted.agent.id).unwrap();
    assert_eq!(agent.session.map(|bound| bound.id()), Some(session.id));
}

/// The record names the directory an agent was last judged in. A start that
/// finds another one at the agent's path judges it as a new agent's and
/// records it; an automation's agent follows no replacement; a link opens for
/// neither.
#[test]
fn a_start_judges_a_directory_that_is_not_the_recorded_one() {
    use crate::agent_execution::DirectoryIdentity;
    if isolated("host::agent_execution::tests::working_directory::a_start_judges_a_directory_that_is_not_the_recorded_one").is_some() {
        return;
    }
    let (host, mut request, path) = fixture();
    let root = path.parent().unwrap().to_path_buf();
    let nested = root.join("project").join("nested");
    std::fs::create_dir(&nested).unwrap();
    let Destination::New(spec) = &mut request.destination else {
        unreachable!()
    };
    spec.target.directory = nested.clone();
    let registry = registry(&host).unwrap();
    let agent = registry
        .lock()
        .unwrap()
        .submit_agent(&Origin::User, &request, now())
        .unwrap()
        .agent;
    let scheduled = Agent {
        origin: Origin::Automation {
            id: uuid::Uuid::new_v4(),
        },
        ..agent.clone()
    };
    let recorded = || registry.lock().unwrap().agent_directory(agent.id).unwrap();
    let at = |path: &Path| DirectoryIdentity::from_metadata(&std::fs::metadata(path).unwrap());
    // A start: the directory is opened and judged, a provider runs in it,
    // and it is recorded.
    let start = |agent: &Agent| {
        super::super::working_directory::open_for_start(&registry, agent).map(|opened| {
            opened.started(&registry).unwrap();
            DirectoryIdentity::from_metadata(&opened.directory.metadata().unwrap())
        })
    };
    let first = at(&nested);
    assert_eq!(recorded(), None);
    assert_eq!(start(&agent), Ok(first));
    assert_eq!(recorded(), Some(first));

    // A boot renumbers devices. The same inode on another device is judged
    // again, for an automation's agent as well.
    let renumbered = DirectoryIdentity {
        device: first.device.wrapping_add(1),
        ..first
    };
    for agent in [&agent, &scheduled] {
        registry
            .lock()
            .unwrap()
            .record_agent_directory(agent.id, renumbered)
            .unwrap();
        assert_eq!(start(agent), Ok(first), "{:?}", agent.origin);
        assert_eq!(recorded(), Some(first), "{:?}", agent.origin);
    }

    // A worktree removed and made again is another directory at the path.
    std::fs::rename(&nested, root.join("kept")).unwrap();
    std::fs::create_dir(&nested).unwrap();
    let second = at(&nested);
    assert_ne!(second, first);
    let error = start(&scheduled).unwrap_err();
    assert!(error.contains("directory changed"), "{error}");
    assert_eq!(recorded(), Some(first));
    // It is recorded once a provider runs in it, not when it is judged: a
    // start refused later leaves the record where a provider may still be.
    let opened = super::super::working_directory::open_for_start(&registry, &agent).unwrap();
    assert_eq!(recorded(), Some(first));
    opened.started(&registry).unwrap();
    assert_eq!(recorded(), Some(second));
    assert_eq!(start(&agent), Ok(second));

    // A link opens for neither, even one that leads to the recorded directory.
    std::fs::remove_dir(&nested).unwrap();
    std::fs::create_dir(root.join("made-again")).unwrap();
    registry
        .lock()
        .unwrap()
        .record_agent_directory(agent.id, at(&root.join("made-again")))
        .unwrap();
    std::os::unix::fs::symlink(root.join("made-again"), &nested).unwrap();
    for agent in [&agent, &scheduled] {
        let error = start(agent).unwrap_err();
        assert!(error.contains("symbolic link"), "{error}");
    }
}

/// An agent accepted before directories were recorded has none. Its next
/// provider start judges its directory as a new agent's and records it; a
/// directory outside its project starts nothing.
#[test]
fn an_agent_accepted_without_a_recorded_directory_is_judged_when_it_starts() {
    if isolated("host::agent_execution::tests::working_directory::an_agent_accepted_without_a_recorded_directory_is_judged_when_it_starts").is_some()
    {
        return;
    }
    let (host, request, path) = fixture();
    let root = path.parent().unwrap().to_path_buf();
    let recorded = |agent| {
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .agent_directory(agent)
            .unwrap()
    };
    let unrecorded = |request: &Submit| {
        registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .submit_agent(&Origin::User, request, now())
            .unwrap()
    };

    // Outside the project, and behind a link inside it that leads out.
    std::fs::create_dir(root.join("outside")).unwrap();
    let link = root.join("project").join("link");
    std::os::unix::fs::symlink(root.join("outside"), &link).unwrap();
    for (key, directory, reason) in [
        ("outside", root.join("outside"), "is not in the project"),
        ("link", link.clone(), link.to_str().unwrap()),
    ] {
        let mut request = request.clone();
        request.idempotency_key = key.into();
        let Destination::New(spec) = &mut request.destination else {
            unreachable!()
        };
        spec.target.directory = directory;
        let refused = unrecorded(&request);
        assert_eq!(recorded(refused.agent.id), None);
        dispatch_accepted(&host, refused.clone()).unwrap();
        let run = settled(&host, refused.run.id);
        assert_eq!(run.state, State::Failed, "{key}: {run:?}");
        let diagnostic = run.result.diagnostic.unwrap();
        assert!(diagnostic.contains(reason), "{key}: {diagnostic}");
        assert_eq!(recorded(refused.agent.id), None, "{key}");
        assert_eq!(spawns(&root), 0, "{key}: nothing started");
    }

    let inside = unrecorded(&request);
    assert_eq!(recorded(inside.agent.id), None);
    dispatch_accepted(&host, inside.clone()).unwrap();
    assert_eq!(settled(&host, inside.run.id).state, State::Succeeded);
    assert_eq!(
        recorded(inside.agent.id),
        Some(crate::agent_execution::DirectoryIdentity::from_metadata(
            &std::fs::metadata(root.join("project")).unwrap()
        ))
    );
}
