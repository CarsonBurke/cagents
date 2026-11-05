#![cfg(all(feature = "host", target_os = "linux"))]

//! A real private daemon establishes and reuses an offline fixture provider.
//! A genuinely nondelegated User environment is required before any guardian
//! or daemon starts. RPCs use the captured inherited authority; HOME and
//! delegation remain unchanged. Only the child's XDG directories, provider
//! homes and PATH are fixture-owned. This exercises the User protocol fixture,
//! not production CLI admission, because no CLI command is invoked.
//!
//! Ignored by default because admission requires genuine nondelegated User
//! scope. Ordinary Cagents terminals carry agent authority and must refuse
//! this manual contract. From a nondelegated User environment, explicitly run:
//!
//! ```sh
//! cargo test --locked --no-default-features --features host \
//!     --test automation_establishment_contract -- --ignored --exact \
//!     failed_first_reuse_start_releases_and_new_firings_establish_then_reuse_one_provider \
//!     --nocapture --test-threads=1
//! ```
#[path = "support/precheck_receipt_guardian.rs"]
#[allow(dead_code)]
mod guardian;

use cagents::{
    agent::Provider,
    agent_execution::{Authority, HostedSession, Run, Spec, State, Surface, Target},
    automations::{
        Automation, Definition, Firing, Request, Response, RunDetail, Schedule, SchedulePattern,
        SessionMode,
    },
    chat::Session,
    control::ControlClient,
    model::{HostSessionProof, ProjectKind},
    terminal_ipc::ShutdownOutcome,
    workspace_model::ModelEdit,
};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, PermissionsExt},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use uuid::Uuid;

const EVIDENCE_ROOT: &str = "CLAUDE_ESTABLISHMENT_FIXTURE_ROOT";

fn require(condition: bool, message: impl Into<String>) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}

/// A disconnected or wedged RPC cannot extend the fixture's operation budget.
struct FixtureUser {
    client: ControlClient,
    authority: Authority,
    finished: Option<mpsc::Sender<()>>,
    watchdog: Option<thread::JoinHandle<()>>,
}

impl FixtureUser {
    fn connected(
        client: ControlClient,
        authority: Authority,
        budget: Duration,
    ) -> Result<Self, String> {
        let hangup = client.hangup().map_err(|error| error.to_string())?;
        let (finished, receiver) = mpsc::channel();
        let watchdog = thread::Builder::new()
            .name("establishment-fixture-deadline".into())
            .spawn(move || {
                if receiver.recv_timeout(budget).is_err() {
                    hangup.hang_up();
                }
            })
            .map_err(|error| error.to_string())?;
        let mut user = Self {
            client,
            authority,
            finished: Some(finished),
            watchdog: Some(watchdog),
        };
        user.client
            .authenticate(user.authority.clone())
            .map_err(|error| error.to_string())?;
        Ok(user)
    }

    fn automation(&mut self, request: Request) -> Result<Response, String> {
        self.client
            .automation(self.authority.clone(), request)
            .map_err(|error| error.to_string())
    }
}

impl Drop for FixtureUser {
    fn drop(&mut self) {
        if let Some(finished) = self.finished.take() {
            let _ = finished.send(());
        }
        if let Some(watchdog) = self.watchdog.take() {
            let _ = watchdog.join();
        }
    }
}

struct OriginalDaemon(Child);

