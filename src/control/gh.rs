use super::*;
use crate::{
    github_identity::{GitHubRepoKey, PullRequestKey},
    github_status as pr_status,
};

#[derive(Debug, clap::Subcommand)]
pub enum GhCommand {
    /// Read a pull request's status through the shared cache.
    #[command(after_help = "Makes at most one network read per PR every two minutes.")]
    PrStatus {
        /// Positive pull request number.
        #[arg(value_parser = clap::value_parser!(u64).range(1..=i32::MAX as u64))]
        number: u64,
        /// Repository owner/name. Required so a cached read needs no discovery request.
        #[arg(short = 'R', long = "repo")]
        repository: String,
        /// GitHub host (defaults to github.com); enterprise hosts are supported.
        #[arg(long, default_value = "github.com")]
        hostname: String,
    },
}

pub(super) fn execute(command: GhCommand) -> Result<CommandOutput, AppError> {
    let GhCommand::PrStatus {
        number,
        repository,
        hostname,
    } = command;
    let key = target(&repository, &hostname, number)
        .map_err(|error| AppError::new("github_status", error))?;
    let report = pr_status::read(&key, &crate::process::CancellationToken::new())
        .map_err(|error| AppError::new("github_status", error))?;
    Ok(CommandOutput {
        command: "gh.pr_status",
        human: human(&report),
        data: serde_json::to_value(report)
            .map_err(|error| AppError::new("github_status", error))?,
    })
}

fn target(repository: &str, hostname: &str, number: u64) -> Result<PullRequestKey, String> {
    let (owner, name) = repository
        .split_once('/')
        .ok_or("Use --repo OWNER/REPOSITORY")?;
    let url = url::Url::parse(&format!("https://{hostname}/"))
        .map_err(|_| "Use a valid GitHub hostname")?;
    if url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err("Use a GitHub hostname without a path or credentials".into());
    }
    let repository = GitHubRepoKey::new(
        url.host_str().ok_or("Missing GitHub hostname")?,
        url.port(),
        owner,
        name,
    )
    .map_err(|error| error.to_string())?;
    PullRequestKey::new(repository, number).map_err(|error| error.to_string())
}

