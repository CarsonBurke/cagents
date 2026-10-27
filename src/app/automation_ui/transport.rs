//! Capture the original request scope before dispatching background work.
use std::path::{Path, PathBuf};

use crate::{
    agent_execution::Authority,
    automations::{Request, Response},
    control::ControlClient,
};

pub(super) struct Context {
    authority: Authority,
    remote: bool,
    no_start: bool,
}

impl Context {
    pub(super) fn capture() -> Self {
        Self {
            authority: Authority::from_environment(),
            remote: crate::device::is_remote(),
            no_start: crate::control::daemon_autostart_disabled(),
        }
    }

    pub(super) fn request(self, request: Request) -> Result<Response, String> {
        self.request_with(
            request,
            || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
            || crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string()),
            |socket| ControlClient::connect(socket).map_err(|error| error.to_string()),
        )
    }

    fn request_with(
        self,
        request: Request,
        selected: impl FnOnce() -> Result<PathBuf, String>,
        start: impl FnOnce() -> Result<PathBuf, String>,
        connect: impl FnOnce(&Path) -> Result<ControlClient, String>,
    ) -> Result<Response, String> {
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
        let mut client = connect(&socket)?;
        // Automation carries its authority in the existing request envelope.
        // A refusal or lost connection never retries with User authority.
        client
            .automation(self.authority, request)
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests;
