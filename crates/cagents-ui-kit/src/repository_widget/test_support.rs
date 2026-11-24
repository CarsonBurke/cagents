//! Fixtures the application's tests share with this module's.

use super::*;
use cosmic::iced::futures::StreamExt;

/// Re-executes exactly one owning test with private filesystem identity. No
/// environment variable is changed in the parallel parent test process.
pub fn isolated_test(name: &str) -> bool {
    isolated_test_inner(name, false)
}

pub fn isolated_test_inner(name: &str, ignored: bool) -> bool {
    const MARKER: &str = "CAGENTS_ROOTED_SAVE_TEST_CHILD";
    const RECEIPT: &str = "CAGENTS_ROOTED_SAVE_TEST_RECEIPT";
    if std::env::var(MARKER).as_deref() == Ok(name) {
        std::fs::write(
            std::env::var_os(RECEIPT).expect("isolated child receipt"),
            name,
        )
        .unwrap();
        return false;
    }
    let directory = tempfile::tempdir().unwrap();
    let receipt = directory.path().join("entered-test");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .arg(name)
        .arg("--exact")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(MARKER, name)
        .env(RECEIPT, &receipt);
    if ignored {
        command.arg("--ignored");
    }
    for (key, leaf) in [
        ("HOME", "home"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_STATE_HOME", "state"),
        ("XDG_DATA_HOME", "data"),
        ("XDG_CONFIG_HOME", "config"),
        ("XDG_CACHE_HOME", "cache"),
    ] {
        let path = directory.path().join(leaf);
        std::fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        command.env(key, path);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "isolated {name} failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(receipt).unwrap(),
        name,
        "the exact owning test must have executed"
    );
    true
}

#[derive(Debug)]
pub struct LoadCompletion {
    pub action: &'static str,
    pub accepted: bool,
    pub stale: bool,
    pub loading: bool,
    pub reading: bool,
    pub attested: bool,
    pub followup_units: usize,
}

#[derive(Debug, Default)]
pub struct TaskTrace {
    pub reads: Vec<LoadCompletion>,
    pub details: Vec<String>,
}

impl TaskTrace {
    /// Whether an actual file-worker completion for the selected document
    /// reached the runtime.
    pub fn answered(&self) -> bool {
        self.reads.iter().any(|read| read.accepted)
    }
}

pub fn load_state(runtime: &RepositoryRuntime) -> String {
    let document = match &runtime.document {
        DocumentState::None => "None".to_owned(),
        DocumentState::Loading { predecessor } => {
            format!("Loading(predecessor={})", predecessor.is_some())
        }
        DocumentState::Editable(_) => "Editable".to_owned(),
        DocumentState::Image { .. } => "Image".to_owned(),
        DocumentState::Pdf { .. } => "Pdf".to_owned(),
        DocumentState::ReadOnly { path, reason } => format!("ReadOnly({path:?}, {reason:?})"),
        DocumentState::Failed { path, message } => format!("Failed({path:?}, {message:?})"),
    };
    format!(
        "document={document}; reading={}; reload_pending={}; attested={}; watch={:?}",
        runtime.document_reading,
        runtime.document_reload_pending,
        runtime.original_read_attested,
        runtime
            .document_watch
            .as_ref()
            .map(|watch| (watch.id(), watch.revision(), watch.error()))
    )
}

/// How long [`finish`] waits before calling the tasks hung. It only guards
/// against a hang; the action limit catches a task that keeps rescheduling
/// itself. The work it waits for can take iced's process-wide font lock,
/// which other tests in the lane hold for up to seconds each while they shape
/// long lines in the unoptimized test profile. In a full `test:ui` run a diff
/// rebuild has taken 8 s, 3.4 s of it in a single wait for that lock; the
/// whole test takes about 0.1 s on its own.
pub const SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

pub async fn finish(runtime: &mut RepositoryRuntime, task: Task<RepositoryAction>) -> TaskTrace {
    let mut trace = TaskTrace::default();
    let settled = tokio::time::timeout(SETTLE_LIMIT, async {
        let mut tasks = vec![task];
        let mut actions = 0;
        while let Some(task) = tasks.pop() {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                // initial_load batches listing and reading. Preserve the whole
                // original stream before driving any queued replacement read.
                while let Some(action) = stream.next().await {
                    actions += 1;
                    assert!(
                        actions <= 256,
                        "repository task action limit; trace={:?}; {}",
                        trace.details,
                        load_state(runtime)
                    );
                    if let iced::runtime::Action::Output(action) = action {
                        let read = match &action {
                            RepositoryAction::FileLoadedAt { identity, result }
                            | RepositoryAction::DocumentReloaded { identity, result } => {
                                let kind = if matches!(&action, RepositoryAction::FileLoadedAt { .. }) {
                                    "FileLoadedAt"
                                } else {
                                    "DocumentReloaded"
                                };
                                let accepted = runtime.accepts_selected(identity);
                                let revision = runtime.document_watch.as_ref().map(DocumentWatch::revision);
                                let read_revision = result.as_ref().ok().and_then(|read| read.watch_revision);
                                let stale = read_revision.zip(revision).is_some_and(|(read, now)| read != now);
                                // These fixtures read their own synthetic files.
                                // Preserve typed outer/inner errors before update.
                                trace.details.push(format!(
                                    "{kind}; path={:?}; identity={identity:?}; accepted={accepted}; result={result:?}; before={}",
                                    identity.worktree.join(&identity.path),
                                    load_state(runtime)
                                ));
                                Some((kind, accepted, stale))
                            }
                            _ => None,
                        };
                        let followup = runtime.update(action);
                        let mut reread = false;
                        if let Some((kind, accepted, stale)) = read {
                            let completion = LoadCompletion {
                                action: kind,
                                accepted,
                                stale,
                                loading: matches!(runtime.document, DocumentState::Loading { .. }),
                                reading: runtime.document_reading,
                                attested: runtime.original_read_attested,
                                followup_units: followup.units(),
                            };
                            reread = accepted && completion.loading && completion.reading;
                            trace.details.push(format!(
                                "{kind}; after={}; followup_units={}",
                                load_state(runtime),
                                completion.followup_units
                            ));
                            trace.reads.push(completion);
                        }
                        if reread {
                            assert!(
                                followup.units() > 0,
                                "accepted read is loading without a reread; trace={:?}",
                                trace.details
                            );
                            tasks.push(followup);
                        } else {
                            // Save, directory refresh, focus/scroll and other
                            // terminal followups keep their original explicit
                            // drivers. In particular, never list Root `/` here.
                            drop(followup);
                        }
                    }
                }
            }
        }
    })
    .await;
    assert!(
        settled.is_ok(),
        "repository tasks did not settle within {SETTLE_LIMIT:?}; trace={:?}; {}",
        trace.details,
        load_state(runtime)
    );
    trace
}
