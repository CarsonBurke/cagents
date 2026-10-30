//! Fresh conversation choices sent to the filesystem-owning daemon.
use crate::{account::AccountId, agent::Provider, model::TerminalId};
use serde::{Deserialize, Serialize};
use std::{os::unix::ffi::OsStrExt, path::PathBuf};

/// A new draft supplies choices, never a client-resolved launch recipe.
/// `None` clears the corresponding application preset; it does not inherit
/// another window's model, effort or permission setting. The daemon retains
/// its saved executable, extra arguments and environment, and provider-owned
/// configuration remains under that provider's normal handling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfiguredChatStart {
    pub id: TerminalId,
    pub provider: Provider,
    pub working_directory: PathBuf,
    pub account: AccountId,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub permission_mode: Option<String>,
    /// Preserve the reader's existing automatic-account routing choice.
    /// This grants no account, credential or source-session authority.
    pub automatic: bool,
}

impl ConfiguredChatStart {
    /// Typed draft metadata may describe only this never-issued conversation.
    /// Account homes and execution policy are resolved later by the daemon.
    pub fn matches_draft(&self, session: &super::Session) -> bool {
        session.is_unstarted()
            && self.id == session.id
            && self.provider == session.launch.provider
            && self.working_directory == session.launch.recipe.working_directory
    }

    pub fn validate(&self) -> Result<(), String> {
        let directory = self.working_directory.as_os_str().as_bytes();
        if self.id.as_uuid().is_nil()
            || !self.working_directory.is_absolute()
            || directory.len() > 16 * 1024
            || directory.contains(&0)
            || self.working_directory.components().any(|part| {
                matches!(
                    part,
                    std::path::Component::ParentDir | std::path::Component::CurDir
                )
            })
        {
            return Err(
                "A fresh conversation needs a new identifier and an absolute directory".into(),
            );
        }
        if self.account.as_str().len() > 256
            || !self
                .account
                .as_str()
                .starts_with(&format!("{}/", self.provider.executable()))
        {
            return Err("Choose an account for this provider on the selected device".into());
        }
        for value in [&self.model, &self.effort, &self.permission_mode]
            .into_iter()
            .flatten()
        {
            if value.len() > 1024 || value.trim().is_empty() || value.chars().any(char::is_control)
            {
                return Err(
                    "Conversation choices must be bounded names without control characters".into(),
                );
            }
        }
        if let Some(mode) = self.permission_mode.as_deref() {
            crate::settings::agents::validate_permission_override(self.provider, mode)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent() -> ConfiguredChatStart {
        ConfiguredChatStart {
            id: TerminalId::new(),
            provider: Provider::Codex,
            working_directory: "/work".into(),
            account: "codex/system".parse().unwrap(),
            model: Some("chosen-model".into()),
            effort: None,
            permission_mode: Some("readOnly".into()),
            automatic: false,
        }
    }

    #[test]
    fn fresh_intent_round_trips_without_recipe_environment_or_source_authority() {
        let request = crate::terminal_ipc::ClientRequest::ChatStartConfigured { start: intent() };
        let bytes = postcard::to_allocvec(&request).unwrap();
        assert_eq!(
            bytes[0], 73,
            "the configured request is appended after current-main ChannelWork (tags 0..72)"
        );
        assert_eq!(
            postcard::from_bytes::<crate::terminal_ipc::ClientRequest>(&bytes).unwrap(),
            request
        );
        let mut json = serde_json::to_value(intent()).unwrap();
        assert!(json.get("environment").is_none());
        assert!(json.get("recipe").is_none());
        assert!(json.get("session_id").is_none());
        json["environment"] = serde_json::json!({"API_KEY":"must-not-be-accepted"});
        assert!(serde_json::from_value::<ConfiguredChatStart>(json).is_err());
    }

    #[test]
    fn invalid_or_foreign_fresh_choices_are_refused() {
        let mut request = intent();
        assert!(request.validate().is_ok());
        request.account = "claude/system".parse().unwrap();
        assert!(request.validate().is_err());
        request = intent();
        request.working_directory = "relative".into();
        assert!(request.validate().is_err());
        request = intent();
        request.model = Some("bad\nchoice".into());
        assert!(request.validate().is_err());
        request = intent();
        request.permission_mode = Some("not-a-mode".into());
        assert!(request.validate().is_err());
    }
}
