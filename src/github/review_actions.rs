//! Interactive pull request actions shared by the native review surfaces.
use super::*;

impl<E: ProcessExecutor> GitHubService<E> {
    /// Search the repository, independently of the browser's loaded pages.
    pub fn search_pull_request_page(
        &self,
        repository: &GitHubRepoKey,
        text: &str,
        cursor: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<PullRequestPage, GitHubError> {
        repository.validate()?;
        let search = search_query(repository, text);
        let query = "query($query:String!,$cursor:String){search(type:ISSUE,query:$query,first:50,after:$cursor){nodes{... on PullRequest{id number title state isDraft headRefName headRefOid baseRefName baseRefOid author{login} updatedAt url reviewDecision headRepository{name owner{login}}}} pageInfo{hasNextPage endCursor}}}";
        let mut variables = vec![("query", search.into())];
        if let Some(cursor) = cursor { variables.push(("cursor", cursor.into())); }
        let data = self.graphql(repository, query, variables, cancellation)?;
        let connection: Connection<serde_json::Value> = decode_value(data.get("search")
            .ok_or_else(|| GitHubError::Inspection("GitHub returned no search results".into()))?)?;
        let next = if connection.page_info.has_next_page {
            Some(next_cursor(&connection.page_info, cursor)?)
        } else { None };
        let pull_requests = connection.nodes.into_iter().map(|mut value| {
            value["headRepositoryOwner"] = value.pointer("/headRepository/owner").cloned().unwrap_or(serde_json::Value::Null);
            let pr: PullRequest = decode_value(&value)?;
            validate_pull_request(&pr, repository)?;
            Ok(pr)
        }).collect::<Result<Vec<_>, GitHubError>>()?;
        Ok(PullRequestPage { pull_requests, next_cursor: next })
    }
}

/// Match the browser's explicit #number and @author affordances. Query terms
/// remain variables; user text cannot change the repository/open constraints.
fn search_query(repository: &GitHubRepoKey, text: &str) -> String {
    let text = text.trim();
    let scope = format!("repo:{}/{} is:pr is:open", repository.owner, repository.name);
    let author = text.strip_prefix('@').filter(|author| !author.is_empty()
        && author.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-'));
    if let Some(author) = author {
        return format!("{scope} author:{author}");
    }
    if let Ok(number) = text.trim_start_matches('#').parse::<u64>() {
        return format!("{scope} {number}");
    }
    let quoted = text.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{scope} \"{quoted}\" in:title")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn search_scope_and_search_modes_are_explicit() {
        let repo = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        assert_eq!(search_query(&repo, "@alice"), "repo:owner/repo is:pr is:open author:alice");
        assert_eq!(search_query(&repo, "#1234"), "repo:owner/repo is:pr is:open 1234");
        assert_eq!(search_query(&repo, "fix sidebar"), "repo:owner/repo is:pr is:open \"fix sidebar\" in:title");
        assert!(!search_query(&repo, "fix repo:other/other").contains(" OR "));
    }
}
