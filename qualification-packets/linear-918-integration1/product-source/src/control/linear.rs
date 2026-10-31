//! Linear commands use the official GraphQL schema; all mutations are single attempts.
use super::{AppError, CommandOutput};
use crate::linear::{self, api::Client};
use clap::{Args, Subcommand, ValueEnum};
use serde_json::{Value, json};
use std::{collections::BTreeSet, io::Read, path::PathBuf};

#[path = "linear_output.rs"]
mod human;

#[derive(Debug, Args)]
pub struct LinearCommand {
    /// Credential workspace alias (CAGENTS_LINEAR_API_KEY_<ALIAS>).
    #[arg(long, global = true)]
    workspace: Option<String>,
    #[command(subcommand)]
    command: Operation,
}

#[derive(Debug, Subcommand)]
enum Operation {
    /// Read an issue; --full includes comments, attachments, children, and relations.
    Issue {
        #[command(flatten)]
        target: IssueTarget,
        #[arg(long)]
        #[arg(help = "Include the complete issue, comments, attachments and relations.")]
        full: bool,
    },
    /// Read a page of comments, including Markdown image URLs.
    Comments {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        page: Page,
    },
    /// List links attached to an issue, such as pull requests.
    Attachments {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        page: Page,
    },
    /// Read child issues and their descriptions.
    Children {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        page: Page,
    },
    /// List outgoing relations; --inverse lists incoming relations.
    Relations {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        page: Page,
        #[arg(long)]
        #[arg(help = "List incoming relations instead of outgoing ones.")]
        inverse: bool,
    },
    /// Search issues in the selected workspace.
    Search {
        #[arg(help = "Search text for matching issues.")]
        query: String,
        #[command(flatten)]
        page: Page,
    },
    /// List issues with optional team, state, assignee, and priority filters.
    #[command(visible_alias = "list-issues")]
    List {
        #[command(flatten)]
        page: Page,
        #[arg(long)]
        #[arg(help = "Team ID, key, or unique team name.")]
        team: Option<String>,
        #[arg(long)]
        #[arg(help = "Workflow state ID or unique name.")]
        state: Option<String>,
        #[arg(long)]
        #[arg(help = "Assignee ID, email, or unique name; use none to clear.")]
        assignee: Option<String>,
        #[arg(long, value_parser=clap::value_parser!(u8).range(0..=4))]
        #[arg(help = "Priority: 0 none, 1 urgent, 2 high, 3 normal, 4 low.")]
        priority: Option<u8>,
    },
    /// Discover teams, workflow states, labels, and members.
    Team {
        #[command(subcommand)]
        command: TeamCommand,
    },
    /// Discover projects in the selected workspace.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Create an issue, or update --id/--current. Names must resolve uniquely.
    #[command(visible_alias = "save-issue")]
    Create(Save),
    /// Update common issue fields; use "none" to clear an optional field.
    Set {
        #[command(flatten)]
        target: Target,
        #[arg(value_enum)]
        #[arg(help = "Issue field to change.")]
        field: Field,
        #[arg(help = "New field value; use none to clear an optional field.")]
        value: String,
    },
    /// Change an issue's workflow state.
    Status {
        #[command(subcommand)]
        command: StatusCommand,
    },
    /// Add a Markdown comment to an issue.
    Comment {
        #[command(subcommand)]
        command: CommentCommand,
    },
    /// Attach an HTTP(S) link to an issue.
    Attach {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        #[arg(help = "HTTP(S) URL to attach.")]
        url: String,
        #[arg(long)]
        #[arg(help = "Title of the issue or attached link.")]
        title: String,
    },
    /// Add, remove, or replace issue labels.
    Label {
        #[command(subcommand)]
        command: LabelCommand,
    },
    /// Create or remove a relation between issues.
    Relation {
        #[command(subcommand)]
        command: RelationCommand,
    },
    /// Save a validated issue UUID and credential alias for this checkout.
    Link {
        #[arg(help = "The stable resource ID.")]
        id: String,
    },
    /// Remove this worktree's ticket link without contacting Linear.
    Unlink,
}
#[derive(Debug, Args)]
struct Target {
    /// Issue identifier, UUID, or official Linear issue URL.
    #[arg(long, required_unless_present = "current", conflicts_with = "current")]
    id: Option<String>,
    /// Use this Git worktree's explicitly linked issue and workspace.
    #[arg(long)]
    current: bool,
}
#[derive(Debug, Args)]
struct IssueTarget {
    #[arg(required_unless_present = "current", conflicts_with = "current")]
    #[arg(help = "The stable resource ID.")]
    id: Option<String>,
    #[arg(long)]
    #[arg(help = "Use the issue linked to this Git worktree.")]
    current: bool,
}
impl From<IssueTarget> for Target {
    fn from(t: IssueTarget) -> Self {
        Self {
            id: t.id,
            current: t.current,
        }
    }
}
#[derive(Debug, Args)]
struct Page {
    /// Maximum results in this page (1 through 100).
    #[arg(long, default_value_t=50, value_parser=clap::value_parser!(u16).range(1..=100))]
    limit: u16,
    /// Continue from the preceding pageInfo.endCursor.
    #[arg(long, visible_alias = "cursor")]
    after: Option<String>,
}
#[derive(Debug, Subcommand)]
enum TeamCommand {
    /// List teams in the selected workspace.
    List {
        #[command(flatten)]
        page: Page,
    },
    /// List workflow states for a team.
    States {
        #[arg(help = "Team ID, key, or unique team name.")]
        team: String,
        #[command(flatten)]
        page: Page,
    },
    /// List labels for a team.
    Labels {
        #[arg(help = "Team ID, key, or unique team name.")]
        team: String,
        #[command(flatten)]
        page: Page,
    },
    /// List members of a team.
    Members {
        #[arg(help = "Team ID, key, or unique team name.")]
        team: String,
        #[command(flatten)]
        page: Page,
    },
}
#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// List projects in the selected workspace.
    List {
        #[command(flatten)]
        page: Page,
    },
}
#[derive(Debug, Args)]
struct Save {
    #[arg(long, conflicts_with = "current")]
    #[arg(help = "The stable resource ID.")]
    id: Option<String>,
    #[arg(long)]
    #[arg(help = "Use the issue linked to this Git worktree.")]
    current: bool,
    #[arg(long)]
    #[arg(help = "Team ID, key, or unique team name.")]
    team: Option<String>,
    #[arg(long)]
    #[arg(help = "Title of the issue or attached link.")]
    title: Option<String>,
    /// Markdown file, or - to read stdin (maximum 2 MiB).
    #[arg(long)]
    body_file: Option<PathBuf>,
    #[arg(long, conflicts_with = "parent_current")]
    #[arg(help = "Parent issue identifier or UUID; use none to clear.")]
    parent: Option<String>,
    #[arg(long)]
    #[arg(help = "Use this worktree’s linked issue as the parent.")]
    parent_current: bool,
    #[arg(long)]
    #[arg(help = "Workflow state ID or unique name.")]
    status: Option<String>,
    #[arg(long)]
    #[arg(help = "Assignee ID, email, or unique name; use none to clear.")]
    assignee: Option<String>,
    #[arg(long)]
    #[arg(help = "Project ID or unique name; use none to clear.")]
    project: Option<String>,
    #[arg(long, value_delimiter = ',')]
    #[arg(help = "Label names or IDs; comma-separate option values.")]
    labels: Option<Vec<String>>,
    #[arg(long, value_parser=clap::value_parser!(u8).range(0..=4))]
    #[arg(help = "Priority: 0 none, 1 urgent, 2 high, 3 normal, 4 low.")]
    priority: Option<u8>,
    #[arg(long, value_parser=clap::value_parser!(u32).range(0..=2147483647))]
    #[arg(help = "Nonnegative issue estimate.")]
    estimate: Option<u32>,
    #[arg(long)]
    #[arg(help = "Due date in YYYY-MM-DD format; use none to clear.")]
    due_date: Option<String>,
    /// UUID for reconciling an uncertain create result. No automatic write retries.
    #[arg(long)]
    request_id: Option<uuid::Uuid>,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum Field {
    Title,
    Description,
    Status,
    Assignee,
    Project,
    Parent,
    Priority,
    Estimate,
    DueDate,
}
#[derive(Debug, Subcommand)]
enum StatusCommand {
    /// Set an issue workflow state.
    Set {
        #[command(flatten)]
        target: Target,
        #[arg(help = "Workflow state ID or unique name.")]
        state: String,
    },
}
#[derive(Debug, Subcommand)]
enum CommentCommand {
    /// Add a Markdown comment to an issue.
    Add {
        #[command(flatten)]
        target: Target,
        #[arg(
            long,
            conflicts_with = "body_file",
            required_unless_present = "body_file"
        )]
        #[arg(help = "Markdown comment text.")]
        body: Option<String>,
        #[arg(long)]
        #[arg(help = "Markdown file; use - to read standard input (maximum 2 MiB).")]
        body_file: Option<PathBuf>,
        #[arg(long)]
        #[arg(help = "Stable retry UUID; reuse it when retrying the same request.")]
        request_id: Option<uuid::Uuid>,
    },
}
#[derive(Debug, Subcommand)]
enum LabelCommand {
    /// Add labels without removing current labels.
    Add {
        #[command(flatten)]
        target: Target,
        #[arg(help = "Label names or IDs as separate arguments; quote names with spaces.")]
        labels: Vec<String>,
    },
    /// Remove the named labels from an issue.
    Remove {
        #[command(flatten)]
        target: Target,
        #[arg(help = "Label names or IDs as separate arguments; quote names with spaces.")]
        labels: Vec<String>,
    },
    /// Replace the complete issue label set.
    Set {
        #[command(flatten)]
        target: Target,
        #[arg(help = "Label names or IDs as separate arguments; quote names with spaces.")]
        labels: Vec<String>,
    },
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum RelationType {
    Blocks,
    Duplicate,
    Related,
    Similar,
}
#[derive(Debug, Subcommand)]
enum RelationCommand {
    /// Create a typed relation between two issues.
    Add {
        #[command(flatten)]
        target: Target,
        #[arg(help = "Related issue identifier, UUID or URL.")]
        related: String,
        #[arg(long, value_enum)]
        #[arg(help = "Relation type to create.")]
        kind: RelationType,
        #[arg(long)]
        #[arg(help = "Stable retry UUID; reuse it when retrying the same request.")]
        request_id: Option<uuid::Uuid>,
    },
    /// Remove by the relation UUID shown by issue --full.
    Remove {
        /// The stable resource ID.
        id: uuid::Uuid,
    },
}

#[cfg(feature = "ui")]
impl LinearCommand {
    /// What the command does, as the verb a transcript step opens with, and
    /// what it did it to. Empty where the verb says it all.
    pub(super) fn describe(&self) -> (&'static str, String) {
        fn ticket(id: &Option<String>) -> String {
            id.clone()
                .unwrap_or_else(|| "this checkout's ticket".to_owned())
        }
        let issue = |target: &Target| ticket(&target.id);
        match &self.command {
            Operation::Issue { target, .. } => ("Read ticket", ticket(&target.id)),
            Operation::Comments { target, .. } => ("Read comments on", issue(target)),
            Operation::Attachments { target, .. } => ("Read links on", issue(target)),
            Operation::Children { target, .. } => ("Read sub-issues of", issue(target)),
            Operation::Relations { target, .. } => ("Read relations of", issue(target)),
            Operation::Search { query, .. } => ("Searched tickets for", query.clone()),
            Operation::List { .. } => ("Listed tickets", String::new()),
            Operation::Team { .. } => ("Read Linear teams", String::new()),
            Operation::Project { .. } => ("Listed Linear projects", String::new()),
            Operation::Create(save) if save.id.is_some() || save.current => {
                ("Updated ticket", ticket(&save.id))
            }
            Operation::Create(save) => ("Created ticket", save.title.clone().unwrap_or_default()),
            Operation::Set { target, .. }
            | Operation::Label {
                command:
                    LabelCommand::Add { target, .. }
                    | LabelCommand::Remove { target, .. }
                    | LabelCommand::Set { target, .. },
            } => ("Updated ticket", issue(target)),
            Operation::Status {
                command: StatusCommand::Set { target, state },
            } => ("Moved ticket", format!("{} to {state}", issue(target))),
            Operation::Comment {
                command: CommentCommand::Add { target, .. },
            } => ("Commented on", issue(target)),
            Operation::Attach { target, .. } => ("Attached a link to", issue(target)),
            Operation::Relation {
                command:
                    RelationCommand::Add {
                        target, related, ..
                    },
            } => ("Related", format!("{} to {related}", issue(target))),
            Operation::Relation {
                command: RelationCommand::Remove { .. },
            } => ("Removed a ticket relation", String::new()),
            Operation::Link { id } => ("Linked this checkout to", id.clone()),
            Operation::Unlink => ("Unlinked this checkout's ticket", String::new()),
        }
    }
}

const ISSUE: &str = "id identifier title description url priority estimate dueDate team { id key name } state { id name type } assignee { id name email } project { id name } parent { id identifier title } labels(first:100) { nodes { id name } pageInfo { hasNextPage endCursor } }";
const PAGE_INFO: &str = "pageInfo { hasNextPage endCursor }";
fn error(message: impl Into<String>) -> AppError {
    AppError::new("linear_input", message.into())
}
fn convert(error: linear::Error) -> AppError {
    AppError {
        kind: error.kind,
        message: error.message,
        details: error.details,
    }
}
fn output(command: &'static str, data: Value, list: Option<human::List>) -> CommandOutput {
    let human = if matches!(
        command,
        "linear.issue" | "linear.comments" | "linear.relations" | "linear.children"
    ) {
        // Detail reads retain complete text, nested fields and exact page cursors.
        serde_json::to_string_pretty(&data).expect("JSON values are serializable")
    } else {
        match list {
            Some(list) => human::render(list, &data),
            None => serde_json::to_string_pretty(&data).unwrap_or_default(),
        }
    };
    CommandOutput {
        command,
        data,
        human,
    }
}
fn request(client: &Client, query: &str, vars: Value, mutation: bool) -> Result<Value, AppError> {
    let identity = if mutation {
        let issue_id = if query.contains("issueUpdate(") {
            vars.get("id")
        } else if query.contains("issueCreate(") {
            vars["input"].get("id")
        } else {
            vars["input"].get("issueId")
        };
        let object_id = if query.contains("issueRelationDelete(") {
            vars.get("id")
        } else {
            vars["input"].get("id")
        };
        Some(json!({
            "workspace": client.workspace(),
            "issueId": issue_id,
            "objectId": object_id,
            "requestId": vars["input"].get("id"),
            "reconcile": "Read the issue and supplied request UUID before repeating a write"
        }))
    } else {
        None
    };
    let result = client
        .request(query, vars, mutation)
        .map_err(convert)
        .and_then(|data| {
            if mutation
                && !data.as_object().is_some_and(|o| {
                    o.values()
                        .any(|v| v.get("success").and_then(Value::as_bool).is_some())
                })
            {
                return Err(AppError {
                    kind: "linear_write_unconfirmed",
                    message: "Linear omitted the mutation confirmation; read back before retrying"
                        .into(),
                    details: Some(data),
                });
            }
            Ok(data)
        });
    result.map_err(|mut e| {
        if let Some(identity) = identity {
            if e.kind == "linear_write_unconfirmed" {
                e.message.push_str(&format!(
                    " (issue: {}; object UUID: {}; workspace: {})",
                    identity["issueId"].as_str().unwrap_or("not applicable"),
                    identity["objectId"].as_str().unwrap_or("existing object"),
                    client.workspace().unwrap_or("default"),
                ));
            }
            let mut details = e.details.take().unwrap_or_else(|| json!({}));
            if let Some(object) = details.as_object_mut() {
                object.insert("writeContext".into(), identity);
            } else {
                details = json!({"response":details,"writeContext":identity});
            }
            e.details = Some(details);
        }
        e
    })
}
fn result_field(data: Value, field: &str) -> Result<Value, AppError> {
    let value = data
        .get(field)
        .filter(|v| !v.is_null())
        .ok_or_else(|| error(format!("Linear returned no {field}")))?;
    Ok(value.clone())
}
fn mutation(
    client: &Client,
    field: &str,
    input_type: &str,
    input: Value,
    selection: &str,
) -> Result<Value, AppError> {
    let query = format!(
        "mutation($input:{input_type}!) {{ {field}(input:$input) {{ success {selection} }} }}"
    );
    checked_mutation(result_field(
        request(client, &query, json!({"input":input}), true)?,
        field,
    )?)
}
fn checked_mutation(value: Value) -> Result<Value, AppError> {
    if value.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(AppError {
            kind: if value.get("success").and_then(Value::as_bool) == Some(false) {
                "linear_mutation_failed"
            } else {
                "linear_write_unconfirmed"
            },
            message: "Linear did not confirm the mutation".into(),
            details: Some(value),
        });
    }
    Ok(value)
}
fn target(target: Target, workspace: Option<String>) -> Result<(String, Option<String>), AppError> {
    if target.current {
        let linked = linear::context::resolve_current().map_err(convert)?;
        Ok((
            linked.issue_id,
            linked_workspace(workspace, linked.workspace)?,
        ))
    } else {
        Ok((
            linear::issue_reference(
                target
                    .id
                    .as_deref()
                    .ok_or_else(|| error("Specify an issue or --current"))?,
            )
            .map_err(convert)?,
            workspace,
        ))
    }
}
fn linked_workspace(
    explicit: Option<String>,
    linked: Option<String>,
) -> Result<Option<String>, AppError> {
    if let Some(a) = &explicit {
        if !linked.as_ref().is_some_and(|b| a.eq_ignore_ascii_case(b)) {
            return Err(error(
                "--workspace conflicts with the linked issue's workspace",
            ));
        }
    }
    Ok(explicit.or(linked))
}
fn client(workspace: Option<&str>) -> Result<Client, AppError> {
    Client::from_workspace(workspace).map_err(convert)
}
fn issue(client: &Client, id: &str, full: bool) -> Result<Value, AppError> {
    let extras = if full {
        "comments(first:100) { nodes { id body createdAt user { id name } } pageInfo { hasNextPage endCursor } } attachments(first:100) { nodes { id title url } pageInfo { hasNextPage endCursor } } relations(first:100) { nodes { id type relatedIssue { id identifier title } } pageInfo { hasNextPage endCursor } } children(first:100) { nodes { id identifier title description url } pageInfo { hasNextPage endCursor } } inverseRelations(first:100) { nodes { id type issue { id identifier title } } pageInfo { hasNextPage endCursor } }"
    } else {
        ""
    };
    result_field(
        request(
            client,
            &format!("query($id:String!) {{ issue(id:$id) {{ {ISSUE} {extras} }} }}"),
            json!({"id":id}),
            false,
        )?,
        "issue",
    )
}
fn issue_id(client: &Client, reference: &str) -> Result<String, AppError> {
    let reference = linear::issue_reference(reference).map_err(convert)?;
    string(&issue(client, &reference, false)?, "id")
}
fn string(value: &Value, field: &str) -> Result<String, AppError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| error(format!("Linear response omitted {field}")))
}
fn page(client: &Client, field: &str, selection: &str, page: &Page) -> Result<Value, AppError> {
    result_field(
        request(
            client,
            &format!(
                "query($first:Int!,$after:String) {{ {field}(first:$first,after:$after) {{ nodes {{ {selection} }} {PAGE_INFO} }} }}"
            ),
            json!({"first":page.limit,"after":page.after}),
            false,
        )?,
        field,
    )
}
/// Exact server-side filters keep lookups cheap; ambiguity still prevents writes.
fn resolve(
    client: &Client,
    field: &str,
    selection: &str,
    reference: &str,
    team: Option<&str>,
) -> Result<String, AppError> {
    let references = [reference.to_owned()];
    let mut ids = resolve_references(client, field, selection, &references, team, |node| {
        team.is_none_or(|t| node["team"]["id"].as_str() == Some(t))
    })?;
    Ok(ids.remove(0))
}

