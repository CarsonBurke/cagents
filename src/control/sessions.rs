use super::*;
use crate::agent_execution::{
    Authority, Operation, Reply,
    directory::{Directory, Entry, Member, Members, Relation, Scope},
};
use crate::model::ChannelId;

#[derive(Debug, clap::Args)]
pub struct SessionsArgs {
    /// List sessions in every project, not just the current one.
    #[arg(long, conflicts_with_all = ["project", "channel"])]
    all: bool,
    /// List sessions in the open project with this name, or containing
    /// this path.
    #[arg(long, conflicts_with = "channel")]
    project: Option<PathBuf>,
    /// List the sessions in this channel, or in yours when no UUID is given,
    /// with how recently each was active. Includes chats whose provider exited.
    #[arg(long, value_name = "CHANNEL", num_args = 0..=1, default_missing_value = "")]
    channel: Option<String>,
}

/// Who is calling, and which other sessions share its channel or checkout.
pub(super) fn whoami() -> Result<CommandOutput, AppError> {
    let directory = read(Scope::Containing(current_directory()?))?;
    let channel = directory.caller.as_ref().and_then(|me| me.channel);
    let now = now_ms();
    let mut human = match &directory.caller {
        Some(me) => list_human(std::iter::once(row(&directory, me)), channel, now),
        None => "Not running inside a cagents session".to_owned(),
    };
    let peers: Vec<_> = directory
        .sessions
        .iter()
        .filter(|entry| {
            directory.caller.is_none()
                || entry.relation != Relation::Peer
                || channel.is_some() && entry.channel == channel
                || shares_checkout(&directory, entry)
        })
        .collect();
    if !peers.is_empty() {
        human.push_str("\n\nRelated sessions:\n");
        human.push_str(&list_human(
            peers.iter().map(|entry| row(&directory, entry)),
            channel,
            now,
        ));
    }
    if channel.is_some() {
        human.push_str(
            "\n\n`cagents ls --channel` lists your channel, including ended sessions; `cagents transcript SESSION` reads one.",
        );
    }
    Ok(CommandOutput {
        command: "whoami",
        data: json!({
            "me": directory.caller,
            "scope": directory.scope,
            "sessions": peers,
            "saved_transcripts": directory.saved_transcripts,
        }),
        human,
    })
}

pub(super) fn list(arguments: SessionsArgs) -> Result<CommandOutput, AppError> {
    if let Some(channel) = arguments.channel {
        let channel =
            if channel.is_empty() {
                None
            } else {
                Some(channel.parse().map_err(|_| {
                    AppError::new("target", format!("{channel} is not a channel UUID"))
                })?)
            };
        return members(channel);
    }
    let scope = if arguments.all {
        Scope::All
    } else if let Some(project) = arguments.project {
        Scope::Project(super::project_argument(project)?)
    } else {
        Scope::Containing(current_directory()?)
    };
    let directory = read(scope)?;
    let channel = directory.caller.as_ref().and_then(|me| me.channel);
    let mut human = list_human(
        directory
            .caller
            .iter()
            .chain(directory.sessions.iter())
            .map(|entry| row(&directory, entry)),
        channel,
        now_ms(),
    );
    human.push_str(&no_shared_pane_guidance(&directory));
    if let Scope::Project(project) = &directory.scope {
        human.push_str(&format!(
            "\n(project {}; --all for every project, --channel for yours)",
            super::format::plain(&project.display().to_string())
        ));
    }
    Ok(CommandOutput {
        command: "sessions",
        data: json!({
            "me": directory.caller,
            "scope": directory.scope,
            "sessions": directory.sessions,
            "saved_transcripts": directory.saved_transcripts,
        }),
        human,
    })
}

