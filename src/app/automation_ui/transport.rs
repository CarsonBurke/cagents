//! Capture the original request scope before dispatching background work.
use std::path::{Path, PathBuf};

use crate::{
    agent_execution::Authority,
    automations::{Request, Response},
    control::ControlClient,
};

pub(in crate::app) struct Context {
    authority: Authority,
    remote: bool,
    no_start: bool,
}

/// One selected connection retains the captured caller for every request in a
/// multi-request operation, including proposal discovery followed by its read.
pub(in crate::app) struct Connection {
    client: ControlClient,
    authority: Authority,
}

impl Connection {
    pub(in crate::app) fn proof(&self) -> &crate::model::HostSessionProof {
        self.client.proof()
    }

    pub(in crate::app) fn read_workspace_model(
        &mut self,
    ) -> Result<crate::workspace_model::WorkspaceModel, String> {
        self.client
            .read_workspace_model()
            .map_err(|error| error.to_string())
    }

    pub(in crate::app) fn request(&mut self, request: Request) -> Result<Response, String> {
        self.client
            .automation(self.authority.clone(), request)
            .map_err(|error| error.to_string())
    }
}

impl Context {
    pub(in crate::app) fn capture() -> Self {
        Self {
            authority: Authority::from_environment(),
            remote: crate::device::is_remote(),
            no_start: crate::control::daemon_autostart_disabled()
                || std::env::var("CAGENTS_NO_START").is_ok_and(|value| {
                    matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
                }),
        }
    }

    pub(in crate::app) fn request(self, request: Request) -> Result<Response, String> {
        self.connect()?.request(request)
    }

    pub(in crate::app) fn connect(self) -> Result<Connection, String> {
        self.connect_with(
            || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
            || crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string()),
            |socket| ControlClient::connect(socket).map_err(|error| error.to_string()),
        )
    }

    /// Notification reads observe the selected host and never start one.
    pub(in crate::app) fn connect_existing(mut self) -> Result<Connection, String> {
        self.no_start = true;
        self.connect()
    }

    #[cfg(test)]
    fn request_with(
        self,
        request: Request,
        selected: impl FnOnce() -> Result<PathBuf, String>,
        start: impl FnOnce() -> Result<PathBuf, String>,
        connect: impl FnOnce(&Path) -> Result<ControlClient, String>,
    ) -> Result<Response, String> {
        self.connect_with(selected, start, connect)?
            .request(request)
    }

    fn connect_with(
        self,
        selected: impl FnOnce() -> Result<PathBuf, String>,
        start: impl FnOnce() -> Result<PathBuf, String>,
        connect: impl FnOnce(&Path) -> Result<ControlClient, String>,
    ) -> Result<Connection, String> {
        // Resolving a remote socket can establish an SSH bridge. Refuse before
        // either resolver, not merely before sending the business request.
        if self.remote && self.authority.token().is_some() {
            return Err(
                "Automation management on another device is unavailable from an agent session"
                    .into(),
            );
        }
        let socket = if self.authority.token().is_some() || self.no_start {
            selected()
        } else {
            start()
        }?;
        let client = connect(&socket)?;
        // Automation carries its authority in the existing request envelope.
        // A refusal or lost connection never retries with User authority.
        Ok(Connection {
            client,
            authority: self.authority,
        })
    }
}

#[cfg(test)]
mod tests;
