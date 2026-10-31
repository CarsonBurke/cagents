//! Selected-user metadata inspection and explicit fixed-file creation.
//! An observation is stale-able context, never a creation or execution permit.
use crate::{account::AccountId, account::readiness::PhysicalIdentity, model::HostSessionProof};
use serde::{Deserialize, Serialize};

use super::{Outcome, Presence};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub account: AccountId,
    pub expected_home: PhysicalIdentity,
    pub expected_email: String,
}

impl InspectRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        let id = self.account.as_str();
        if !id.starts_with("codex/") || id == "codex/system" {
            return Err("Choose one registered Codex account");
        }
        let email = self.expected_email.trim();
        if email.is_empty()
            || self.expected_email.len() > 320
            || self.expected_email.chars().any(char::is_control)
            || email.chars().any(char::is_whitespace)
            || !email.split_once('@').is_some_and(|(local, domain)| {
                !local.is_empty() && !domain.is_empty() && !domain.contains('@')
            })
        {
            return Err("Provide the expected account email explicitly");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    pub account: AccountId,
    pub host: HostSessionProof,
    pub home: PhysicalIdentity,
    pub presence: Presence,
    pub expected_email: String,
}

impl Observation {
    pub fn request(&self) -> InspectRequest {
        InspectRequest {
            account: self.account.clone(),
            expected_home: self.home,
            expected_email: self.expected_email.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateRequest {
    pub observation: Observation,
    pub network_access: bool,
}

impl CreateRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.observation.request().validate()?;
        if self.observation.presence != Presence::Absent {
            return Err("Inspect the absent account configuration before creating it");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateReport {
    pub account: AccountId,
    pub host: HostSessionProof,
    pub home: PhysicalIdentity,
    pub network_access: bool,
    pub outcome: Outcome,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BootId, HostIncarnation, HostSessionId};

    pub(super) fn observation() -> Observation {
        Observation {
            account: "codex/00000000-0000-4000-8000-000000000001"
                .parse()
                .unwrap(),
            host: HostSessionProof {
                host_session_id: HostSessionId::new("coding-config-fixture"),
                boot_id: BootId::new("fixture-boot"),
                host_incarnation: HostIncarnation::new("fixture-incarnation"),
            },
            home: PhysicalIdentity {
                device: 4,
                inode: 9,
            },
            presence: Presence::Absent,
            expected_email: "selected@example.test".into(),
        }
    }

    #[test]
    fn requests_require_registered_codex_explicit_identity_and_absent_observation() {
        let original = observation();
        assert!(original.request().validate().is_ok());
        for account in [
            "codex/system",
            "claude/00000000-0000-4000-8000-000000000001",
        ] {
            let mut request = original.request();
            request.account = account.parse().unwrap();
            assert!(request.validate().is_err());
        }
        for email in [
            "",
            "not-an-email",
            "a@@b",
            "\nselected@example.test",
            "a\nb@example.test",
            "a b@example.test",
        ] {
            let mut request = original.request();
            request.expected_email = email.into();
            assert!(request.validate().is_err());
        }
        for presence in [Presence::Present, Presence::Unknown] {
            let mut observation = original.clone();
            observation.presence = presence;
            assert!(
                CreateRequest {
                    observation,
                    network_access: false
                }
                .validate()
                .is_err()
            );
        }
        let mut value = serde_json::to_value(CreateRequest {
            observation: original,
            network_access: false,
        })
        .unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("config_contents".into(), "not accepted".into());
        assert!(serde_json::from_value::<CreateRequest>(value).is_err());
    }
}