/// What to do about a listed session that no shared view holds. Empty when
/// there is none.
fn no_shared_pane_guidance(directory: &Directory) -> String {
    let unplaced = directory
        .caller
        .iter()
        .chain(&directory.sessions)
        .filter(|entry| entry.has_no_shared_pane() && !directory.is_saved_transcript(entry))
        .collect::<Vec<_>>();
    if unplaced.is_empty() {
        return String::new();
    }
    let attachable = unplaced
        .iter()
        .filter(|entry| entry.project.is_some())
        .collect::<Vec<_>>();
    let mut guidance =
        "\n\nA session with no shared pane may still be open in a window.".to_owned();
    if let [entry] = attachable.as_slice() {
        let path = entry.directory.as_deref().map_or_else(
            || "PATH".to_owned(),
            |path| super::format::plain(&path.display().to_string()),
        );
        guidance.push_str(&format!(
            " If none shows it, put it in a channel:\n  cagents channel move {} --to CHANNEL_ID\nFind a channel with `cagents channel list --repository {path}`, or create one with `cagents channel spawn --repository {path}`.",
            entry.id
        ));
    } else if !attachable.is_empty() {
        guidance.push_str(
            " If none shows one, put it in a channel:\n  cagents channel move SESSION --to CHANNEL_ID\nFind a channel with `cagents channel list --repository PATH`, or create one with `cagents channel spawn --repository PATH`.",
        );
    }
    if attachable.len() < unplaced.len() {
        guidance.push_str(
            "\nA session outside every open project can't go in a channel until its project is open: `cagents project open PATH`.",
        );
    }
    if directory.caller.is_some() {
        guidance.push_str(if attachable.len() < unplaced.len() {
            "\nSessions inside cagents can't open projects or place panes, so run that from a terminal outside cagents."
        } else {
            "\nSessions inside cagents can't place panes, so run that from a terminal outside cagents."
        });
    }
    guidance
}

/// Everyone in a channel, with how recently each worked and how it ended.
fn members(channel: Option<ChannelId>) -> Result<CommandOutput, AppError> {
    let members = request(Operation::Members(channel), |reply| match reply {
        Reply::Members(members) => Some(members),
        _ => None,
    })?;
    let human = members_human(&members, now_ms());
    let Members {
        channel,
        caller,
        sessions,
        saved_transcripts,
    } = &members;
    Ok(CommandOutput {
        command: "sessions",
        data: json!({
            "channel": channel,
            "me": caller,
            "sessions": sessions,
            "saved_transcripts": saved_transcripts,
        }),
        human,
    })
}

fn members_human(members: &Members, now: u64) -> String {
    let Members {
        channel,
        caller,
        sessions,
        saved_transcripts,
    } = members;
    if caller.is_none() && sessions.is_empty() {
        return format!("No sessions in channel {channel}");
    }
    // Every row shares the channel the footer names, so none is marked.
    let rows = caller.iter().chain(sessions).map(|member| Row {
        saved_transcript: saved_transcripts.contains(&member.session.id),
        ..Row::from(member)
    });
    let mut human = list_human(rows, None, now);
    let whose = if caller.is_some() {
        "your channel"
    } else {
        "channel"
    };
    human.push_str(&format!(
        "\n({whose} {channel}; read a conversation with `cagents transcript SESSION`)"
    ));
    human
}

/// One listed session, with what a member view adds to it.
pub(super) struct Row<'a> {
    entry: &'a Entry,
    exited: bool,
    failure: Option<&'a str>,
    active_at: Option<u64>,
    saved_transcript: bool,
}

impl<'a> From<&'a Entry> for Row<'a> {
    fn from(entry: &'a Entry) -> Self {
        Self {
            entry,
            exited: false,
            failure: None,
            active_at: None,
            saved_transcript: false,
        }
    }
}

impl<'a> From<&'a Member> for Row<'a> {
    fn from(member: &'a Member) -> Self {
        Self {
            entry: &member.session,
            exited: member.exited,
            failure: member.failure.as_deref(),
            active_at: member.active_at,
            saved_transcript: false,
        }
    }
}

