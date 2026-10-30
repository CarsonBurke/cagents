//! Real OS-worker custody must not serialize healthy terminal shutdown.
use super::*;
use crate::{
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalHandle, TerminalSendError},
};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    sync::mpsc,
    time::{Duration, Instant},
};

const ROOT: &str = "CAGENTS_SHUTDOWN_BATCH_ROOT";
const SCOPE: &str = "CAGENTS_SHUTDOWN_BATCH_AUTHORITY_PRESENCE";

// The guardian is another exact-test process, not a shared-libtest subreaper.
mod fixture {
    use super::{ROOT, SCOPE};
    use std::{
        fs::{self, File, OpenOptions},
        io::{self, Read, Write},
        net::Shutdown,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                fs::{OpenOptionsExt, PermissionsExt},
                net::UnixStream,
                process::CommandExt,
            },
        },
        path::{Path, PathBuf},
        process::{Child, Command, ExitStatus, Stdio},
        thread,
        time::{Duration, Instant},
    };

    const SPEC: &str = "CAGENTS_SHUTDOWN_GUARDIAN_SPEC";
    const CONTROL: &str = "CAGENTS_SHUTDOWN_GUARDIAN_CONTROL";
    const GUARDIAN: &str = "host::shutdown_tests::fixture::guardian";
    const BODY_CONTROL: &str = "host::shutdown_tests::fixture::body_control";
    const LIMIT: Duration = Duration::from_secs(30);
    const CAPTURE_LIMIT: u64 = 256 * 1024;
    const RECEIPT_LIMIT: u64 = 16 * 1024;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Spec {
        test: String,
        control: Option<String>,
        owner_fd: i32,
        deadline_ns: u64,
    }

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Receipt {
        version: u8,
        test: String,
        guardian: u32,
        body: u32,
        body_success: bool,
        cancelled: bool,
        descendants_reaped: usize,
    }

    // Only constructed after the exact original guardian has been reaped and
    // its receipt matches this run. A failed body can settle without passing.
    struct Settlement {
        root: PathBuf,
        receipt: Receipt,
    }

    struct EvidenceDirectory(Option<PathBuf>);
    impl EvidenceDirectory {
        fn remove_after_assertions(mut self, proof: &Settlement) {
            assert_eq!(self.0.as_ref(), Some(&proof.root));
            let root = self.0.take().unwrap();
            fs::remove_dir_all(&root).unwrap_or_else(|error| {
                panic!(
                    "removing settled shutdown fixture {}; remaining evidence retained: {error}",
                    root.display()
                )
            });
            eprintln!("Removed settled shutdown fixture: {}", root.display());
        }
    }
    impl Drop for EvidenceDirectory {
        fn drop(&mut self) {
            if let Some(root) = self.0.take() {
                eprintln!("Retained shutdown fixture: {}", root.display());
            }
        }
    }

    fn directory() -> (EvidenceDirectory, PathBuf) {
        // Honour the command owner's legitimate non-repository TMPDIR and its
        // short socket paths. Never sweep any directory from an earlier run.
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let _retained = directory.keep();
        let evidence = EvidenceDirectory(Some(root.clone()));
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        for name in ["home", "run", "data", "config", "cache", "tmp"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        (evidence, root)
    }

    fn capture(path: &Path) -> File {
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap()
    }

    fn monotonic_ns() -> u64 {
        let mut now = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        assert_eq!(
            unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut now) },
            0
        );
        (now.tv_sec as u64)
            .saturating_mul(1_000_000_000)
            .saturating_add(now.tv_nsec as u64)
    }

    // EOF means the owner timed out, unwound, or exited. The descriptor is
    // CLOEXEC in the guardian before it starts the body, so no body can keep
    // this ownership channel alive. No message contains delegation authority.
    fn owner_cancelled(owner: &mut UnixStream, deadline_ns: u64) -> io::Result<bool> {
        if monotonic_ns() >= deadline_ns {
            return Ok(true);
        }
        let mut byte = [0];
        loop {
            match owner.read(&mut byte) {
                Ok(_) => return Ok(true),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    if monotonic_ns() >= deadline_ns {
                        return Ok(true);
                    }
                }
                Err(error) => return Err(error),
            }
        }
    }

    struct Body(Child);
    impl Drop for Body {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }

    struct Guardian {
        child: Child,
        owner: UnixStream,
        reaped: bool,
    }
    impl Guardian {
        fn cancel_and_reap(&mut self) -> io::Result<ExitStatus> {
            // The guardian kills/reaps its original BODY Child. Killing only
            // the guardian at this boundary would abandon that live body.
            let _ = self.owner.shutdown(Shutdown::Write);
            let status = self.child.wait()?;
            self.reaped = true;
            Ok(status)
        }
    }
    impl Drop for Guardian {
        fn drop(&mut self) {
            if !self.reaped {
                let _ = self.cancel_and_reap();
            }
        }
    }

    struct Run {
        guardian: u32,
        status: ExitStatus,
        timed_out: bool,
        guardian_stdout: String,
        guardian_stderr: String,
        stdout: String,
        stderr: String,
        capture_errors: Vec<String>,
    }
    impl Run {
        fn settlement(&self, root: &Path, test: &str) -> Result<Settlement, String> {
            let bytes = read_bounded(root, "family-settled.json", RECEIPT_LIMIT)
                .map_err(|error| format!("unreadable family settlement receipt: {error}"))?;
            let receipt: Receipt = serde_json::from_slice(&bytes)
                .map_err(|error| format!("invalid family settlement receipt: {error}"))?;
            if receipt.version != 1
                || receipt.test != test
                || receipt.guardian != self.guardian
                || receipt.body <= 1
                || receipt.body == receipt.guardian
            {
                return Err("family settlement receipt does not identify this run".into());
            }
            Ok(Settlement {
                root: root.to_owned(),
                receipt,
            })
        }

        fn success(&self, root: &Path, test: &str) -> Result<Settlement, String> {
            let proof = self.settlement(root, test)?;
            if !self.capture_errors.is_empty() {
                return Err(format!(
                    "shutdown capture failed: {:?}",
                    self.capture_errors
                ));
            }
            if self.timed_out
                || !self.status.success()
                || !proof.receipt.body_success
                || proof.receipt.cancelled
            {
                return Err(
                    "shutdown guardian or body failed or exceeded the original 30s budget".into(),
                );
            }
            if !self
                .guardian_stdout
                .contains(&format!("test {GUARDIAN} ... ok"))
                || !self
                    .guardian_stdout
                    .contains("test result: ok. 1 passed; 0 failed; 0 ignored;")
            {
                return Err("missing exact guardian test".into());
            }
            if !self.stdout.contains(&format!("test {test} ... ok"))
                || !self
                    .stdout
                    .contains("test result: ok. 1 passed; 0 failed; 0 ignored;")
            {
                return Err("zero, failed or incorrect body test selection".into());
            }
            Ok(proof)
        }
    }

    fn read_bounded(root: &Path, name: &str, limit: u64) -> Result<Vec<u8>, String> {
        let path = root.join(name);
        // Limit the actual read from one descriptor: a failed or uncertain
        // family may keep appending after any metadata observation. The full
        // captured file remains untouched, including overflow and invalid UTF-8.
        // This is a read bound, not a bound on the writer's live disk growth.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .map_err(|error| format!("unreadable {name}: {error}"))?;
        if !file
            .metadata()
            .map_err(|error| format!("cannot inspect {name}: {error}"))?
            .is_file()
        {
            return Err(format!("{name} is not a regular evidence file"));
        }
        let mut bytes = Vec::new();
        file.take(limit + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("unreadable {name}: {error}"))?;
        if bytes.len() as u64 > limit {
            return Err(format!("{name} exceeds its {limit}-byte read limit"));
        }
        Ok(bytes)
    }

    fn read_log(root: &Path, name: &str) -> Result<String, String> {
        let bytes = read_bounded(root, name, CAPTURE_LIMIT)?;
        String::from_utf8(bytes).map_err(|error| format!("invalid UTF-8 in {name}: {error}"))
    }

    fn run(
        root: &Path,
        test: &str,
        control: Option<&str>,
        limit: Duration,
        cancel_after_ready: bool,
    ) -> Run {
        let (owner, remote) = UnixStream::pair().unwrap();
        let remote_fd = remote.as_raw_fd();
        let spec = Spec {
            test: test.into(),
            control: control.map(str::to_owned),
            owner_fd: remote_fd,
            deadline_ns: monotonic_ns().saturating_add(limit.as_nanos() as u64),
        };
        let mut command = Command::new(std::env::current_exe().unwrap());
        // Leave delegation inherited and opaque through BOTH exec boundaries.
        // Equality crosses the boundary only as a public presence boolean.
        for (name, _) in std::env::vars_os() {
            if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV) {
                command.env_remove(name);
            }
        }
        command
            .args([
                "--exact",
                GUARDIAN,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                "--color=never",
            ])
            .current_dir(root)
            .env(SPEC, serde_json::to_string(&spec).unwrap())
            .env(ROOT, root)
            .env(
                SCOPE,
                if crate::agent_execution::Authority::from_environment()
                    != crate::agent_execution::Authority::default()
                {
                    "present"
                } else {
                    "absent"
                },
            )
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::from(capture(&root.join("guardian.stdout"))))
            .stderr(Stdio::from(capture(&root.join("guardian.stderr"))));
        // SAFETY: the descriptor stays owned until spawn returns. Only the
        // async-signal-safe fcntl runs in this between-fork-and-exec hook.
        unsafe {
            command.pre_exec(move || {
                if libc::fcntl(remote_fd, libc::F_SETFD, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(remote);
        let guardian_id = child.id();
        let mut guardian = Guardian {
            child,
            owner,
            reaped: false,
        };
        let deadline = Instant::now() + limit;
        let (status, timed_out) = loop {
            if let Some(status) = guardian.child.try_wait().unwrap() {
                guardian.reaped = true;
                break (status, false);
            }
            if cancel_after_ready && root.join("control-payload").is_file() {
                break (guardian.cancel_and_reap().unwrap(), false);
            }
            if Instant::now() >= deadline {
                break (guardian.cancel_and_reap().unwrap(), true);
            }
            thread::sleep(Duration::from_millis(10));
        };
        // As in the old exact-child kill+wait, uninterruptible kernel I/O can
        // extend cancellation/reaping beyond the 30s polling deadline.
        let mut capture_errors = Vec::new();
        let mut load = |name| match read_log(root, name) {
            Ok(text) => text,
            Err(error) => {
                capture_errors.push(error.clone());
                error
            }
        };
        let guardian_stdout = load("guardian.stdout");
        let guardian_stderr = load("guardian.stderr");
        let stdout = load("test.stdout");
        let stderr = load("test.stderr");
        Run {
            guardian: guardian_id,
            status,
            timed_out,
            guardian_stdout,
            guardian_stderr,
            stdout,
            stderr,
            capture_errors,
        }
    }

    pub(super) fn private_runtime(test: &str) -> Option<PathBuf> {
        if let Some(root) = std::env::var_os(ROOT) {
            let expected = match std::env::var(SCOPE).as_deref() {
                Ok("present") => true,
                Ok("absent") => false,
                _ => panic!("missing or invalid shutdown fixture authority presence marker"),
            };
            assert_eq!(
                crate::agent_execution::Authority::from_environment()
                    != crate::agent_execution::Authority::default(),
                expected,
                "isolated shutdown fixture lost its inherited authority scope"
            );
            return Some(PathBuf::from(root).canonicalize().unwrap());
        }
        let (evidence, root) = directory();
        let run = run(&root, test, None, LIMIT, false);
        let proof = run.success(&root, test).unwrap_or_else(|error| {
            panic!("{error}; retained {}:\nBODY STDOUT:\n{}\nBODY STDERR:\n{}\nGUARDIAN STDOUT:\n{}\nGUARDIAN STDERR:\n{}", root.display(), run.stdout, run.stderr, run.guardian_stdout, run.guardian_stderr)
        });
        evidence.remove_after_assertions(&proof);
        None
    }

    fn settle(owner: &mut UnixStream, deadline_ns: u64) -> io::Result<(usize, bool)> {
        let mut reaped = 0;
        loop {
            // Check EVERY iteration, including successful reap and EINTR paths.
            // Continuous orphan churn cannot evade the original owner's budget.
            let cancelled = owner_cancelled(owner, deadline_ns)?;
            let waited = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) {
                return Ok((reaped, cancelled));
            }
            if cancelled {
                return Err(io::Error::other(
                    "shutdown family remains unsettled after cancellation; no receipt",
                ));
            }
            if waited > 0 {
                reaped += 1;
                continue;
            }
            if waited < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(error);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    #[ignore = "private guardian entrypoint, selected only by shutdown fixture parents"]
    fn guardian() {
        let spec: Spec =
            serde_json::from_str(&std::env::var(SPEC).expect("private guardian specification"))
                .unwrap();
        assert!(std::env::var_os(ROOT).is_some(), "private guardian root");
        // Check the public presence marker in the guardian as well as the body.
        let root = private_runtime(&spec.test).expect("private guardian runtime");
        // SAFETY: this is the dedicated exact guardian process. Shared parent
        // libtests and actual fixture bodies never change their subreaper state.
        assert_eq!(
            unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) },
            0
        );
        assert!(spec.owner_fd > 2);
        let mut owner = unsafe { UnixStream::from_raw_fd(spec.owner_fd) };
        assert_eq!(
            unsafe { libc::fcntl(owner.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) },
            0
        );
        owner.set_nonblocking(true).unwrap();
        assert!(
            !owner_cancelled(&mut owner, spec.deadline_ns).unwrap(),
            "owner cancelled before body launch"
        );
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                &spec.test,
                "--nocapture",
                "--test-threads=1",
                "--color=never",
            ])
            .env_remove(SPEC)
            .env_remove(CONTROL)
            .current_dir(&root)
            .stdin(Stdio::null())
            .stdout(Stdio::from(capture(&root.join("test.stdout"))))
            .stderr(Stdio::from(capture(&root.join("test.stderr"))));
        if let Some(control) = spec.control {
            command.arg("--ignored").env(CONTROL, control);
        }
        let mut body = Body(command.spawn().unwrap());
        let body_id = body.0.id();
        fs::write(root.join("body.pid"), body_id.to_string()).unwrap();
        let mut cancelled = false;
        let status = loop {
            if let Some(status) = body.0.try_wait().unwrap() {
                break status;
            }
            if owner_cancelled(&mut owner, spec.deadline_ns).unwrap() {
                cancelled = true;
                body.0.kill().unwrap();
                break body.0.wait().unwrap();
            }
            thread::sleep(Duration::from_millis(10));
        };
        // The ORIGINAL BODY process is now reaped. Its background threads,
        // TLS and atexit callbacks can no longer create new fixture writers.
        let (descendants_reaped, settlement_cancelled) =
            settle(&mut owner, spec.deadline_ns).unwrap();
        cancelled |= settlement_cancelled;
        let receipt = Receipt {
            version: 1,
            test: spec.test,
            guardian: std::process::id(),
            body: body_id,
            body_success: status.success(),
            cancelled,
            descendants_reaped,
        };
        capture(&root.join("family-settled.json"))
            .write_all(&serde_json::to_vec(&receipt).unwrap())
            .unwrap();
        assert!(
            receipt.body_success && !receipt.cancelled,
            "private shutdown body failed or was cancelled; settled failure evidence retained"
        );
    }

    // This control registers an atexit writer AFTER libtest reports body success.
    // The writer closes output pipes and leaves the body session; only the
    // guardian's post-body-exit ECHILD census can establish its final write.
    extern "C" fn start_late_writer() {
        let launch = || -> io::Result<()> {
            let root = PathBuf::from(
                std::env::var_os(ROOT)
                    .ok_or_else(|| io::Error::other("missing late-writer root"))?,
            );
            let script = r#"import os,pathlib,sys,time
root=pathlib.Path(sys.argv[1]); body=int(sys.argv[2]); proc=pathlib.Path('/proc')/str(body)/'stat'
def identity():
 try: return proc.read_bytes().rsplit(b')',1)[1].split()[19]
 except FileNotFoundError: return None
original=identity(); os.setsid(); (root/'late-ready').write_text(str(os.getpid()))
deadline=time.monotonic()+5
while original is not None and identity()==original:
 if time.monotonic()>=deadline: raise RuntimeError('original body did not exit')
 time.sleep(0.005)
(root/'late-payload').write_text('written after original body exit')
"#;
            let _child = Command::new("/usr/bin/python3")
                .args(["-c", script])
                .arg(&root)
                .arg(std::process::id().to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            let deadline = Instant::now() + Duration::from_secs(5);
            while !root.join("late-ready").exists() {
                if Instant::now() >= deadline {
                    return Err(io::Error::other("late writer did not start"));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Ok(())
        };
        if let Err(error) = launch() {
            let _ = writeln!(io::stderr(), "late-writer atexit control failed: {error}");
            unsafe {
                libc::_exit(98);
            }
        }
    }

    #[test]
    #[ignore = "private body controls, selected only by fixture regression parents"]
    fn body_control() {
        let root = private_runtime(BODY_CONTROL).expect("private control body");
        let control = std::env::var(CONTROL).unwrap();
        fs::write(root.join("control-payload"), "private failure evidence").unwrap();
        match control.as_str() {
            "late" => assert_eq!(unsafe { libc::atexit(start_late_writer) }, 0),
            "failure" => panic!("intentional private fixture body failure"),
            "missing-capture" => fs::remove_file(root.join("test.stderr")).unwrap(),
            "oversized-capture" => io::stderr()
                .write_all(&vec![b'x'; CAPTURE_LIMIT as usize + 4096])
                .unwrap(),
            "non-utf8-capture" => io::stderr().write_all(&[0xff, b'a']).unwrap(),
            "limit-capture" => io::stderr()
                .write_all(&vec![b'v'; CAPTURE_LIMIT as usize])
                .unwrap(),
            "blocked" => loop {
                thread::park();
            },
            "success" => {}
            _ => panic!("unknown private body control"),
        }
    }

    #[test]
    fn cleanup_waits_for_a_writer_created_after_body_success() {
        let (evidence, root) = directory();
        let run = run(&root, BODY_CONTROL, Some("late"), LIMIT, false);
        let proof = run
            .success(&root, BODY_CONTROL)
            .unwrap_or_else(|error| panic!("{error}: {}", run.guardian_stderr));
        assert!(
            proof.receipt.descendants_reaped >= 1,
            "the late writer must belong to this guardian"
        );
        assert_eq!(
            fs::read_to_string(root.join("late-payload")).unwrap(),
            "written after original body exit"
        );
        evidence.remove_after_assertions(&proof);
        assert!(!root.exists());
    }

    #[test]
    fn failed_and_empty_body_selections_keep_evidence_until_owned_settlement_is_checked() {
        for (test, control) in [
            (BODY_CONTROL, "failure"),
            (BODY_CONTROL, "missing-capture"),
            ("host::shutdown_tests::fixture::no_such_test", "success"),
        ] {
            let (evidence, root) = directory();
            let run = run(&root, test, Some(control), LIMIT, false);
            let proof = run
                .settlement(&root, test)
                .expect("even the negative body family must be fully settled");
            assert!(run.success(&root, test).is_err());
            if control == "failure" {
                assert_eq!(
                    fs::read_to_string(root.join("control-payload")).unwrap(),
                    "private failure evidence"
                );
                assert!(!proof.receipt.body_success);
            } else if control == "missing-capture" {
                assert!(
                    run.capture_errors
                        .iter()
                        .any(|error| error.contains("test.stderr"))
                );
            }
            drop(evidence);
            assert!(
                root.is_dir(),
                "failure paths must preserve the original namespace"
            );
            // This passing meta-test owns ONLY this freshly created negative
            // control. Its valid settlement proof permits final no-leak cleanup.
            EvidenceDirectory(Some(root.clone())).remove_after_assertions(&proof);
            assert!(!root.exists());
        }
    }

    #[test]
    fn cancellation_reaps_the_original_body_and_keeps_its_evidence() {
        let (evidence, root) = directory();
        let run = run(&root, BODY_CONTROL, Some("blocked"), LIMIT, true);
        let proof = run
            .settlement(&root, BODY_CONTROL)
            .expect("cancelled synthetic body must be reaped with ECHILD");
        assert!(run.success(&root, BODY_CONTROL).is_err());
        assert!(proof.receipt.cancelled && !proof.receipt.body_success);
        assert_eq!(
            proof.receipt.descendants_reaped, 0,
            "the blocked control starts no descendants"
        );
        assert_eq!(
            fs::read_to_string(root.join("control-payload")).unwrap(),
            "private failure evidence"
        );
        drop(evidence);
        assert!(root.is_dir());
        EvidenceDirectory(Some(root.clone())).remove_after_assertions(&proof);
        assert!(!root.exists());
    }

    #[test]
    fn capture_read_limits_preserve_rejected_files_and_accept_the_exact_boundary() {
        for (control, payload, error) in [
            (
                "oversized-capture",
                vec![b'x'; CAPTURE_LIMIT as usize + 4096],
                Some("exceeds its 262144-byte"),
            ),
            ("non-utf8-capture", vec![0xff, b'a'], Some("invalid UTF-8")),
            ("limit-capture", vec![b'v'; CAPTURE_LIMIT as usize], None),
        ] {
            let (evidence, root) = directory();
            let run = run(&root, BODY_CONTROL, Some(control), LIMIT, false);
            let proof = run
                .settlement(&root, BODY_CONTROL)
                .expect("capture control must fully settle");
            assert!(run.status.success() && proof.receipt.body_success && !proof.receipt.cancelled);
            if let Some(error) = error {
                assert!(run.success(&root, BODY_CONTROL).is_err());
                assert!(
                    run.capture_errors
                        .iter()
                        .any(|detail| detail.contains(error))
                );
            } else {
                assert!(run.success(&root, BODY_CONTROL).is_ok());
                assert_eq!(run.stderr.as_bytes(), payload);
            }
            let path = root.join("test.stderr");
            assert_eq!(fs::metadata(&path).unwrap().len(), payload.len() as u64);
            assert_eq!(
                fs::read(&path).unwrap(),
                payload,
                "capture reading changed the original bytes"
            );
            drop(evidence);
            assert!(
                root.is_dir(),
                "rejected captures must keep the original namespace"
            );
            assert_eq!(
                fs::read(&path).unwrap(),
                payload,
                "retention changed the original bytes"
            );
            // All synthetic writers have exited under the genuine ECHILD
            // proof. Remove only this passing meta-test's fresh control root.
            EvidenceDirectory(Some(root.clone())).remove_after_assertions(&proof);
            assert!(!root.exists());
        }
    }

    #[test]
    fn missing_malformed_or_mismatched_receipts_and_bad_output_never_authorize_removal() {
        let (evidence, root) = directory();
        let mut run = run(&root, BODY_CONTROL, Some("success"), LIMIT, false);
        let proof = run.success(&root, BODY_CONTROL).unwrap();
        let path = root.join("family-settled.json");
        let original = fs::read(&path).unwrap();
        fs::remove_file(&path).unwrap();
        assert!(run.success(&root, BODY_CONTROL).is_err());
        fs::write(&path, b"not JSON").unwrap();
        assert!(run.success(&root, BODY_CONTROL).is_err());
        let oversized_receipt = vec![b'x'; RECEIPT_LIMIT as usize + 4096];
        fs::write(&path, &oversized_receipt).unwrap();
        assert!(run.success(&root, BODY_CONTROL).is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            oversized_receipt,
            "receipt reading changed oversized evidence"
        );
        let mut wrong: Receipt = serde_json::from_slice(&original).unwrap();
        wrong.guardian += 1;
        fs::write(&path, serde_json::to_vec(&wrong).unwrap()).unwrap();
        assert!(run.success(&root, BODY_CONTROL).is_err());
        fs::write(&path, original).unwrap();
        run.stdout = "test result: ok. 0 passed; 0 failed;".into();
        assert!(run.success(&root, BODY_CONTROL).is_err());
        drop(evidence);
        assert!(root.is_dir());
        // The earlier genuine proof still identifies this already settled
        // synthetic family; corrupting a receipt never starts another writer.
        EvidenceDirectory(Some(root.clone())).remove_after_assertions(&proof);
        assert!(!root.exists());
    }
}

fn private_runtime(test: &str) -> Option<PathBuf> {
    fixture::private_runtime(test)
}

fn session(id: u128, root: &Path) -> TerminalSession {
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf '%s' \"$$\" > root-pid; exec /bin/sleep 600".into(),
        ],
        working_directory: root.to_owned(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id: TerminalId::from_uuid(Uuid::from_u128(id)),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    }
}

fn start_time(pid: u32) -> io::Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .into())
}

