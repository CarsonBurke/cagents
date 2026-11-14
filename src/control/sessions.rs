use super::*;
use crate::agent_execution::{
    Authority, Operation, Reply,
    directory::{Entry, Member, Members, Relation, Scope, SessionAccount, Sessions, UsageLimit},
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
    let channel = directory.caller.as_ref().and_then(|me| me.session.channel);
    let now = now_ms();
    let mut human = match &directory.caller {
        Some(me) => list_human(std::iter::once(me).map(Row::from), channel, now),
        None => "Not running inside a cagents session".to_owned(),
    };
    let peers: Vec<_> = directory
        .sessions
        .iter()
        .filter(|member| {
            let entry = &member.session;
            directory.caller.is_none()
                || entry.relation != Relation::Peer
                || channel.is_some() && entry.channel == channel
                || shares_checkout(&directory, entry)
        })
        .collect();
    if !peers.is_empty() {
        human.push_str("\n\nRelated sessions:\n");
        human.push_str(&list_human(
            peers.iter().copied().map(Row::from),
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
            "me": directory.caller.as_ref().map(session_json),
            "scope": directory.scope,
            "sessions": peers.into_iter().map(session_json).collect::<Vec<_>>(),
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
    let channel = directory.caller.as_ref().and_then(|me| me.session.channel);
    let mut human = list_human(
        directory
            .caller
            .iter()
            .chain(directory.sessions.iter())
            .map(Row::from),
        channel,
        now_ms(),
    );
    if let Scope::Project(project) = &directory.scope {
        human.push_str(&format!(
            "\n(project {}; --all for every project, --channel for yours)",
            super::format::plain(&project.display().to_string())
        ));
    }
    Ok(CommandOutput {
        command: "sessions",
        data: json!({
            "me": directory.caller.as_ref().map(session_json),
            "scope": directory.scope,
            "sessions": directory.sessions.iter().map(session_json).collect::<Vec<_>>(),
        }),
        human,
    })
}

/// A listed session as `--json` reports it: its directory entry, with its
/// usage limit and account. Its `state` reads `Limited` while a usage limit
/// stops it, whatever activity the provider last reported.
pub(super) fn session_json(member: &Member) -> Value {
    let mut value = json!(member.session);
    if member.limit.is_some() {
        value["state"] = json!("Limited");
    }
    value["limit"] = json!(member.limit);
    value["account"] = json!(member.account);
    value["failure"] = json!(member.failure);
    value
}

/// A channel member as `--json` reports it, with its session's `state` as
/// [`session_json`] names it.
fn member_json(member: &Member) -> Value {
    let mut value = json!(member);
    if member.limit.is_some() {
        value["session"]["state"] = json!("Limited");
    }
    value
}

/// A usage limit in a phrase: "Claude weekly limit reached, resets in 9 hr".
/// A reset the account's usage reading dates is given relative to `now`, in
/// Unix milliseconds; otherwise as the provider worded it.
pub(super) fn limit_phrase(limit: &UsageLimit, now: u64) -> String {
    let mut phrase = limit.summary.clone();
    match (limit.resets_at.filter(|at| *at >= 0), &limit.reset) {
        (Some(at), _) => {
            phrase.push_str(", resets ");
            phrase.push_str(&super::format::relative_time(
                (at as u64).saturating_mul(1000),
                now,
            ));
        }
        (None, Some(reset)) => {
            phrase.push_str(", resets ");
            phrase.push_str(reset);
        }
        (None, None) => {}
    }
    phrase
}

/// An account and what is left of it, in a phrase:
/// "claude/system: 17% of Weekly left, resets in 3 days".
pub(super) fn account_phrase(account: &SessionAccount, now: u64) -> String {
    let name = match &account.label {
        Some(label) => format!("{} ({label})", account.id),
        None => account.id.to_string(),
    };
    let Some(window) = account.tightest() else {
        return match account.status {
            Some(crate::account::UsageStatus::NeedsLogin) => format!("{name}: needs sign-in"),
            _ => format!("{name}: allowance unknown"),
        };
    };
    let mut phrase = format!(
        "{name}: {:.0}% of {} left",
        (window.remaining_fraction * 100.0).floor(),
        window.window.label
    );
    if let Some(at) = window.window.resets_at.filter(|at| *at >= 0) {
        phrase.push_str(", resets ");
        phrase.push_str(&super::format::relative_time(
            (at as u64).saturating_mul(1000),
            now,
        ));
    }
    if account.stale {
        phrase.push_str(" (as last read; may be out of date)");
    }
    phrase
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
    } = &members;
    Ok(CommandOutput {
        command: "sessions",
        data: json!({
            "channel": channel,
            "me": caller.as_ref().map(member_json),
            "sessions": sessions.iter().map(member_json).collect::<Vec<_>>(),
        }),
        human,
    })
}

fn members_human(members: &Members, now: u64) -> String {
    let Members {
        channel,
        caller,
        sessions,
    } = members;
    if caller.is_none() && sessions.is_empty() {
        return format!("No sessions in channel {channel}");
    }
    // Every row shares the channel the footer names, so none is marked.
    let mut human = list_human(caller.iter().chain(sessions).map(Row::from), None, now);
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
    state: &'static str,
    failure: Option<&'a str>,
    active_at: Option<u64>,
    limit: Option<&'a UsageLimit>,
    account: Option<&'a SessionAccount>,
}

impl<'a> From<&'a Member> for Row<'a> {
    fn from(member: &'a Member) -> Self {
        Self {
            entry: &member.session,
            state: member.state_label(),
            failure: member.failure.as_deref(),
            active_at: member.active_at,
            limit: member.limit.as_ref(),
            account: member.account.as_ref(),
        }
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
    use crate::agent_execution::directory::Owner;
    super::format::table(
        ["SESSION", "PROVIDER", "STATE", "CONTEXT"],
        rows.into_iter()
            .map(|row| {
                let entry = row.entry;
                let mut context = Vec::new();
                if let Some(limit) = row.limit {
                    context.push(format!("stopped: {}", limit_phrase(limit, now)));
                }
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
                // A limit already says what failed.
                if let Some(failure) = row.failure.filter(|_| row.limit.is_none()) {
                    context.push(format!("failed: {failure}"));
                }
                if let Some(account) = row.account {
                    context.push(format!("account {}", account_phrase(account, now)));
                }
                [
                    entry.id.to_string(),
                    entry.provider.map_or("shell", |p| p.executable()).into(),
                    row.state.into(),
                    context.join("; "),
                ]
            })
            .collect(),
        "No live sessions",
    )
}

/// Peers in the caller's own checkout are the ones most likely to collide.
fn shares_checkout(directory: &Sessions, entry: &Entry) -> bool {
    directory
        .caller
        .as_ref()
        .and_then(|me| me.session.worktree.as_ref())
        .is_some_and(|worktree| entry.worktree.as_ref() == Some(worktree))
}

fn current_directory() -> Result<PathBuf, AppError> {
    std::env::current_dir().map_err(|e| AppError::new("target", e))
}

fn read(scope: Scope) -> Result<Sessions, AppError> {
    request(Operation::Sessions(scope), |reply| match reply {
        Reply::Sessions(sessions) => Some(sessions),
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
            lineage_channel: None,
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

    fn live(session: Entry) -> Member {
        Member {
            session,
            exited: false,
            failure: None,
            active_at: None,
            limit: None,
            account: None,
        }
    }

    fn account(remaining: f32, stale: bool) -> SessionAccount {
        use crate::account::{UsageStatus, UsageWindow, WindowKind, info::RemainingWindow};
        SessionAccount {
            id: "claude/system".parse().unwrap(),
            label: Some("work".into()),
            status: Some(UsageStatus::Warning),
            remaining: vec![RemainingWindow {
                window: UsageWindow {
                    key: "weekly".into(),
                    kind: WindowKind::Weekly,
                    label: "Weekly".into(),
                    used: 1.0 - remaining,
                    resets_at: Some(3 * 3600),
                    span_secs: None,
                    binding: true,
                },
                remaining_fraction: remaining,
                reset_elapsed: false,
            }],
            stale,
            observed_at: None,
            ordinary_usage_allowed: None,
        }
    }

    #[test]
    fn a_limited_session_reads_limited_with_its_reset_and_account() {
        let mut member = live(entry());
        member.failure = Some("Claude weekly limit reached · resets 3pm".into());
        member.limit = Some(UsageLimit {
            summary: "Claude weekly limit reached".into(),
            reset: Some("3pm".into()),
            resets_at: Some(3 * 3600),
        });
        member.account = Some(account(0.0, false));
        let human = list_human(std::iter::once(Row::from(&member)), None, 3600 * 1000);
        let row = human.lines().nth(1).unwrap();
        assert!(row.contains(" limited "), "{row}");
        assert!(
            row.contains("stopped: Claude weekly limit reached, resets in 2 hr"),
            "{row}"
        );
        assert!(
            !row.contains("failed:"),
            "the limit already says what failed: {row}"
        );
        assert!(
            row.ends_with("; account claude/system (work): 0% of Weekly left, resets in 2 hr"),
            "{row}"
        );

        let value = session_json(&member);
        assert_eq!(value["state"], "Limited");
        assert_eq!(value["limit"]["summary"], "Claude weekly limit reached");
        assert_eq!(value["limit"]["resets_at"], 3 * 3600);
        assert_eq!(value["account"]["id"], "claude/system");
        assert_eq!(value["account"]["remaining"][0]["remaining_fraction"], 0.0);
        assert_eq!(value["id"], json!(member.session.id));

        // Without a dated reading, the provider's own words date the reset.
        member.limit.as_mut().unwrap().resets_at = None;
        assert_eq!(
            limit_phrase(member.limit.as_ref().unwrap(), 0),
            "Claude weekly limit reached, resets 3pm"
        );
        member.limit = None;
        let value = session_json(&member);
        assert_eq!(value["state"], "Waiting");
        assert!(value["limit"].is_null());
    }

    #[test]
    fn an_account_phrase_says_when_its_reading_is_stale_or_missing() {
        assert_eq!(
            account_phrase(&account(0.42, true), 3600 * 1000),
            "claude/system (work): 42% of Weekly left, resets in 2 hr (as last read; may be out of date)"
        );
        let mut unknown = account(0.42, false);
        unknown.remaining.clear();
        unknown.label = None;
        assert_eq!(
            account_phrase(&unknown, 0),
            "claude/system: allowance unknown"
        );
        unknown.status = Some(crate::account::UsageStatus::NeedsLogin);
        assert_eq!(account_phrase(&unknown, 0), "claude/system: needs sign-in");
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
            limit: None,
            account: None,
        };
        let mut members = Members {
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
        let member = live(entry.clone());
        let human = list_human(std::iter::once(Row::from(&member)), None, 0);
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
        let member = live(entry);
        assert_eq!(
            list_human(std::iter::once(Row::from(&member)), None, 0),
            "SESSION                               PROVIDER  STATE    CONTEXT\n00000000-0000-4000-8000-000000000001  shell     unknown  me"
        );
    }
}
