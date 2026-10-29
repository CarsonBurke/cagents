//! App effects and reducers over private framed peers in env-cleared children.
//! Synthetic peers test client custody, not Host authorization or native UX.
use super::*;
use crate::{
    agent_execution::Authority,
    channel_work::{Change, Claim, Entry, HomeLifecycle, Role, Snapshot, Source, Target},
    model::{BootId, HostIncarnation, HostSessionId, HostSessionProof},
    terminal::ipc::{
        ClientEnvelope, ClientRequest, PROTOCOL_VERSION, ServerEnvelope, ServerMessage,
        ServerResponse, read_frame, write_frame,
    },
};
use cosmic::iced::futures::StreamExt;
use std::{
    ffi::OsString,
    fs::{self, File},
    io,
    os::unix::{
        ffi::OsStringExt,
        fs::PermissionsExt,
        net::{UnixListener, UnixStream},
    },
    process::{Command, Stdio},
    thread::{self, JoinHandle},
};
const CHILD: &str = "CAGENTS_TEST_CHANNEL_WORK_UI_ROLE";
const ROOT: &str = "CAGENTS_TEST_CHANNEL_WORK_UI_ROOT";
const MARKER: &str = "CAGENTS_TEST_CHANNEL_WORK_UI_SSH";
const ORIGINAL: &str = "synthetic-ui-original-scope";
const REPLACEMENT: &str = "synthetic-ui-later-scope";
const REFUSAL: &str = "Private peer refused the captured mutation";