// A failure guard addresses only the exact private root through its pidfd.
// Normal success proves the original reaped and disarms it without a signal.
struct RootPin {
    descriptor: File,
    retired: bool,
}

impl RootPin {
    fn exit_ready(&self) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result == 1 && descriptor.revents & libc::POLLIN != 0)
        }
    }
}

impl Drop for RootPin {
    fn drop(&mut self) {
        if !self.retired && !matches!(self.exit_ready(), Ok(true)) {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.descriptor.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            use std::io::Write;
            let _ = writeln!(
                io::stderr(),
                "private PTY failure cleanup used its pinned pidfd: {result}"
            );
        }
    }
}

// Sender destruction releases the actual blocking OS-thread body, including
// unwind before thread::scope waits. It never substitutes a completion bit.
struct ReleaseWorkers(Vec<crossbeam_channel::Sender<()>>);

fn unfinished_workers_do_not_starve_healthy(root: &Path, count: usize) {
    let baseline = TerminalHandle::reserved_worker_joins_for_test();
    assert_eq!(baseline, 0, "the isolated child must start without workers");
    assert_eq!(TerminalHandle::worker_join_capacity_for_test(), 1024);
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("shutdown-batch"),
        boot_id: crate::model::BootId::new("shutdown-batch"),
        host_incarnation: HostIncarnation::new("shutdown-batch"),
    });
    state.register_client(1);
    let mut blocked = Vec::new();
    let mut releases = Vec::new();
    for id in 1..=count {
        let (handle, release) = TerminalHandle::spawn_with_wedged_worker_for_test();
        state
            .finish_spawn(1, session(id as u128, root), handle.clone(), true)
            .unwrap();
        blocked.push(handle);
        releases.push(release);
    }
    let healthy_session = session((count + 1) as u128, root);
    let healthy =
        TerminalHandle::spawn(healthy_session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    state
        .finish_spawn(1, healthy_session, healthy.clone(), true)
        .unwrap();
    let ready_by = Instant::now() + Duration::from_secs(5);
    let pid: u32 = loop {
        if let Ok(pid) = fs::read_to_string(root.join("root-pid"))
            && let Ok(pid) = pid.trim().parse()
        {
            break pid;
        }
        assert!(Instant::now() < ready_by, "private PTY root did not start");
        thread::sleep(Duration::from_millis(5));
    };
    let original_start = start_time(pid).unwrap();
    assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(), root);
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(descriptor >= 0, "{}", io::Error::last_os_error());
    let root_pin = RootPin {
        descriptor: unsafe { File::from_raw_fd(descriptor as i32) },
        retired: false,
    };
    assert_eq!(start_time(pid).unwrap(), original_start);
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        baseline + count + 1
    );
    let expected: Vec<_> = (1..=count + 1)
        .map(|id| TerminalId::from_uuid(Uuid::from_u128(id as u128)))
        .collect();
    assert_eq!(
        state.terminals.keys().copied().collect::<Vec<_>>(),
        expected
    );
    let (handles, _) = state.graceful_shutdown();
    assert_eq!(handles.len(), count + 1);

    thread::scope(|scope| {
        let mut release = ReleaseWorkers(releases);
        let mut root_pin = root_pin;
        let (finished, result) = mpsc::sync_channel(1);
        let started = Instant::now();
        let batch = scope.spawn(move || {
            let _ = finished.send(finish_terminal_shutdown(&handles));
        });
        // Both unfinished bodies precede the real PTY in terminal-ID order.
        // They must not occupy the fixed join lanes or defer its shutdown.
        let healthy_by = started + Duration::from_secs(3);
        loop {
            let reaped = match start_time(pid) {
                Ok(start) => start != original_start,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::ESRCH) =>
                {
                    true
                }
                Err(error) => panic!("could not inspect the private PTY root: {error}"),
            };
            if reaped && healthy.worker_joined_for_test() {
                assert!(root_pin.exit_ready().unwrap());
                root_pin.retired = true;
                break;
            }
            assert!(
                Instant::now() < healthy_by,
                "healthy PTY root and actual worker were not reaped while earlier bodies blocked"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(healthy.is_completed());
        assert!(blocked.iter().all(|worker| !worker.is_completed()));
        assert!(
            blocked
                .iter()
                .all(|worker| !worker.worker_joined_for_test())
        );
        let batch_by = started + Duration::from_secs(8);
        let outcomes = result
            .recv_timeout(batch_by.saturating_duration_since(Instant::now()))
            .expect(
                "shutdown exceeded its single shared five-second deadline and settlement margin",
            );
        assert!(started.elapsed() < Duration::from_secs(8));
        assert_eq!(outcomes.len(), count + 1);
        for outcome in &outcomes[..count] {
            assert_eq!(*outcome, Err(TerminalSendError::Closed));
        }
        assert_eq!(outcomes[count], Ok(()));
        assert_eq!(
            TerminalHandle::reserved_worker_joins_for_test(),
            baseline + count
        );
        assert!(blocked.iter().all(|worker| !worker.is_completed()));
        assert!(
            blocked
                .iter()
                .all(|worker| !worker.worker_joined_for_test())
        );
        release.0.clear();
        let released_by = Instant::now() + Duration::from_secs(5);
        while blocked
            .iter()
            .any(|worker| !worker.worker_joined_for_test())
            || TerminalHandle::reserved_worker_joins_for_test() != baseline
        {
            assert!(
                Instant::now() < released_by,
                "released OS workers did not join"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // These factories have no PTY completion to certify, even after their
        // actual threads joined. Cached handles do not retain admission tickets.
        for worker in &blocked {
            assert!(!worker.is_completed());
            assert_eq!(worker.shutdown(), Err(TerminalSendError::Closed));
            assert!(worker.worker_joined_for_test());
            let _cached_screen = worker.snapshot();
        }
        assert_eq!(healthy.shutdown(), Ok(()));
        assert_eq!(TerminalHandle::reserved_worker_joins_for_test(), baseline);
        batch.join().unwrap();
        fs::write(
            root.join("worker-evidence.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "blocked_workers": count,
                "healthy_root_pid": pid,
                "healthy_root_start": original_start,
                "healthy_root_reaped": true,
                "healthy_worker_joined": healthy.worker_joined_for_test(),
                "all_released_workers_joined": blocked.iter().all(|worker| worker.worker_joined_for_test()),
                "reserved_worker_joins": TerminalHandle::reserved_worker_joins_for_test(),
                "root_pin_disarmed_after_reap": root_pin.retired,
            }))
            .unwrap(),
        )
        .unwrap();
    });
}

