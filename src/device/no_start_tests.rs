//! Exercise policy capture through the real private proxy construction and
//! command builder. No client connects, so no SSH or remote daemon is run.
use super::*;

const ROLE: &str = "CAGENTS_TEST_DEVICE_START_POLICY_ROLE";
const ROOT: &str = "CAGENTS_TEST_DEVICE_START_POLICY_ROOT";

fn captured_policy_survives_delayed_proxy(role: &str) {
    let expected = match role {
        "one" | "true" | "mixed-yes" => true,
        "absent" | "false" | "empty" => false,
        _ => panic!("unknown private policy role"),
    };
    assert!(
        !crate::control::daemon_autostart_disabled(),
        "this UI-like child must not acquire a CLI StartPolicy"
    );
    let root = PathBuf::from(std::env::var_os(ROOT).unwrap());
    let context = ConnectionContext::new(Device {
        name: "private-policy-fixture".into(),
        host: "policy-fixture.invalid".into(),
        executable: "/fixture/cagents".into(),
    });
    // SAFETY: this is a dedicated, single-test child; the harness main thread
    // waits for this test and no application worker exists yet. Only this
    // synthetic policy variable changes, before starting the proxy worker.
    unsafe {
        std::env::set_var("CAGENTS_NO_START", if expected { "false" } else { "YeS" });
    }
    let socket = context.socket_path().unwrap();
    assert!(socket.starts_with(root.join("runtime")));
    assert!(socket.exists());
    let transport = context
        .bridge
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .transport
        .clone();
    // Observe Transport, rather than a newly introduced context field, so the
    // same regression compiles against the predecessor's public constructor.
    let captured = transport.no_start;
    let command = bridge_command(&transport, None);
    let arguments = command
        .get_args()
        .map(|argument| argument.to_owned())
        .collect::<Vec<_>>();
    assert!(transport.pool.lock().unwrap().is_empty());
    assert_eq!(context.socket_path().unwrap(), socket);
    context.close();
    assert!(context.socket_path().is_err());
    assert!(transport.cancelled.is_cancelled());
    assert!(transport.pool.lock().unwrap().is_empty());
    drop(command);
    drop(transport);
    assert!(!socket.exists(), "private proxy cleanup must finish");

    assert_eq!(captured, expected, "original {role} policy must survive");
    assert_eq!(
        &arguments[arguments.len() - 3..],
        [
            std::ffi::OsString::from("--"),
            std::ffi::OsString::from("policy-fixture.invalid"),
            std::ffi::OsString::from(if expected {
                "'/fixture/cagents' --no-start daemon bridge"
            } else {
                "'/fixture/cagents' daemon bridge"
            }),
        ],
        "the remote command must retain the original {role} selection policy"
    );
}

#[test]
fn selected_device_keeps_inherited_start_policy_through_delayed_proxy_creation() {
    if let Some(role) = std::env::var_os(ROLE) {
        captured_policy_survives_delayed_proxy(role.to_str().unwrap());
        return;
    }
    let selector = thread::current().name().unwrap().to_owned();
    for (role, initial) in [
        ("absent", None),
        ("one", Some("1")),
        ("true", Some("true")),
        ("mixed-yes", Some("YeS")),
        ("false", Some("false")),
        ("empty", Some("")),
    ] {
        let directory = tempfile::Builder::new().prefix("dn-").tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for leaf in ["home", "runtime", "config", "data", "cache", "state", "bin"] {
            let path = root.join(leaf);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let stdout_path = root.join("stdout");
        let stderr_path = root.join("stderr");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .env_clear()
            .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
            .current_dir(&root)
            .env(ROLE, role)
            .env(ROOT, &root)
            .env("PATH", root.join("bin"))
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_STATE_HOME", root.join("state"))
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout_path).unwrap())
            .stderr(fs::File::create(&stderr_path).unwrap());
        if let Some(initial) = initial {
            command.env("CAGENTS_NO_START", initial);
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("private device policy child exceeded its bound: {role}");
            }
            thread::sleep(Duration::from_millis(5));
        };
        assert!(fs::metadata(&stdout_path).unwrap().len() <= 1024 * 1024);
        assert!(fs::metadata(&stderr_path).unwrap().len() <= 1024 * 1024);
        let stdout = fs::read_to_string(stdout_path).unwrap();
        let stderr = fs::read_to_string(stderr_path).unwrap();
        assert!(status.success(), "{role}: {stdout}\n{stderr}");
        assert!(
            stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
            "exactly one private policy child test must run: {role}"
        );
    }
}