fn proof(restarted: bool) -> HostSessionProof {
    HostSessionProof {
        host_session_id: HostSessionId::new("channel-work-ui-fixture"),
        boot_id: BootId::new(if restarted { "restarted" } else { "original" }),
        host_incarnation: HostIncarnation::new(if restarted { "new-host" } else { "old-host" }),
    }
}
fn answer(
    peer: &mut UnixStream,
    request: &ClientEnvelope,
    proof: &HostSessionProof,
    response: ServerResponse,
) {
    write_frame(
        peer,
        &ServerEnvelope {
            version: PROTOCOL_VERSION,
            request_id: Some(request.request_id),
            proof: proof.clone(),
            message: ServerMessage::Response { response },
        },
    )
    .unwrap();
}
fn next(peer: &mut UnixStream, proof: &HostSessionProof) -> ClientEnvelope {
    let request: ClientEnvelope = read_frame(peer).unwrap().unwrap();
    assert_eq!(request.proof.as_ref(), Some(proof));
    request
}
#[derive(Clone, Copy)]
enum Step {
    Read,
    RefuseMutation,
    UncertainMutation,
    WrongHome(Home),
    InitialWrongHome(Home),
    Restart,
    Receipt,
}
struct Peer {
    worker: Option<JoinHandle<Vec<ClientRequest>>>,
}
impl Peer {
    fn new(socket: PathBuf, home: Home, loaded: Snapshot, steps: Vec<Step>) -> Self {
        assert!(socket.starts_with(root()));
        fs::create_dir_all(socket.parent().unwrap()).unwrap();
        if socket.exists() {
            fs::remove_file(&socket).unwrap();
        }
        let listener = UnixListener::bind(socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            worker: Some(thread::spawn(move || {
                let mut seen = Vec::new();
                let mut operation = None;
                for step in steps {
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut peer = loop {
                        match listener.accept() {
                            Ok((peer, _)) => break peer,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                assert!(
                                    Instant::now() < deadline,
                                    "private App effect did not connect"
                                );
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(error) => panic!("private listener: {error}"),
                        }
                    };
                    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    peer.set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let current = proof(matches!(
                        step,
                        Step::WrongHome(_) | Step::Restart | Step::Receipt
                    ));
                    let hello: ClientEnvelope = read_frame(&mut peer).unwrap().unwrap();
                    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
                    assert!(hello.proof.is_none());
                    answer(
                        &mut peer,
                        &hello,
                        &current,
                        ServerResponse::Connected {
                            proof: current.clone(),
                            resumed: vec![],
                        },
                    );
                    let mut request = next(&mut peer, &current);
                    // Serve the causal predecessor's User model read validly;
                    // custody is judged after delivering actual business replies.
                    if matches!(request.request, ClientRequest::ReadWorkspaceModel) {
                        assert!(
                            !matches!(step, Step::Receipt),
                            "same-proof Check cannot restart proof admission"
                        );
                        seen.push(request.request.clone());
                        answer(
                            &mut peer,
                            &request,
                            &current,
                            ServerResponse::WorkspaceModel {
                                model: crate::workspace_model::WorkspaceModel {
                                    identity: home.registry,
                                    ..Default::default()
                                },
                            },
                        );
                        request = next(&mut peer, &current);
                    }
                    if matches!(
                        &request.request,
                        ClientRequest::ChannelWork {
                            request: api::Request::ReadHome { .. },
                            ..
                        }
                    ) {
                        assert!(
                            !matches!(step, Step::Receipt),
                            "a validated replacement proof must be reused"
                        );
                        assert!(
                            matches!(&request.request, ClientRequest::ChannelWork { request: api::Request::ReadHome { home: actual }, .. } if *actual == home)
                        );
                        seen.push(request.request.clone());
                        let returned = match step {
                            Step::WrongHome(other) | Step::InitialWrongHome(other) => other,
                            _ => home,
                        };
                        answer(
                            &mut peer,
                            &request,
                            &current,
                            ServerResponse::ChannelWork(api::Response::Home {
                                home: returned,
                                lifecycle: HomeLifecycle::Active,
                                revision: loaded.revision,
                            }),
                        );
                        if matches!(step, Step::WrongHome(_) | Step::InitialWrongHome(_)) {
                            assert!(
                                read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                                "wrong full Home cannot authorize Receipt or mutation"
                            );
                            continue;
                        }
                        request = next(&mut peer, &current);
                    } else {
                        assert!(
                            !matches!(step, Step::WrongHome(_) | Step::Restart),
                            "a restarted endpoint requires original ReadHome proof"
                        );
                    }
                    seen.push(request.request.clone());
                    let ClientRequest::ChannelWork {
                        request: business, ..
                    } = &request.request
                    else {
                        panic!("expected ChannelWork business, never Authenticate/User fallback")
                    };
                    let response = match step {
                        Step::Read | Step::InitialWrongHome(_) => {
                            assert!(
                                matches!(business, api::Request::Read { home: actual, page } if *actual == home && page.limit == 50 && page.before.is_none() && page.current_before.is_none())
                            );
                            api::Response::Snapshot(loaded.clone())
                        }
                        Step::RefuseMutation | Step::UncertainMutation => {
                            let api::Request::Mutate { mutation, caller } = business else {
                                panic!("loaded End must send mutation")
                            };
                            assert!(caller.is_none(), "do not manufacture actor placement");
                            assert_eq!(mutation.home, home);
                            assert_eq!(mutation.expected_revision, loaded.entries[0].revision);
                            assert!(!mutation.operation.is_nil());
                            assert_eq!(
                                mutation.change,
                                Change::End {
                                    target: loaded.entries[0].target.clone()
                                }
                            );
                            assert!(
                                operation.replace(mutation.operation).is_none(),
                                "no mutation replay"
                            );
                            api::Response::Failed(if matches!(step, Step::RefuseMutation) {
                                api::Failure::Refused(REFUSAL.into())
                            } else {
                                api::Failure::Uncertain
                            })
                        }
                        Step::Restart | Step::Receipt => {
                            assert!(
                                matches!(business, api::Request::Receipt { home: actual, operation: op } if *actual == home && Some(*op) == operation)
                            );
                            api::Response::Receipt(None)
                        }
                        Step::WrongHome(_) => unreachable!(),
                    };
                    answer(
                        &mut peer,
                        &request,
                        &current,
                        ServerResponse::ChannelWork(response),
                    );
                    assert!(
                        read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none(),
                        "one business request; no automatic retry/fallback"
                    );
                }
                assert!(
                    matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
                );
                seen
            })),
        }
    }
    fn finished(mut self) -> Vec<ClientRequest> {
        self.worker.take().unwrap().join().unwrap()
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
fn root() -> PathBuf {
    PathBuf::from(std::env::var_os(ROOT).unwrap())
}
fn fixture() -> (Cagents, Home, Snapshot) {
    let mut app = super::super::tests::context_inspector_test_app(true);
    assert!(app.session.is_none());
    let runtime = root().join("protection");
    fs::create_dir_all(&runtime).unwrap();
    app.worktree_protections.use_fixture_runtime(runtime);
    let home = Home {
        registry: uuid::Uuid::new_v4(),
        channel: ChannelId::new(),
        creation_ordinal: 19,
    };
    app.workspace_model.identity = home.registry;
    let loaded = Snapshot {
        home,
        lifecycle: HomeLifecycle::Active,
        revision: 7,
        entries: vec![Entry {
            id: uuid::Uuid::new_v4(),
            target: Target::Local(uuid::Uuid::new_v4()),
            claims: vec![Claim {
                source: Source::User,
                role: Role::Review,
                active: true,
                confirmed_generation: 0,
            }],
            contexts: vec![],
            suppressed: false,
            user_ended: false,
            observation: None,
            last_terminal_generation: 0,
            ever_claimed: true,
            revision: 7,
        }],
        history: vec![],
        next_current: None,
        next_history: None,
    };
    loaded.validate().unwrap();
    (app, home, loaded)
}
fn initial(app: &mut Cagents, home: Home) -> Task<Message> {
    assert!(
        app.channel_work.routes.is_empty(),
        "never use a cached fake User route"
    );
    let effect = app
        .channel_work
        .view
        .open(home, "Private loaded work".into())
        .unwrap();
    app.channel_work_effect(effect)
}
fn change_synthetic_capability() {
    // SAFETY: this dedicated env-cleared single-test child changes only its
    // synthetic variable, before any Tokio or peer worker is constructed.
    // Neither ambient parent authority nor a real session token is touched.
    unsafe {
        std::env::set_var(crate::agent_execution::AUTHORITY_ENV, REPLACEMENT);
    }
}
fn drive(task: Task<Message>) -> Message {
    let mut stream = cosmic::iced::runtime::task::into_stream(task).expect("App effect task");
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            tokio::time::timeout(Duration::from_secs(8), async {
                while let Some(action) = stream.next().await {
                    if let cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) =
                        action
                    {
                        return message;
                    }
                }
                panic!("App effect returned no callback")
            })
            .await
            .expect("bounded private App transport")
        })
}
fn assert_custody(seen: &[ClientRequest], expected: &Authority) {
    let mut business = 0;
    for request in seen {
        if let ClientRequest::ChannelWork { authority, .. } = request {
            assert!(
                authority == expected,
                "every business and Home proof must retain original route scope"
            );
            business += 1;
        } else {
            assert!(matches!(request, ClientRequest::ReadWorkspaceModel));
        }
    }
    assert!(business > 0);
    if expected.token().is_some() {
        assert!(
            !seen
                .iter()
                .any(|request| matches!(request, ClientRequest::ReadWorkspaceModel)),
            "scoped route must prove exact Home through original scope"
        );
    }
}
fn exercise_refusal() {
    let expected = Authority::from_environment();
    let (mut app, home, loaded) = fixture();
    let cards = app.durable.cards.clone();
    let focused = app.workspace.focused();
    let drafts = app.persistent_workspace().chat_drafts.clone();
    let task = initial(&mut app, home);
    let socket = app.channel_work.routes[&home].socket.clone();
    change_synthetic_capability();
    let peer = Peer::new(
        socket,
        home,
        loaded.clone(),
        vec![Step::Read, Step::RefuseMutation],
    );
    let callback = drive(task);
    assert!(
        matches!(&callback, Message::ChannelWork(ui::Action::Loaded { result: Ok(snapshot), .. }) if snapshot == &loaded)
    );
    drop(app.update(callback));
    let end = app
        .channel_work
        .view
        .fixture_end_action(&loaded.entries[0].target)
        .expect("loaded claim exposes real End action");
    let callback = drive(app.update_channel_work(end));
    assert!(
        matches!(&callback, Message::ChannelWork(ui::Action::Applied { result: Err(Failure::Refused(reason)), .. }) if reason == REFUSAL)
    );
    drop(app.update(callback));
    let seen = peer.finished();
    assert_eq!(
        app.channel_work.view.fixture_loaded_snapshot(),
        Some(&loaded)
    );
    assert!(app.channel_work.view.pending_homes().next().is_none());
    assert!(
        app.channel_work
            .view
            .fixture_end_action(&loaded.entries[0].target)
            .is_some()
    );
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.workspace.focused(), focused);
    assert_eq!(app.persistent_workspace().chat_drafts, drafts);
    assert_custody(&seen, &expected);
}
fn initial_wrong_home() {
    let expected = Authority::from_environment();
    for changed in 0..3 {
        let (mut app, home, loaded) = fixture();
        let wrong = match changed {
            0 => Home {
                registry: uuid::Uuid::new_v4(),
                ..home
            },
            1 => Home {
                channel: ChannelId::new(),
                ..home
            },
            _ => Home {
                creation_ordinal: home.creation_ordinal + 1,
                ..home
            },
        };
        let task = initial(&mut app, home);
        let socket = app.channel_work.routes[&home].socket.clone();
        let peer = Peer::new(socket, home, loaded, vec![Step::InitialWrongHome(wrong)]);
        let callback = drive(task);
        // The predecessor is served a valid business Read if it omits the
        // exact-Home proof. It then fails this causal result, not a timeout.
        assert!(matches!(
            &callback,
            Message::ChannelWork(ui::Action::Loaded {
                result: Err(Failure::Unavailable),
                ..
            })
        ));
        drop(app.update(callback));
        assert!(
            app.channel_work.routes[&home]
                .proof
                .lock()
                .unwrap()
                .is_none()
        );
        assert!(app.channel_work.view.fixture_loaded_snapshot().is_none());
        let seen = peer.finished();
        assert_custody(&seen, &expected);
    }
}

