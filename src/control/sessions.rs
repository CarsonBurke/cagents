use super::fields::{self, Detail, Row as Fields};
use super::*;
use crate::agent_execution::{
    Authority, Operation, Reply,
    directory::{
        Entry, Member, Members, Owner, Relation, Scope, SessionAccount, Sessions, Surface,
        UsageLimit,
    },
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
    #[command(flatten)]
    fields: FieldsArgs,
}

impl SessionsArgs {
    pub(super) fn selects_fields(&self) -> bool {
        self.fields.selected()
    }
}

/// `--fields` on a listing of sessions.
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct FieldsArgs {
    #[arg(long, value_name = "FIELDS", value_delimiter = ',', help = fields::HELP,
        value_parser = clap::builder::PossibleValuesParser::new(FIELDS))]
    pub(super) fields: Vec<String>,
}

impl FieldsArgs {
    pub(super) fn selected(&self) -> bool {
        !self.fields.is_empty()
    }
}

/// Who is calling, and which other sessions share its channel or checkout.
pub(super) fn whoami(arguments: FieldsArgs) -> Result<CommandOutput, AppError> {
    let directory = read(Scope::Containing(current_directory()?))?;
    let peers = related(&directory);
    Ok(CommandOutput {
        command: "whoami",
        human: whoami_human(&directory, &peers, &super::format::program(), now_ms()),
        data: listing_json(
            json!({ "scope": fields::word(&directory.scope) }),
            directory.caller.iter().chain(peers.iter().copied()),
            None,
            &arguments.fields,
            now_ms(),
        ),
    })
}

/// The sessions `whoami` lists beside the caller: its relatives, and peers
/// in its channel or checkout; everyone in scope for a caller outside
/// cagents.
fn related(directory: &Sessions) -> Vec<&Member> {
    let channel = directory.caller.as_ref().and_then(|me| me.session.channel);
    directory
        .sessions
        .iter()
        .filter(|member| {
            let entry = &member.session;
            directory.caller.is_none()
                || entry.relation != Relation::Peer
                || channel.is_some() && entry.channel == channel
                || shares_checkout(directory, entry)
        })
        .collect()
}

/// The caller first, then `peers`, in one listing, then what to run next.
/// `program` names this CLI in those commands.
fn whoami_human(directory: &Sessions, peers: &[&Member], program: &str, now: u64) -> String {
    let channel = directory.caller.as_ref().and_then(|me| me.session.channel);
    let listed = list_human(
        directory
            .caller
            .iter()
            .chain(peers.iter().copied())
            .map(Row::from),
        channel,
        None,
        now,
    );
    let human = match &directory.caller {
        Some(_) => listed,
        None if peers.is_empty() => "Not running inside a cagents session".to_owned(),
        None => format!("Not running inside a cagents session\n\n{listed}"),
    };
    let mut steps = conversing(program, peers.iter().copied());
    if channel.is_some() {
        steps.push((
            format!("{program} ls --channel"),
            "lists your channel, including ended sessions",
        ));
    }
    with_hint(human, &steps)
}

/// How to read and message the listed `sessions`, where any has a
/// conversation to read; a plain shell has none.
fn conversing<'a>(
    program: &str,
    mut sessions: impl Iterator<Item = &'a Member>,
) -> Vec<(String, &'static str)> {
    if sessions.any(|member| member.session.provider.is_some()) {
        vec![
            (format!("{program} transcript SESSION"), "reads one"),
            (format!("{program} send SESSION MESSAGE"), "messages one"),
        ]
    } else {
        Vec::new()
    }
}

/// `human`, then a line of `steps` where there are any.
fn with_hint(mut human: String, steps: &[(String, &str)]) -> String {
    if !steps.is_empty() {
        human.push('\n');
        human.push_str(&super::format::hint(steps));
    }
    human
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
        return members(channel, &arguments.fields.fields);
    }
    let scope = if arguments.all {
        Scope::All
    } else if let Some(project) = arguments.project {
        Scope::Project(super::project_argument(project)?)
    } else {
        Scope::Containing(current_directory()?)
    };
    let directory = read(scope)?;
    Ok(CommandOutput {
        command: "sessions",
        human: sessions_human(&directory, &super::format::program(), now_ms()),
        data: listing_json(
            json!({ "scope": fields::word(&directory.scope) }),
            directory.caller.iter().chain(&directory.sessions),
            None,
            &arguments.fields.fields,
            now_ms(),
        ),
    })
}

/// The caller and every session in scope, then what to run next.
/// `program` names this CLI in those commands.
fn sessions_human(directory: &Sessions, program: &str, now: u64) -> String {
    let channel = directory.caller.as_ref().and_then(|me| me.session.channel);
    let human = list_human(
        directory
            .caller
            .iter()
            .chain(directory.sessions.iter())
            .map(Row::from),
        channel,
        None,
        now,
    );
    let mut steps = conversing(program, directory.sessions.iter());
    if matches!(directory.scope, Scope::Project(_)) {
        steps.push((format!("{program} ls --all"), "lists every project"));
    }
    with_hint(human, &steps)
}

/// A listed session as `--json` reports it: its directory entry, with its
/// usage limit and account, and its prompt cache where a message would find
/// it cold. Its `state` reads `Limited` while a usage limit stops it,
/// whatever activity the provider last reported.
pub(super) fn session_json(member: &Member, now: u64) -> Value {
    let mut value = json!(member.session);
    value["lineage_channel"] = json!(member.session.lineage_channel);
    if member.limit.is_some() {
        value["state"] = json!("Limited");
    }
    value["limit"] = json!(member.limit);
    value["account"] = json!(member.account);
    value["failure"] = json!(member.failure);
    if member.cold_context(now).is_some() {
        value["cache"] = cache_json(member, now, Detail::Sparse);
    }
    value
}

