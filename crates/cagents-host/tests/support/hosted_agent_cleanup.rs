//! Explicit public cleanup after private retained-owner contract assertions.
use cagents::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs,
    io::Read,
    path::Path,
    process::Output,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

// Private seams exercise the real cleanup loop without creating a daemon or
// depending on an OS-wide thread limit. The public API still uses cli and the
// same scoped worker creation below.
type CleanupCommand<'a> = dyn Fn(&Path, &[&str], Instant) -> Result<Value, String>
    + Sync
    + std::panic::RefUnwindSafe
    + 'a;
type CleanupJob<'scope> = Box<dyn FnOnce() -> Result<Value, String> + Send + 'scope>;

trait CleanupSpawner {
    fn spawn<'scope, 'env>(
        &self,
        scope: &'scope thread::Scope<'scope, 'env>,
        job: CleanupJob<'scope>,
    ) -> std::io::Result<thread::ScopedJoinHandle<'scope, Result<Value, String>>>
    where
        'env: 'scope;
}

struct ScopedCleanupSpawner;

impl CleanupSpawner for ScopedCleanupSpawner {
    fn spawn<'scope, 'env>(
        &self,
        scope: &'scope thread::Scope<'scope, 'env>,
        job: CleanupJob<'scope>,
    ) -> std::io::Result<thread::ScopedJoinHandle<'scope, Result<Value, String>>>
    where
        'env: 'scope,
    {
        thread::Builder::new().spawn_scoped(scope, job)
    }
}

#[derive(Default)]
pub struct Agents(Mutex<BTreeSet<String>>);

impl Agents {
    /// Accept only UUIDs returned by this fixture's matched public CLI. This
    /// avoids a truncated directory query and a large stdout pipe at 100 chats.
    pub fn observe(&self, output: &Output) {
        let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
            return;
        };
        let agent = &value["data"]["agent"];
        if value["ok"] == true
            && agent["origin"] == "User"
            && let Some(id) = agent["id"].as_str()
            && uuid::Uuid::parse_str(id).is_ok()
        {
            self.0
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id.to_owned());
        }
    }

    pub fn close(&self, root: &Path) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        self.close_with(root, deadline, &cli, &ScopedCleanupSpawner)
    }

    fn close_with(
        &self,
        root: &Path,
        deadline: Instant,
        command: &CleanupCommand<'_>,
        spawner: &impl CleanupSpawner,
    ) -> Result<(), String> {
        let ids: Vec<_> = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();
        let status = command(root, &["daemon", "status"], deadline)?;
        if status["running"] != true {
            return if ids.is_empty() {
                Ok(())
            } else {
                Err("Private daemon disappeared before exact accepted-agent cleanup".into())
            };
        }
        quiesce_automations(root, deadline, command)?;
        let mut failures = Vec::new();
        for batch in ids.chunks(4) {
            if Instant::now() >= deadline {
                failures.push("Private accepted-agent cleanup deadline expired".to_owned());
                break;
            }
            thread::scope(|scope| {
                let workers: Vec<_> = batch
                    .iter()
                    .map(|id| {
                        spawner.spawn(
                            scope,
                            Box::new(move || command(root, &["agent", "close", id], deadline)),
                        )
                    })
                    .collect();
                for (id, worker) in batch.iter().zip(workers) {
                    // Cleanup also runs while unwinding a failed assertion.
                    // A full task budget must not turn a failed worker spawn
                    // into a second panic that skips the daemon's teardown.
                    let result = match worker {
                        Ok(worker) => worker.join(),
                        Err(_) => std::panic::catch_unwind(|| {
                            command(root, &["agent", "close", id], deadline)
                        }),
                    };
                    match result {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => failures.push(format!("agent {id}: {error}")),
                        Err(_) => failures.push(format!("agent {id}: cleanup panicked")),
                    }
                }
            });
        }
        if !failures.is_empty() {
            return Err(failures.join("; "));
        }
        let remaining = command(root, &["agent", "list"], deadline)?;
        if !remaining["agents"].as_array().is_some_and(Vec::is_empty) {
            return Err(
                "Private fixture still owns open agents after exact accepted-ID cleanup".into(),
            );
        }
        Ok(())
    }
}