/// Fetch exact metadata matches in one filtered connection for all requested
/// names. Explicit UUIDs need no lookup. The bounded fallback is only for
/// connection types for which no name filter is defined.
fn resolve_references(
    client: &Client,
    field: &str,
    selection: &str,
    references: &[String],
    team: Option<&str>,
    eligible: impl Fn(&Value) -> bool,
) -> Result<Vec<String>, AppError> {
    let explicit = references
        .iter()
        .map(|r| uuid::Uuid::parse_str(r).ok().map(|id| id.to_string()))
        .collect::<Vec<_>>();
    if explicit.iter().all(Option::is_some) {
        return Ok(explicit.into_iter().flatten().collect());
    }
    let mut matches = vec![Vec::new(); references.len()];
    let mut cursor = None;
    for _ in 0..10 {
        let (query, variables) =
            metadata_query(field, selection, references, team, cursor.as_deref());
        let data = result_field(request(client, &query, variables, false)?, field)?;
        let nodes = data["nodes"]
            .as_array()
            .ok_or_else(|| error("Linear response omitted nodes"))?;
        for node in nodes.iter().filter(|node| eligible(node)) {
            for (index, reference) in references
                .iter()
                .enumerate()
                .filter(|(i, _)| explicit[*i].is_none())
            {
                if ["name", "key", "email"].iter().any(|k| {
                    node[*k]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(reference))
                }) {
                    matches[index].push(string(node, "id")?);
                }
            }
        }
        if !data["pageInfo"]["hasNextPage"]
            .as_bool()
            .ok_or_else(|| error("Linear response omitted pagination status"))?
        {
            return references
                .iter()
                .enumerate()
                .map(|(i, r)| match &explicit[i] {
                    Some(id) => Ok(id.clone()),
                    None => unique(std::mem::take(&mut matches[i]), r),
                })
                .collect();
        }
        let next = string(&data["pageInfo"], "endCursor")?;
        if cursor.as_ref() == Some(&next) {
            return Err(error("Linear repeated its pagination cursor"));
        }
        cursor = Some(next);
    }
    Err(error(
        "Metadata pagination exceeded the safety limit; use a UUID",
    ))
}

