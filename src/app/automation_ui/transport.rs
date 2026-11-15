//! Capture the original request scope before dispatching background work.
use super::super::request_failure::{Activity, Failure};
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

    pub(in crate::app) fn workspace_registry_identity(&mut self) -> Result<uuid::Uuid, String> {
        self.client
            .workspace_registry_identity()
            .map_err(|error| error.to_string())
    }

    pub(in crate::app) fn read_agent_labels(
        &mut self,
    ) -> Result<Vec<(crate::model::AgentId, String)>, crate::control::ControlError> {
        self.client.read_agent_labels().map(|(_, labels)| labels)
    }

    pub(in crate::app) fn request(
        &mut self,
        request: Request,
    ) -> Result<Response, crate::control::ControlError> {
        self.client.automation(self.authority.clone(), request)
    }

    /// Stable agent names, read on the same captured connection, so an
    /// explicit Open names its agent before adoption.
    pub(in crate::app) fn read_agent_labels(
        &mut self,
    ) -> Result<Vec<(crate::model::AgentId, String)>, crate::control::ControlError> {
        self.client.read_agent_labels().map(|(_, labels)| labels)
    }
}

impl Context {
    pub(in crate::app) fn capture() -> Self {
        Self {
            authority: Authority::from_environment(),
            remote: crate::device::is_remote(),
            no_start: crate::control::daemon_autostart_disabled(),
        }
    }

    pub(in crate::app) fn request(self, request: Request) -> Result<Response, String> {
        let activity = Activity::automation(&request);
        let scoped = self.is_scoped();
        self.connect(activity)?
            .request(request)
            .map_err(|error| super::super::request_failure::message(error, activity, scoped))
    }

    pub(in crate::app) fn is_scoped(&self) -> bool {
        self.authority.token().is_some()
    }

    pub(in crate::app) fn connect(self, activity: Activity) -> Result<Connection, String> {
        let scoped = self.is_scoped();
        self.connect_with(
            activity,
            || crate::terminal::existing_daemon_socket_path().map_err(|error| error.to_string()),
            || crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string()),
            |socket| {
                ControlClient::connect(socket).map_err(|error| {
                    super::super::request_failure::message(error, activity, scoped)
                })
            },
        )
    }

    /// Notification reads observe the selected host and never start one.
    pub(in crate::app) fn connect_existing(mut self) -> Result<Connection, String> {
        self.no_start = true;
        self.connect(Activity::ViewLifecycle)
    }

    #[cfg(test)]
    fn request_with(
        self,
        request: Request,
        selected: impl FnOnce() -> Result<PathBuf, String>,
        start: impl FnOnce() -> Result<PathBuf, String>,
        connect: impl FnOnce(&Path) -> Result<ControlClient, String>,
    ) -> Result<Response, String> {
        self.connect_with(Activity::automation(&request), selected, start, connect)?
            .request(request)
            .map_err(|error| error.to_string())
    }

    fn connect_with(
        self,
        activity: Activity,
        selected: impl FnOnce() -> Result<PathBuf, String>,
        start: impl FnOnce() -> Result<PathBuf, String>,
        connect: impl FnOnce(&Path) -> Result<ControlClient, String>,
    ) -> Result<Connection, String> {
        // Resolving a remote socket can establish an SSH bridge. Refuse before
        // either resolver, not merely before sending the business request.
        if self.remote && self.authority.token().is_some() {
            return Err(Failure::remote(activity));
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
