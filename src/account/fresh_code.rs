//! A separate explicit fresh-home/device-code action. These are internal
//! contracts until the owning transport's wire graph is reviewed separately.
//! No request contains a path, recipe, environment, credential or authority.
use super::preparation::{PreparationBinding, PreparationPayload};
use crate::{
    agent::Provider,
    model::{HostSessionProof, TerminalId},
};
use serde::{Deserialize, Serialize};
use std::io;
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConditionalPlan {
    pub(crate) request_id: Uuid,
    pub(crate) expected_installation: Uuid,
    pub(crate) payload: PreparationPayload,
}

impl ConditionalPlan {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.request_id.is_nil()
            || self.expected_installation.is_nil()
            || self.payload.provider != Provider::Codex
        {
            return Err(invalid("The fresh code plan is invalid"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodeMethod {
    CodexDeviceCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeAttempt {
    pub(crate) id: Uuid,
    pub(crate) terminal_id: TerminalId,
    pub(crate) method: CodeMethod,
}

impl CodeAttempt {
    pub(crate) fn validate(&self) -> io::Result<()> {
        if self.id.is_nil() || self.terminal_id.as_uuid().is_nil() {
            return Err(invalid("The fresh code attempt is invalid"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FreshCodeStart {
    pub(crate) binding: PreparationBinding,
    pub(crate) attempt: CodeAttempt,
}

impl FreshCodeStart {
    pub(crate) fn validate(&self) -> io::Result<()> {
        validate_binding(&self.binding)?;
        self.attempt.validate()
    }
}

/// This phase records custody, never authority to repeat a constructor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CodeState {
    /// Durably owned before any constructor. After a crash this is Unknown.
    Owned,
    /// The exact original host acknowledged its actual hosted terminal.
    Started,
    Registered,
    /// The official probe found a duplicate. Files and pending metadata remain.
    DuplicateRetained,
}

/// Stored in accounts.toml. Paths and opaque caller authority never cross this
/// boundary. A loaded owner, including Started, mints no live spawn capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CodeOwner {
    pub(crate) binding: PreparationBinding,
    pub(crate) attempt: CodeAttempt,
    pub(crate) host: HostSessionProof,
    pub(crate) state: CodeState,
}

impl CodeOwner {
    pub(crate) fn validate(&self) -> io::Result<()> {
        validate_binding(&self.binding)?;
        self.attempt.validate()?;
        if !self.host.is_complete() {
            return Err(invalid("The fresh code owner has no complete host proof"));
        }
        Ok(())
    }

    pub(crate) fn matches(&self, request: &FreshCodeStart) -> bool {
        self.binding == request.binding && self.attempt == request.attempt
    }
}

pub(crate) fn validate_binding(binding: &PreparationBinding) -> io::Result<()> {
    if binding.request_id.is_nil()
        || binding.installation_id.is_nil()
        || binding.key.is_nil()
        || binding.payload.provider != Provider::Codex
    {
        return Err(invalid("The fresh code binding is invalid"));
    }
    Ok(())
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