#[test]
fn shutdown_batch_continues_after_an_unfinished_worker() {
    const TEST: &str = "host::shutdown_tests::shutdown_batch_continues_after_an_unfinished_worker";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
    unfinished_workers_do_not_starve_healthy(&root, 1);
}

#[test]
fn shutdown_batch_two_blocked_workers_do_not_starve_healthy_cleanup() {
    const TEST: &str =
        "host::shutdown_tests::shutdown_batch_two_blocked_workers_do_not_starve_healthy_cleanup";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
    unfinished_workers_do_not_starve_healthy(&root, 2);
}

#[test]
fn shutdown_batch_observes_a_real_worker_panic_without_retaining_its_ticket() {
    const TEST: &str = "host::shutdown_tests::shutdown_batch_observes_a_real_worker_panic_without_retaining_its_ticket";
    let Some(_root) = private_runtime(TEST) else {
        return;
    };
    let baseline = TerminalHandle::reserved_worker_joins_for_test();
    assert_eq!(baseline, 0);
    let (worker, release) = TerminalHandle::spawn_with_panicking_worker_for_test();
    let mut release = ReleaseWorkers(vec![release]);
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        baseline + 1
    );
    assert!(!worker.is_completed());
    assert!(!worker.worker_joined_for_test());
    worker.request_shutdown();
    release.0.clear();
    let deadline = Instant::now() + Duration::from_secs(5);
    while TerminalHandle::reserved_worker_joins_for_test() != baseline {
        assert!(
            Instant::now() < deadline,
            "panicked worker join was not observed"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!worker.is_completed());
    assert!(
        !worker.worker_joined_for_test(),
        "a panicked join cannot be successful"
    );
    assert_eq!(
        finish_terminal_shutdown(&[worker.clone()]),
        [Err(TerminalSendError::Closed)]
    );
    assert_eq!(worker.shutdown(), Err(TerminalSendError::Closed));
    assert!(!worker.worker_joined_for_test());
    assert_eq!(TerminalHandle::reserved_worker_joins_for_test(), baseline);
    let _cached_screen = worker.snapshot();
}

