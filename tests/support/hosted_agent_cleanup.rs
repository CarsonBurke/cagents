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
        let ids: Vec<_> = self
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .cloned()
            .collect();
        let status = cli(root, &["daemon", "status"], deadline)?;
        if status["running"] != true {
            return if ids.is_empty() {
                Ok(())
            } else {
                Err("Private daemon disappeared before exact accepted-agent cleanup".into())
            };
        }
        quiesce_automations(root, deadline)?;
        let mut failures = Vec::new();
        for batch in ids.chunks(4) {
            if Instant::now() >= deadline {
                failures.push("Private accepted-agent cleanup deadline expired".to_owned());
                break;
            }
            thread::scope(|scope| {
                let workers: Vec<_> = batch
                    .iter()
                    .map(|id| scope.spawn(move || cli(root, &["agent", "close", id], deadline)))
                    .collect();
                for (id, worker) in batch.iter().zip(workers) {
                    match worker.join() {
                        Ok(Ok(_)) => {}
                        Ok(Err(error)) => failures.push(format!("agent {id}: {error}")),
                        Err(_) => failures.push(format!("agent {id}: cleanup worker panicked")),
                    }
                }
            });
        }
        if !failures.is_empty() {
            return Err(failures.join("; "));
        }
        let remaining = cli(root, &["agent", "list"], deadline)?;
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
fn quiesce_automations(root: &Path, deadline: Instant) -> Result<(), String> {
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