fn human(report: &pr_status::Report) -> String {
    let target = format!(
        "{} #{}",
        report.target.repository.gh_selector(),
        report.target.number
    );
    let age = report.age_seconds.map_or_else(
        || "no saved snapshot".into(),
        |age| format!("snapshot {age}s old"),
    );
    if let Some(error) = &report.error {
        return format!(
            "{target}: status unavailable ({age})\n{error}{}",
            report.retry_at.map_or_else(String::new, |due| format!(
                "\nNext network attempt after Unix timestamp {due}"
            ))
        );
    }
    let Some(snapshot) = &report.snapshot else {
        return format!("{target}: status unavailable");
    };
    let pr = &snapshot.pull_request;
    let (passed, attention): (Vec<_>, Vec<_>) = snapshot
        .checks
        .iter()
        .partition(|check| pr_status::check_passed(check));
    let mut result = format!(
        "{target}: {}{}\n{}\nHead: {}\nChecks: {}{}{}\nReview: {} · Merge: {}\n{} ({}, {age})",
        pr.state,
        if pr.is_draft { " (draft)" } else { "" },
        pr.title,
        pr.head_ref_oid,
        if snapshot.is_rest_bootstrap() {
            "unknown (REST bootstrap read)"
        } else {
            snapshot.checks_state.as_deref().unwrap_or("none reported")
        },
        if passed.is_empty() {
            String::new()
        } else {
            format!(", {} passed", passed.len())
        },
        match (snapshot.checks_complete, snapshot.transport) {
            _ if snapshot.is_rest_bootstrap() => "",
            (true, _) => "",
            (false, pr_status::Transport::Graphql) => {
                " (first 100 contexts; aggregate includes all)"
            }
            (false, pr_status::Transport::Rest) => " (first 100 contexts; the rest unread)",
        },
        if snapshot.is_rest_bootstrap() {
            "unknown"
        } else if snapshot.transport == pr_status::Transport::Rest {
            "unknown (REST read while GraphQL is rate limited)"
        } else {
            pr.review_decision.as_deref().unwrap_or("none reported")
        },
        pr.merge_state_status.as_deref().unwrap_or(&pr.mergeable),
        pr.url,
        report.source
    );
    // Passing checks are counted above; only the rest need a line, and a
    // finished one its link, where its log is.
    for check in attention {
        let name = check["name"]
            .as_str()
            .or_else(|| check["context"].as_str())
            .unwrap_or("Unnamed check");
        let state = check["conclusion"]
            .as_str()
            .filter(|value| !value.is_empty())
            .or_else(|| check["status"].as_str())
            .or_else(|| check["state"].as_str())
            .unwrap_or("unknown");
        result.push_str(&format!("\n  {state}: {name}"));
        let finished = if check["__typename"] == "StatusContext" {
            !matches!(check["state"].as_str(), Some("PENDING" | "EXPECTED"))
        } else {
            check["status"] == "COMPLETED"
        };
        if let Some(link) = check["detailsUrl"]
            .as_str()
            .or_else(|| check["targetUrl"].as_str())
            .filter(|_| finished)
        {
            result.push_str(&format!(" {link}"));
        }
        if let Some(description) = check["description"]
            .as_str()
            .filter(|description| finished && !description.is_empty())
        {
            result.push_str(&format!(" ({description})"));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_target_requires_no_discovery_and_rejects_ambiguous_selectors() {
        assert_eq!(
            target("Owner/Repo", "GHE.EXAMPLE:8443", 4)
                .unwrap()
                .repository
                .gh_selector(),
            "ghe.example:8443/Owner/Repo"
        );
        for (repository, hostname, number) in [
            ("o/r/other", "github.com", 4),
            ("o/r", "user@github.com", 4),
            ("o/r", "github.com/private", 4),
            ("o/r", "github.com", 0),
        ] {
            assert!(target(repository, hostname, number).is_err());
        }
        assert!(
            Cli::try_parse_from(["cagents", "gh", "pr-status", "2", "-R", "o/r", "--json"]).is_ok()
        );
        assert!(Cli::try_parse_from(["cagents", "gh", "pr-status", "2"]).is_err());
        let command = Cli::try_parse_from(["cagents", "gh", "pr-status", "2", "-R", "o/r"])
            .unwrap()
            .command;
        assert!(!staged_command_requires_daemon(&command));
        assert!(!routes_to_staged_owner(&command));
    }

    #[test]
    fn human_status_counts_passing_checks_and_links_finished_failures() {
        let key = target("Owner/Repo", "github.com", 7).unwrap();
        let link = "https://github.com/Owner/Repo/actions/runs/1/job/2";
        let pull_request = serde_json::from_value(serde_json::json!({
            "number":7,"title":"Fix it","url":"https://github.com/Owner/Repo/pull/7","state":"OPEN",
            "isDraft":false,"headRefName":"fix","headRefOid":"a".repeat(40),"baseRefName":"main",
            "mergeable":"MERGEABLE","mergeStateStatus":"BLOCKED","reviewDecision":null,
            "updatedAt":"2026-11-09T00:00:00Z"}))
        .unwrap();
        let checks = vec![
            serde_json::json!({"__typename":"CheckRun","name":"Contract structure","status":"COMPLETED","conclusion":"SUCCESS"}),
            serde_json::json!({"__typename":"CheckRun","name":"Formatting","status":"COMPLETED","conclusion":"SUCCESS"}),
            serde_json::json!({"__typename":"CheckRun","name":"Suites (tooling)","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":link}),
            serde_json::json!({"__typename":"CheckRun","name":"Script contracts","status":"IN_PROGRESS","conclusion":null,"detailsUrl":link}),
            serde_json::json!({"__typename":"StatusContext","context":"lint","state":"ERROR","targetUrl":link,"description":"Lint crashed"}),
            serde_json::json!({"__typename":"StatusContext","context":"deploy","state":"PENDING","targetUrl":link,"description":"Waiting"}),
        ];
        let report = pr_status::Report {
            target: key,
            fetched_at: Some(100),
            age_seconds: Some(5),
            stale: false,
            source: "cache",
            snapshot: Some(pr_status::Snapshot {
                pull_request,
                checks_state: Some("FAILURE".into()),
                checks,
                checks_complete: true,
                primary_cost: Some(1),
                transport: pr_status::Transport::Graphql,
            }),
            error: None,
            retry_at: None,
            outcome: "succeeded",
            limitations: Vec::new(),
        };
        assert_eq!(
            human(&report),
            format!(
                "github.com/Owner/Repo #7: OPEN\nFix it\nHead: {}\nChecks: FAILURE, 2 passed\n\
                 Review: none reported · Merge: BLOCKED\nhttps://github.com/Owner/Repo/pull/7 (cache, snapshot 5s old)\n  \
                 FAILURE: Suites (tooling) {link}\n  IN_PROGRESS: Script contracts\n  \
                 ERROR: lint {link} (Lint crashed)\n  PENDING: deploy",
                "a".repeat(40)
            )
        );
    }
}