fn exercise_restart() {
    initial_wrong_home();
    let expected = Authority::from_environment();
    let (mut app, home, loaded) = fixture();
    let cards = app.durable.cards.clone();
    let focused = app.workspace.focused();
    let task = initial(&mut app, home);
    let socket = app.channel_work.routes[&home].socket.clone();
    change_synthetic_capability();
    let peer = Peer::new(
        socket,
        home,
        loaded.clone(),
        vec![
            Step::Read,
            Step::UncertainMutation,
            Step::WrongHome(Home {
                registry: uuid::Uuid::new_v4(),
                ..home
            }),
            Step::WrongHome(Home {
                channel: ChannelId::new(),
                ..home
            }),
            Step::WrongHome(Home {
                creation_ordinal: home.creation_ordinal + 1,
                ..home
            }),
            Step::Restart,
            Step::Receipt,
        ],
    );
    drop(app.update(drive(task)));
    let end = app
        .channel_work
        .view
        .fixture_end_action(&loaded.entries[0].target)
        .unwrap();
    let callback = drive(app.update_channel_work(end));
    assert!(matches!(
        &callback,
        Message::ChannelWork(ui::Action::Applied {
            result: Err(Failure::Unavailable),
            ..
        })
    ));
    drop(app.update(callback));
    assert_eq!(
        app.channel_work.view.pending_homes().collect::<Vec<_>>(),
        [home]
    );
    for _ in 0..3 {
        let check = app
            .channel_work
            .view
            .fixture_check_action()
            .expect("uncertain change exposes actual Check action");
        let callback = drive(app.update_channel_work(check));
        assert!(matches!(
            &callback,
            Message::ChannelWork(ui::Action::LookedUp {
                result: Err(Failure::Unavailable),
                ..
            })
        ));
        drop(app.update(callback));
        assert_eq!(
            *app.channel_work.routes[&home].proof.lock().unwrap(),
            Some(proof(false)),
            "wrong full Home cannot replace original proof"
        );
        assert_eq!(
            app.channel_work.view.fixture_loaded_snapshot(),
            Some(&loaded)
        );
        assert_eq!(
            app.channel_work.view.pending_homes().collect::<Vec<_>>(),
            [home]
        );
    }
    for _ in 0..2 {
        let check = app.channel_work.view.fixture_check_action().unwrap();
        let callback = drive(app.update_channel_work(check));
        assert!(matches!(
            &callback,
            Message::ChannelWork(ui::Action::LookedUp {
                result: Ok(None),
                ..
            })
        ));
        drop(app.update(callback));
        assert_eq!(
            *app.channel_work.routes[&home].proof.lock().unwrap(),
            Some(proof(true))
        );
        assert_eq!(
            app.channel_work.view.pending_homes().collect::<Vec<_>>(),
            [home],
            "missing Receipt remains unknown; never replay mutation"
        );
    }
    let seen = peer.finished();
    assert_eq!(
        seen.iter()
            .filter(|request| matches!(
                request,
                ClientRequest::ChannelWork {
                    request: api::Request::Mutate { .. },
                    ..
                }
            ))
            .count(),
        1
    );
    assert_eq!(
        app.channel_work.view.fixture_loaded_snapshot(),
        Some(&loaded)
    );
    assert_eq!(app.durable.cards, cards);
    assert_eq!(app.workspace.focused(), focused);
    assert_custody(&seen, &expected);
}
fn proxy_directories() -> Vec<PathBuf> {
    let runtime = root()
        .join("runtime")
        .join(crate::instance::RUNTIME_SUBDIRECTORY);
    let mut directories = match fs::read_dir(runtime) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("device-")
            })
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("private runtime: {error}"),
    };
    directories.sort();
    directories
}
fn exercise_remote() {
    // This starts with an already selected remote context. It validates only
    // this App effect's refusal before Route/socket resolution, not device
    // selection, global UI startup, or other device-service delegation.
    assert!(Authority::from_environment().token().is_some());
    let configuration = crate::device::configuration_path().unwrap();
    fs::create_dir_all(configuration.parent().unwrap()).unwrap();
    fs::write(
        configuration,
        br#"{"devices":[{"name":"fixture","host":"ui-scope.invalid","executable":"cagents"}]}"#,
    )
    .unwrap();
    crate::device::select(Some("fixture")).unwrap();
    let before = proxy_directories();
    let (mut app, home, _) = fixture();
    let task = initial(&mut app, home);
    assert_eq!(
        proxy_directories(),
        before,
        "refuse scoped remote before socket resolver creates proxy"
    );
    assert!(app.channel_work.routes.is_empty());
    let callback = drive(task);
    assert!(matches!(
        &callback,
        Message::ChannelWork(ui::Action::Loaded {
            result: Err(Failure::Unavailable),
            ..
        })
    ));
    drop(app.update(callback));
    assert!(app.channel_work.view.fixture_loaded_snapshot().is_none());
    assert!(crate::device::is_remote());
    assert_eq!(crate::device::selected().unwrap().name, "fixture");
    assert_eq!(proxy_directories(), before);
    assert!(!Path::new(&std::env::var_os(MARKER).unwrap()).exists());
}
fn isolated(roles: &[&str], exercise: fn()) {
    if std::env::var_os(CHILD).is_some() {
        exercise();
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in roles {
        let directory = tempfile::Builder::new().prefix("wu-").tempdir().unwrap();
        let private = directory.path().canonicalize().unwrap();
        for leaf in ["home", "runtime", "config", "data", "cache", "state", "bin"] {
            let path = private.join(leaf);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let marker = private.join("ssh-called");
        let ssh = private.join("bin/ssh");
        fs::write(&ssh, b"#!/bin/sh\nprintf 'unexpected ssh\\n' >> \"$CAGENTS_TEST_CHANNEL_WORK_UI_SSH\"\nexit 1\n").unwrap();
        fs::set_permissions(ssh, fs::Permissions::from_mode(0o700)).unwrap();
        let stdout = private.join("stdout");
        let stderr = private.join("stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .current_dir(&private)
            .env(CHILD, role)
            .env(ROOT, &private)
            .env(MARKER, &marker)
            .env("PATH", private.join("bin"))
            .env("HOME", private.join("home"))
            .env("XDG_RUNTIME_DIR", private.join("runtime"))
            .env("XDG_CONFIG_HOME", private.join("config"))
            .env("XDG_DATA_HOME", private.join("data"))
            .env("XDG_CACHE_HOME", private.join("cache"))
            .env("XDG_STATE_HOME", private.join("state"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        match *role {
            "user" => {}
            "present" => {
                command.env(crate::agent_execution::AUTHORITY_ENV, ORIGINAL);
            }
            "empty" => {
                command.env(crate::agent_execution::AUTHORITY_ENV, "");
            }
            "non-unicode" => {
                command.env(
                    crate::agent_execution::AUTHORITY_ENV,
                    OsString::from_vec(vec![0xff]),
                );
            }
            _ => unreachable!(),
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(35);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("private App child exceeded bound: {role}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(fs::metadata(&stdout).unwrap().len() <= 1024 * 1024);
        assert!(fs::metadata(&stderr).unwrap().len() <= 1024 * 1024);
        let stdout = fs::read_to_string(stdout).unwrap();
        let stderr = fs::read_to_string(stderr).unwrap();
        assert!(!stdout.contains(ORIGINAL) && !stderr.contains(ORIGINAL));
        assert!(!stdout.contains(REPLACEMENT) && !stderr.contains(REPLACEMENT));
        assert!(status.success(), "{role}: {stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
        assert!(!marker.exists());
    }
}
#[test]
fn actual_channel_work_read_and_end_keep_original_route_scope_and_refused_work() {
    isolated(
        &["user", "present", "empty", "non-unicode"],
        exercise_refusal,
    );
}
#[test]
fn explicit_receipt_check_keeps_original_scope_and_validates_full_restarted_home() {
    isolated(
        &["user", "present", "empty", "non-unicode"],
        exercise_restart,
    );
}
#[test]
fn inherited_remote_channel_work_refuses_before_creating_a_proxy_or_resolving_ssh() {
    isolated(&["present", "empty", "non-unicode"], exercise_remote);
}