/// A session's prompt cache as `--json` reports it: whether it is `cold` at
/// `now`, when it likely expires in Unix seconds, the context in tokens a
/// message would read again were it cold, and whether its lifetime was
/// `observed` or `assumed` from the provider's documentation.
pub(super) fn cache_json(member: &Member, now: u64, detail: Detail) -> Value {
    use crate::agent_execution::directory::{TtlBasis, Warmth};
    let Some(cache) = &member.cache else {
        return Value::Null;
    };
    Fields::new(detail)
        .put("cold", member.warmth(now) == Warmth::Cold)
        .put("expires_at", fields::seconds(cache.expires_at()))
        .put("context_tokens", cache.context_tokens)
        .put(
            "basis",
            match cache.basis {
                TtlBasis::Assumed => "assumed",
                TtlBasis::Observed => "observed",
            },
        )
        .into_value()
}

/// The fields `--json` reports for a listed session, which `--fields`
/// selects from.
pub(super) const FIELDS: &[&str] = &[
    "id",
    "relation",
    "provider",
    "surface",
    "state",
    "title",
    "project",
    "directory",
    "worktree",
    "worktree_view",
    "channel",
    "channel_repository",
    "lineage",
    "lineage_channel",
    "agent",
    "owner",
    "run",
    "run_state",
    "account",
    "limit",
    "failure",
    "active_at",
    "cache",
];

/// A listing of `members` as `--json` reports it: `data`, which names the
/// listing's scope, with a row for each member, the caller first, and each
/// account they run on once, with what is left of it. `channel` is the
/// one the listing names, which its rows then leave out. With `selected`
/// fields, only the rows, with only those fields.
pub(super) fn listing_json<'a>(
    mut data: Value,
    members: impl IntoIterator<Item = &'a Member>,
    channel: Option<ChannelId>,
    selected: &[String],
    now: u64,
) -> Value {
    let members: Vec<&Member> = members.into_iter().collect();
    let detail = Detail::of(selected);
    let rows = members
        .iter()
        .map(|member| session_row(member, channel, detail, now))
        .collect();
    if detail == Detail::Complete {
        return json!({ "sessions": fields::select(rows, selected) });
    }
    data["sessions"] = Value::Array(rows);
    let accounts = accounts_json(members);
    if !accounts.is_empty() {
        data["accounts"] = Value::Object(accounts);
    }
    data
}

/// A listed session as `--json` reports it, with only the facts it has.
/// `relation` is `me`, `parent` or `child`, and left out for a peer.
/// `surface` is left out for a chat. `state` is the one a listing shows:
/// `exited` or `limited` outrank the provider's last activity. `project`
/// is the open project's name, `worktree` the checkout containing
/// `directory` where that is not `directory` itself, and `account` the ID
/// the listing's `accounts` describe. `channel` is left out where the
/// listing names it, `lineage` where it is the session's own ID, and
/// `owner` where it is the caller. `active_at` is in Unix seconds. `cache`
/// is left out unless a message at `now` would find it cold.
pub(super) fn session_row(
    member: &Member,
    channel: Option<ChannelId>,
    detail: Detail,
    now: u64,
) -> Value {
    let entry = &member.session;
    let delegation = entry.delegation.as_ref();
    let relation = match entry.relation {
        Relation::Myself => "me",
        Relation::Parent => "parent",
        Relation::Child => "child",
        Relation::Peer => "peer",
    };
    let surface = match entry.surface {
        Surface::Chat => "chat",
        Surface::Terminal => "terminal",
    };
    let worktree_view = match &entry.view_scope {
        Some(crate::agent_execution::Placement::Worktree { path, .. }) => Some(path),
        _ => None,
    };
    let owner = delegation.map(|delegation| match &delegation.owner {
        Owner::User => json!("user"),
        Owner::Session(lineage) => json!(lineage),
        Owner::Automation(id) => json!({ "automation": id }),
    });
    Fields::new(detail)
        .put("id", entry.id)
        .put_unless("relation", relation, entry.relation == Relation::Peer)
        .put(
            "provider",
            entry.provider.map(|provider| provider.executable()),
        )
        .put_unless("surface", surface, entry.surface == Surface::Chat)
        .put("state", member.state_label())
        .put("title", &entry.title)
        .put("project", &entry.project_name)
        .put("directory", &entry.directory)
        .put_unless(
            "worktree",
            &entry.worktree,
            entry.worktree.is_some() && entry.worktree == entry.directory,
        )
        .put("worktree_view", worktree_view)
        .put_unless(
            "channel",
            entry.channel,
            entry.channel.is_some() && entry.channel == channel,
        )
        .put_unless(
            "channel_repository",
            &entry.channel_repository,
            entry.channel_repository.is_some() && entry.channel_repository == entry.project,
        )
        .put_unless("lineage", entry.lineage, entry.lineage.0 == entry.id)
        .put(
            "lineage_channel",
            entry.lineage_channel.as_ref().map(|home| {
                Fields::new(detail)
                    .put("id", home.id)
                    .put_unless(
                        "repository",
                        &home.repository,
                        entry.project.as_ref() == Some(&home.repository),
                    )
                    .into_value()
            }),
        )
        .put("agent", delegation.map(|delegation| delegation.agent))
        // An agent the caller owns needs no owner named.
        .put_unless("owner", owner, entry.relation == Relation::Child)
        .put("run", delegation.and_then(|delegation| delegation.run))
        .put(
            "run_state",
            delegation
                .and_then(|delegation| delegation.run_state)
                .map(fields::word),
        )
        .put(
            "account",
            member.account.as_ref().map(|account| &account.id),
        )
        .put(
            "limit",
            member.limit.as_ref().map(|limit| {
                Fields::new(detail)
                    .put("summary", &limit.summary)
                    .put("reset", &limit.reset)
                    .put("resets_at", limit.resets_at)
                    .into_value()
            }),
        )
        .put("failure", &member.failure)
        .put("active_at", member.active_at.map(fields::seconds))
        .put_unless(
            "cache",
            cache_json(member, now, detail),
            member.cold_context(now).is_none(),
        )
        .into_value()
}