impl OriginalDaemon {
    fn wait_until(&mut self, deadline: Instant) -> Result<(), String> {
        loop {
            if let Some(status) = self.0.try_wait().map_err(|error| error.to_string())? {
                return require(
                    status.success(),
                    format!("private daemon cleanup exited unsuccessfully: {status}"),
                );
            }
            require(Instant::now() < deadline, "private daemon did not exit")?;
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for OriginalDaemon {
    fn drop(&mut self) {
        // Only this unreaped original child is a fallback signal target.
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}

struct Fixture {
    root: tempfile::TempDir,
    daemon: OriginalDaemon,
    user: FixtureUser,
    socket: PathBuf,
    proof: HostSessionProof,
    automation: Option<Uuid>,
    sources: Vec<Session>,
    cleaned: bool,
}

impl Fixture {
    fn start(custody: &guardian::Custody, authority: Authority) -> Result<Self, String> {
        // An absent private binary must never fall through to a real provider.
        for path in ["/usr/bin/claude", "/bin/claude"] {
            require(
                matches!(fs::symlink_metadata(path), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                format!("fixture requires no provider at {path}"),
            )?;
        }
        let mut root = tempfile::Builder::new()
            .prefix("e-")
            .tempdir_in(custody.root())
            .map_err(|error| error.to_string())?;
        // The existing guardian retains this namespace through family
        // settlement and removes it only after its positive ECHILD receipt.
        root.disable_cleanup(true);
        let root_path = root
            .path()
            .canonicalize()
            .map_err(|error| error.to_string())?;
        require(root_path == root.path(), "fixture root must be canonical")?;
        for leaf in [
            "runtime", "config", "data", "state", "cache", "tmp", "claude", "codex", "pi", "bin",
            "project",
        ] {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(root.path().join(leaf))
                .map_err(|error| error.to_string())?;
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_cagentsd"));
        for (name, leaf) in [
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("TMPDIR", "tmp"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("CODEX_HOME", "codex"),
            ("PI_CODING_AGENT_DIR", "pi"),
        ] {
            command.env(name, root.path().join(leaf));
        }
        // No env_clear, HOME override, or authority environment mutation.
        let mut daemon = OriginalDaemon(
            command
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
                )
                .env("SHELL", "/bin/sh")
                .env(EVIDENCE_ROOT, root.path())
                .current_dir(root.path())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(
                    fs::File::create(root.path().join("daemon.log"))
                        .map_err(|error| error.to_string())?,
                )
                .spawn()
                .map_err(|error| error.to_string())?,
        );
        // Transfer exact original-child custody before readiness or any
        // request can start a detached retained provider helper.
        custody
            .register(&daemon.0, root.path())
            .map_err(|error| error.to_string())?;
        let socket = root
            .path()
            .join("runtime")
            .join(cagents::instance::RUNTIME_SUBDIRECTORY)
            .join(cagents::terminal_ipc::SOCKET_FILE_NAME);
        let deadline = Instant::now() + Duration::from_secs(15);
        let client = loop {
            if let Ok(client) = ControlClient::connect(&socket) {
                break client;
            }
            require(
                daemon
                    .0
                    .try_wait()
                    .map_err(|error| error.to_string())?
                    .is_none(),
                "private daemon exited before readiness",
            )?;
            require(
                Instant::now() < deadline,
                "private daemon readiness expired",
            )?;
            thread::sleep(Duration::from_millis(10));
        };
        let user = FixtureUser::connected(client, authority, Duration::from_secs(40))?;
        let proof = user.client.proof().clone();
        require(
            proof.is_complete(),
            "private daemon issued incomplete proof",
        )?;
        Ok(Self {
            root,
            daemon,
            user,
            socket,
            proof,
            automation: None,
            sources: Vec::new(),
            cleaned: false,
        })
    }

    fn show(&mut self, id: Uuid) -> Result<Automation, String> {
        match self.user.automation(Request::Show { id })? {
            Response::Automation(value) => Ok(value),
            _ => Err("unexpected automation response".into()),
        }
    }

    fn detail(&mut self, id: Uuid) -> Result<RunDetail, String> {
        let Response::Detail(detail) = self.user.automation(Request::ShowFiring { id })? else {
            return Err("unexpected firing response".into());
        };
        if let Some(HostedSession::Chat(source)) = detail
            .agent
            .as_ref()
            .and_then(|agent| agent.session.as_ref())
            && !self.sources.iter().any(|known| same_process(known, source))
        {
            self.sources.push(source.clone());
        }
        Ok(detail)
    }

    fn run_now(&mut self, automation: &Automation, request: Uuid) -> Result<Firing, String> {
        match self.user.automation(Request::RunNow {
            id: automation.id,
            execution_revision: automation.execution_revision,
            request,
            confirmed: true,
        })? {
            Response::Firing(value) => Ok(value),
            _ => Err("run-now omitted its accepted firing".into()),
        }
    }

    fn settled(&mut self, id: Uuid) -> Result<RunDetail, String> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let detail = self.detail(id)?;
            if detail.run.as_ref().is_some_and(|run| {
                run.state.is_terminal()
                    && !detail.firing.state.is_active()
                    && detail.firing.finished_ms == run.finished_ms
            }) {
                return Ok(detail);
            }
            require(
                Instant::now() < deadline,
                format!("firing did not settle: {:?}", detail.firing),
            )?;
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn released(&mut self, automation: Uuid, firing: Uuid) -> Result<RunDetail, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let definition = self.show(automation)?;
            let detail = self.detail(firing)?;
            // On the old source this reports ReleaseBlocked after its grace,
            // rather than hiding the causal failure behind a generic timeout.
            require(
                definition.enabled && definition.blocked.is_none(),
                format!(
                    "conclusive startup failure unexpectedly paused its schedule: enabled={}, active={:?}, blocked={:?}, release_pending={}, run={:?}",
                    definition.enabled,
                    definition.active_firing,
                    definition.blocked,
                    detail.firing.release_pending,
                    detail.run,
                ),
            )?;
            if definition.active_firing.is_none() && !detail.firing.release_pending {
                return Ok(detail);
            }
            require(
                Instant::now() < deadline,
                format!(
                    "release did not finish: {definition:?}; {:?}",
                    detail.firing
                ),
            )?;
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn exercise(&mut self) -> Result<(), String> {
        let project = self.root.path().join("project");
        self.user
            .client
            .edit_workspace_model(ModelEdit::OpenProject {
                repository: project.clone(),
                name: "Establishment fixture".into(),
                kind: ProjectKind::Folder,
            })
            .map_err(|error| error.to_string())?;
        let hour = ((SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_secs()
            / 3600
            + 12)
            % 24) as u8;
        let Response::Automation(created) = self.user.automation(Request::Create {
            definition: Definition {
                name: "First reusable provider".into(),
                prompt: "Return the next fixture result".into(),
                agent: Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository: project.clone(),
                        directory: project,
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                },
                schedule: Schedule {
                    pattern: SchedulePattern::Daily { hour, minute: 0 },
                    timezone: "UTC".into(),
                },
                session_mode: SessionMode::Reuse,
                precheck: None,
            },
        })?
        else {
            return Err("create omitted its definition".into());
        };
        self.automation = Some(created.id);
        let Response::Automation(enabled) = self.user.automation(Request::SetEnabled {
            id: created.id,
            revision: created.revision,
            enabled: true,
            confirmed: true,
        })?
        else {
            return Err("enable omitted its definition".into());
        };

        let failed_request = Uuid::new_v4();
        let failed_firing = self.run_now(&enabled, failed_request)?;
        let failed_detail = self.settled(failed_firing.id)?;
        let failed_run = failed_detail
            .run
            .as_ref()
            .ok_or("failed firing omitted its run")?
            .clone();
        let failed_agent = failed_detail
            .agent
            .as_ref()
            .ok_or("failed firing omitted its agent")?;
        let channel = failed_agent
            .channel()
            .ok_or("first firing omitted its automation-owned channel")?;
        require(
            failed_run.state == State::Failed
                && failed_run.started_ms.is_none()
                && failed_run.provider_key.is_none(),
            format!("missing fixture provider was not conclusively unstarted: {failed_run:?}"),
        )?;
        require(
            failed_agent.session.is_none() && failed_agent.console.is_none(),
            "failed startup published a provider binding",
        )?;
        let diagnostic = failed_run
            .result
            .diagnostic
            .as_ref()
            .ok_or("startup failure lost its original diagnostic")?;
        require(
            diagnostic
                == "Provider executable claude is not executable in the preserved launch environment",
            format!("missing executable lost its launch-validation diagnostic: {diagnostic}"),
        )?;
        require(
            failed_detail.firing.diagnostic.as_ref() == Some(diagnostic),
            "firing hid the original launch diagnostic",
        )?;
        let released_failure = self.released(enabled.id, failed_firing.id)?;
        require(
            released_failure.run.as_ref() == Some(&failed_run),
            "empty-agent retirement changed the failed result",
        )?;
        // A closed empty agent is a durable receipt: close refuses a pending
        // private launch. The focused model tests inspect that predicate too.
        require(
            released_failure
                .agent
                .as_ref()
                .is_some_and(|agent| agent.closed && agent.session.is_none()),
            "empty agent did not retire before release",
        )?;
        require(
            self.user
                .client
                .list_chats()
                .map_err(|error| error.to_string())?
                .is_empty(),
            "failed startup left a native owner",
        )?;
        require(
            !self.root.path().join("spawns").exists() && !self.root.path().join("prompts").exists(),
            "failed startup invoked the fixture provider",
        )?;
        let retry = self.run_now(&enabled, failed_request)?;
        require(
            retry.id == failed_firing.id && retry.run == failed_firing.run,
            "a transport retry replayed the failed firing",
        )?;

        fs::write(
            self.root.path().join("bin/claude"),
            include_str!("fixtures/automation-establishment-claude.sh"),
        )
        .map_err(|error| error.to_string())?;
        fs::set_permissions(
            self.root.path().join("bin/claude"),
            fs::Permissions::from_mode(0o700),
        )
        .map_err(|error| error.to_string())?;
        let fresh_request = Uuid::new_v4();
        let fresh_firing = self.run_now(&enabled, fresh_request)?;
        let fresh = self.settled(fresh_firing.id)?;
        let fresh = self.released(enabled.id, fresh.firing.id)?;
        let fresh_run = successful_run(&fresh, "ESTABLISHMENT_RESULT_1")?;
        let fresh_agent = fresh
            .agent
            .as_ref()
            .ok_or("established firing omitted its agent")?;
        let fresh_source = native_source(&fresh)?.clone();
        require(
            fresh_agent.id != failed_agent.id
                && !fresh_agent.closed
                && fresh_agent.channel() == Some(channel)
                && fresh_source.proof.as_ref() == Some(&self.proof),
            "new firing did not establish a fresh provider in the original channel",
        )?;
        let same_request = self.run_now(&enabled, fresh_request)?;
        require(
            same_request.id == fresh_firing.id && same_request.run == fresh_firing.run,
            "an established firing was repeated by its request ID",
        )?;
        let next_firing = self.run_now(&enabled, Uuid::new_v4())?;
        let next = self.settled(next_firing.id)?;
        let next = self.released(enabled.id, next.firing.id)?;
        let next_run = successful_run(&next, "ESTABLISHMENT_RESULT_2")?;
        require(
            next_firing.id != fresh_firing.id
                && next_run.id != fresh_run.id
                && next_run.provider_key != fresh_run.provider_key,
            "reuse lost distinct firing/run/turn receipts",
        )?;
        require(
            next.agent
                .as_ref()
                .is_some_and(|agent| agent.id == fresh_agent.id && !agent.closed)
                && same_process(&fresh_source, native_source(&next)?),
            "second firing replaced the reusable provider",
        )?;
        require(
            fs::read_to_string(self.root.path().join("spawns"))
                .map_err(|error| error.to_string())?
                == "spawn\n",
            "reuse spawned another provider",
        )?;
        require(
            fs::read_to_string(self.root.path().join("prompts"))
                .map_err(|error| error.to_string())?
                == "prompt\nprompt\n",
            "a failed or retried firing replayed a prompt",
        )?;
        let after = self.show(enabled.id)?;
        require(
            after.enabled
                && after.blocked.is_none()
                && after.active_firing.is_none()
                && after.next == enabled.next
                && after.channel == Some(channel)
                && after.reuse_agent == Some(fresh_agent.id),
            format!("reuse changed schedule ownership: {after:?}"),
        )?;
        let historical = self.detail(failed_firing.id)?;
        require(
            historical.run.as_ref() == Some(&failed_run)
                && historical.firing == released_failure.firing,
            "later firings changed failed-run history",
        )?;
        let Response::History(history) = self.user.automation(Request::History {
            id: Some(enabled.id),
            before: None,
            limit: 10,
        })?
        else {
            return Err("history returned an unexpected response".into());
        };
        require(
            history.firings.len() == 3 && history.next.is_none(),
            "run-now retries created extra firings",
        )
    }

    fn cleanup(&mut self) -> Result<(), String> {
        let client = ControlClient::connect(&self.socket).map_err(|error| error.to_string())?;
        require(
            client.proof() == &self.proof,
            "cleanup connected to a different daemon",
        )?;
        let mut user =
            FixtureUser::connected(client, self.user.authority.clone(), Duration::from_secs(25))?;
        if let Some(id) = self.automation {
            let Response::Automation(mut automation) = user.automation(Request::Show { id })?
            else {
                return Err("cleanup show failed".into());
            };
            if automation.enabled {
                let Response::Automation(disabled) = user.automation(Request::SetEnabled {
                    id,
                    revision: automation.revision,
                    enabled: false,
                    confirmed: false,
                })?
                else {
                    return Err("cleanup disable failed".into());
                };
                automation = disabled;
            }
            if let Some(firing) = automation.active_firing {
                let Response::Detail(detail) =
                    user.automation(Request::ShowFiring { id: firing })?
                else {
                    return Err("cleanup firing lookup failed".into());
                };
                if let Some(HostedSession::Chat(source)) =
                    detail.agent.and_then(|agent| agent.session)
                {
                    self.sources.push(source);
                }
                if detail.firing.state.is_active() {
                    user.automation(Request::Cancel { id, firing })?;
                }
            }
            for source in user
                .client
                .list_chats()
                .map_err(|error| error.to_string())?
            {
                require(
                    self.sources
                        .iter()
                        .any(|known| same_process(known, &source)),
                    "cleanup found an unrecorded native owner; evidence retained",
                )?;
                user.client
                    .close_chat(source)
                    .map_err(|error| error.to_string())?;
            }
            let deadline = Instant::now() + Duration::from_secs(8);
            loop {
                let Response::Automation(current) = user.automation(Request::Show { id })? else {
                    return Err("cleanup release lookup failed".into());
                };
                if current.active_firing.is_none() && current.blocked.is_none() {
                    user.automation(Request::Remove {
                        id,
                        revision: current.revision,
                    })?;
                    break;
                }
                // The pre-fix failure retains an empty logical slot. Its
                // original Failed receipt and empty inventory still permit
                // stopping this private daemon without inventing an actor.
                if let Some(firing) = current.active_firing {
                    let Response::Detail(detail) =
                        user.automation(Request::ShowFiring { id: firing })?
                    else {
                        return Err("cleanup outcome lookup failed".into());
                    };
                    if detail.run.as_ref().is_some_and(|run| {
                        run.state == State::Failed
                            && run.started_ms.is_none()
                            && run.provider_key.is_none()
                    }) && detail
                        .agent
                        .as_ref()
                        .is_some_and(|agent| agent.session.is_none())
                    {
                        break;
                    }
                }
                require(
                    Instant::now() < deadline,
                    "owned provider cleanup did not settle; evidence retained",
                )?;
                thread::sleep(Duration::from_millis(20));
            }
        }
        require(
            user.client
                .list_chats()
                .map_err(|error| error.to_string())?
                .is_empty()
                && user
                    .client
                    .list()
                    .map_err(|error| error.to_string())?
                    .is_empty(),
            "private owners remain after exact cleanup",
        )?;
        let preflight = user
            .client
            .shutdown_preflight()
            .map_err(|error| error.to_string())?;
        require(
            preflight.capability.proof == self.proof
                && preflight.capability.live_terminals == 0
                && preflight.capability.background_subagents == 0,
            "private shutdown did not prove empty ownership",
        )?;
        require(
            matches!(
                user.client
                    .confirm_shutdown(preflight.capability)
                    .map_err(|error| error.to_string())?,
                ShutdownOutcome::Accepted { .. }
            ),
            "private shutdown was not accepted",
        )?;
        self.daemon
            .wait_until(Instant::now() + Duration::from_secs(3))?;
        self.cleaned = true;
        Ok(())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if !self.cleaned {
            if let Err(error) = self.cleanup() {
                self.root.disable_cleanup(true);
                eprintln!(
                    "Private establishment cleanup remains unproved at {}: {error}",
                    self.root.path().display()
                );
            }
        }
    }
}

fn same_process(a: &Session, b: &Session) -> bool {
    a.id == b.id
        && a.proof == b.proof
        && a.launch.provider == b.launch.provider
        && a.launch.recipe == b.launch.recipe
}

fn native_source(detail: &RunDetail) -> Result<&Session, String> {
    match detail
        .agent
        .as_ref()
        .and_then(|agent| agent.session.as_ref())
    {
        Some(HostedSession::Chat(source)) => Ok(source),
        _ => Err("successful firing omitted its native provider binding".into()),
    }
}

fn successful_run<'a>(detail: &'a RunDetail, expected: &str) -> Result<&'a Run, String> {
    let run = detail
        .run
        .as_ref()
        .ok_or("successful firing omitted its run")?;
    require(
        run.state == State::Succeeded
            && run.started_ms.is_some()
            && run.provider_key.is_some()
            && run.result.text.contains(expected),
        format!("provider did not produce its correlated result: {run:?}"),
    )?;
    Ok(run)
}

fn unexpected_fixture_actor(stage: &str, _root: &std::path::Path) {
    panic!("establishment fixture has no actor stage: {stage}");
}

#[test]
#[ignore = "requires genuine nondelegated User scope; see module documentation"]
fn failed_first_reuse_start_releases_and_new_firings_establish_then_reuse_one_provider() {
    let authority = Authority::from_environment();
    assert_eq!(
        authority,
        Authority::default(),
        "this private-daemon protocol fixture requires an actual ordinary User environment; delegated execution is refused before any guardian or daemon starts"
    );
    guardian::isolated(
        "failed_first_reuse_start_releases_and_new_firings_establish_then_reuse_one_provider",
        unexpected_fixture_actor,
        |custody| {
            let inherited_home = std::env::var_os("HOME");
            let mut fixture = Fixture::start(custody, authority.clone())
                .expect("private establishment fixture could not start");
            let result = fixture.exercise();
            let cleanup = fixture.cleanup();
            // Collect observations and retire exact owned work before any assertion.
            if result.is_err() || cleanup.is_err() {
                fixture.root.disable_cleanup(true);
                eprintln!(
                    "Private establishment evidence retained at {}",
                    fixture.root.path().display()
                );
            }
            assert_eq!(std::env::var_os("HOME"), inherited_home);
            assert_eq!(Authority::from_environment(), authority);
            assert!(
                result.is_ok(),
                "establishment contract: {result:?}; cleanup: {cleanup:?}"
            );
            assert!(cleanup.is_ok(), "private ownership cleanup: {cleanup:?}");
        },
    );
}
