//! Closing a Codex conversation whose native owner died without leaving a
//! close receipt. Its pane must close once nothing could still be running for
//! it, and only then.
use super::*;
use crate::daemon::holder_registry::{self as registry, Kind, Record};
use crate::model::{HostIncarnation, HostSessionId};
use std::os::unix::fs::PermissionsExt;

const MODE: &str = "CAGENTS_TEST_LOST_CODEX_OWNER_ROOT";

/// Reruns `test` with a private runtime directory, so its daemon lease and
/// holder registry are its own. Returns the fixture root in that rerun, and
/// `None` in the outer process once the rerun has passed.
fn isolated(test: &str) -> Option<std::path::PathBuf> {
    if let Some(root) = std::env::var_os(MODE) {
        let root = std::path::PathBuf::from(root);
        assert_eq!(
            std::env::var_os("XDG_RUNTIME_DIR"),
            Some(root.join("run").into_os_string())
        );
        return Some(root);
    }
    let directory = tempfile::tempdir().unwrap();
    let runtime = directory.path().join("run");
    crate::secure_file::prepare_private_directory(&runtime).unwrap();
    let stdout = directory.path().join("child.stdout");
    let stderr = directory.path().join("child.stderr");
    struct OriginalChild(std::process::Child);
    impl Drop for OriginalChild {
        fn drop(&mut self) {
            if let Ok(None) = self.0.try_wait() {
                let _ = self.0.kill();
            }
            let _ = self.0.wait();
        }
    }
    let mut child = OriginalChild(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("host::chat::lost_codex_owner_close_tests::{test}"),
                "--color=never",
            ])
            .env(MODE, directory.path())
            .env("XDG_RUNTIME_DIR", &runtime)
            .stdout(std::fs::File::create(&stdout).unwrap())
            .stderr(std::fs::File::create(&stderr).unwrap())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "lost owner fixture did not settle"
        );
        thread::sleep(Duration::from_millis(5));
    };
    let stdout = std::fs::read_to_string(stdout).unwrap();
    assert!(
        status.success() && stdout.contains("1 passed"),
        "private lost owner fixture failed:\n{stdout}\n{}",
        std::fs::read_to_string(stderr).unwrap()
    );
    None
}

struct Fixture {
    host: SharedHost,
    source: Session,
    _lease: crate::daemon::HostLease,
}