/// Each account `members` run on, once, by its ID: its label, how it
/// stands, and what each of its windows has left as a whole percentage,
/// with when it resets in Unix seconds. `stale` marks a reading too old to
/// plan on.
pub(super) fn accounts_json(members: Vec<&Member>) -> serde_json::Map<String, Value> {
    let mut accounts = serde_json::Map::new();
    for account in members
        .into_iter()
        .filter_map(|member| member.account.as_ref())
    {
        if accounts.contains_key(account.id.as_str()) {
            continue;
        }
        let windows: Vec<Value> = account
            .remaining
            .iter()
            .map(|window| {
                Fields::new(Detail::Sparse)
                    .put("window", &window.window.label)
                    .put("left_percent", fields::percent(window.remaining_fraction))
                    .put("resets_at", window.window.resets_at)
                    .put("binding", window.window.binding)
                    .put("reset", window.reset_elapsed)
                    .into_value()
            })
            .collect();
        // An account nothing is known of yet still gets an entry, so every
        // row's `account` names one.
        accounts.insert(
            account.id.as_str().to_owned(),
            Fields::new(Detail::Sparse)
                .put("label", &account.label)
                .put("status", account.status.map(fields::word))
                .put("windows", windows)
                .put("stale", account.stale)
                .put("ordinary_usage_allowed", account.ordinary_usage_allowed)
                .into_value(),
        );
    }
    accounts
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
    format!("{name}: {}", allowance_phrase(account, now))
}

/// What is left of an account: "17% of Weekly left, resets in 3 days".
fn allowance_phrase(account: &SessionAccount, now: u64) -> String {
    match (account.tightest(), allowance(account, now)) {
        (None, (unknown, _)) => unknown,
        (Some(_), (left, resets)) => {
            let mut phrase = format!("{left} left");
            if let Some(resets) = resets {
                phrase.push_str(&format!(", resets {resets}"));
            }
            if account.stale {
                phrase.push_str(" (as last read; may be out of date)");
            }
            phrase
        }
    }
}

/// What is left of an account's tightest window, as a whole percentage
/// rounded down, such as "17% of Weekly", or why that is not known; and
/// when the window resets, such as "in 3 days". A listing marks a stale
/// reading after the percentage.
fn allowance(account: &SessionAccount, now: u64) -> (String, Option<String>) {
    let Some(window) = account.tightest() else {
        return (
            match account.status {
                Some(crate::account::UsageStatus::NeedsLogin) => "needs sign-in",
                _ => "allowance unknown",
            }
            .into(),
            None,
        );
    };
    let left = format!(
        "{}% of {}",
        fields::percent(window.remaining_fraction),
        window.window.label
    );
    let resets = window
        .window
        .resets_at
        .filter(|at| *at >= 0)
        .map(|at| super::format::relative_time((at as u64).saturating_mul(1000), now));
    (left, resets)
}

/// Everyone in a channel, with how recently each worked and how it ended.
fn members(channel: Option<ChannelId>, selected: &[String]) -> Result<CommandOutput, AppError> {
    let members = request(Operation::Members(channel), |reply| match reply {
        Reply::Members(members) => Some(members),
        _ => None,
    })?;
    let human = members_human(&members, &super::format::program(), now_ms());
    let Members {
        channel,
        caller,
        sessions,
    } = &members;
    Ok(CommandOutput {
        command: "sessions",
        data: listing_json(
            json!({ "channel": channel }),
            caller.iter().chain(sessions),
            Some(*channel),
            selected,
            now_ms(),
        ),
        human,
    })
}

fn members_human(members: &Members, program: &str, now: u64) -> String {
    let Members {
        channel,
        caller,
        sessions,
    } = members;
    if caller.is_none() && sessions.is_empty() {
        return format!("No sessions in channel {channel}");
    }
    let whose = if caller.is_some() {
        "your channel"
    } else {
        "channel"
    };
    // Every row shares the channel the listing names, so none is marked.
    let human = list_human(
        caller.iter().chain(sessions).map(Row::from),
        None,
        Some(&format!("{whose} {channel}")),
        now,
    );
    // Exited members can still be read, but not messaged.
    let readable = caller
        .iter()
        .chain(sessions)
        .any(|member| member.session.provider.is_some());
    let steps: &[(String, &str)] = if readable {
        &[(format!("{program} transcript SESSION"), "reads one")]
    } else {
        &[]
    };
    with_hint(human, steps)
}

