//! Reading another session's conversation from its provider's saved history.
use super::*;
use crate::agent_execution::{
    Placement,
    directory::{Directory, Members, Scope},
    transcript::{Content, Page, Query},
};
use crate::model::{ChannelId, TerminalId};
use serde_json::json;
use std::io::Write;

fn query(session: &str, before: Option<&str>, limit: u16) -> Query {
    Query {
        session: session.into(),
        before: before.map(Into::into),
        limit,
        tools: false,
        full: false,
    }
}

fn read(host: &SharedHost, authority: &Authority, query: Query) -> Result<Page, String> {
    match process(host, authority.clone(), Operation::Transcript(query))? {
        Reply::Transcript(page) => Ok(page),
        reply => panic!("unexpected reply {reply:?}"),
    }
}

fn members(
    host: &SharedHost,
    authority: &Authority,
    channel: Option<ChannelId>,
) -> Result<Members, String> {
    match process(host, authority.clone(), Operation::Members(channel))? {
        Reply::Members(members) => Ok(members),
        reply => panic!("unexpected reply {reply:?}"),
    }
}

fn directory(host: &SharedHost, authority: &Authority, scope: Scope) -> Result<Directory, String> {
    match process(host, authority.clone(), Operation::Directory(scope))? {
        Reply::Directory(directory) => Ok(directory),
        reply => panic!("unexpected reply {reply:?}"),
    }
}

/// Writes the conversation Claude would have saved for the fixture project:
/// a prompt, a run of tool calls, an answer, a compaction and a second prompt.
fn save_claude_transcript(host: &SharedHost, session: TerminalId, root: &Path) {
    // The conversation cagents chose for the provider, which it confirmed.
    let conversation = host.lock().unwrap().chats[&session]
        .handle
        .confirmed_session_id()
        .expect("the fixture provider confirmed its conversation");
    let project = root.join("project");
    let sanitized: String = project
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let directory = root.join("home/.claude/projects").join(sanitized);
    std::fs::create_dir_all(&directory).unwrap();
    let rows = [
        json!({"type":"user","uuid":"u1","sessionId":conversation,
            "message":{"role":"user","content":"Find why the login test is flaky"}}),
        json!({"type":"assistant","uuid":"t1","parentUuid":"u1","sessionId":conversation,
            "message":{"id":"m1","role":"assistant","content":[
                {"type":"tool_use","id":"bash-1","name":"Bash","input":{"command":"cargo test login"}},
                {"type":"tool_use","id":"edit-1","name":"Edit","input":{"file_path":"/repo/src/auth.rs","old_string":"a","new_string":"b"}}]}}),
        json!({"type":"user","uuid":"r1","parentUuid":"t1","sessionId":conversation,
            "message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"bash-1","content":"test failed","is_error":true},
                {"type":"tool_result","tool_use_id":"edit-1","content":"ok"}]}}),
        json!({"type":"assistant","uuid":"a1","parentUuid":"r1","sessionId":conversation,
            "message":{"id":"m2","role":"assistant","content":[{"type":"text","text":"The cookie expires during the retry."}]}}),
        json!({"type":"user","uuid":"c1","parentUuid":"a1","sessionId":conversation,"isCompactSummary":true,
            "message":{"role":"user","content":"Summary: the login test was fixed by extending the cookie."}}),
        json!({"type":"user","uuid":"u2","parentUuid":"c1","sessionId":conversation,
            "message":{"role":"user","content":"Now open a PR"}}),
    ];
    let mut file = std::fs::File::create(directory.join(format!("{conversation}.jsonl"))).unwrap();
    for row in rows {
        writeln!(file, "{row}").unwrap();
    }
}