fn metadata_query(
    field: &str,
    selection: &str,
    references: &[String],
    team: Option<&str>,
    after: Option<&str>,
) -> (String, Value) {
    let definition = match field {
        "teams" => Some(("TeamFilter", &["name", "key"][..])),
        "workflowStates" => Some(("WorkflowStateFilter", &["name"][..])),
        "users" => Some(("UserFilter", &["name", "email"][..])),
        "projects" => Some(("ProjectFilter", &["name"][..])),
        "issueLabels" => Some(("IssueLabelFilter", &["name"][..])),
        _ => None,
    };
    let mut variables = json!({"first":100,"after":after});
    let (declaration, argument) = if let Some((filter_type, fields)) = definition {
        let alternatives: Vec<_> = references
            .iter()
            .filter(|reference| uuid::Uuid::parse_str(reference).is_err())
            .flat_map(|reference| {
                fields
                    .iter()
                    .map(move |field| json!({(*field): {"eqIgnoreCase":reference}}))
            })
            .collect();
        let mut filter = json!({"or":alternatives});
        if field == "workflowStates"
            && let Some(team) = team
        {
            filter["team"] = json!({"id":{"eq":team}});
        }
        variables["filter"] = filter;
        (format!(",$filter:{filter_type}!"), ",filter:$filter")
    } else {
        (String::new(), "")
    };
    (
        format!(
            "query($first:Int!,$after:String{declaration}) {{ {field}(first:$first,after:$after{argument}) {{ nodes {{ {selection} }} {PAGE_INFO} }} }}"
        ),
        variables,
    )
}