/// One listed session, with what a member view adds to it.
pub(super) struct Row<'a> {
    entry: &'a Entry,
    state: &'static str,
    failure: Option<&'a str>,
    active_at: Option<u64>,
    limit: Option<&'a UsageLimit>,
    account: Option<&'a SessionAccount>,
    member: &'a Member,
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
            member,
        }
    }
}

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Session rows, then each account they run on and what is left of it,
/// once however many rows share it. Session and agent IDs are the shortest
/// prefixes that tell them apart; a plain shell's is whole, since only
/// `terminal` commands take it and they need all of it. A project every
/// row shares is named once, with where it is, rather than on each row, and
/// directories are shown relative to it. `channel` is the caller's, so a
/// session sharing it is marked. `scope` names what else every row shares,
/// beside the project.
pub(super) fn list_human<'a>(
    rows: impl IntoIterator<Item = Row<'a>>,
    channel: Option<crate::model::ChannelId>,
    scope: Option<&str>,
    now: u64,
) -> String {
    let rows: Vec<Row<'a>> = rows.into_iter().collect();
    let Some(first) = rows.first() else {
        return "No live sessions".into();
    };
    let ids: Vec<String> = rows
        .iter()
        .flat_map(|row| {
            row.entry
                .provider
                .map(|_| row.entry.id.to_string())
                .into_iter()
                .chain(
                    row.entry
                        .delegation
                        .as_ref()
                        .map(|delegation| delegation.agent.to_string()),
                )
        })
        .collect();
    let length = super::format::abbreviation(ids.iter().map(String::as_str));
    let shared = first.entry.project.as_deref().filter(|project| {
        rows.iter()
            .all(|row| row.entry.project.as_deref() == Some(*project))
    });
    let mut accounts: Vec<&SessionAccount> = Vec::new();
    for account in rows.iter().filter_map(|row| row.account) {
        if !accounts.iter().any(|known| known.id == account.id) {
            accounts.push(account);
        }
    }
    let keys = AccountKeys::of(&accounts);
    let mut human = super::format::sparse_table(
        ["SESSION", "PROVIDER", "STATE", "ACCOUNT", "CONTEXT"],
        rows.iter()
            .map(|row| {
                let id = row.entry.id.to_string();
                [
                    match row.entry.provider {
                        Some(_) => super::format::prefix(&id, length).to_owned(),
                        None => id,
                    },
                    row.entry
                        .provider
                        .map_or("shell", |p| p.executable())
                        .into(),
                    row.state.into(),
                    row.account
                        .map(|account| keys.key(&account.id))
                        .unwrap_or_default(),
                    context(row, length, shared.is_none(), channel, now),
                ]
            })
            .collect(),
        "No live sessions",
    );
    let project = shared.map(|project| {
        let name = super::format::plain(first.entry.project_name.as_deref().unwrap_or_default());
        match primary_checkout(project) {
            Some(root) => format!("project {name} at {}", super::format::location(root, None)),
            None => format!("project {name}"),
        }
    });
    let shares: Vec<String> = project
        .into_iter()
        .chain(scope.map(str::to_owned))
        .collect();
    if !shares.is_empty() {
        human.push_str(&format!("\n({})", shares.join("; ")));
    }
    if !accounts.is_empty() {
        human.push_str("\n\n");
        human.push_str(&super::format::sparse_table(
            ["ACCOUNT", "LEFT", "RESETS", "LABEL"],
            accounts
                .iter()
                .map(|account| {
                    let (mut left, resets) = allowance(account, now);
                    if account.stale {
                        left.push_str(" (stale)");
                    }
                    [
                        keys.key(&account.id),
                        left,
                        resets.unwrap_or_else(|| "-".into()),
                        account
                            .label
                            .as_deref()
                            .map(super::format::plain)
                            .unwrap_or_default(),
                    ]
                })
                .collect(),
            "",
        ));
    }
    human
}

/// A token count in a few characters, such as `120k` or `1.2M`.
pub(super) fn token_count(tokens: u64) -> String {
    match tokens {
        0..1_000 => tokens.to_string(),
        1_000..1_000_000 => format!("{}k", tokens / 1_000),
        _ => format!("{:.1}M", tokens as f64 / 1_000_000.0),
    }
}

/// What a row adds beyond its ID, provider, state and account, most
/// pressing first. `projects` says whether rows span projects, so each
/// names its own and shows its directory in full.
fn context(
    row: &Row<'_>,
    length: usize,
    projects: bool,
    channel: Option<crate::model::ChannelId>,
    now: u64,
) -> String {
    use crate::agent_execution::{State, directory::Owner};
    let entry = row.entry;
    let checkout = entry.project.as_deref().and_then(primary_checkout);
    // Across projects a relative path would have no stated base.
    let root = checkout.filter(|_| !projects);
    let shown = |path: &std::path::Path| {
        (Some(path) != checkout).then(|| super::format::location(path, root))
    };
    let mut context = Vec::new();
    if let Some(limit) = row.limit {
        context.push(format!("stopped: {}", limit_phrase(limit, now)));
    }
    if let Some(tokens) = row.member.cold_context(now) {
        context.push(format!("cache cold ({} tokens)", token_count(tokens)));
    }
    if projects && let Some(project) = &entry.project_name {
        context.push(format!("[{}]", super::format::plain(project)));
    }
    if let Some(directory) = entry.directory.as_deref().and_then(shown) {
        context.push(directory);
    }
    if let Some(title) = entry.title.as_deref().filter(|title| !title.is_empty()) {
        context.push(format!("\"{}\"", super::format::clip(title, TITLE_WIDTH)));
    }
    if let Some(crate::agent_execution::Placement::Worktree { path, .. }) = &entry.view_scope
        && entry.directory.as_ref() != Some(path)
    {
        context.push(format!("view {}", super::format::location(path, root)));
    }
    if entry.relation != Relation::Myself && channel.is_some() && entry.channel == channel {
        context.push("my channel".into());
    }
    let run = |delegation: &crate::agent_execution::directory::Delegation| {
        delegation
            .run_state
            .map(|state| format!("run {}", super::agents::state_label(state).to_lowercase()))
    };
    // Only an agent the caller owns can be controlled by its ID; of
    // anyone else's run, only one needing attention is worth a word.
    let owned = |who: &str, delegation: &crate::agent_execution::directory::Delegation| {
        let agent = delegation.agent.to_string();
        let agent = format!("{who} {}", super::format::prefix(&agent, length));
        match run(delegation) {
            Some(run) => format!("{agent}, {run}"),
            None => agent,
        }
    };
    let attention = entry
        .delegation
        .as_ref()
        .filter(|delegation| matches!(delegation.run_state, Some(State::Waiting | State::Failed)))
        .and_then(run);
    match (entry.relation, entry.delegation.as_ref()) {
        (Relation::Myself, _) => context.push("me".into()),
        (Relation::Parent, _) => context.push(match attention {
            Some(run) => format!("my parent, {run}"),
            None => "my parent".into(),
        }),
        (Relation::Child, Some(delegation)) if delegation.owner == Owner::User => {
            context.push(owned("user agent", delegation));
        }
        (Relation::Child, Some(delegation)) => context.push(owned("my agent", delegation)),
        (Relation::Child, None) => context.push("my agent".into()),
        (Relation::Peer, _) => context.extend(attention.map(|run| format!("agent {run}"))),
    }
    if let Some(at) = row.active_at {
        context.push(format!("active {}", super::format::relative_time(at, now)));
    }
    // A limit already says what failed.
    if let Some(failure) = row.failure.filter(|_| row.limit.is_none()) {
        context.push(format!("failed: {}", super::format::plain(failure)));
    }
    context.join("; ")
}

