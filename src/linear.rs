//! Linear ticket operations, independent of the desktop UI and daemon.
pub mod api;
pub mod context;
pub mod credentials;

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    pub kind: &'static str,
    pub message: String,
    pub details: Option<Value>,
}

impl Error {
    pub fn new(kind: &'static str, message: impl std::fmt::Display) -> Self {
        Self {
            kind,
            message: message.to_string(),
            details: None,
        }
    }

    pub fn details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

/// Accept a Linear UUID, issue identifier, or official issue URL. In particular,
/// do not use arbitrary URL path fragments as mutation targets.
pub fn issue_reference(value: &str) -> Result<String, Error> {
    let value = value.trim();
    let invalid = || {
        Error::new(
            "linear_invalid_issue",
            "Expected a Linear issue UUID, TEAM-123, or https://linear.app/.../issue/TEAM-123 URL",
        )
    };
    if value.contains("://") {
        let url = url::Url::parse(value).map_err(|_| invalid())?;
        if url.scheme() != "https"
            || url.host_str() != Some("linear.app")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.port().is_some()
        {
            return Err(invalid());
        }
        let mut segments = url.path_segments().ok_or_else(invalid)?;
        while let Some(segment) = segments.next() {
            if segment == "issue" {
                let key = segments.next().ok_or_else(invalid)?;
                return issue_key(key).ok_or_else(invalid);
            }
        }
        return Err(invalid());
    }
    if let Ok(id) = uuid::Uuid::parse_str(value) {
        return Ok(id.to_string());
    }
    issue_key(value).ok_or_else(invalid)
}

fn issue_key(value: &str) -> Option<String> {
    let (team, number) = value.rsplit_once('-')?;
    (!team.is_empty()
        && team.bytes().all(|b| b.is_ascii_alphabetic())
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit()))
    .then(|| value.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_references_are_explicit_and_validated() {
        assert_eq!(issue_reference("eng-12").unwrap(), "ENG-12");
        assert_eq!(
            issue_reference("https://linear.app/team/issue/ENG-12/title?view=all").unwrap(),
            "ENG-12"
        );
        for value in [
            "feature/ENG-12",
            "https://evil.test/issue/ENG-12",
            "https://linear.app@evil.test/issue/ENG-12",
            "https://user@linear.app/issue/ENG-12",
            "https://linear.app/issue/ENG-no",
            "",
            "ENG-12/title",
        ] {
            assert!(issue_reference(value).is_err(), "{value}");
        }
    }
}

/// Metadata needed to preview and create a ticket's checkout. Secrets never
/// leave the client and this value is safe to send to the UI worker result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuePreview {
    pub id: String,
    pub identifier: String,
    pub title: String,
    pub branch_name: String,
    pub url: String,
    pub state: String,
}

impl IssuePreview {
    pub fn fetch(reference: &str, workspace: Option<&str>) -> Result<Self, Error> {
        let reference = issue_reference(reference)?;
        Self::query(&api::Client::from_workspace(workspace)?, &reference)
    }

    /// Resolves with a client the caller already holds, so resolving several
    /// issues reads the credential once.
    pub fn fetch_with(client: &api::Client, reference: &str) -> Result<Self, Error> {
        Self::query(client, &issue_reference(reference)?)
    }

    fn query(client: &api::Client, reference: &str) -> Result<Self, Error> {
        Self::from_response(client.request(
            "query($id:String!){issue(id:$id){id identifier title branchName url state{name}}}",
            serde_json::json!({"id": reference}),
            false,
        )?)
    }

    fn from_response(response: Value) -> Result<Self, Error> {
        let issue = &response["issue"];
        let text = |field: &str| {
            issue[field]
                .as_str()
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| {
                    Error::new(
                        "linear_issue_invalid",
                        format!("Linear issue response omitted {field}"),
                    )
                })
        };
        Ok(Self {
            id: text("id")?,
            identifier: text("identifier")?,
            title: text("title")?,
            branch_name: text("branchName")?,
            url: text("url")?,
            state: issue["state"]["name"]
                .as_str()
                .unwrap_or("Unknown state")
                .to_owned(),
        })
    }
}

#[cfg(test)]
mod preview_tests {
    use super::*;
    #[test]
    fn mocked_linear_response_retains_suggested_branch_title_and_state() {
        let issue = IssuePreview::from_response(serde_json::json!({"issue": {
            "id": "2b498c52-7dc7-4c76-ad59-dfc488c40111", "identifier":"ENG-123",
            "title":"Fix login redirect", "branchName":"user/eng-123-fix-login-redirect",
            "url":"https://linear.app/team/issue/ENG-123", "state":{"name":"Todo"}
        }}))
        .unwrap();
        assert_eq!(issue.branch_name, "user/eng-123-fix-login-redirect");
        assert_eq!(issue.title, "Fix login redirect");
        assert_eq!(issue.state, "Todo");
        assert!(issue_reference("ENG-123").is_ok());
        assert!(issue_reference("https://linear.app/team/issue/ENG-123/title").is_ok());
        assert!(issue_reference("feature/plain-branch").is_err());
    }
}