fn label_eligible(node: &Value, teams: &BTreeSet<String>) -> bool {
    node.get("team").is_some_and(|team| {
        team.is_null() || team["id"].as_str().is_some_and(|id| teams.contains(id))
    })
}
fn visit_label_team(teams: &mut BTreeSet<String>, id: &str) -> Result<(), AppError> {
    if teams.contains(id) {
        return Err(error("Linear team ancestry contains a cycle"));
    }
    if teams.len() >= 64 {
        return Err(error("Linear team ancestry exceeds 64 teams"));
    }
    teams.insert(id.to_owned());
    Ok(())
}
fn label_teams(client: &Client, team_id: &str) -> Result<BTreeSet<String>, AppError> {
    let mut teams = BTreeSet::new();
    let mut id = team_id.to_owned();
    loop {
        visit_label_team(&mut teams, &id)?;
        let team = result_field(
            request(
                client,
                "query($id:String!){team(id:$id){id parent{id}}}",
                json!({"id":id}),
                false,
            )?,
            "team",
        )?;
        if string(&team, "id")? != id {
            return Err(error(
                "Linear returned a different team during ancestry lookup",
            ));
        }
        let parent = team
            .get("parent")
            .ok_or_else(|| error("Linear response omitted team parent"))?;
        if parent.is_null() {
            return Ok(teams);
        }
        id = string(parent, "id")?;
    }
}
fn resolve_labels(
    client: &Client,
    references: &[String],
    team_id: &str,
) -> Result<Vec<String>, AppError> {
    if references.iter().all(|r| uuid::Uuid::parse_str(r).is_ok()) {
        return references
            .iter()
            .map(|r| {
                uuid::Uuid::parse_str(r)
                    .map(|id| id.to_string())
                    .map_err(|e| AppError::new("linear_input", e))
            })
            .collect();
    }
    let teams = label_teams(client, team_id)?;
    resolve_references(
        client,
        "issueLabels",
        "id name team { id }",
        references,
        None,
        |node| label_eligible(node, &teams),
    )
}
fn unique(mut ids: Vec<String>, reference: &str) -> Result<String, AppError> {
    ids.sort();
    ids.dedup();
    match ids.len() {
        1 => Ok(ids.remove(0)),
        0 => Err(error(format!(
            "No exact match for {reference:?}; use a UUID"
        ))),
        _ => Err(error(format!("Ambiguous name {reference:?}; use a UUID"))),
    }
}
fn team(client: &Client, name: &str) -> Result<String, AppError> {
    resolve(client, "teams", "id key name", name, None)
}
fn field_value(
    client: &Client,
    field: Field,
    value: &str,
    team_id: &str,
) -> Result<(&'static str, Value), AppError> {
    let clear = value == "none";
    let pair = match field {
        Field::Title => {
            if value.trim().is_empty() {
                return Err(error("Title must not be empty"));
            }
            ("title", json!(value))
        }
        Field::Description => ("description", json!(value)),
        Field::Status => (
            "stateId",
            json!(resolve(
                client,
                "workflowStates",
                "id name team { id }",
                value,
                Some(team_id)
            )?),
        ),
        Field::Assignee => (
            "assigneeId",
            if clear {
                Value::Null
            } else if value == "me" {
                result_field(
                    request(client, "query { viewer { id } }", json!({}), false)?,
                    "viewer",
                )?["id"]
                    .clone()
            } else {
                json!(resolve(client, "users", "id name email", value, None)?)
            },
        ),
        Field::Project => (
            "projectId",
            if clear {
                Value::Null
            } else {
                json!(resolve(client, "projects", "id name", value, None)?)
            },
        ),
        Field::Parent => (
            "parentId",
            if clear {
                Value::Null
            } else {
                json!(issue_id(client, value)?)
            },
        ),
        Field::Priority => {
            let p = value
                .parse::<u8>()
                .map_err(|_| error("Priority must be 0 through 4"))?;
            if p > 4 {
                return Err(error("Priority must be 0 through 4"));
            }
            ("priority", json!(p))
        }
        Field::Estimate => (
            "estimate",
            if clear {
                Value::Null
            } else {
                let estimate = value
                    .parse::<i32>()
                    .map_err(|_| error("Estimate must be a nonnegative 32-bit integer"))?;
                if estimate < 0 {
                    return Err(error("Estimate must be nonnegative"));
                }
                json!(estimate)
            },
        ),
        Field::DueDate => (
            "dueDate",
            if clear {
                Value::Null
            } else {
                validate_date(value)?;
                json!(value)
            },
        ),
    };
    Ok(pair)
}
fn validate_date(value: &str) -> Result<(), AppError> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(i, b)| i == 4 || i == 7 || b.is_ascii_digit())
    {
        return Err(error("Due date must be YYYY-MM-DD"));
    }
    let y = value[..4].parse::<u16>().unwrap();
    let m = value[5..7].parse::<u8>().unwrap();
    let d = value[8..].parse::<u8>().unwrap();
    let max = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    if y == 0 || d == 0 || d > max {
        return Err(error("Due date is not a valid calendar date"));
    }
    Ok(())
}
fn body(path: &PathBuf) -> Result<String, AppError> {
    let mut reader: Box<dyn Read> = if path.as_os_str() == "-" {
        Box::new(std::io::stdin())
    } else {
        Box::new(std::fs::File::open(path).map_err(|e| AppError::new("linear_body", e))?)
    };
    let mut text = String::new();
    reader
        .by_ref()
        .take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|e| AppError::new("linear_body", e))?;
    if text.len() > 2 * 1024 * 1024 {
        return Err(error("Body exceeds 2 MiB"));
    }
    Ok(text)
}
fn update(client: &Client, id: &str, input: Value) -> Result<Value, AppError> {
    let query = format!(
        "mutation($id:String!,$input:IssueUpdateInput!) {{ issueUpdate(id:$id,input:$input) {{ success issue {{ {ISSUE} }} }} }}"
    );
    checked_mutation(result_field(
        request(client, &query, json!({"id":id,"input":input}), true)?,
        "issueUpdate",
    )?)
}