#[test]
fn shutdown_batch_requires_actual_join_after_a_completed_body_blocks_in_tls() {
    const TEST: &str = "host::shutdown_tests::shutdown_batch_requires_actual_join_after_a_completed_body_blocks_in_tls";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
    let baseline = TerminalHandle::reserved_worker_joins_for_test();
    assert_eq!(baseline, 0);
    let (worker, release, entered) = TerminalHandle::spawn_with_blocked_tls_for_test();
    let mut release = ReleaseWorkers(vec![release]);
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the actual OS thread did not enter its TLS destructor");
    // Body/process publication is not an OS-thread join receipt. This control
    // has no PTY; the preceding cases independently prove real root reaping.
    assert!(worker.is_completed());
    assert!(!worker.worker_joined_for_test());
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        baseline + 1
    );
    let _cached_screen = worker.snapshot();
    let started = Instant::now();
    let unconfirmed = worker.close_checked(true);
    assert!(
        matches!(&unconfirmed, crate::terminal_ipc::TerminalCloseResult::Unknown(detail) if !detail.is_empty()),
        "a blocked TLS destructor cannot certify Close: {unconfirmed:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(worker.is_completed());
    assert!(!worker.worker_joined_for_test());
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        baseline + 1
    );
    release.0.clear();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker.worker_joined_for_test()
        || TerminalHandle::reserved_worker_joins_for_test() != baseline
    {
        assert!(
            Instant::now() < deadline,
            "released TLS thread did not actually join"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        worker.close_checked(true),
        crate::terminal_ipc::TerminalCloseResult::Closed
    );
    assert_eq!(worker.shutdown(), Ok(()));
    assert!(worker.worker_joined_for_test());
    assert_eq!(TerminalHandle::reserved_worker_joins_for_test(), baseline);
    fs::write(
        root.join("worker-evidence.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "real_tls_destructor_entered": true,
            "body_completed_before_join": true,
            "initial_close": format!("{unconfirmed:?}"),
            "final_worker_joined": worker.worker_joined_for_test(),
            "reserved_worker_joins": TerminalHandle::reserved_worker_joins_for_test(),
            "pty_scope": "none in this TLS-only control",
        }))
        .unwrap(),
    )
    .unwrap();
}
