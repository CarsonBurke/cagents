use super::ControlClient;
use crate::agent_execution::Authority;
use std::path::PathBuf;

/// The caller of one native operation, captured before transport or task handoff.
/// This stays in memory: a saved conversation never carries a caller capability.
#[derive(Clone, Debug)]
pub(crate) struct CallerContext {
    authority: Authority,
    remote: bool,
    no_start: bool,
}

#[derive(Clone, Copy)]
pub(crate) enum NativeReadiness {
    Existing,
    UserReady,
}

impl CallerContext {
    pub(crate) fn capture() -> Self {
        Self {
            authority: Authority::from_environment(),
            remote: crate::device::is_remote(),
            no_start: super::daemon_autostart_disabled(),
        }
    }

    /// Presence, including an empty or invalid capability, remains scoped.
    pub(crate) fn authentication(&self) -> Option<Authority> {
        self.authority
            .token()
            .is_some()
            .then(|| self.authority.clone())
    }

    pub(crate) fn socket(&self, readiness: NativeReadiness) -> Result<PathBuf, String> {
        self.socket_with(
            readiness,
            || crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string()),
            || crate::terminal::ensure_daemon_ready().map_err(|error| error.to_string()),
        )
    }

    fn socket_with(
        &self,
        readiness: NativeReadiness,
        selected: impl FnOnce() -> Result<PathBuf, String>,
        ready: impl FnOnce() -> Result<PathBuf, String>,
    ) -> Result<PathBuf, String> {
        let scoped = self.authority.token().is_some();
        // Resolving a remote endpoint may launch an SSH bridge. Refuse before
        // either resolver; an agent capability is local to its issuing daemon.
        if scoped && self.remote {
            return Err(
                "Native conversation operations from an agent session require a user terminal on the remote device"
                    .into(),
            );
        }
        if scoped || self.no_start || matches!(readiness, NativeReadiness::Existing) {
            selected()
        } else {
            ready()
        }
    }

    pub(crate) fn connect_control(&self) -> Result<ControlClient, String> {
        let socket = self.socket(NativeReadiness::Existing)?;
        let mut client = ControlClient::connect(&socket).map_err(|error| error.to_string())?;
        if let Some(authority) = self.authentication() {
            client
                .authenticate(authority)
                .map_err(|error| error.to_string())?;
        }
        Ok(client)
    }

    #[cfg(test)]
    pub(crate) fn for_test(authority: Authority, remote: bool, no_start: bool) -> Self {
        Self {
            authority,
            remote,
            no_start,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_native_connections_never_start_or_resolve_remote_transport() {
        for token in ["", "synthetic-invalid-capability"] {
            for readiness in [NativeReadiness::Existing, NativeReadiness::UserReady] {
                let authority = Authority::session(token.into());
                let local = CallerContext::for_test(authority.clone(), false, false);
                assert_eq!(local.authentication(), Some(authority.clone()));
                assert_eq!(
                    local.socket_with(
                        readiness,
                        || Ok(PathBuf::from("/synthetic/selected.sock")),
                        || panic!("a scoped caller must never start the daemon"),
                    ),
                    Ok(PathBuf::from("/synthetic/selected.sock"))
                );
                let remote = CallerContext::for_test(authority, true, false);
                assert!(
                    remote
                        .socket_with(
                            readiness,
                            || panic!("remote resolution may start an SSH bridge"),
                            || panic!("remote readiness must not run"),
                        )
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn user_native_connections_keep_existing_readiness_and_no_start_choices() {
        for remote in [false, true] {
            for no_start in [false, true] {
                let caller = CallerContext::for_test(Authority::default(), remote, no_start);
                assert!(caller.authentication().is_none());
                assert_eq!(
                    caller.socket_with(
                        NativeReadiness::Existing,
                        || Ok(PathBuf::from("existing")),
                        || panic!("a passive operation must never start the daemon"),
                    ),
                    Ok(PathBuf::from("existing"))
                );
                assert_eq!(
                    caller.socket_with(
                        NativeReadiness::UserReady,
                        || {
                            assert!(no_start);
                            Ok(PathBuf::from("existing"))
                        },
                        || {
                            assert!(!no_start);
                            Ok(PathBuf::from("ready"))
                        },
                    ),
                    Ok(PathBuf::from(if no_start { "existing" } else { "ready" }))
                );
            }
        }
    }
}