fn row<'a>(directory: &Directory, entry: &'a Entry) -> Row<'a> {
    Row {
        saved_transcript: directory.is_saved_transcript(entry),
        ..Row::from(entry)
    }
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// `channel` is the caller's, so a session sharing it is marked.
pub(super) fn list_human<'a>(
    rows: impl IntoIterator<Item = Row<'a>>,
    channel: Option<crate::model::ChannelId>,
    now: u64,
) -> String {
    use crate::agent::ActivityState;
    use crate::agent_execution::directory::Owner;
    super::format::table(
        ["SESSION", "PROVIDER", "STATE", "CONTEXT"],
        rows.into_iter()
            .map(|row| {
                let entry = row.entry;
                let mut context = Vec::new();
                if let Some(project) = &entry.project_name {
                    context.push(format!("[{project}]"));
                }
                if let Some(directory) = &entry.directory {
                    context.push(directory.display().to_string());
                }
                if let Some(title) = entry.title.as_deref().filter(|title| !title.is_empty()) {
                    context.push(format!("\"{title}\""));
                }
                if let Some(crate::agent_execution::Placement::Worktree { path, .. }) =
                    &entry.view_scope
                {
                    context.push(format!("worktree view {}", path.display()));
                }
                if row.saved_transcript {
                    context.push("saved transcript".to_owned());
                } else if entry.has_no_shared_pane() && !row.exited {
                    context.push("no shared pane".to_owned());
                }
                if entry.relation != Relation::Myself
                    && channel.is_some()
                    && entry.channel == channel
                {
                    context.push("my channel".into());
                }
                match (entry.relation, entry.delegation.as_ref().map(|d| &d.owner)) {
                    (Relation::Myself, _) => context.push("me".into()),
                    (Relation::Parent, _) => context.push("my parent".into()),
                    (Relation::Child, Some(Owner::User)) => context.push("user agent".into()),
                    (Relation::Child, _) => context.push("my agent".into()),
                    (Relation::Peer, _) => {}
                }
                if entry.lineage.0 != entry.id {
                    context.push(format!("lineage {}", entry.lineage));
                }
                if let Some(delegation) = &entry.delegation {
                    context.push(format!("agent {}", delegation.agent));
                    if let Some(state) = delegation.run_state {
                        context.push(format!("latest run {}", super::agents::state_label(state)));
                    }
                }
                if let Some(at) = row.active_at {
                    context.push(format!("active {}", super::format::relative_time(at, now)));
                }
                if let Some(failure) = row.failure {
                    context.push(format!("failed: {failure}"));
                }
                [
                    entry.id.to_string(),
                    entry.provider.map_or("shell", |p| p.executable()).into(),
                    match entry.state {
                        _ if row.exited => "exited",
                        ActivityState::Unknown => "unknown",
                        ActivityState::Idle => "idle",
                        ActivityState::Working => "working",
                        ActivityState::Waiting => "waiting",
                    }
                    .into(),
                    context.join("; "),
                ]
            })
            .collect(),
        "No live sessions",
    )
}

/// Peers in the caller's own checkout are the ones most likely to collide.
fn shares_checkout(directory: &Directory, entry: &Entry) -> bool {
    directory
        .caller
        .as_ref()
        .and_then(|me| me.worktree.as_ref())
        .is_some_and(|worktree| entry.worktree.as_ref() == Some(worktree))
}

fn current_directory() -> Result<PathBuf, AppError> {
    std::env::current_dir().map_err(|e| AppError::new("target", e))
}

fn read(scope: Scope) -> Result<Directory, AppError> {
    request(Operation::Directory(scope), |reply| match reply {
        Reply::Directory(directory) => Some(directory),
        _ => None,
    })
}