fn list_kind(command: &Operation) -> Option<human::List> {
    match command {
        Operation::List { .. } | Operation::Search { .. } | Operation::Children { .. } => {
            Some(human::List::Issues)
        }
        Operation::Comments { .. } => Some(human::List::Comments),
        Operation::Attachments { .. } => Some(human::List::Attachments),
        Operation::Relations { .. } => Some(human::List::Relations),
        Operation::Team { command } => Some(match command {
            TeamCommand::List { .. } => human::List::Teams,
            TeamCommand::States { .. } => human::List::States,
            TeamCommand::Labels { .. } => human::List::Labels,
            TeamCommand::Members { .. } => human::List::Members,
        }),
        Operation::Project { .. } => Some(human::List::Projects),
        _ => None,
    }
}

pub(super) fn execute(args: LinearCommand) -> Result<CommandOutput, AppError> {
    let list = list_kind(&args.command);
    let workspace = args.workspace;
    let (name, data) = match args.command {
        Operation::Unlink => {
            linear::context::unlink().map_err(convert)?;
            ("linear.unlink", json!({"unlinked":true}))
        }
        Operation::Link { id } => {
            let c = client(workspace.as_deref())?;
            let id = issue_id(&c, &id)?;
            let linked = linear::context::link(&id, workspace.as_deref()).map_err(convert)?;
            (
                "linear.link",
                json!({"issueId":linked.issue_id,"workspace":linked.workspace}),
            )
        }
        Operation::Issue { target: t, full } => {
            let (id, w) = target(t.into(), workspace)?;
            ("linear.issue", issue(&client(w.as_deref())?, &id, full)?)
        }
        Operation::Comments { target: t, page: p } => (
            "linear.comments",
            issue_connection(
                t,
                workspace,
                &p,
                "comments",
                "id body url createdAt user { id name }",
            )?,
        ),
        Operation::Attachments { target: t, page: p } => (
            "linear.attachments",
            issue_connection(t, workspace, &p, "attachments", "id title url")?,
        ),
        Operation::Children { target: t, page: p } => (
            "linear.children",
            issue_connection(t, workspace, &p, "children", ISSUE)?,
        ),
        Operation::Relations {
            target: t,
            page: p,
            inverse,
        } => (
            "linear.relations",
            issue_connection(
                t,
                workspace,
                &p,
                if inverse {
                    "inverseRelations"
                } else {
                    "relations"
                },
                "id type issue { id identifier title } relatedIssue { id identifier title }",
            )?,
        ),
        Operation::Search { query, page: p } => {
            let c = client(workspace.as_deref())?;
            let q = format!(
                "query($term:String!,$first:Int!,$after:String) {{ searchIssues(term:$term,first:$first,after:$after) {{ nodes {{ {ISSUE} }} {PAGE_INFO} }} }}"
            );
            (
                "linear.search",
                result_field(
                    request(
                        &c,
                        &q,
                        json!({"term":query,"first":p.limit,"after":p.after}),
                        false,
                    )?,
                    "searchIssues",
                )?,
            )
        }
        Operation::List {
            page: p,
            team: t,
            state,
            assignee,
            priority,
        } => {
            let c = client(workspace.as_deref())?;
            let mut filter = json!({});
            let tid = t.as_deref().map(|t| team(&c, t)).transpose()?;
            if let Some(t) = &tid {
                filter["team"] = json!({"id":{"eq":t}});
            }
            if let Some(s) = state {
                filter["state"] = json!({"id":{"eq":resolve(&c,"workflowStates","id name team { id }",&s,tid.as_deref())?}});
            }
            if let Some(a) = assignee {
                let (_, v) = field_value(&c, Field::Assignee, &a, "")?;
                filter["assignee"] = if v.is_null() {
                    json!({"null":true})
                } else {
                    json!({"id":{"eq":v}})
                };
            }
            if let Some(p) = priority {
                filter["priority"] = json!({"eq":p});
            }
            let q = format!(
                "query($filter:IssueFilter!,$first:Int!,$after:String) {{ issues(filter:$filter,first:$first,after:$after) {{ nodes {{ {ISSUE} }} {PAGE_INFO} }} }}"
            );
            (
                "linear.list",
                result_field(
                    request(
                        &c,
                        &q,
                        json!({"filter":filter,"first":p.limit,"after":p.after}),
                        false,
                    )?,
                    "issues",
                )?,
            )
        }
        Operation::Team { command } => {
            let c = client(workspace.as_deref())?;
            let data = match &command {
                TeamCommand::List { page: p } => page(&c, "teams", "id key name", p)?,
                TeamCommand::States { team: t, page: p }
                | TeamCommand::Labels { team: t, page: p }
                | TeamCommand::Members { team: t, page: p } => {
                    let tid = team(&c, t)?;
                    // Choose connection using the already parsed command discriminant below.
                    let (field, selection) = match &command {
                        TeamCommand::States { .. } => ("states", "id name type"),
                        TeamCommand::Labels { .. } => ("labels", "id name"),
                        _ => ("members", "id name email"),
                    };
                    let q = format!(
                        "query($id:String!,$first:Int!,$after:String) {{ team(id:$id) {{ {field}(first:$first,after:$after) {{ nodes {{ {selection} }} {PAGE_INFO} }} }} }}"
                    );
                    result_field(
                        result_field(
                            request(
                                &c,
                                &q,
                                json!({"id":tid,"first":p.limit,"after":p.after}),
                                false,
                            )?,
                            "team",
                        )?,
                        field,
                    )?
                }
            };
            ("linear.team", data)
        }
        Operation::Project {
            command: ProjectCommand::List { page: p },
        } => (
            "linear.project.list",
            page(
                &client(workspace.as_deref())?,
                "projects",
                "id name url",
                &p,
            )?,
        ),
        Operation::Create(save) => ("linear.save", save_issue(save, workspace)?),
        Operation::Set {
            target: t,
            field,
            value,
        } => ("linear.set", set_field(t, workspace, field, &value)?),
        Operation::Status {
            command: StatusCommand::Set { target: t, state },
        } => (
            "linear.status.set",
            set_field(t, workspace, Field::Status, &state)?,
        ),
        Operation::Comment {
            command:
                CommentCommand::Add {
                    target: t,
                    body: b,
                    body_file,
                    request_id,
                },
        } => {
            let text = if let Some(p) = body_file {
                body(&p)?
            } else {
                b.unwrap_or_default()
            };
            if text.trim().is_empty() {
                return Err(error("Comment body must not be empty"));
            }
            let (id, w) = target(t, workspace)?;
            let c = client(w.as_deref())?;
            let id = issue_id(&c, &id)?;
            (
                "linear.comment.add",
                mutation(
                    &c,
                    "commentCreate",
                    "CommentCreateInput",
                    json!({"issueId":id,"body":text,"id":request_id.unwrap_or_else(uuid::Uuid::new_v4)}),
                    "comment { id body url }",
                )?,
            )
        }
        Operation::Attach {
            target: t,
            url,
            title,
        } => {
            if title.trim().is_empty() {
                return Err(error("Attachment title must not be empty"));
            }
            let parsed = url::Url::parse(&url)
                .map_err(|_| error("Attachment URL must be absolute HTTP(S)"))?;
            if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                return Err(error("Attachment URL must be absolute HTTP(S)"));
            }
            let (id, w) = target(t, workspace)?;
            let c = client(w.as_deref())?;
            let id = issue_id(&c, &id)?;
            (
                "linear.attach",
                mutation(
                    &c,
                    "attachmentCreate",
                    "AttachmentCreateInput",
                    json!({"issueId":id,"url":url,"title":title,"id":uuid::Uuid::new_v4()}),
                    "attachment { id title url }",
                )?,
            )
        }
        Operation::Label { command } => {
            let (t, labels, key) = match command {
                LabelCommand::Add { target, labels } => (target, labels, "addedLabelIds"),
                LabelCommand::Remove { target, labels } => (target, labels, "removedLabelIds"),
                LabelCommand::Set { target, labels } => (target, labels, "labelIds"),
            };
            if labels.is_empty() && key != "labelIds" {
                return Err(error("Specify at least one label"));
            }
            let (id, w) = target(t, workspace)?;
            let c = client(w.as_deref())?;
            let i = issue(&c, &id, false)?;
            let tid = string(&i["team"], "id")?;
            let ids = resolve_labels(&c, &labels, &tid)?;
            (
                "linear.label",
                update(&c, &string(&i, "id")?, json!({key:ids}))?,
            )
        }
        Operation::Relation { command } => match command {
            RelationCommand::Add {
                target: t,
                related,
                kind,
                request_id,
            } => {
                let (id, w) = target(t, workspace)?;
                let c = client(w.as_deref())?;
                let id = issue_id(&c, &id)?;
                let related = issue_id(&c, &related)?;
                if id == related {
                    return Err(error("An issue cannot relate to itself"));
                }
                let kind = match kind {
                    RelationType::Blocks => "blocks",
                    RelationType::Duplicate => "duplicate",
                    RelationType::Related => "related",
                    RelationType::Similar => "similar",
                };
                (
                    "linear.relation.add",
                    mutation(
                        &c,
                        "issueRelationCreate",
                        "IssueRelationCreateInput",
                        json!({"id":request_id.unwrap_or_else(uuid::Uuid::new_v4),"issueId":id,"relatedIssueId":related,"type":kind}),
                        "issueRelation { id type }",
                    )?,
                )
            }
            RelationCommand::Remove { id } => {
                let c = client(workspace.as_deref())?;
                (
                    "linear.relation.remove",
                    checked_mutation(result_field(
                        request(
                            &c,
                            "mutation($id:String!) { issueRelationDelete(id:$id) { success } }",
                            json!({"id":id}),
                            true,
                        )?,
                        "issueRelationDelete",
                    )?)?,
                )
            }
        },
    };
    Ok(output(name, data, list))
}
fn issue_connection(
    t: Target,
    workspace: Option<String>,
    p: &Page,
    field: &str,
    selection: &str,
) -> Result<Value, AppError> {
    let (id, w) = target(t, workspace)?;
    let c = client(w.as_deref())?;
    let q = format!(
        "query($id:String!,$first:Int!,$after:String) {{ issue(id:$id) {{ {field}(first:$first,after:$after) {{ nodes {{ {selection} }} {PAGE_INFO} }} }} }}"
    );
    result_field(
        result_field(
            request(
                &c,
                &q,
                json!({"id":id,"first":p.limit,"after":p.after}),
                false,
            )?,
            "issue",
        )?,
        field,
    )
}
fn set_field(t: Target, w: Option<String>, field: Field, value: &str) -> Result<Value, AppError> {
    let (id, w) = target(t, w)?;
    let c = client(w.as_deref())?;
    let i = issue(&c, &id, false)?;
    let (k, v) = field_value(&c, field, value, &string(&i["team"], "id")?)?;
    update(&c, &string(&i, "id")?, json!({k:v}))
}
fn save_issue(save: Save, mut workspace: Option<String>) -> Result<Value, AppError> {
    let existing = if save.current || save.id.is_some() {
        let (id, w) = target(
            Target {
                id: save.id,
                current: save.current,
            },
            workspace,
        )?;
        workspace = w;
        Some(id)
    } else {
        None
    };
    let parent = if save.parent_current {
        let linked = linear::context::resolve_current().map_err(convert)?;
        workspace = linked_workspace(workspace, linked.workspace)?;
        Some(linked.issue_id)
    } else {
        save.parent
    };
    let c = client(workspace.as_deref())?;
    let current = existing
        .as_deref()
        .map(|id| issue(&c, id, false))
        .transpose()?;
    let tid = if let Some(t) = save.team {
        team(&c, &t)?
    } else if let Some(i) = &current {
        string(&i["team"], "id")?
    } else {
        return Err(error("Creating an issue requires --team"));
    };
    let mut input = json!({});
    if let Some(title) = save.title {
        let (k, v) = field_value(&c, Field::Title, &title, &tid)?;
        input[k] = v;
    } else if current.is_none() {
        return Err(error("Creating an issue requires --title"));
    }
    if let Some(path) = save.body_file {
        input["description"] = json!(body(&path)?);
    }
    for (f, v) in [
        (Field::Status, save.status),
        (Field::Assignee, save.assignee),
        (Field::Project, save.project),
        (Field::Parent, parent),
        (Field::Priority, save.priority.map(|p| p.to_string())),
        (Field::Estimate, save.estimate.map(|p| p.to_string())),
        (Field::DueDate, save.due_date),
    ] {
        if let Some(v) = v {
            let (k, v) = field_value(&c, f, &v, &tid)?;
            input[k] = v;
        }
    }
    if let Some(labels) = save.labels {
        input["labelIds"] = json!(resolve_labels(&c, &labels, &tid)?);
    }
    if let Some(i) = current {
        if save.request_id.is_some() {
            return Err(error("--request-id applies only to creates"));
        }
        // Explicit team selection can move an existing issue.
        if i["team"]["id"].as_str() != Some(&tid) {
            input["teamId"] = json!(tid);
        }
        if input.as_object().is_none_or(|o| o.is_empty()) {
            return Err(error("Specify at least one field to update"));
        }
        update(&c, &string(&i, "id")?, input)
    } else {
        input["teamId"] = json!(tid);
        input["id"] = json!(save.request_id.unwrap_or_else(uuid::Uuid::new_v4));
        mutation(
            &c,
            "issueCreate",
            "IssueCreateInput",
            input,
            &format!("issue {{ {ISSUE} }}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn metadata_lookups_use_one_exact_filtered_query() {
        for (field, kind) in [
            ("teams", "TeamFilter"),
            ("workflowStates", "WorkflowStateFilter"),
            ("users", "UserFilter"),
        ] {
            let (query, variables) = metadata_query(
                field,
                "id name",
                &["In Progress".into()],
                Some("team-id"),
                None,
            );
            assert!(query.contains(&format!("$filter:{kind}!")));
            assert!(query.contains("filter:$filter"));
            assert_eq!(
                variables["filter"]["or"][0]["name"]["eqIgnoreCase"],
                "In Progress"
            );
            assert!(variables["after"].is_null());
            if field == "workflowStates" {
                assert_eq!(variables["filter"]["team"]["id"]["eq"], "team-id");
            }
        }
    }

    #[test]
    fn readable_linear_actual_commands_select_columns_and_preserve_json() {
        use clap::Parser;
        let data = json!({"nodes":[{"id":"one","identifier":"ENG-7","title":"Issue","name":"A name","key":"ENG","type":"started","state":{"name":"In progress"},"assignee":null}],"opaque":{"Provider":"Claude","raw_ms":123}});
        for (args, header) in [
            (vec!["list"], "ISSUE"),
            (vec!["search", "a query"], "ISSUE"),
            (vec!["children", "--id", "ENG-1"], "ISSUE"),
            (vec!["comments", "--id", "ENG-1"], "COMMENT"),
            (vec!["attachments", "--id", "ENG-1"], "ATTACHMENT"),
            (vec!["relations", "--id", "ENG-1"], "RELATION"),
            (vec!["team", "list"], "TEAM"),
            (vec!["team", "states", "ENG"], "STATE"),
            (vec!["team", "labels", "ENG"], "LABEL"),
            (vec!["team", "members", "ENG"], "MEMBER"),
            (vec!["project", "list"], "PROJECT"),
        ] {
            let cli =
                super::super::Cli::try_parse_from(["cagents", "linear"].into_iter().chain(args))
                    .unwrap();
            let super::super::TopCommand::Linear(command) = cli.command else {
                panic!("expected Linear");
            };
            // Check the list projection independently of detail-command rendering.
            let result = output("linear.test", data.clone(), list_kind(&command.command));
            assert!(result.human.starts_with(header), "{}", result.human);
            assert_eq!(result.data, data);
        }
        let cli = super::super::Cli::try_parse_from([
            "cagents",
            "linear",
            "set",
            "--id",
            "ENG-1",
            "title",
            "New title",
        ])
        .unwrap();
        let super::super::TopCommand::Linear(command) = cli.command else {
            panic!("expected Linear");
        };
        let result = output("linear.set", data.clone(), list_kind(&command.command));
        assert_eq!(result.human, serde_json::to_string_pretty(&data).unwrap());
        assert_eq!(result.data, data);
    }

    #[test]
    fn human_issue_details_preserve_full_connections_and_cursors() {
        use clap::Parser;
        let cli =
            super::super::Cli::try_parse_from(["cagents", "linear", "issue", "ENG-1", "--full"])
                .unwrap();
        let super::super::TopCommand::Linear(command) = cli.command else {
            panic!("expected Linear");
        };
        assert!(matches!(
            command.command,
            Operation::Issue { full: true, .. }
        ));
        let data = json!({
            "identifier": "ENG-1",
            "title": "Example",
            "description": "Complete issue description\nwith a second line",
            "comments": {"nodes": [{"body": "Requested full comment"}],
                "pageInfo": {"hasNextPage": true, "endCursor": "comment next\nexact"}},
            "attachments": {"nodes": [{"url": "https://example.invalid/file"}]},
            "children": {"nodes": [{"identifier": "ENG-2"}]},
            "relations": {"nodes": [{"relatedIssue": {"identifier": "ENG-3"}}]},
            "inverseRelations": {"nodes": [{"issue": {"identifier": "ENG-4"}}]},
            "unknownFetchedField": {"kept": true}
        });
        let rendered = output("linear.issue", data.clone(), list_kind(&command.command));
        assert_eq!(rendered.data, data);
        assert_eq!(
            serde_json::from_str::<Value>(&rendered.human).unwrap(),
            data
        );
    }

    #[test]
    fn human_comment_relation_and_child_details_are_lossless() {
        use clap::Parser;
        let comment = "complete comment body\nwith Markdown **content** ".repeat(20);
        for (operation, command, populated) in [
            (
                "comments",
                "linear.comments",
                json!({"nodes": [{"id": "comment-1", "body": comment,
                    "user": {"id": "user-1", "name": "Comment author"}}],
                    "pageInfo": {"hasNextPage": true, "endCursor": "comment next\nexact"},
                    "unknownFetchedField": {"kept": true}}),
            ),
            (
                "relations",
                "linear.relations",
                json!({"nodes": [{"id": "relation-1", "type": "related",
                    "issue": {"identifier": "ENG-1", "title": "Original issue"},
                    "relatedIssue": {"identifier": "ENG-2", "title": "Related issue"}}],
                    "pageInfo": {"hasNextPage": true, "endCursor": "relation next\nexact"},
                    "unknownFetchedField": {"kept": true}}),
            ),
            (
                "children",
                "linear.children",
                json!({"nodes": [{"identifier": "ENG-2",
                    "description": "The complete child issue description\nwith another line"}],
                    "pageInfo": {"hasNextPage": true, "endCursor": "child next\nexact"},
                    "unknownFetchedField": {"kept": true}}),
            ),
        ] {
            let cli = super::super::Cli::try_parse_from([
                "cagents", "linear", operation, "--id", "ENG-1",
            ])
            .unwrap();
            let super::super::TopCommand::Linear(parsed) = cli.command else {
                panic!("expected Linear");
            };
            for data in [
                populated,
                json!({"nodes": [],
                    "pageInfo": {"hasNextPage": false, "endCursor": null},
                    "unknownFetchedField": {"kept": true}}),
            ] {
                let rendered = output(command, data.clone(), list_kind(&parsed.command));
                assert_eq!(rendered.data, data);
                assert_eq!(
                    serde_json::from_str::<Value>(&rendered.human).unwrap(),
                    data
                );
            }
        }
    }

    #[test]
    fn dates_validate_calendar() {
        for d in ["2024-02-29", "2026-10-09"] {
            assert!(validate_date(d).is_ok());
        }
        for d in [
            "2025-02-29",
            "2026-04-31",
            "0000-01-01",
            "2026-0x-10",
            "2026-01-00",
        ] {
            assert!(validate_date(d).is_err());
        }
    }
    #[test]
    fn labels_accept_global_and_ancestor_teams_only() {
        let teams = BTreeSet::from(["child".to_owned(), "parent".to_owned()]);
        for node in [
            json!({"team":null}),
            json!({"team":{"id":"child"}}),
            json!({"team":{"id":"parent"}}),
        ] {
            assert!(label_eligible(&node, &teams));
        }
        for node in [
            json!({"team":{"id":"sibling"}}),
            json!({}),
            json!({"team":{}}),
        ] {
            assert!(!label_eligible(&node, &teams));
        }
    }
    #[test]
    fn ancestry_rejects_cycles_and_excessive_depth() {
        let mut teams = BTreeSet::new();
        visit_label_team(&mut teams, "child").unwrap();
        visit_label_team(&mut teams, "parent").unwrap();
        assert!(visit_label_team(&mut teams, "child").is_err());
        for i in 2..64 {
            visit_label_team(&mut teams, &i.to_string()).unwrap();
        }
        assert_eq!(teams.len(), 64);
        assert!(visit_label_team(&mut teams, "too-deep").is_err());
    }
    #[test]
    fn names_require_unique_ids() {
        assert_eq!(unique(vec!["a".into(), "a".into()], "x").unwrap(), "a");
        assert!(unique(vec![], "x").is_err());
        assert!(unique(vec!["a".into(), "b".into()], "x").is_err());
    }
    #[test]
    fn false_mutations_are_errors() {
        assert!(checked_mutation(json!({"success":false})).is_err());
        assert!(checked_mutation(json!({})).is_err());
        assert!(checked_mutation(json!({"success":true})).is_ok());
    }
    #[test]
    fn workspace_conflicts_are_rejected() {
        assert!(linked_workspace(Some("a".into()), Some("b".into())).is_err());
        assert_eq!(
            linked_workspace(None, Some("b".into())).unwrap(),
            Some("b".into())
        );
    }
    #[test]
    fn current_targets_skip_issue_position() {
        use clap::Parser;
        for args in [
            vec![
                "cagents",
                "linear",
                "set",
                "--current",
                "title",
                "new title",
            ],
            vec!["cagents", "linear", "status", "set", "--current", "Done"],
            vec![
                "cagents",
                "linear",
                "relation",
                "add",
                "--current",
                "ENG-2",
                "--kind",
                "related",
            ],
            vec!["cagents", "linear", "label", "add", "--current", "bug"],
            vec!["cagents", "linear", "label", "set", "--current"],
            vec![
                "cagents",
                "linear",
                "set",
                "--id",
                "ENG-1",
                "title",
                "new title",
            ],
        ] {
            assert!(
                super::super::Cli::try_parse_from(args.clone()).is_ok(),
                "{args:?}"
            );
        }
    }
    #[test]
    fn label_target_and_values_are_unambiguous() {
        use clap::Parser;
        let cli = super::super::Cli::try_parse_from([
            "cagents",
            "linear",
            "label",
            "add",
            "--current",
            "bug",
            "enhancement",
        ])
        .unwrap();
        let super::super::TopCommand::Linear(args) = cli.command else {
            panic!("expected Linear")
        };
        let Operation::Label {
            command: LabelCommand::Add { target, labels },
        } = args.command
        else {
            panic!("expected labels")
        };
        assert!(target.current);
        assert_eq!(target.id, None);
        assert_eq!(labels, vec!["bug", "enhancement"]);
        let cli = super::super::Cli::try_parse_from([
            "cagents", "linear", "label", "set", "--id", "ENG-1",
        ])
        .unwrap();
        let super::super::TopCommand::Linear(args) = cli.command else {
            panic!("expected Linear")
        };
        let Operation::Label {
            command: LabelCommand::Set { target, labels },
        } = args.command
        else {
            panic!("expected labels")
        };
        assert!(!target.current);
        assert_eq!(target.id.as_deref(), Some("ENG-1"));
        assert!(labels.is_empty());
    }
    #[test]
    fn command_tree_is_consistent() {
        use clap::CommandFactory;
        super::super::Cli::command().debug_assert();
    }
}
