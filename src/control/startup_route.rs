//! Which route a public command takes to the daemon, for the authority
//! fixtures. Readiness hands its proving connection to the request (#3127),
//! so on the wire a readiness handshake and a direct connection look alike.
//! With the daemon gone and an update holding the launch fence, they part:
//! only readiness would start a daemon, and the fence refuses that start
//! before any daemon binary is looked for, so nothing launches.
use super::*;

/// How a command reaches the daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// Proves the daemon ready, starting it when it is absent.
    Readiness,
    /// `--no-start`: confirms an absent daemon and stops.
    NoStart,
    /// Connects to the existing endpoint and nothing else.
    Direct,
}

impl Route {
    /// The route an ordinary User, a `--no-start` User, or a scoped caller
    /// that must not start the daemon takes.
    pub(super) fn of(direct: bool, no_start: bool) -> Self {
        if direct {
            Self::Direct
        } else if no_start {
            Self::NoStart
        } else {
            Self::Readiness
        }
    }

    /// The error kind the command reports for an absent daemon while an
    /// update holds the launch fence.
    pub(super) fn absent_kind(self) -> &'static str {
        match self {
            Self::Readiness => "daemon_start",
            Self::NoStart => "daemon_not_running",
            Self::Direct => "daemon_connection",
        }
    }
}

/// Holds the update's launch fence, so a route that would start the absent
/// daemon reports the update instead of launching one.
pub(super) fn hold_launch_fence() -> crate::instance::UiUpdateGuard {
    crate::instance::UiUpdateGuard::acquire().unwrap()
}

/// Runs the public command `arguments` against the absent daemon at its
/// stale socket under [`hold_launch_fence`]. Its JSON error, on stderr, names
/// the route it took ([`Route::absent_kind`]).
pub(super) fn run_absent(arguments: Vec<OsString>) {
    let socket = daemon_socket_path().unwrap();
    assert!(
        UnixStream::connect(&socket).is_err(),
        "the daemon must be gone"
    );
    let _fence = hold_launch_fence();
    let result = run_from_with_staged_lookup(arguments, |_, _| Ok(None));
    assert_eq!(result, ExitCode::FAILURE);
    assert!(
        UnixStream::connect(&socket).is_err(),
        "no daemon was started"
    );
}

/// Asserts the JSON errors in `stderr` that report an absent daemon name
/// `expected`, in order. A readiness route's must be the update's refusal:
/// it tried to start the daemon rather than failing some other way.
pub(super) fn assert_absent_routes(stderr: &str, expected: &[Route]) {
    let absent = stderr
        .lines()
        .filter_map(|line| line.find('{').map(|start| &line[start..]))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(reports_absent_daemon)
        .collect::<Vec<_>>();
    let kinds = absent
        .iter()
        .map(|error| error["error"]["kind"].as_str().unwrap())
        .collect::<Vec<_>>();
    let expected_kinds = expected
        .iter()
        .map(|route| route.absent_kind())
        .collect::<Vec<_>>();
    assert_eq!(kinds, expected_kinds, "{stderr}");
    for (error, route) in absent.iter().zip(expected) {
        if *route == Route::Readiness {
            let message = error["error"]["message"].as_str().unwrap();
            assert!(
                message.contains(
                    &crate::terminal::HostedTerminalStartError::UpdateInProgress(
                        crate::instance::UpdateInProgress
                    )
                    .to_string()
                ),
                "{message}"
            );
        }
    }
}

/// Whether the JSON error `error` is one [`assert_absent_routes`] reads, so
/// a fixture can account for every other error itself.
pub(super) fn reports_absent_daemon(error: &Value) -> bool {
    error["error"]["kind"]
        .as_str()
        .is_some_and(|kind| kind.starts_with("daemon_"))
}
