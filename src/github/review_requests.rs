//! Current review authority and separately paged historical evidence. A review
//! decision (or an earlier COMMENTED review) does not identify a request.
use super::*;
pub mod scan;

const CURRENT: &str =
    "viewerLatestReviewRequest{id asCodeOwner requestedReviewer{__typename ... on Node{id}}}";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Viewer {
    pub id: String,
    pub login: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reviewer {
    #[serde(rename = "__typename")]
    pub kind: String,
    /// Unknown future union members without Node identity remain unclassified.
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Current {
    pub id: String,
    pub as_code_owner: bool,
    pub requested_reviewer: Option<Reviewer>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub number: u64,
    pub title: String,
    pub url: Url,
    pub viewer_latest_review_request: Option<Current>,
}

#[derive(Debug, Clone)]
pub struct Page {
    pub viewer: Viewer,
    pub entries: Vec<Entry>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestedEvent {
    pub id: String,
    pub created_at: String,
    pub requested_reviewer: Option<Reviewer>,
}

#[derive(Debug, Clone)]
pub struct HistoryPage {
    pub viewer: Viewer,
    pub current: Option<Current>,
    pub events: Vec<RequestedEvent>,
    pub previous_cursor: Option<String>,
}

impl<E: ProcessExecutor> GitHubService<E> {
    /// One stable creation-order page, independent of the browser's loaded
    /// rows. Callers retain the cursor between bounded polling batches.
    pub fn review_request_page(
        &self,
        repository: &GitHubRepoKey,
        cursor: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Page, GitHubError> {
        repository.validate()?;
        let query = format!(
            "query($owner:String!,$name:String!,$cursor:String){{viewer{{id login}} repository(owner:$owner,name:$name){{pullRequests(first:50,after:$cursor,states:OPEN,orderBy:{{field:CREATED_AT,direction:DESC}}){{nodes{{number title url {CURRENT}}} pageInfo{{hasNextPage endCursor}}}}}}}}"
        );
        let mut variables = vec![
            ("owner", repository.owner.as_str().into()),
            ("name", repository.name.as_str().into()),
        ];
        if let Some(cursor) = cursor {
            variables.push(("cursor", cursor.into()));
        }
        let data = self.graphql(repository, &query, variables, cancellation)?;
        let viewer = viewer(&data)?;
        let connection: Connection<Entry> = decode_value(
            data.pointer("/repository/pullRequests")
                .ok_or_else(|| missing("pull request page"))?,
        )?;
        for entry in &connection.nodes {
            if entry.number == 0 || pull_request_number(&entry.url, repository)? != entry.number {
                return Err(
                    GitHubValidationError::InvalidPullRequestUrl(entry.url.to_string()).into(),
                );
            }
            validate_current(entry.viewer_latest_review_request.as_ref())?;
        }
        let next_cursor = connection
            .page_info
            .has_next_page
            .then(|| next_cursor(&connection.page_info, None))
            .transpose()?;
        Ok(Page {
            viewer,
            entries: connection.nodes,
            next_cursor,
        })
    }

    /// Newest history first. Every page also reports the current request and
    /// viewer so an in-flight account switch/re-request cannot mix evidence.
    pub fn review_request_history_page(
        &self,
        key: &PullRequestKey,
        before: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<HistoryPage, GitHubError> {
        key.validate()?;
        let query = format!(
            "query($owner:String!,$name:String!,$number:Int!,$before:String){{viewer{{id login}} repository(owner:$owner,name:$name){{pullRequest(number:$number){{{CURRENT} timelineItems(last:50,before:$before,itemTypes:[REVIEW_REQUESTED_EVENT]){{nodes{{... on ReviewRequestedEvent{{id createdAt requestedReviewer{{__typename ... on Node{{id}}}}}}}} pageInfo{{hasPreviousPage startCursor}}}}}}}}}}"
        );
        let mut variables = Self::pull_request_variables(key);
        if let Some(before) = before {
            variables.push(("before", before.into()));
        }
        let data = self.graphql(&key.repository, &query, variables, cancellation)?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct History {
            viewer_latest_review_request: Option<Current>,
            timeline_items: Timeline,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Timeline {
            nodes: Vec<RequestedEvent>,
            page_info: Previous,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Previous {
            has_previous_page: bool,
            start_cursor: Option<String>,
        }
        let viewer = viewer(&data)?;
        let history: History = decode_value(
            data.pointer("/repository/pullRequest")
                .ok_or_else(|| missing("pull request history"))?,
        )?;
        validate_current(history.viewer_latest_review_request.as_ref())?;
        let info = history.timeline_items.page_info;
        let previous_cursor = if info.has_previous_page {
            Some(
                info.start_cursor
                    .filter(|cursor| !cursor.is_empty())
                    .ok_or_else(|| missing("history cursor"))?,
            )
        } else {
            None
        };
        for event in &history.timeline_items.nodes {
            validate_node_id("review request event", &event.id)?;
            validate_reviewer(event.requested_reviewer.as_ref())?;
        }
        Ok(HistoryPage {
            viewer,
            current: history.viewer_latest_review_request,
            events: history.timeline_items.nodes,
            previous_cursor,
        })
    }
}

fn missing(subject: &str) -> GitHubError {
    GitHubError::Inspection(format!("GitHub returned no valid {subject}."))
}

fn viewer(data: &serde_json::Value) -> Result<Viewer, GitHubError> {
    let viewer: Viewer = decode_value(data.get("viewer").ok_or_else(|| missing("viewer"))?)?;
    validate_node_id("viewer", &viewer.id)?;
    if viewer.login.is_empty() {
        return Err(missing("viewer login"));
    }
    Ok(viewer)
}

fn validate_current(current: Option<&Current>) -> Result<(), GitHubError> {
    if let Some(current) = current {
        validate_node_id("review request", &current.id)?;
        validate_reviewer(current.requested_reviewer.as_ref())?;
    }
    Ok(())
}

fn validate_reviewer(reviewer: Option<&Reviewer>) -> Result<(), GitHubError> {
    if let Some(id) = reviewer.and_then(|reviewer| reviewer.id.as_deref()) {
        validate_node_id("requested reviewer", id)?;
    }
    Ok(())
}