#[test]
fn transcript_reads_a_sessions_saved_conversation_compactly_and_pages_back() {
    if isolated("host::agent_execution::tests::transcript::transcript_reads_a_sessions_saved_conversation_compactly_and_pages_back").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let root = path.parent().unwrap();
    let accepted = submit(&host, &Authority::default(), request).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let session = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .agent(accepted.agent.id)
        .unwrap()
        .session
        .unwrap()
        .id();
    let handle = session.to_string();
    // The provider's file is absent until it saves one.
    let missing = read(&host, &Authority::default(), query(&handle, None, 10)).unwrap_err();
    assert!(missing.contains("Claude history"), "{missing}");
    save_claude_transcript(&host, session, root);

    let page = read(&host, &Authority::default(), query(&handle[..8], None, 10)).unwrap();
    assert_eq!(page.session.session.id, session);
    assert!(!page.session.exited);
    assert_eq!(page.earlier, None, "the whole conversation fits");
    let kinds: Vec<_> = page
        .entries
        .iter()
        .map(|entry| match &entry.content {
            Content::User(text) => format!("user {}", text.text),
            Content::Assistant { text, .. } => format!("assistant {}", text.text),
            Content::Tools(run) => format!(
                "tools {} failed {} edited {:?}",
                run.calls
                    .iter()
                    .map(|call| format!("{}x{}", call.name, call.count))
                    .collect::<Vec<_>>()
                    .join(","),
                run.failed,
                run.edited
            ),
            Content::Delivery { summary, body, .. } => format!("notice {summary}: {}", body.text),
            Content::Tool(tool) => format!("tool {}", tool.name),
        })
        .collect();
    assert_eq!(kinds.len(), 5, "{kinds:#?}");
    assert_eq!(kinds[0], "user Find why the login test is flaky");
    assert!(kinds[1].starts_with("tools "), "{kinds:#?}");
    assert!(kinds[1].contains("failed 1"), "{kinds:#?}");
    assert!(kinds[1].contains("auth.rs"), "{kinds:#?}");
    assert_eq!(kinds[2], "assistant The cookie expires during the retry.");
    assert!(
        kinds[3].starts_with("notice ") && kinds[3].contains("extending the cookie"),
        "the compaction summary is kept: {kinds:#?}"
    );
    assert_eq!(kinds[4], "user Now open a PR");

    // A page of one message names the cursor for the page before it, which
    // ends exactly where this one began.
    let last = read(&host, &Authority::default(), query(&handle, None, 1)).unwrap();
    assert_eq!(last.entries.len(), 2, "{:#?}", last.entries);
    assert!(
        matches!(&last.entries[1].content, Content::User(text) if text.text == "Now open a PR")
    );
    let cursor = last.earlier.clone().expect("earlier messages remain");
    let before = read(
        &host,
        &Authority::default(),
        query(&handle, Some(&cursor), 1),
    )
    .unwrap();
    assert!(
        matches!(&before.entries.last().unwrap().content, Content::Assistant { text, .. } if text.text.starts_with("The cookie")),
        "{:#?}",
        before.entries
    );
    assert!(before.earlier.is_some());
    let unknown = read(
        &host,
        &Authority::default(),
        query(&handle, Some("no-such-entry"), 1),
    )
    .unwrap_err();
    assert!(
        unknown.contains("no-such-entry") || unknown.contains("cursor"),
        "{unknown}"
    );

    // Addressing follows `cagents send`, and refuses what it cannot name.
    for (handle, expected) in [
        ("abc", "at least 4"),
        ("ffffffff", "No session"),
        ("parent", "no parent"),
    ] {
        let error = read(&host, &Authority::default(), query(handle, None, 1)).unwrap_err();
        assert!(error.contains(expected), "{handle}: {error}");
    }
    let refused = read(&host, &Authority::default(), query(&handle, None, 0)).unwrap_err();
    assert!(refused.contains("between 1 and"), "{refused}");

    // An agent reads as the user would: its own authority grants no more.
    let authority = registry(&host)
        .unwrap()
        .lock()
        .unwrap()
        .retained_agent_authority(session)
        .unwrap();
    let mine = read(&host, &authority, query(&handle, None, 10)).unwrap();
    assert_eq!(mine.entries, page.entries);
}

