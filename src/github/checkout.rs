//! Forge metadata needed to check out the writable head of a pull request.
use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutHead {
    pub repository: GitHubRepoKey,
    pub branch: String,
    pub oid: String,
    pub clone_url: String,
    pub ssh_url: String,
    pub can_push: bool,
}

impl<E: ProcessExecutor> GitHubService<E> {
    pub fn checkout_head(&self, key: &PullRequestKey, cancellation: &CancellationToken) -> Result<CheckoutHead, GitHubError> {
        key.validate()?;
        let data = self.graphql(&key.repository,
            "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){viewerPermission pullRequest(number:$number){headRefName headRefOid maintainerCanModify headRepository{name url sshUrl viewerPermission owner{login}}}}}",
            Self::pull_request_variables(key), cancellation)?;
        let pr = data.pointer("/repository/pullRequest")
            .ok_or_else(|| GitHubError::Inspection("Pull request no longer exists".into()))?;
        let head = pr.get("headRepository").filter(|head| !head.is_null())
            .ok_or_else(|| GitHubError::Inspection("The pull request's head repository was deleted".into()))?;
        let text = |value: &serde_json::Value, field: &str| value.get(field)
            .and_then(serde_json::Value::as_str).map(str::to_owned)
            .ok_or_else(|| GitHubError::Inspection(format!("GitHub returned no checkout {field}")));
        let owner = head.pointer("/owner/login").and_then(serde_json::Value::as_str)
            .ok_or_else(|| GitHubError::Inspection("GitHub returned no head repository owner".into()))?;
        let repository = GitHubRepoKey::new(&key.repository.host, key.repository.port, owner, text(head, "name")?)?;
        let branch = text(pr, "headRefName")?;
        crate::repository::ValidatedBranchName::parse(&branch)
            .map_err(|error| GitHubError::Inspection(error.to_string()))?;
        let writable = |permission: Option<&serde_json::Value>| permission.and_then(serde_json::Value::as_str)
            .is_some_and(|permission| matches!(permission, "WRITE" | "MAINTAIN" | "ADMIN"));
        let can_push = writable(head.get("viewerPermission"))
            || (pr.get("maintainerCanModify").and_then(serde_json::Value::as_bool) == Some(true)
                && writable(data.pointer("/repository/viewerPermission")));
        Ok(CheckoutHead { repository, branch, oid: text(pr, "headRefOid")?,
            clone_url: format!("{}.git", text(head, "url")?.trim_end_matches('/')),
            ssh_url: text(head, "sshUrl")?, can_push })
    }
}