/// How many columns of a title a row shows; the whole title is in `--json`.
const TITLE_WIDTH: usize = 60;

/// The primary checkout of the project whose repository key is `project`:
/// the directory holding a Git checkout's own `.git`, or an open folder
/// itself.
fn primary_checkout(project: &std::path::Path) -> Option<&std::path::Path> {
    if project.file_name()? == ".git" {
        project.parent()
    } else {
        Some(project)
    }
}

/// Names for the accounts one listing shows, the same in a row and in the
/// account table: an account's key alone, since a row's provider column
/// says whose it is, with an opaque key shortened as IDs are. Where two
/// providers' accounts would read the same, both are named with their
/// provider.
struct AccountKeys {
    length: usize,
    qualified: Vec<String>,
}

impl AccountKeys {
    fn of(accounts: &[&SessionAccount]) -> Self {
        let length = super::format::abbreviation(
            accounts
                .iter()
                .filter_map(|account| Self::opaque(account.id.as_str())),
        );
        let short = |id: &str| Self::short(id, length);
        let qualified = accounts
            .iter()
            .map(|account| account.id.as_str())
            .filter(|id| {
                accounts
                    .iter()
                    .any(|other| other.id.as_str() != *id && short(other.id.as_str()) == short(id))
            })
            .map(str::to_owned)
            .collect();
        Self { length, qualified }
    }

    /// The part of an account ID after its provider, when it is a UUID.
    fn opaque(id: &str) -> Option<&str> {
        let (_, key) = id.split_once('/')?;
        uuid::Uuid::parse_str(key).is_ok().then_some(key)
    }

    fn short(id: &str, length: usize) -> String {
        match Self::opaque(id) {
            Some(key) => super::format::prefix(key, length).to_owned(),
            None => id.split_once('/').map_or(id, |(_, key)| key).to_owned(),
        }
    }

    fn key(&self, id: &crate::account::AccountId) -> String {
        let id = id.as_str();
        let short = Self::short(id, self.length);
        if self.qualified.iter().any(|qualified| qualified == id) {
            let provider = id.split_once('/').map_or(id, |(provider, _)| provider);
            format!("{provider}/{short}")
        } else {
            short
        }
    }
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
    request(
        Operation::Context {
            scope,
            standing: true,
        },
        |reply| match reply {
            Reply::Context(context) => Some(context.into_sessions()),
            _ => None,
        },
    )
}