#[test]
fn channel_directory_lists_members_with_activity_including_an_exited_chat() {
    use crate::{
        model::{CardId, CardKind},
        workspace_model::views::Pane,
    };
    if isolated("host::agent_execution::tests::transcript::channel_directory_lists_members_with_activity_including_an_exited_chat").is_some() {
        return;
    }
    let (host, request, path) = fixture();
    let root = path.parent().unwrap();
    let project = root.join("project");
    let accepted = submit(&host, &Authority::default(), request.clone()).unwrap();
    assert_eq!(settled(&host, accepted.run.id).state, State::Succeeded);
    let channel = ChannelId::new();
    let other = ChannelId::new();
    let (session, authority) = {
        let owned = registry(&host).unwrap();
        let mut model = owned.lock().unwrap();
        let session = model.agent(accepted.agent.id).unwrap().session.unwrap();
        let crate::agent_execution::HostedSession::Chat(chat) = session.clone() else {
            panic!("chat caller")
        };
        model
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository: project.clone(),
                title: "Login".into(),
                title_source: Default::default(),
            })
            .unwrap();
        model
            .apply(&ModelEdit::CreateChannel {
                id: other,
                repository: project.clone(),
                title: "Other".into(),
                title_source: Default::default(),
            })
            .unwrap();
        model
            .apply(&ModelEdit::SeedView {
                scope: Placement::Channel(channel),
                closed: vec![],
                panes: vec![Pane {
                    id: CardId::from_uuid(accepted.agent.id.as_uuid()),
                    title: "Caller".into(),
                    user_title: None,
                    pending_terminal_start: false,
                    width_fraction: 1.0,
                    kind: CardKind::Chat(chat),
                    scope: Some(project.clone()),
                }],
            })
            .unwrap();
        (
            session.id(),
            model.retained_agent_authority(session.id()).unwrap(),
        )
    };

    save_claude_transcript(&host, session, root);

    // A caller outside every channel must name one.
    let unplaced = members(&host, &Authority::default(), None).unwrap_err();
    assert!(unplaced.contains("--channel"), "{unplaced}");
    let unknown = members(&host, &Authority::default(), Some(ChannelId::new())).unwrap_err();
    assert!(unknown.contains("No channel"), "{unknown}");

    let mine = members(&host, &authority, None).unwrap();
    assert_eq!(mine.channel, channel);
    let me = mine.caller.as_ref().unwrap();
    assert_eq!(me.session.id, session);
    assert!(me.active_at.is_some(), "the saved conversation's time");
    // Another channel lists only its own members, never the caller.
    let elsewhere = members(&host, &authority, Some(other)).unwrap();
    assert_eq!(elsewhere.channel, other);
    assert_eq!(elsewhere.caller, None);
    assert!(elsewhere.sessions.is_empty(), "{elsewhere:?}");
    let named = members(&host, &Authority::default(), Some(channel)).unwrap();
    assert_eq!(
        named
            .sessions
            .iter()
            .map(|member| member.session.id)
            .collect::<Vec<_>>(),
        vec![session]
    );
    assert_ne!(
        named.sessions[0].session.title.as_deref(),
        Some("Login"),
        "a member is not named after its channel"
    );
    // Nor, in a wider listing, is a session in the caller's own channel.
    let wider = directory(&host, &authority, Scope::All).unwrap();
    assert_ne!(
        wider.caller.as_ref().unwrap().title.as_deref(),
        Some("Login")
    );
    assert_eq!(
        directory(&host, &Authority::default(), Scope::All)
            .unwrap()
            .sessions
            .iter()
            .find(|entry| entry.id == session)
            .and_then(|entry| entry.title.as_deref()),
        Some("Login"),
        "outside the channel, its title still hints at its work"
    );

    // A provider that exits, as at a usage limit, stays listed and readable
    // until its card is closed.
    let handle = host.lock().unwrap().chats[&session].handle.clone();
    let mut exit = request;
    exit.destination = Destination::Existing(accepted.agent.id);
    exit.idempotency_key = "exit".into();
    exit.prompt = "EXIT_NOW".into();
    let exiting = submit(&host, &Authority::default(), exit).unwrap();
    settled(&host, exiting.run.id);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !handle.is_completed() {
        assert!(Instant::now() < deadline, "the provider never exited");
        std::thread::sleep(Duration::from_millis(10));
    }
    let after = members(&host, &Authority::default(), Some(channel)).unwrap();
    let exited = after
        .sessions
        .iter()
        .find(|member| member.session.id == session)
        .expect("an exited chat stays in its channel's listing");
    assert!(exited.exited, "{exited:?}");
    let page = read(
        &host,
        &Authority::default(),
        query(&session.to_string(), None, 2),
    )
    .unwrap();
    assert!(page.session.exited);
    assert!(
        matches!(&page.entries.last().unwrap().content, Content::User(text) if text.text == "Now open a PR")
    );
    // The ordinary project listing still shows only live sessions.
    let live = directory(&host, &Authority::default(), Scope::All).unwrap();
    assert!(live.sessions.iter().all(|entry| entry.id != session));
}
