//! Explicit User test callers, separate from the invoking hosted session.
use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const SELECTOR: &str = "CAGENTS_TEST_USER_FIXTURE_SELECTOR";

/// Returns true only in the fresh, exact-test User child. The parent never
/// reads, copies, removes or replaces its own delegation credential.
pub(super) fn enter() -> bool {
    let name = thread::current()
        .name()
        .expect("User fixture must run under libtest")
        .to_owned();
    if let Some(selected) = std::env::var_os(SELECTOR) {
        assert_eq!(selected, name.as_str());
        return true;
    }

    let root = tempfile::Builder::new()
        .prefix("u")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    assert_eq!(fs::canonicalize(root.path()).unwrap(), root.path());
    for leaf in ["home", "runtime", "data", "config", "cache", "tmp"] {
        let path = root.path().join(leaf);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let output = |leaf| {
        fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(root.path().join(leaf))
            .unwrap()
    };
    // This creates a separate synthetic User caller, not a fallback for an
    // operational connection. No inherited environment value is consulted.
    let child = Command::new(std::env::current_exe().unwrap())
        .env_clear()
        .args([
            "--exact",
            &name,
            "--nocapture",
            "--test-threads=1",
            "--color=never",
        ])
        .env(SELECTOR, &name)
        .env("HOME", root.path().join("home"))
        .env("XDG_RUNTIME_DIR", root.path().join("runtime"))
        .env("XDG_DATA_HOME", root.path().join("data"))
        .env("XDG_CONFIG_HOME", root.path().join("config"))
        .env("XDG_CACHE_HOME", root.path().join("cache"))
        .env("TMPDIR", root.path())
        .env("PATH", "/usr/bin:/bin")
        .current_dir(root.path())
        .stdin(Stdio::null())
        .stdout(output("stdout"))
        .stderr(output("stderr"))
        .spawn()
        .unwrap();
    struct OriginalChild(Child);
    impl Drop for OriginalChild {
        fn drop(&mut self) {
            if self.0.try_wait().unwrap().is_none() {
                self.0.kill().unwrap();
            }
            self.0.wait().unwrap();
        }
    }
    let mut child = OriginalChild(child);
    let deadline = Instant::now() + Duration::from_secs(90);
    let status = loop {
        for leaf in ["stdout", "stderr"] {
            assert!(
                fs::metadata(root.path().join(leaf)).unwrap().len() <= 1024 * 1024,
                "User fixture output exceeded its limit: {name}"
            );
        }
        if let Some(status) = child.0.try_wait().unwrap() {
            break status;
        }
        assert!(Instant::now() < deadline, "User fixture timed out: {name}");
        thread::sleep(Duration::from_millis(5));
    };
    let read = |leaf| {
        let path = root.path().join(leaf);
        assert!(fs::metadata(&path).unwrap().len() <= 1024 * 1024);
        fs::read_to_string(path).unwrap()
    };
    let stdout = read("stdout");
    let stderr = read("stderr");
    assert!(status.success(), "{name}: {status}\n{stdout}\n{stderr}");
    assert!(
        stdout
            .lines()
            .any(|line| line == format!("test {name} ... ok")),
        "exact User fixture did not pass: {stdout}\n{stderr}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| { line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;") }),
        "User fixture must execute exactly one original test: {stdout}\n{stderr}"
    );
    false
}
