//! Public-entry refusal, with private device configuration and synthetic scope.
//! No real SSH endpoint, daemon, account or issued capability is used.
use super::*;
use std::{
    ffi::OsString,
    fs::{self, File},
    os::unix::{ffi::OsStringExt, fs::PermissionsExt},
};

#[test]
fn scoped_remote_write_reports_permission_before_device_preparation() {
    const ROLE: &str = "CAGENTS_TEST_PROJECT_CHANNEL_REMOTE_ROLE";
    const MARKER: &str = "CAGENTS_TEST_PROJECT_CHANNEL_SSH_MARKER";
    const TOKEN: &str = "fixture-original-remote-scope";
    let selector = thread::current().name().unwrap().to_owned();
    let Some(role) = std::env::var_os(ROLE) else {
        for role in ["present", "empty", "non-unicode"] {
            let private = tempfile::Builder::new()
                .prefix("pc-remote-")
                .tempdir()
                .unwrap();
            for leaf in ["home", "runtime", "config", "data", "cache", "state", "bin"] {
                let path = private.path().join(leaf);
                fs::create_dir(&path).unwrap();
                fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            let ssh = private.path().join("bin/ssh");
            fs::write(&ssh, b"#!/bin/sh\nprintf 'unexpected ssh\\n' >> \"$CAGENTS_TEST_PROJECT_CHANNEL_SSH_MARKER\"\nexit 1\n").unwrap();
            fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
            let marker = private.path().join("ssh-called");
            let stdout = private.path().join("stdout");
            let stderr = private.path().join("stderr");
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .env_clear()
                .args(["--exact", &selector, "--nocapture", "--test-threads=1"])
                .current_dir(private.path())
                .env(ROLE, role)
                .env(MARKER, &marker)
                .env("PATH", private.path().join("bin"))
                .env("HOME", private.path().join("home"))
                .env("XDG_RUNTIME_DIR", private.path().join("runtime"))
                .env("XDG_CONFIG_HOME", private.path().join("config"))
                .env("XDG_DATA_HOME", private.path().join("data"))
                .env("XDG_CACHE_HOME", private.path().join("cache"))
                .env("XDG_STATE_HOME", private.path().join("state"))
                .stdin(Stdio::null())
                .stdout(File::create(&stdout).unwrap())
                .stderr(File::create(&stderr).unwrap());
            command.env(
                crate::agent_execution::AUTHORITY_ENV,
                match role {
                    "present" => OsString::from(TOKEN),
                    "empty" => OsString::new(),
                    "non-unicode" => OsString::from_vec(vec![0xff]),
                    _ => unreachable!(),
                },
            );
            let mut child = command.spawn().unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if Instant::now() >= deadline {
                    child.kill().unwrap();
                    child.wait().unwrap();
                    panic!("private remote-refusal child exceeded its bound: {role}");
                }
                thread::sleep(Duration::from_millis(5));
            };
            assert!(!marker.exists(), "refused scope must not start SSH");
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
                .collect::<Vec<_>>();
            assert_eq!(errors.len(), 1);
            assert_eq!(errors[0]["ok"], false);
            assert_eq!(errors[0]["error"]["kind"], "permission_denied");
            let message = errors[0]["error"]["message"].as_str().unwrap();
            assert!(message.contains("own terminal"));
            assert!(!message.contains("daemon start"));
        }
        return;
    };
    assert!(["present", "empty", "non-unicode"].contains(&role.to_str().unwrap()));
    let devices = crate::device::configuration_path().unwrap();
    fs::create_dir_all(devices.parent().unwrap()).unwrap();
    fs::write(devices, br#"{"devices":[{"name":"fixture-remote","host":"scope-fixture.invalid","executable":"cagents"}]}"#).unwrap();
    let channel = ChannelId::new().to_string();
    let result = run_from_with_staged_lookup(
        [
            "cagents",
            "--device",
            "fixture-remote",
            "--json",
            "channel",
            "close",
            &channel,
        ],
        |_, _| panic!("scoped remote write must not consult a staged User owner"),
    );
    assert_eq!(result, ExitCode::FAILURE);
    assert!(!Path::new(&std::env::var_os(MARKER).unwrap()).exists());
}
