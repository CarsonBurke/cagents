//! Linear ticket operations, independent of the desktop UI and daemon.
pub mod api;
pub mod context;

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
