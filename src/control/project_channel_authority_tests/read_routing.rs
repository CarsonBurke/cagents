//! Synthetic local peers exercise global reads, never production permission or
//! provider execution. Each role has an isolated process and private XDG tree.
use super::*;
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::ffi::OsStringExt,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
};

const ROLE: &str = "CAGENTS_TEST_GLOBAL_LIST_ROLE";
const TOKEN: &str = "synthetic-global-list-capability";

fn arguments(command: &str, no_start: bool) -> Vec<OsString> {
    let mut args = vec![
        "cagents".into(),
        "--device".into(),
        "local".into(),
        "--json".into(),
    ];
    if no_start {
        args.push("--no-start".into());
    }
    args.extend([command.into(), "list".into()]);
    args
}

fn exercise() {
    for command in ["project", "channel"] {
        for no_start in [false, true] {
            let socket = daemon_socket_path().unwrap();
            fs::create_dir_all(socket.parent().unwrap()).unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            listener.set_nonblocking(true).unwrap();
            let worker = thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(5);
                let mut ready = 0;
                let mut raw = 0;
                loop {
                    let mut peer = loop {
                        match listener.accept() {
                            Ok((peer, _)) => break peer,
                            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                                assert!(Instant::now() < deadline, "global read did not connect");
                                thread::sleep(Duration::from_millis(2));
                            }
                            Err(error) => panic!("private list listener: {error}"),
                        }
                    };
                    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    peer.set_write_timeout(Some(Duration::from_secs(3)))
                        .unwrap();
                    let Some(hello) = read_frame::<ClientEnvelope>(&mut peer).unwrap() else {
                        raw += 1;
                        assert!(raw <= 1, "no repeated existence probe");
                        continue;
                    };
                    assert!(matches!(hello.request, ClientRequest::Connect { .. }));
                    assert!(hello.proof.is_none());
                    reply(&mut peer, &hello, connected());
                    let Some(request) = read_frame::<ClientEnvelope>(&mut peer).unwrap() else {
                        ready += 1;
                        assert!(ready <= 1, "no repeated readiness probe");
                        continue;
                    };
                    assert_eq!(request.proof, Some(proof()));
                    assert!(
                        matches!(request.request, ClientRequest::ReadWorkspaceModel),
                        "global reads never authenticate or mutate"
                    );
                    model_reply(&mut peer, &request, WorkspaceModel::default());
                    assert!(read_frame::<ClientEnvelope>(&mut peer).unwrap().is_none());
                    return (listener, ready, raw);
                }
            });
            let staged = Cell::new(0);
            let result = run_from_with_staged_lookup(arguments(command, no_start), |_, recover| {
                staged.set(staged.get() + 1);
                assert_eq!(recover, !no_start);
                Ok(None)
            });
            // Join even if dispatch failed, so a fixture failure cannot leave a
            // detached peer using an endpoint after its private tree is removed.
            let (listener, ready, raw) = worker.join().unwrap();
            assert_eq!(result, ExitCode::SUCCESS);
            assert_eq!(staged.get(), 1, "global reads retain staged-owner lookup");
            assert_eq!(
                ready,
                usize::from(!no_start),
                "readiness is independent of scope"
            );
            assert_eq!(
                raw,
                usize::from(no_start),
                "no-start keeps its absence probe"
            );
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
            );
            drop(listener);
            fs::remove_file(socket).unwrap();
        }
        let socket = daemon_socket_path().unwrap();
        for busy in [false, true] {
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(socket.with_file_name("cagentsd.lock"))
                .unwrap();
            if busy {
                assert_eq!(
                    unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
                    0
                );
            }
            let staged = Cell::new(0);
            let result = run_from_with_staged_lookup(arguments(command, true), |_, recover| {
                staged.set(staged.get() + 1);
                assert!(!recover);
                Ok(None)
            });
            assert_eq!(result, ExitCode::FAILURE);
            assert_eq!(staged.get(), 1);
            assert!(!socket.exists(), "missing daemon must remain absent");
            drop(lock);
        }
    }
}

#[test]
fn public_lists_preserve_readiness_and_no_start_errors_for_every_caller_scope() {
    if std::env::var_os(ROLE).is_some() {
        exercise();
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for role in ["user", "present", "empty", "non-unicode"] {
        let private = tempfile::Builder::new()
            .prefix("pc-read-")
            .tempdir()
            .unwrap();
        for leaf in [
            "home", "runtime", "config", "data", "cache", "state", "bin", "tmp",
        ] {
            let path = private.path().join(leaf);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let stdout = private.path().join("stdout");
        let stderr = private.path().join("stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .current_dir(private.path())
            .env(ROLE, role)
            .env("PATH", private.path().join("bin"))
            .env("HOME", private.path().join("home"))
            .env("XDG_RUNTIME_DIR", private.path().join("runtime"))
            .env("XDG_CONFIG_HOME", private.path().join("config"))
            .env("XDG_DATA_HOME", private.path().join("data"))
            .env("XDG_CACHE_HOME", private.path().join("cache"))
            .env("XDG_STATE_HOME", private.path().join("state"))
            .env("TMPDIR", private.path().join("tmp"))
            .stdin(Stdio::null())
            .stdout(File::create(&stdout).unwrap())
            .stderr(File::create(&stderr).unwrap());
        if role != "user" {
            command.env(
                crate::agent_execution::AUTHORITY_ENV,
                match role {
                    "present" => OsString::from(TOKEN),
                    "empty" => OsString::new(),
                    "non-unicode" => OsString::from_vec(vec![0xff]),
                    _ => unreachable!(),
                },
            );
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("private list child exceeded bound: {role}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(fs::metadata(&stdout).unwrap().len() <= 1024 * 1024);
        assert!(fs::metadata(&stderr).unwrap().len() <= 1024 * 1024);
        let stdout = fs::read_to_string(stdout).unwrap();
        let stderr = fs::read_to_string(stderr).unwrap();
        assert!(status.success(), "{role}: {stdout}\n{stderr}");
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored;"));
        assert!(!stdout.contains(TOKEN) && !stderr.contains(TOKEN));
        let errors = stderr
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .map(|value| value["error"]["kind"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            errors,
            [
                "daemon_not_running",
                "daemon_busy",
                "daemon_not_running",
                "daemon_busy"
            ]
        );
        let successes = stdout
            .lines()
            .filter_map(|line| line.find('{').map(|start| &line[start..]))
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect::<Vec<_>>();
        assert_eq!(successes.len(), 4);
        for success in successes {
            assert_eq!(success["ok"], true);
            assert!([json!("project.list"), json!("channel.list")].contains(&success["command"]));
        }
    }
}