impl Fixture {
    /// A started Codex conversation of this daemon, with an owned listener and
    /// nothing resident: what a pane holds after its holder died.
    fn new(root: &std::path::Path) -> Self {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("lost-codex-owner"),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let paths = crate::daemon::RuntimePaths::discover().unwrap();
        let lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
        // The daemon prepares its holder registry as it starts.
        crate::secure_file::prepare_private_directory(&registry::directory(Kind::Native).unwrap())
            .unwrap();
        let host = Arc::new(HostLock::new(HostState::new(proof.clone())));
        let store =
            super::super::model::Registry::at(&root.join("lost-codex-owner.sqlite")).unwrap();
        host.lock().unwrap().execution_registry = Some(Arc::new(std::sync::Mutex::new(store)));
        let id = TerminalId::new();
        let allocation = root.join(format!("cagents-codex-{id}"));
        std::fs::create_dir(&allocation).unwrap();
        std::fs::set_permissions(&allocation, std::fs::Permissions::from_mode(0o700)).unwrap();
        let socket = allocation.join("control.sock");
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let source = Session {
            id,
            launch: chat::Launch {
                provider: Provider::Codex,
                session_id: Some(uuid::Uuid::new_v4().to_string()),
                recipe: crate::model::LaunchRecipe {
                    program: "codex".into(),
                    arguments: vec!["--remote".into(), format!("unix://{}", socket.display())],
                    working_directory: project,
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: Some(chat::OwnedCodexListener {
                socket,
                allocator: proof,
            }),
            native_continuity: chat::NativeContinuity::ReconnectOnly,
        };
        assert!(source.owns_codex_listener());
        Self {
            host,
            source,
            _lease: lease,
        }
    }

    fn socket(&self) -> &std::path::Path {
        &self.source.owned_codex_listener.as_ref().unwrap().socket
    }

    /// A discovery record for this conversation's holder, as the holder writes
    /// it, naming `holder` and the provider `root`.
    fn record(&self, holder: registry::Process, root: registry::Process) -> Record {
        let directory = registry::directory(Kind::Native).unwrap();
        crate::secure_file::prepare_private_directory(&directory).unwrap();
        let record = Record {
            version: 4,
            build: None,
            kind: Kind::Native,
            application: crate::instance::APPLICATION_ID.into(),
            boot: crate::daemon::read_boot_id().unwrap(),
            terminal: self.source.id,
            holder,
            root,
        };
        record.create(&directory).unwrap();
        assert!(has_native_owner(&self.source).unwrap());
        record
    }

    fn assert_settled(&self) {
        let state = self.host.lock().unwrap();
        assert!(!state.native_closing.contains_key(&self.source.id));
        assert!(!state.chats.contains_key(&self.source.id));
    }
}

fn current() -> registry::Process {
    registry::Process::read(std::process::id() as i32).unwrap()
}

/// A process that has exited, leaving no group or session behind.
fn exited(pid: i32) -> registry::Process {
    registry::Process { pid, start: 1 }
}

/// #2426 and the pane that could not be closed: the holder is gone and so is
/// its record, so exact cleanup has nothing to adopt. Before, every Close was
/// refused with "No exact native holder or confirmed close receipt is
/// available" and the pane stayed open for good.
#[test]
fn a_pane_whose_codex_owner_left_no_trace_closes() {
    let Some(root) = isolated("a_pane_whose_codex_owner_left_no_trace_closes") else {
        return;
    };
    let fixture = Fixture::new(&root);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    fixture.assert_settled();
    // The allocation it no longer needs is given back.
    assert!(!fixture.socket().parent().unwrap().exists());
    // Closing it again is not a new refusal.
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
}

/// A holder that died with its provider: its record outlives both until
/// discovery runs, and names nothing that could still be stopped.
#[test]
fn a_pane_whose_holder_and_provider_both_exited_closes_and_retires_their_record() {
    let Some(root) =
        isolated("a_pane_whose_holder_and_provider_both_exited_closes_and_retires_their_record")
    else {
        return;
    };
    let fixture = Fixture::new(&root);
    fixture.record(exited(i32::MAX - 1), exited(i32::MAX - 2));
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
    assert!(!has_native_owner(&fixture.source).unwrap());
    fixture.assert_settled();
}

/// A provider that outlived its holder may still be running a turn. Nothing
/// can stop it through the pane, so the pane keeps refusing, and its record
/// stays as the only evidence of the provider.
#[test]
fn a_provider_that_outlived_its_holder_keeps_the_refusal() {
    let Some(root) = isolated("a_provider_that_outlived_its_holder_keeps_the_refusal") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let record = fixture.record(exited(i32::MAX - 1), current());
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert_eq!(
        registry::recovery_candidate(
            &registry::socket(
                &registry::directory(Kind::Native).unwrap(),
                fixture.source.id
            ),
            Kind::Native,
        )
        .unwrap(),
        Some(record)
    );
}

/// A server still accepting connections at the conversation's endpoint is
/// running, whatever happened to the record that would have named it.
#[test]
fn a_listener_still_being_served_keeps_the_refusal() {
    let Some(root) = isolated("a_listener_still_being_served_keeps_the_refusal") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let _server = std::os::unix::net::UnixListener::bind(fixture.socket()).unwrap();
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(fixture.socket().exists());
}

/// A start or close that holds the conversation's allocation may be about to
/// publish a server; the pane waits for it rather than declaring the owner lost.
#[test]
fn a_start_holding_the_allocation_keeps_the_refusal() {
    let Some(root) = isolated("a_start_holding_the_allocation_keeps_the_refusal") else {
        return;
    };
    let fixture = Fixture::new(&root);
    let reservation = {
        let mut state = fixture.host.lock().unwrap();
        capacity::Reservation::closing(&fixture.host, &mut state, &fixture.source).unwrap()
    };
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap_err();
    assert!(fixture.socket().parent().unwrap().exists());
    drop(reservation);
    close_retained_from_pane(&fixture.host, &fixture.source).unwrap();
}