fn request<T>(
    operation: Operation,
    expected: impl FnOnce(Reply) -> Option<T>,
) -> Result<T, AppError> {
    let mut client = control_daemon_client()?;
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

    #[test]
    fn a_session_with_a_cold_cache_says_so_only_where_it_matters() {
        use crate::agent_execution::directory::{PromptCache, TtlBasis};
        const HOUR: u64 = 3600 * 1000;
        let mut member = live(entry());
        member.cache = Some(PromptCache {
            requested_at: 0,
            ttl_ms: HOUR,
            basis: TtlBasis::Assumed,
            context_tokens: 120_000,
        });
        let row = |member: &Member, now| context(&Row::from(member), 8, false, None, now);
        assert!(row(&member, 2 * HOUR).starts_with("cache cold (120k tokens); "));
        assert_eq!(
            session_row(&member, None, Detail::Sparse, 2 * HOUR)["cache"],
            json!({
                "cold": true,
                "expires_at": 3600,
                "context_tokens": 120_000,
                "basis": "assumed",
            })
        );
        assert_eq!(session_json(&member, 2 * HOUR)["cache"]["cold"], true);

        // Still warm, too little context to matter, the caller itself, or
        // working: nothing to say.
        let quiet = |member: &Member, now| {
            assert!(!row(member, now).contains("cache"), "{}", row(member, now));
            assert!(
                session_row(member, None, Detail::Sparse, now)
                    .get("cache")
                    .is_none()
            );
            assert!(session_json(member, now).get("cache").is_none());
        };
        quiet(&member, HOUR / 2);
        let mut small = member.clone();
        small.cache.as_mut().unwrap().context_tokens = 4_000;
        quiet(&small, 2 * HOUR);
        let mut me = member.clone();
        me.session.relation = Relation::Myself;
        quiet(&me, 2 * HOUR);
        let mut working = member.clone();
        working.session.state = ActivityState::Working;
        quiet(&working, 2 * HOUR);
        // Selected, it is reported whatever it says.
        assert_eq!(
            session_row(&member, None, Detail::Complete, HOUR / 2)["cache"]["cold"],
            false
        );
    }

    fn live(session: Entry) -> Member {
        Member {
            session,
            exited: false,
            failure: None,
            active_at: None,
            limit: None,
            account: None,
            cache: None,
        }
    }

    #[test]
    fn a_prompt_cache_is_warm_until_it_likely_expires() {
        use crate::agent_execution::directory::{PromptCache, TtlBasis, Warmth};
        let mut member = live(entry());
        assert_eq!(member.warmth(0), Warmth::Unknown);
        member.cache = Some(PromptCache {
            requested_at: 1_000,
            ttl_ms: 60_000,
            basis: TtlBasis::Reported,
            context_tokens: 50_000,
        });
        assert_eq!(member.warmth(60_999), Warmth::Warm);
        assert_eq!(member.warmth(61_000), Warmth::Cold);
        // Working keeps it warm; an exited session is reopened cold.
        member.session.state = ActivityState::Working;
        assert_eq!(member.warmth(120_000), Warmth::Warm);
        member.exited = true;
        assert_eq!(member.warmth(2_000), Warmth::Cold);
    }

    #[test]
    fn lineage_context_keeps_the_frozen_entry_wire_layout_and_reports_cli_json() {
        use crate::agent_execution::directory::{ChannelHome, Context};
        let mut caller = entry();
        let frozen = postcard::to_allocvec(&caller).unwrap();
        let home = ChannelHome {
            id: ChannelId::new(),
            repository: "/fixture/.git".into(),
        };
        caller.lineage_channel = Some(home.clone());
        assert_eq!(postcard::to_allocvec(&caller).unwrap(), frozen);
        let context = Context {
            sessions: Sessions {
                caller: Some(live(caller)),
                scope: Scope::All,
                sessions: vec![],
            },
            lineage_channel: Some(home.clone()),
        };
        let bytes = postcard::to_allocvec(&context).unwrap();
        let decoded: Context = postcard::from_bytes(&bytes).unwrap();
        assert!(
            decoded
                .sessions
                .caller
                .as_ref()
                .unwrap()
                .session
                .lineage_channel
                .is_none()
        );
        let sessions = decoded.into_sessions();
        // Its repository is the session's project, so it is not repeated.
        assert_eq!(
            session_row(sessions.caller.as_ref().unwrap(), None, Detail::Sparse, 0)["lineage_channel"],
            json!({ "id": home.id })
        );
    }

    #[test]
    fn listing_json_states_each_fact_once() {
        let mut first = live(entry());
        first.account = Some(account(0.74, false));
        first.limit = Some(UsageLimit {
            summary: "Claude weekly limit reached".into(),
            reset: None,
            resets_at: Some(3 * 3600),
        });
        let mut second = live(Entry {
            id: "00000000-0000-4000-8000-000000000005".parse().unwrap(),
            lineage: Lineage("00000000-0000-4000-8000-000000000005".parse().unwrap()),
            relation: Relation::Peer,
            directory: Some("/fixture/linked/src".into()),
            view_scope: None,
            channel: Some(ChannelId::new()),
            channel_repository: Some("/fixture/.git".into()),
            title: None,
            ..entry()
        });
        second.account = first.account.clone();
        second.exited = true;
        second.active_at = Some(60_000);
        let data = listing_json(json!({ "scope": "All" }), [&first, &second], None, &[], 0);
        assert_eq!(
            data,
            json!({
                "scope": "All",
                "sessions": [
                    {
                        "id": "00000000-0000-4000-8000-000000000001",
                        "relation": "child",
                        "provider": "claude",
                        "state": "limited",
                        "title": "review\n\u{1b}title",
                        "project": "Fixture",
                        "directory": "/fixture/linked",
                        "worktree_view": "/fixture/linked",
                        "lineage": "00000000-0000-4000-8000-000000000003",
                        "agent": "00000000-0000-4000-8000-000000000002",
                        "run": "00000000-0000-4000-8000-000000000004",
                        "run_state": "interrupted",
                        "account": "claude/system",
                        "limit": {"summary": "Claude weekly limit reached", "resets_at": 3 * 3600},
                    },
                    {
                        "id": "00000000-0000-4000-8000-000000000005",
                        "provider": "claude",
                        "state": "exited",
                        "project": "Fixture",
                        "directory": "/fixture/linked/src",
                        "worktree": "/fixture/linked",
                        "channel": second.session.channel,
                        "agent": "00000000-0000-4000-8000-000000000002",
                        "owner": "user",
                        "run": "00000000-0000-4000-8000-000000000004",
                        "run_state": "interrupted",
                        "account": "claude/system",
                        "active_at": 60,
                    },
                ],
                // The account both run on, once, as whole percentages.
                "accounts": {"claude/system": {
                    "label": "work",
                    "status": "warning",
                    "windows": [{
                        "window": "Weekly",
                        "left_percent": 74,
                        "resets_at": 3 * 3600,
                        "binding": true,
                    }],
                }},
            })
        );
        // Selected fields alone, from every row, each stated in full: a
        // listing of a channel still names it, and a peer and a chat say so.
        let channel = second.session.channel;
        assert_eq!(
            listing_json(
                json!({}),
                [&first, &second],
                channel,
                &[
                    "state".into(),
                    "channel".into(),
                    "relation".into(),
                    "surface".into(),
                    "failure".into(),
                ],
                0,
            ),
            json!({"sessions": [
                {
                    "state": "limited",
                    "channel": first.session.channel,
                    "relation": "child",
                    "surface": "chat",
                    "failure": null,
                },
                {
                    "state": "exited",
                    "channel": channel,
                    "relation": "peer",
                    "surface": "chat",
                    "failure": null,
                },
            ]})
        );
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
        let human = list_human(std::iter::once(Row::from(&member)), None, None, 3600 * 1000);
        let lines: Vec<&str> = human.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["SESSION", "PROVIDER", "STATE", "ACCOUNT", "CONTEXT"]
        );
        let row = lines[1];
        assert!(row.contains(" limited  system "), "{row}");
        assert!(
            row.contains("stopped: Claude weekly limit reached, resets in 2 hr"),
            "{row}"
        );
        assert!(
            !row.contains("failed:"),
            "the limit already says what failed: {row}"
        );
        assert_eq!(
            lines[2..],
            [
                "(project Fixture at /fixture)",
                "",
                "ACCOUNT  LEFT          RESETS   LABEL",
                "system   0% of Weekly  in 2 hr  work",
            ]
        );

        let value = session_json(&member, 0);
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
        let value = session_json(&member, 0);
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
    fn each_account_is_listed_once_after_the_rows_that_run_on_it() {
        let opaque = "claude/a84362d7-c93d-4130-af49-34e6fd23b531";
        let mut shared = account(0.06, false);
        shared.id = opaque.parse().unwrap();
        shared.label = None;
        let rows: Vec<Member> = (1..=3)
            .map(|index| {
                let mut session = entry();
                session.id = format!("{index}0000000-0000-4000-8000-000000000001")
                    .parse()
                    .unwrap();
                session.delegation = None;
                session.title = None;
                let mut member = live(session);
                member.account = Some(if index == 2 {
                    account(0.73, true)
                } else {
                    shared.clone()
                });
                member
            })
            .collect();
        let human = list_human(rows.iter().map(Row::from), None, None, 3600 * 1000);
        let lines: Vec<&str> = human.lines().collect();
        assert!(
            lines[1].starts_with("10000000  claude    waiting  a84362d7  "),
            "{human}"
        );
        assert!(
            lines[2].starts_with("20000000  claude    waiting  system    "),
            "{human}"
        );
        // The table names each account as its rows do.
        assert_eq!(
            lines[4..],
            [
                "(project Fixture at /fixture)",
                "",
                "ACCOUNT   LEFT                   RESETS   LABEL",
                "a84362d7  6% of Weekly           in 2 hr",
                "system    73% of Weekly (stale)  in 2 hr  work",
            ],
            "{human}"
        );
    }

    /// Two providers' accounts with the same key are told apart by provider
    /// wherever either is named.
    #[test]
    fn accounts_that_would_read_alike_are_named_with_their_provider() {
        let claude = account(0.5, false);
        let mut codex = account(0.5, false);
        codex.id = "codex/system".parse().unwrap();
        let other = {
            let mut other = account(0.5, false);
            other.id = "codex/a84362d7-c93d-4130-af49-34e6fd23b531"
                .parse()
                .unwrap();
            other
        };
        let keys = AccountKeys::of(&[&claude, &codex, &other]);
        assert_eq!(keys.key(&claude.id), "claude/system");
        assert_eq!(keys.key(&codex.id), "codex/system");
        assert_eq!(keys.key(&other.id), "a84362d7");
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
            cache: None,
        };
        let mut members = Members {
            channel,
            caller: None,
            sessions: vec![exited],
        };
        let human = members_human(&members, "cagents", 12 * 60_000);
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
                "\n(project Fixture at /fixture; channel {channel})\n\
                 hint: `cagents transcript SESSION` reads one"
            )),
            "{human}"
        );

        let mut me = members.sessions[0].clone();
        me.session.relation = Relation::Myself;
        members.caller = Some(me);
        assert!(
            members_human(&members, "cagents", 0).contains(&format!("; your channel {channel})\n"))
        );

        members.caller = None;
        members.sessions.clear();
        assert_eq!(
            members_human(&members, "cagents", 0),
            format!("No sessions in channel {channel}")
        );
    }

    #[test]
    fn readable_session_rows_keep_identity_and_delegation_context_without_repeating_scope() {
        let entry = entry();
        let before = serde_json::to_value(&entry).unwrap();
        let member = live(entry.clone());
        let human = list_human(std::iter::once(Row::from(&member)), None, None, 0);
        let lines: Vec<&str> = human.lines().collect();
        assert_eq!(
            lines[0].split_whitespace().collect::<Vec<_>>(),
            ["SESSION", "PROVIDER", "STATE", "CONTEXT"]
        );
        // The session and its agent share eight digits, so both are shown
        // until they differ.
        assert_eq!(
            lines[1],
            "00000000-0000-4000-8000-000000000001  claude    waiting  \
             linked; \"review  title\"; \
             user agent 00000000-0000-4000-8000-000000000002, run outcome uncertain"
        );
        // The project is named once, with the checkout its directories are
        // relative to; the view is the session's own directory.
        assert_eq!(lines[2..], ["(project Fixture at /fixture)"]);
        for debug in ["Some(", "None", "Interrupted", "{ ", "_ms", "lineage"] {
            assert!(!human.contains(debug), "{human}");
        }
        assert!(!human.contains('\u{1b}'));
        assert_eq!(serde_json::to_value(&entry).unwrap(), before);
    }

    #[test]
    fn rows_across_projects_name_their_own_and_place_directories_in_them() {
        let mut primary = entry();
        primary.directory = Some("/fixture".into());
        primary.view_scope = None;
        primary.delegation = None;
        primary.relation = Relation::Peer;
        primary.title = Some("x".repeat(80));
        let mut other = primary.clone();
        other.id = "10000000-0000-4000-8000-000000000001".parse().unwrap();
        other.project = Some("/other/.git".into());
        other.project_name = Some("Other".into());
        other.directory = Some("/other/.worktrees/fix".into());
        other.title = None;
        other.view_scope = Some(Placement::Worktree {
            repository: "/other/.git".into(),
            path: "/other/.worktrees/view".into(),
        });
        let rows = [live(primary), live(other)];
        let human = list_human(rows.iter().map(Row::from), None, None, 0);
        let lines: Vec<&str> = human.lines().collect();
        assert_eq!(
            lines[1],
            format!(
                "00000000  claude    waiting  [Fixture]; \"{}…\"",
                "x".repeat(TITLE_WIDTH - 1)
            )
        );
        // With no one root to be relative to, directories are whole.
        assert_eq!(
            lines[2],
            "10000000  claude    waiting  [Other]; /other/.worktrees/fix; view /other/.worktrees/view"
        );
        assert_eq!(lines.len(), 3, "rows span projects, so none is named after");
    }

    #[test]
    fn listings_end_with_the_commands_that_follow_from_them() {
        let mut me = live(entry());
        me.session.relation = Relation::Myself;
        me.session.channel = Some("00000000-0000-4000-8000-0000000000c1".parse().unwrap());
        me.session.delegation = None;
        let mut peer = live(entry());
        peer.session.id = "10000000-0000-4000-8000-000000000001".parse().unwrap();
        peer.session.relation = Relation::Peer;
        let directory = Sessions {
            caller: Some(me),
            scope: Scope::Project("/fixture/.git".into()),
            sessions: vec![peer],
        };
        let listed = sessions_human(&directory, "cagents", 0);
        assert!(
            listed.ends_with(
                "\nhint: `cagents transcript SESSION` reads one; \
                 `cagents send SESSION MESSAGE` messages one; \
                 `cagents ls --all` lists every project"
            ),
            "{listed}"
        );

        let peers: Vec<&Member> = directory.sessions.iter().collect();
        let who = whoami_human(&directory, &peers, "cagents --device fw16", 0);
        let rows: Vec<&str> = who.lines().skip(1).take(2).collect();
        assert!(rows[0].contains("; me"), "{who}");
        assert!(rows[1].starts_with("10000000"), "{who}");
        assert!(
            who.ends_with(
                "\nhint: `cagents --device fw16 transcript SESSION` reads one; \
                 `cagents --device fw16 send SESSION MESSAGE` messages one; \
                 `cagents --device fw16 ls --channel` lists your channel, including ended sessions"
            ),
            "{who}"
        );

        // A plain shell has no conversation to read or message.
        let mut shell = directory.sessions[0].clone();
        shell.session.provider = None;
        let shells = Sessions {
            sessions: vec![shell],
            caller: None,
            scope: Scope::All,
        };
        assert!(!sessions_human(&shells, "cagents", 0).contains("hint:"));

        let outside = Sessions {
            caller: None,
            scope: Scope::All,
            sessions: Vec::new(),
        };
        // With no one else listed, nothing follows but a wider listing.
        assert_eq!(
            whoami_human(&outside, &[], "cagents", 0),
            "Not running inside a cagents session"
        );
        assert_eq!(sessions_human(&outside, "cagents", 0), "No live sessions");
        let alone = Sessions {
            scope: Scope::Project("/fixture/.git".into()),
            ..outside
        };
        assert_eq!(
            sessions_human(&alone, "cagents", 0),
            "No live sessions\nhint: `cagents ls --all` lists every project"
        );
    }

    #[test]
    fn readable_sessions_empty_and_plain_shell_rows_are_explicit() {
        assert_eq!(
            list_human(std::iter::empty(), None, None, 0),
            "No live sessions"
        );
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
            list_human(std::iter::once(Row::from(&member)), None, None, 0),
            "SESSION                               PROVIDER  STATE    CONTEXT\n\
             00000000-0000-4000-8000-000000000001  shell     unknown  me"
        );
    }

    /// Of a run that is not the caller's, only one that needs attention is
    /// worth a word; the caller's own agents say which run state is whose.
    #[test]
    fn rows_say_whose_agent_runs_need_attention() {
        let row = |relation, state| {
            let mut entry = entry();
            entry.relation = relation;
            entry.title = None;
            entry.view_scope = None;
            entry.directory = None;
            entry.delegation.as_mut().unwrap().owner = Owner::Session(entry.lineage);
            entry.delegation.as_mut().unwrap().run_state = Some(state);
            let member = live(entry);
            context(&Row::from(&member), 8, false, None, 0)
        };
        assert_eq!(row(Relation::Peer, State::Succeeded), "");
        assert_eq!(row(Relation::Peer, State::Failed), "agent run failed");
        assert_eq!(
            row(Relation::Parent, State::Waiting),
            "my parent, run waiting for input"
        );
        assert_eq!(row(Relation::Parent, State::Running), "my parent");
        assert_eq!(
            row(Relation::Child, State::Succeeded),
            "my agent 00000000, run succeeded"
        );
    }
}