fn request<T>(
    operation: Operation,
    expected: impl FnOnce(Reply) -> Option<T>,
) -> Result<T, AppError> {
    let socket = control_daemon_socket()?;
    let mut client =
        ControlClient::connect(&socket).map_err(|e| AppError::new("daemon_connection", e))?;
    let reply = client
        .agent(Authority::from_environment(), operation)
        .map_err(protocol_error)?;
    expected(reply).ok_or_else(|| AppError::new("sessions", "Unexpected reply from the daemon"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ActivityState;
    use crate::agent_execution::{
        Lineage, Placement, State,
        directory::{Delegation, Owner, Surface},
    };

    fn entry() -> Entry {
        let id = "00000000-0000-4000-8000-000000000001".parse().unwrap();
        Entry {
            id,
            surface: Surface::Chat,
            provider: Some(crate::agent::Provider::Claude),
            title: Some("review\n\u{1b}title".into()),
            channel: None,
            view_scope: Some(Placement::Worktree {
                repository: "/fixture/.git".into(),
                path: "/fixture/linked".into(),
            }),
            channel_repository: None,
            state: ActivityState::Waiting,
            directory: Some("/fixture/linked".into()),
            worktree: Some("/fixture/linked".into()),
            project: Some("/fixture/.git".into()),
            project_name: Some("Fixture".into()),
            delegation: Some(Delegation {
                agent: "00000000-0000-4000-8000-000000000002".parse().unwrap(),
                owner: Owner::User,
                run: Some("00000000-0000-4000-8000-000000000004".parse().unwrap()),
                run_state: Some(State::Interrupted),
            }),
            relation: Relation::Child,
            lineage: Lineage("00000000-0000-4000-8000-000000000003".parse().unwrap()),
        }
    }

    #[test]
    fn a_channel_lists_how_each_member_ended_and_when_it_last_worked() {
        let mut session = entry();
        session.relation = Relation::Peer;
        session.delegation = None;
        session.view_scope = None;
        session.title = None;
        let channel = "00000000-0000-4000-8000-0000000000c1".parse().unwrap();
        let exited = Member {
            session,
            exited: true,
            failure: Some("Claude usage limit reached · resets 3pm".into()),
            active_at: Some(0),
        };
        let mut members = Members {
            saved_transcripts: vec![],
            channel,
            caller: None,
            sessions: vec![exited],
        };
        let human = members_human(&members, 12 * 60_000);
        let row = human.lines().nth(1).unwrap();
        assert!(row.contains(" exited "), "{row}");
        assert!(row.contains("; active 12 min ago"), "{row}");
        assert!(
            row.ends_with("; failed: Claude usage limit reached · resets 3pm"),
            "{row}"
        );
        assert!(!row.contains("my channel"), "{row}");
        assert!(
            human.ends_with(&format!(
                "\n(channel {channel}; read a conversation with `cagents transcript SESSION`)"
            )),
            "{human}"
        );

        let mut me = members.sessions[0].clone();
        me.session.relation = Relation::Myself;
        members.caller = Some(me);
        assert!(members_human(&members, 0).contains(&format!("\n(your channel {channel};")));

        // A reader in the listing says so; only the daemon's list marks one.
        let mut reader = members.sessions[0].clone();
        reader.exited = false;
        reader.failure = None;
        reader.session.relation = Relation::Peer;
        let reader_id = reader.session.id;
        members.caller = None;
        members.sessions = vec![reader];
        assert!(!members_human(&members, 0).contains("saved transcript"));
        members.saved_transcripts = vec![reader_id];
        assert!(members_human(&members, 0).contains("saved transcript"));
        members.saved_transcripts.clear();

        members.caller = None;
        members.sessions.clear();
        assert_eq!(
            members_human(&members, 0),
            format!("No sessions in channel {channel}")
        );
    }

    #[test]
    fn readable_session_rows_keep_original_identity_and_delegation_context() {
        let entry = entry();
        let before = serde_json::to_value(&entry).unwrap();
        let human = list_human(std::iter::once(Row::from(&entry)), None, 0);
        assert_eq!(human.lines().count(), 2);
        assert_eq!(
            human
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>(),
            ["SESSION", "PROVIDER", "STATE", "CONTEXT"]
        );
        for expected in [
            "00000000-0000-4000-8000-000000000001",
            "claude",
            "waiting",
            "[Fixture]",
            "\"review  title\"",
            "worktree view /fixture/linked",
            "user agent",
            "lineage 00000000-0000-4000-8000-000000000003",
            "agent 00000000-0000-4000-8000-000000000002",
            "latest run Outcome uncertain",
        ] {
            assert!(human.contains(expected), "{human}");
        }
        for debug in ["Some(", "None", "Interrupted", "{ ", "_ms"] {
            assert!(!human.contains(debug));
        }
        assert!(!human.contains('\u{1b}'));
        assert_eq!(serde_json::to_value(&entry).unwrap(), before);
    }

    #[test]
    fn sessions_with_no_shared_pane_are_marked_and_explain_what_works_from_here() {
        let directory = |caller: Option<Entry>, sessions: Vec<Entry>| Directory {
            caller,
            scope: Scope::All,
            sessions,
            saved_transcripts: vec![],
        };
        let mut entry = entry();
        let id = entry.id;
        // A delegated session is placed through its agent, whatever its view.
        entry.view_scope = None;
        assert!(
            !list_human(std::iter::once(Row::from(&entry)), None, 0).contains("no shared pane")
        );
        assert_eq!(
            no_shared_pane_guidance(&directory(None, vec![entry.clone()])),
            ""
        );

        entry.delegation = None;
        assert!(list_human(std::iter::once(Row::from(&entry)), None, 0).contains("no shared pane"));
        let outside = no_shared_pane_guidance(&directory(None, vec![entry.clone()]));
        assert!(outside.contains(&format!("cagents channel move {id} --to CHANNEL_ID")));
        assert!(outside.contains("cagents channel list --repository /fixture/linked"));
        assert!(outside.contains("cagents channel spawn --repository /fixture/linked"));
        assert!(!outside.contains("outside cagents"));

        let mut me = entry.clone();
        me.id = "00000000-0000-4000-8000-000000000009".parse().unwrap();
        me.relation = Relation::Myself;
        me.view_scope = Some(crate::agent_execution::Placement::Channel(
            crate::model::ChannelId::new(),
        ));
        let inside = no_shared_pane_guidance(&directory(Some(me.clone()), vec![entry.clone()]));
        assert!(inside.contains("can't place panes"));
        assert!(inside.contains("from a terminal outside cagents"));
        assert!(!inside.contains("open projects"));

        // Opening its project is refused in there too, so that gets the same warning.
        let mut closed = entry.clone();
        closed.project = None;
        let hosted = no_shared_pane_guidance(&directory(Some(me), vec![closed]));
        assert!(hosted.contains("`cagents project open"));
        assert!(hosted.contains("can't open projects"));

        // A saved transcript open for reading has nothing running to place.
        let reader = entry.clone();
        let row = list_human(
            std::iter::once(Row {
                saved_transcript: true,
                ..Row::from(&reader)
            }),
            None,
            0,
        );
        assert!(row.contains("saved transcript"));
        assert!(!row.contains("no shared pane"));
        let saved_transcripts = vec![reader.id];
        assert_eq!(
            no_shared_pane_guidance(&Directory {
                saved_transcripts,
                ..directory(None, vec![reader])
            }),
            ""
        );

        // A channel only accepts sessions of its own project.
        entry.project = None;
        let unopened = no_shared_pane_guidance(&directory(None, vec![entry]));
        assert!(unopened.contains("cagents project open"));
        assert!(!unopened.contains("channel move"));
    }

    #[test]
    fn readable_sessions_empty_and_plain_shell_rows_are_explicit() {
        assert_eq!(list_human(std::iter::empty(), None, 0), "No live sessions");
        let mut entry = entry();
        entry.provider = None;
        entry.surface = Surface::Terminal;
        entry.state = ActivityState::Unknown;
        entry.relation = Relation::Myself;
        entry.delegation = None;
        entry.title = None;
        entry.directory = None;
        entry.worktree = None;
        entry.project = None;
        entry.project_name = None;
        entry.view_scope = None;
        entry.lineage = Lineage(entry.id);
        assert_eq!(
            list_human(std::iter::once(Row::from(&entry)), None, 0),
            "SESSION                               PROVIDER  STATE    CONTEXT\n00000000-0000-4000-8000-000000000001  shell     unknown  me"
        );
    }
}