// Current private automation fixtures use fresh sessions. Stop future firings
// and ask the public scheduler to cancel/retire active executions before stop;
// killing the daemon alone deliberately preserves those retained owners.
fn quiesce_automations(
    root: &Path,
    deadline: Instant,
    cli: &CleanupCommand<'_>,
) -> Result<(), String> {
    let listed = cli(root, &["automation", "list", "--limit", "100"], deadline)?;
    let automations = listed["automations"]
        .as_array()
        .ok_or("Invalid public automation inventory")?;
    if automations.len() >= 100 {
        return Err("Private cleanup automation inventory exceeded its fixture bound".into());
    }
    for summary in automations {
        let id = summary["id"]
            .as_str()
            .ok_or("Private automation inventory omitted its ID")?;
        uuid::Uuid::parse_str(id)
            .map_err(|_| "Private automation inventory returned an invalid ID")?;
        let shown = cli(root, &["automation", "show", id], deadline)?;
        if shown["automation"]["enabled"] == true {
            let revision = shown["automation"]["revision"]
                .as_u64()
                .ok_or("Private automation omitted its revision")?
                .to_string();
            cli(
                root,
                &["automation", "disable", id, "--revision", &revision],
                deadline,
            )?;
        }
        let mut requested = BTreeSet::new();
        loop {
            let history = cli(
                root,
                &["automation", "history", id, "--limit", "100"],
                deadline,
            )?;
            let history: cagents::automations::HistoryPage =
                serde_json::from_value(history["history"].clone())
                    .map_err(|error| format!("Invalid public automation history: {error}"))?;
            if history.next.is_some() {
                return Err("Private cleanup history exceeded its fixture bound".into());
            }
            let active: Vec<_> = history
                .firings
                .iter()
                .filter(|firing| firing.state.is_active())
                .collect();
            if active.is_empty() {
                break;
            }
            for firing in active {
                if requested.insert(firing.id) {
                    cli(
                        root,
                        &[
                            "automation",
                            "cancel",
                            id,
                            "--firing",
                            &firing.id.to_string(),
                        ],
                        deadline,
                    )?;
                }
            }
            if Instant::now() >= deadline {
                return Err(
                    "Private automation cancellation did not settle before cleanup deadline".into(),
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}

/// The only executable is this integration target's matched CLI. All inherited
/// environment is removed before selecting the fixture profile. --no-start is
/// a public flag: cleanup cannot create a replacement daemon after a failure.
pub fn cli(root: &Path, arguments: &[&str], deadline: Instant) -> Result<Value, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err("Private cleanup command deadline expired".into());
    }
    let program = Path::new(env!("CARGO_BIN_EXE_cagents"));
    let mut spec = CommandSpec::new(program)
        .args(["--device", "local", "--no-start", "--json"])
        .args(arguments.iter().copied())
        .cwd(root)
        .timeout(remaining)
        .output_limits(128 * 1024, 64 * 1024);
    for (name, _) in std::env::vars_os() {
        spec = spec.env_remove(name);
    }
    for (name, path) in [
        ("HOME", "home"),
        ("XDG_RUNTIME_DIR", "run"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
        ("TMPDIR", "tmp"),
    ] {
        spec = spec.env(name, root.join(path));
    }
    spec = spec
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .env("TERM", "xterm-256color");
    let result = SystemProcessExecutor
        .execute(&spec, &CancellationToken::new())
        .and_then(|output| output.require_success(program))
        .map_err(|error| format!("{error}: {}", error.reported_detail()));
    let value = result
        .and_then(|output| {
            serde_json::from_slice::<Value>(&output.stdout)
                .map_err(|error| format!("Invalid public cleanup reply: {error}"))
        })
        .and_then(|value| {
            if value["ok"] == true {
                Ok(value["data"].clone())
            } else {
                Err("Public cleanup command was refused".into())
            }
        });
    // No prompts, provider history, credentials or successful result text enter
    // cleanup logs. Keep only the fixed command/UUIDs and bounded diagnostics.
    static SEQUENCE: AtomicUsize = AtomicUsize::new(0);
    let entry = serde_json::json!({"arguments":arguments,"ok":value.is_ok(),
        "error":value.as_ref().err()});
    let path = root.join(format!(
        "cleanup-{}.json",
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    if let Ok(file) = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        let _ = serde_json::to_writer(file, &entry);
    }
    value
}

pub fn stop(root: &Path) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    if cli(root, &["daemon", "status"], deadline)?["running"] != true {
        return Ok(());
    }
    let preflight = cli(root, &["daemon", "preflight"], deadline)?;
    let capability = preflight
        .get("capability")
        .ok_or("Private preflight omitted its capability")?
        .to_string();
    cli(
        root,
        &["daemon", "stop", "--confirm", &capability],
        deadline,
    )
    .map(drop)
}

pub fn diagnostics(root: &Path) {
    // These are private daemon diagnostics only. Never open the provider
    // transcript, SQLite state or account files while reporting a failure.
    for path in [
        root.join("daemon.log"),
        root.join("data")
            .join(cagents::instance::STORAGE_NAME)
            .join("cagentsd.log"),
    ] {
        if let Ok(file) = fs::File::open(&path) {
            let mut bytes = Vec::new();
            if file.take(32 * 1024).read_to_end(&mut bytes).is_ok() {
                eprintln!(
                    "private fixture {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&bytes)
                );
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{os::unix::process::ExitStatusExt, path::PathBuf, process::ExitStatus};

    struct RefuseWorkers(AtomicUsize);

    impl CleanupSpawner for RefuseWorkers {
        fn spawn<'scope, 'env>(
            &self,
            _scope: &'scope thread::Scope<'scope, 'env>,
            _job: CleanupJob<'scope>,
        ) -> std::io::Result<thread::ScopedJoinHandle<'scope, Result<Value, String>>>
        where
            'env: 'scope,
        {
            self.0.fetch_add(1, Ordering::Relaxed);
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        }
    }

    #[derive(Debug)]
    struct Call {
        root: PathBuf,
        arguments: Vec<String>,
        deadline: Instant,
    }

    fn five_accepted_agents() -> (Agents, Vec<String>) {
        let agents = Agents::default();
        let ids: Vec<_> = (1..=5)
            .map(|id| uuid::Uuid::from_u128(id).to_string())
            .collect();
        for id in &ids {
            agents.observe(&Output {
                status: ExitStatus::from_raw(0),
                stdout: serde_json::to_vec(&serde_json::json!({
                    "ok": true, "data": {"agent": {"origin": "User", "id": id}}
                }))
                .unwrap(),
                stderr: Vec::new(),
            });
        }
        (agents, ids)
    }

    fn fake_cli<'a>(
        calls: &'a Mutex<Vec<Call>>,
        error_id: Option<&'a str>,
        panic_id: Option<&'a str>,
    ) -> impl Fn(&Path, &[&str], Instant) -> Result<Value, String> + Sync + std::panic::RefUnwindSafe + 'a
    {
        move |root: &Path, arguments: &[&str], deadline: Instant| {
            // Release the trace lock before the deliberately caught panic.
            calls.lock().unwrap().push(Call {
                root: root.to_owned(),
                arguments: arguments
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
                deadline,
            });
            match arguments {
                ["daemon", "status"] => Ok(serde_json::json!({"running": true})),
                ["automation", "list", "--limit", "100"] => {
                    Ok(serde_json::json!({"automations": []}))
                }
                ["agent", "close", id] => {
                    if Some(*id) == panic_id {
                        panic!("injected inline cleanup panic");
                    }
                    if Some(*id) == error_id {
                        return Err("injected cleanup error".to_owned());
                    }
                    Ok(serde_json::json!({}))
                }
                ["agent", "list"] => Ok(serde_json::json!({"agents": []})),
                _ => panic!("unexpected cleanup command: {arguments:?}"),
            }
        }
    }

    fn assert_calls(calls: &[Call], ids: &[String], root: &Path, deadline: Instant) {
        assert!(
            calls
                .iter()
                .all(|call| call.root == root && call.deadline == deadline)
        );
        assert_eq!(calls[0].arguments, ["daemon", "status"]);
        assert_eq!(calls[1].arguments, ["automation", "list", "--limit", "100"]);
        let closed: Vec<_> = calls
            .iter()
            .filter(|call| {
                call.arguments.first().is_some_and(|value| value == "agent")
                    && call.arguments.get(1).is_some_and(|value| value == "close")
            })
            .map(|call| call.arguments[2].clone())
            .collect();
        assert_eq!(closed, ids);
    }

    #[test]
    fn spawn_refusal_closes_exact_ids_inline_with_original_deadline() {
        let (agents, ids) = five_accepted_agents();
        let root = Path::new("/not-created/private-cleanup-fixture");
        let deadline = Instant::now() + Duration::from_secs(60);
        let calls = Mutex::new(Vec::new());
        let spawner = RefuseWorkers(AtomicUsize::new(0));
        let command = fake_cli(&calls, None, None);
        assert_eq!(
            agents.close_with(root, deadline, &command, &spawner),
            Ok(())
        );
        assert_eq!(spawner.0.load(Ordering::Relaxed), 5);
        let calls = calls.lock().unwrap();
        assert_calls(&calls, &ids, root, deadline);
        assert_eq!(calls.len(), 8);
        assert_eq!(calls.last().unwrap().arguments, ["agent", "list"]);
    }

    #[test]
    fn spawn_refusal_accumulates_inline_error_and_panic() {
        let (agents, ids) = five_accepted_agents();
        let root = Path::new("/not-created/private-cleanup-fixture");
        let deadline = Instant::now() + Duration::from_secs(60);
        let calls = Mutex::new(Vec::new());
        let spawner = RefuseWorkers(AtomicUsize::new(0));
        let command = fake_cli(&calls, Some(&ids[0]), Some(&ids[1]));
        struct DuringUnwind<F: FnOnce()>(Option<F>);
        impl<F: FnOnce()> Drop for DuringUnwind<F> {
            fn drop(&mut self) {
                if let Some(cleanup) = self.0.take() {
                    cleanup();
                }
            }
        }
        let was_unwinding = Mutex::new(false);
        let result = Mutex::new(None);
        let original = std::panic::catch_unwind(|| {
            let _cleanup = DuringUnwind(Some(|| {
                *was_unwinding.lock().unwrap() = thread::panicking();
                let closed = agents.close_with(root, deadline, &command, &spawner);
                *result.lock().unwrap() = Some(closed);
            }));
            panic!("original fixture assertion");
        });
        assert_eq!(
            original.unwrap_err().downcast_ref::<&str>(),
            Some(&"original fixture assertion")
        );
        assert!(*was_unwinding.lock().unwrap());
        let error = result.into_inner().unwrap().unwrap().unwrap_err();
        assert_eq!(
            error,
            format!(
                "agent {}: injected cleanup error; agent {}: cleanup panicked",
                ids[0], ids[1]
            )
        );
        assert_eq!(spawner.0.load(Ordering::Relaxed), 5);
        let calls = calls.lock().unwrap();
        assert_calls(&calls, &ids, root, deadline);
        assert_eq!(calls.len(), 7);
        assert!(!calls.iter().any(|call| call.arguments == ["agent", "list"]));
    }
}
