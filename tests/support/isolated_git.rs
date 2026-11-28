use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
use std::{
    fs,
    io::{self, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

const TEST: &str = "CAGENTS_ISOLATED_GIT_TEST";
const ROOT: &str = "CAGENTS_ISOLATED_GIT_ROOT";

fn test_name(name: &str) -> &str {
    name.strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(name)
}

pub(crate) fn fixture_git_environment(mut command: CommandSpec) -> CommandSpec {
    const CONFIG: [(&str, &str); 6] = [
        ("core.fsmonitor", "false"),
        ("core.hooksPath", "/dev/null"),
        ("commit.gpgsign", "false"),
        ("init.templateDir", ""),
        ("gc.auto", "0"),
        ("maintenance.auto", "false"),
    ];
    command = command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", CONFIG.len().to_string());
    for (index, (key, value)) in CONFIG.into_iter().enumerate() {
        command = command
            .env(format!("GIT_CONFIG_KEY_{index}"), key)
            .env(format!("GIT_CONFIG_VALUE_{index}"), value);
    }
    command
}

pub(super) fn child_root() -> PathBuf {
    std::env::var_os(ROOT)
        .expect("owning private Git fixture namespace")
        .into()
}

/// A dedicated worker can adopt escaped Git descendants. Its success report
/// must come after those descendants exit, since the owner removes the root.
#[doc(hidden)]
pub struct IsolatedGitChild {
    _private: (),
}

impl IsolatedGitChild {
    pub(super) fn begin(name: &str) -> Option<Self> {
        if std::env::var(TEST).as_deref() != Ok(test_name(name)) {
            return None;
        }
        // SAFETY: prctl takes no pointers here. This runs only in the
        // single-test child, never in the parallel harness.
        #[cfg(target_os = "linux")]
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0,
            "{}",
            io::Error::last_os_error()
        );
        Some(Self { _private: () })
    }
}

impl Drop for IsolatedGitChild {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        // Darwin has no subreaper. The private Git config disables automatic
        // maintenance, so ordinary Git calls do not leave detached children.
        loop {
            // SAFETY: a null status pointer is valid for waitpid.
            if unsafe { libc::waitpid(-1, std::ptr::null_mut(), 0) } > 0 {
                continue;
            }
            let error = io::Error::last_os_error();
            match error.raw_os_error() {
                Some(libc::ECHILD) => break,
                Some(libc::EINTR) => {}
                _ => panic!("settling the Git fixture process family: {error}"),
            }
        }
    }
}

struct Namespace(PathBuf);

impl Drop for Namespace {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("Retained Git fixture root: {}", self.0.display());
        }
    }
}

pub(super) fn run(name: &str, directory: tempfile::TempDir, timeout: Duration) {
    let name = test_name(name);
    let directory = Namespace(directory.keep());
    let mut command = CommandSpec::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            name,
            "--nocapture",
            "--test-threads=1",
            "--color=never",
        ])
        .timeout(timeout)
        .output_limits(1024 * 1024, 1024 * 1024);
    for (key, _) in std::env::vars_os() {
        if key.as_encoded_bytes().starts_with(b"GIT_") {
            command = command.env_remove(key);
        }
    }
    let command = fixture_git_environment(command)
        .env(TEST, name)
        .env(ROOT, &directory.0)
        .env("TMPDIR", &directory.0);
    let (success, stdout, stderr, status) =
        match SystemProcessExecutor.execute(&command, &CancellationToken::new()) {
            Ok(output) => (
                output.success,
                output.stdout,
                output.stderr,
                format!("exit {:?}, signal {:?}", output.exit_code, output.signal),
            ),
            Err(failure) => (
                false,
                failure.stdout,
                failure.stderr,
                format!("{:?}: {}", failure.kind, failure.message),
            ),
        };
    write_capture(&directory.0, "stdout", &stdout);
    write_capture(&directory.0, "stderr", &stderr);
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(success, "{status}\n{stdout}\n{stderr}");
    let passed = format!("test {name} ... ok");
    assert!(
        stdout.lines().any(|line| line.contains(&passed)),
        "{stdout}"
    );
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
        "{stdout}"
    );
    print!("{stdout}");
    eprint!("{stderr}");
    if let Err(error) = fs::remove_dir_all(&directory.0)
        && error.kind() != io::ErrorKind::NotFound
    {
        panic!(
            "removing settled Git fixture root {}: {error}",
            directory.0.display()
        );
    }
}

fn write_capture(root: &Path, name: &str, bytes: &[u8]) {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join(name))
        .unwrap()
        .write_all(bytes)
        .unwrap();
}
