//! Real execed holder/provider and kernel daemon-lease qualification. This
//! isolates HOME/runtime/data; native UI qualification separately kills cagentsd.
use super::*;
use crate::{
    agent_execution::{
        Authority, Destination, Origin, Spec, State, Submit, Surface, Target as ExecutionTarget,
    },
    model::{HostIncarnation, HostSessionId, ProjectKind},
    workspace_model::ModelEdit,
};
use std::{fs, os::unix::fs::PermissionsExt, path::Path};

const TEST: &str = "host::chat::retained_actor_tests::slow_turn_keeps_exact_provider_and_frames_across_daemon_lease_rotation";
const MODE: &str = "CAGENTS_CLAUDE_HOLDER_TEST";

pub(super) fn retained_helper_executable() -> std::path::PathBuf {
    let executable = std::path::PathBuf::from(
        std::env::var_os("CAGENTS_TEST_RETAINED_ACTOR_HELPER")
            .expect("real retained-actor fixtures require the same-head cagentsd executable"),
    );
    assert!(
        executable.is_absolute(),
        "retained-actor helper path must be absolute"
    );
    executable
}

#[test]
fn slow_turn_keeps_exact_provider_and_frames_across_daemon_lease_rotation() {
    let _helper_executable = retained_helper_executable();
    if std::env::var(MODE).as_deref() != Ok("fixture") {
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        for path in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "project",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_helper_executable(),
            )
            .env(MODE, "fixture")
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root)
            .env("HOME", root.join("home"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated retained actor qualification failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    install_provider(&root.join("bin/claude"));
    // Only this lease-rotation fixture opts into the third-frame barrier.
    fs::write(root.join("lease-third-frame-gate"), b"hold").unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |incarnation| HostSessionProof {
        host_session_id: HostSessionId::new("retained-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(incarnation),
    };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let path = root.join("registry.sqlite3");
    let mut registry = super::super::model::Registry::at(&path).unwrap();
    registry
        .apply(&ModelEdit::OpenProject {
            repository: root.join("project"),
            name: "Retained actor fixture".into(),
            kind: ProjectKind::Folder,
        })
        .unwrap();
    let accepted = registry
        .submit_agent(
            &Origin::User,
            &Submit {
                name: None,
                destination: Destination::New(Spec {
                    provider: Provider::Claude,
                    account: Some(crate::account::Account::system(Provider::Claude).id()),
                    target: ExecutionTarget {
                        repository: root.join("project"),
                        directory: root.join("project"),
                        placement: None,
                    },
                    surface: Surface::Chat,
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "One exact slow turn".into(),
                idempotency_key: "one-slow-turn".into(),
            },
            1,
        )
        .unwrap();
    let mut state = HostState::new(first.clone());
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(notifications.handle());
    let source = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = source.id;
    let context =
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap());
    start_execution(&host, 0, source, context).unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle
        .actor_identity()
        .expect("the test opts into a real retained actor");
    let active_proof = Arc::new(std::sync::Mutex::new(first.clone()));
    struct Cleanup {
        owner: crate::daemon::holder_registry::Record,
        pair: Option<crate::daemon::holder_registry::Record>,
        proof: Arc<std::sync::Mutex<HostSessionProof>>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let proof = self.proof.lock().unwrap().clone();
            if let Ok(inspection) = Handle::inspect_remote(&self.owner, proof.clone())
                && let Ok(handle) =
                    Handle::adopt_remote(&self.owner, inspection.source, proof, |_| {})
                && handle.close().is_ok()
                && handle.finish_retained_close().is_ok()
                && self
                    .pair
                    .as_ref()
                    .is_none_or(|pair| !pair.root.is_current() && !pair.holder.is_current())
            {
                return;
            }
            // Only this fixture's exact process incarnations; no name/group sweep.
            for owner in std::iter::once(&self.owner).chain(self.pair.iter()) {
                for process in [&owner.root, &owner.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
    let mut cleanup = Cleanup {
        owner: identity.record.clone(),
        pair: None,
        proof: active_proof.clone(),
    };
    // Fail the independent notification transaction after the execution cursor
    // can commit. Adoption must replay that attention before acknowledging it.
    let attention_path = crate::instance::project_dirs()
        .unwrap()
        .data_dir()
        .join("agent-attention.sqlite3");
    wait_until(
        || {
            attention_path.exists()
                && rusqlite::Connection::open(&attention_path).is_ok_and(|db| {
                    db.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='seen')",
                [], |row| row.get::<_, bool>(0),
            ).unwrap_or(false)
                })
        },
        "notification journal did not initialize",
    );
    let attention_fault = rusqlite::Connection::open(&attention_path).unwrap();
    attention_fault.execute_batch("CREATE TRIGGER reject_retained_attention BEFORE INSERT ON seen BEGIN SELECT RAISE(ABORT, 'fixture attention commit failure'); END;").unwrap();
    // A real holder-owned thin console shares this native actor's exact ID.
    // The raw terminal record will retain RelaunchAvailable on later adoption;
    // recovery must project the authenticated current worker, not trust that raw
    // saved continuity or replace its PTY process.
    host.lock().unwrap().enable_holder_recovery();
    let console_recipe = crate::model::LaunchRecipe {
        program: "/bin/cat".into(),
        arguments: Vec::new(),
        working_directory: root.join("project"),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    host.lock()
        .unwrap()
        .chats
        .get_mut(&id)
        .unwrap()
        .console_recipe = Some(console_recipe.clone());
    let terminal =
        crate::terminal_ipc::TerminalDescriptor::relaunchable(id, console_recipe.clone(), None)
            .session;
    let (descriptor, console) = super::super::state::spawn_terminal_view_owned(
        &host,
        0,
        terminal,
        crate::terminal_ipc::TerminalSpawnOptions::default(),
        None,
        true,
    )
    .unwrap();
    let pair = console
        .holder_identity()
        .expect("fixture requires actual terminal holder")
        .clone();
    cleanup.pair = Some(pair.clone());
    let mut paired_source = handle
        .actor_identity()
        .and_then(|owner| Handle::inspect_remote(&owner.record, first.clone()).ok())
        .unwrap()
        .source;
    paired_source.terminal = Some(descriptor.session.clone());
    handle
        .update_native_source(
            paired_source.clone(),
            Some(console_recipe.clone()),
            Some(pair.clone()),
        )
        .unwrap();
    {
        let mut state = host.lock().unwrap();
        let record = state.chats.get_mut(&id).unwrap();
        record.session = paired_source;
        record.paired_terminal = Some(console.clone());
    }
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let hosted_source = host.lock().unwrap().chats[&id].session.clone();
    registry
        .lock()
        .unwrap()
        .bind_agent(
            accepted.agent.id,
            None,
            crate::agent_execution::HostedSession::Chat(hosted_source.clone()),
        )
        .unwrap();
    handle
        .command_for_run(
            accepted.run.id,
            chat::Command::Prompt {
                text: "One exact slow turn".into(),
                attachments: Vec::new(),
                mode: chat::PromptMode::Send,
            },
        )
        .unwrap();
    wait_until_with_diagnostic(
        || {
            fs::read_to_string(root.join("produced"))
                .unwrap_or_default()
                .lines()
                .count()
                >= 3
        },
        "provider never began its slow turn",
        || slow_turn_diagnostic(&root, &handle),
    );
    assert_eq!(
        fs::read_to_string(root.join("produced"))
            .unwrap()
            .lines()
            .count(),
        3,
        "the opt-in provider gate must hold after exactly the third frame",
    );
    // Provider writes and HOST semantic commits are independent. Hold the
    // actual turn open while awaiting the real transactional run/cursor pair;
    // neither this predicate nor its polling drives the semantic broker.
    let commit_deadline = Instant::now() + Duration::from_secs(15);
    wait_until_before_with_diagnostic(
        commit_deadline,
        || {
            let registry = registry.lock().unwrap();
            registry.agent_run(accepted.run.id).unwrap().state == State::Running
                && registry
                    .native_actor_cursor_for_test(identity.instance)
                    .unwrap()
                    .is_some_and(|sequence| sequence > 0)
        },
        "provider began but its Running state and semantic cursor did not commit",
        || {
            running_commit_diagnostic(
                &root,
                &host,
                &handle,
                &registry,
                &identity,
                accepted.run.id,
                &first,
            )
        },
    );
    assert_eq!(
        registry
            .lock()
            .unwrap()
            .agent_run(accepted.run.id)
            .unwrap()
            .state,
        State::Running
    );
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_cursor_for_test(identity.instance)
            .unwrap()
            .is_some_and(|sequence| sequence > 0)
    );
    // Invalid hybrids must still be refused by the actual current helper;
    // recovery must project a genuine issued descriptor rather than relax it.
    let inspection = Handle::inspect_remote(&identity.record, first.clone()).unwrap();
    let mut invalid = inspection.source.clone();
    invalid.launch.session_id = handle.confirmed_session_id();
    assert!(invalid.launch.session_id.is_some());
    invalid.native_continuity = chat::NativeContinuity::ReconnectOnly;
    let refusal = handle
        .update_native_source(invalid, Some(console_recipe.clone()), Some(pair.clone()))
        .unwrap_err();
    assert!(refusal.contains("issued recipe identity"), "{refusal}");
    let mut wrong_uuid = inspection.source;
    wrong_uuid.native_continuity = handle.native_continuity();
    wrong_uuid.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
    assert!(
        handle
            .update_native_source(wrong_uuid, Some(console_recipe.clone()), Some(pair.clone()))
            .is_err()
    );
    let old_host = Arc::downgrade(&host);
    let old_proxy_alive = handle.resource_lifetime();
    console.preserve_for_restart().unwrap();
    drop(console);
    drop(handle);
    drop(notifications);
    drop(host);
    drop(lease.take());
    wait_until(
        || old_host.strong_count() == 0 && !old_proxy_alive(),
        "the original HOST/proxy did not detach before provider release",
    );
    fs::write(root.join("lease-third-frame-release"), b"continue").unwrap();
    // The actual reducer, accepted command UUID, Store and protocol pipes stay
    // in the same helper while there is no daemon lease or attached proxy.
    wait_until(
        || root.join("provider-done").exists(),
        "provider did not complete while detached",
    );
    assert!(
        identity.record.root.is_current(),
        "the original provider must still own its stream"
    );
    let next = proof("second");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
    *active_proof.lock().unwrap() = next.clone();
    let mut state = HostState::new(next);
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    let _notifications = super::super::notifications::Broker::start(
        &successor,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    successor.lock().unwrap().notifications = Some(_notifications.handle());
    super::super::state::restore_orphaned_holders(&successor).unwrap();
    assert!(
        matches!(
            successor.lock().unwrap().terminals[&id].session.continuity,
            crate::model::TerminalContinuity::RelaunchAvailable { .. }
        ),
        "raw restored console intentionally remains unpublished until native binding"
    );
    // One real restore attempt exposes its exact refusal in the surviving raw
    // panic log; orphan discovery otherwise logs and intentionally skips errors.
    super::native_recovery::restore(&successor, 0, &hosted_source).unwrap();
    {
        let state = successor.lock().unwrap();
        let record = &state.chats[&id];
        assert_eq!(state.terminals[&id].handle.holder_identity(), Some(&pair));
        assert!(
            pair.root.is_current(),
            "the exact original console process must survive"
        );
        assert_eq!(
            record.session.terminal.as_ref(),
            Some(&state.native_terminal_projection(id).unwrap())
        );
        assert!(
            record
                .paired_terminal
                .as_ref()
                .unwrap()
                .same_worker(&state.terminals[&id].handle)
        );
    }
    let handle = successor.lock().unwrap().chats[&id].handle.clone();
    assert_eq!(
        handle.actor_identity(),
        Some(identity.clone()),
        "adoption must retain both helper and original provider incarnations"
    );
    let retained = retained_actor::retained_runs(&successor);
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    let mut replayed_evidence = None;
    wait_until(
        || {
            let page = handle.uncommitted_outcomes().unwrap();
            let Some(entry) = page.iter().find(|entry| {
                matches!(
                    &entry.update,
                    Update::Attention(_) | Update::AttentionContext { .. }
                )
            }) else {
                return false;
            };
            let committed = registry
                .lock()
                .unwrap()
                .native_actor_cursor_for_test(identity.instance)
                .unwrap()
                .is_some_and(|cursor| cursor >= entry.sequence);
            if committed {
                let evidence = match &entry.update {
                    Update::Attention(evidence) | Update::AttentionContext { evidence, .. } => {
                        evidence
                    }
                    _ => unreachable!(),
                };
                replayed_evidence = Some(serde_json::to_string(evidence).unwrap());
            }
            committed
        },
        "the failed notification write must retain attention despite a committed registry cursor",
    );
    attention_fault
        .execute_batch("DROP TRIGGER reject_retained_attention;")
        .unwrap();
    drop(attention_fault);
    registry
        .lock()
        .unwrap()
        .interrupt_unproven_agent_runs_except(2, &retained)
        .unwrap();
    wait_until(
        || {
            registry
                .lock()
                .unwrap()
                .agent_run(accepted.run.id)
                .unwrap()
                .state
                == State::Succeeded
        },
        "retained final outcome did not commit as Succeeded",
    );
    let run = registry.lock().unwrap().agent_run(accepted.run.id).unwrap();
    for index in 0..40 {
        assert_eq!(
            run.result
                .text
                .matches(&format!("FRAME_{index:03}|"))
                .count(),
            1,
            "missing or duplicate frame {index}"
        );
    }
    handle.set_visible(true);
    wait_until(
        || {
            handle
                .snapshot(None)
                .is_some_and(|snapshot| snapshot.items.len() >= 40)
        },
        "retained transcript failed to hydrate",
    );
    let snapshot = handle.snapshot(None).unwrap();
    let transcript = snapshot
        .items
        .iter()
        .filter_map(|item| match &item.content {
            chat::ItemContent::Text {
                text,
                thinking: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..40 {
        assert_eq!(transcript.matches(&format!("FRAME_{index:03}|")).count(), 1);
    }
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1,
        "adoption must never resend the accepted prompt"
    );
    wait_until(
        || handle.uncommitted_outcomes().unwrap().is_empty(),
        "all semantic stores must commit before helper ACK",
    );
    let attention = rusqlite::Connection::open(&attention_path).unwrap();
    let committed: u64 = attention
        .query_row(
            "SELECT count(*) FROM seen WHERE evidence=?1",
            [replayed_evidence.unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        committed, 1,
        "the retained attention must commit once before helper ACK"
    );
    super::super::agent_execution::process(
        &successor,
        Authority::default(),
        crate::agent_execution::Operation::Close(accepted.agent.id),
    )
    .unwrap();
    wait_until_with_diagnostic(
        || {
            !identity.record.holder.is_current()
                && !identity.record.root.is_current()
                && !pair.holder.is_current()
                && !pair.root.is_current()
        },
        "explicit close left a native or paired owner",
        || close_owner_diagnostic(&identity.record, &pair),
    );
    drop(lease);
}

// Failure-only inspection of the four process incarnations admitted by this
// fixture. No discovery, RPC, signal, wait, account read or ownership mutation.
fn close_process_diagnostic(
    role: &str,
    expected: &crate::daemon::holder_registry::Process,
    custody_contract: &str,
) -> serde_json::Value {
    use std::io::Read;
    let observed = fs::File::open(format!("/proc/{}/stat", expected.pid))
        .and_then(|file| {
            let mut bytes = Vec::new();
            file.take(4096).read_to_end(&mut bytes)?;
            Ok(bytes)
        })
        .map(|bytes| {
            let text = String::from_utf8_lossy(&bytes);
            let fields = text
                .rsplit_once(')')
                .map(|(_, tail)| tail.split_whitespace().collect::<Vec<_>>())
                .unwrap_or_default();
            let start =
                crate::model::agent_recovery::process_identity(&bytes).map(|(_, start)| start);
            serde_json::json!({
                "state": fields.first().copied(),
                "parent_pid": fields.get(1).and_then(|value| value.parse::<i32>().ok()),
                "observed_start": start,
                "matches_admitted_start": start == Some(expected.start),
            })
        })
        .unwrap_or_else(|error| serde_json::json!({"read_error": error.to_string()}));
    serde_json::json!({
        "role": role,
        "admitted_pid": expected.pid,
        "admitted_start": expected.start,
        "observed_stat": observed,
        "source_custody_contract_not_a_live_child_handle_observation": custody_contract,
    })
}

fn close_owner_diagnostic(
    native: &crate::daemon::holder_registry::Record,
    paired: &crate::daemon::holder_registry::Record,
) -> String {
    serde_json::json!({
        "fixture_pid": std::process::id(),
        "scope": "One bounded stat read per admitted role after the unchanged close deadline; no RPC, signal, wait or scan. Missing or mismatched identities are reported without reuse as authority.",
        "roles": [
            close_process_diagnostic("native helper", &native.holder,
                "The original helper Child was assigned to the cagents-actor-owner wait thread in this fixture process."),
            close_process_diagnostic("native provider", &native.root,
                "The retained helper runtime owns the provider Child."),
            close_process_diagnostic("paired holder", &paired.holder,
                "This fixture process spawned the original terminal Child, then detached its Holder. The adopted Holder has no original Child; this describes source custody, not a newly observed wait handle."),
            close_process_diagnostic("paired root", &paired.root,
                "The paired holder owns its PTY root Child; successful close receipt requires that exact root reaped."),
        ],
    }).to_string()
}

fn sigkill_retirement_diagnostic(
    identity: &chat::runtime::ActorIdentity,
    proof: &HostSessionProof,
) -> String {
    let error_category = |mut error: String| {
        if error.len() > 512 {
            let mut end = 512;
            while !error.is_char_boundary(end) {
                end -= 1;
            }
            error.truncate(end);
        }
        serde_json::json!({ "kind": "Err", "error": error })
    };
    // Existing typed read-only evidence readers: no hydration, helper RPC,
    // refresh, retry, ACK or change to retirement authority.
    let retired = match Handle::retired_actor_receipt(&identity.record, proof) {
        Ok(Some(_)) => serde_json::json!({ "kind": "Some" }),
        Ok(None) => serde_json::json!({ "kind": "None" }),
        Err(error) => error_category(error),
    };
    let pending = match crate::daemon::holder_registry::retirement_pending(&identity.record) {
        Ok(value) => serde_json::json!({ "kind": "Ok", "pending": value }),
        Err(error) => error_category(error.to_string()),
    };
    serde_json::json!({
        "successor_pid": std::process::id(),
        "successor_parent_pid": unsafe { libc::getppid() },
        "scope": "Failure-only bounded exact-role observations after owning Close returned its original error; existing deadlines are unchanged. No RPC, signal, wait, retry, scan or raw receipt contents. These categories grant no cleanup authority and cannot distinguish every None cause.",
        "typed_retired_receipt": retired,
        "diagnostic_only_discovery_retirement_pending": pending,
        "roles": [
            close_process_diagnostic("native helper", &identity.record.holder,
                "The original helper Child and cagents-actor-owner wait thread belonged to the killed source process. This successor has no original Child; the observed parent is reported separately."),
            close_process_diagnostic("native provider", &identity.record.root,
                "The retained helper owns the provider Child and its descendant reaping; observed state and parent are separate evidence."),
        ],
    }).to_string()
}

fn wait_until(mut condition: impl FnMut() -> bool, reason: &str) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "{reason}");
        thread::sleep(Duration::from_millis(20));
    }
}

// Capture only cached metadata and bounded private fixture files. In particular,
// a timeout must not initiate an RPC or read transcript/prompt contents.
fn slow_turn_diagnostic(root: &Path, handle: &Handle) -> String {
    use std::io::Read;
    let state = handle.attachment_snapshot();
    let pending = handle.pending_retained_inputs();
    let read = |name: &str, limit: u64| {
        fs::File::open(root.join(name))
            .and_then(|file| {
                let mut bytes = Vec::new();
                file.take(limit).read_to_end(&mut bytes)?;
                Ok(bytes)
            })
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    };
    let count = |name: &str| read(name, 4096).map(|text| text.lines().count());
    let error = state
        .error
        .as_deref()
        .map(|text| &text[..text.floor_char_boundary(2048)]);
    format!(
        "cached actor: generation={} ready={} busy={} waiting={} exited={} requests={} failure={error:?}; pending input count={} first sequences={:?}; prompts(lines, first 4096 bytes)={:?}; produced(lines, first 4096 bytes)={:?}; provider trace(first 8192 bytes)={:?}",
        state.generation,
        state.ready,
        state.busy,
        state.waiting,
        state.exited,
        state.requests.len(),
        pending.len(),
        pending
            .iter()
            .take(16)
            .map(|input| input.sequence)
            .collect::<Vec<_>>(),
        count("prompts"),
        count("produced"),
        read("provider-trace.jsonl", 8192),
    )
}

// Failure-only typed reads: do not acknowledge, adopt, refresh the registry,
// or otherwise substitute for the product's independent outcome broker.
fn running_commit_diagnostic(
    root: &Path,
    host: &SharedHost,
    handle: &Handle,
    registry: &super::super::agent_execution::SharedRegistry,
    identity: &chat::runtime::ActorIdentity,
    run: crate::model::RunId,
    proof: &HostSessionProof,
) -> String {
    let bounded_error = |error: String| error[..error.floor_char_boundary(512)].to_owned();
    let (state, cursor) = {
        let registry = registry.lock().unwrap();
        (
            registry
                .agent_run(run)
                .map(|run| run.state)
                .map_err(&bounded_error),
            registry
                .native_actor_cursor_for_test(identity.instance)
                .map_err(&bounded_error),
        )
    };
    let expected_source = host
        .lock()
        .unwrap()
        .chats
        .get(&identity.record.terminal)
        .map(|record| record.session.clone());
    let source = expected_source.map(|source| {
        (
            source.id,
            source.launch.session_id,
            source.proof.as_ref() == Some(proof),
        )
    });
    // The absolute probe deadline has expired. Never create a new RPC budget
    // here: helper journal/source/inspection/adoption evidence stays unobserved.
    format!(
        "run={run} state={state:?} cursor={cursor:?}; cached HOST source(id, session, proof matches fixture)={source:?}; helper ACK/latest/source/inspection/adoption=unobserved (deadline exhausted; no RPC); {}",
        slow_turn_diagnostic(root, handle),
    )
}

fn wait_until_with_diagnostic(
    condition: impl FnMut() -> bool,
    reason: &str,
    diagnostic: impl FnOnce() -> String,
) {
    wait_until_before_with_diagnostic(
        Instant::now() + Duration::from_secs(15),
        condition,
        reason,
        diagnostic,
    );
}

fn wait_until_before_with_diagnostic(
    deadline: Instant,
    mut condition: impl FnMut() -> bool,
    reason: &str,
    diagnostic: impl FnOnce() -> String,
) {
    loop {
        if Instant::now() >= deadline {
            panic!("{reason}: {}", diagnostic());
        }
        let satisfied = condition();
        if Instant::now() >= deadline {
            panic!("{reason}: {}", diagnostic());
        }
        if satisfied {
            return;
        }
        thread::sleep(
            Duration::from_millis(20).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

pub(super) fn install_provider(path: &Path) {
    fs::write(path, r#"#!/usr/bin/python3
import json, os, pathlib, sys, time
scriptroot=pathlib.Path(__file__).resolve().parent.parent
tracepath=scriptroot/'provider-trace.jsonl'
def trace(value):
    # Fixed allowlist below; never record prompt bodies, argv, or the environment.
    data=(json.dumps(value,ensure_ascii=True)[:2047]+'\n').encode()
    try:
        with tracepath.open('ab') as log:
            if log.tell()+len(data)<=8192: log.write(data)
    except OSError:
        pass  # Diagnostic I/O must not change the provider's protocol behavior.
trace({'event':'bootstrap','HOME':os.environ.get('HOME'),'CLAUDE_CONFIG_DIR':os.environ.get('CLAUDE_CONFIG_DIR'),'cwd':os.getcwd(),'scriptroot':str(scriptroot)})
root=pathlib.Path(os.environ['HOME']).parent
trace({'event':'marker-root','markerroot':str(root)})
session=next((arg.split('=',1)[1] for arg in sys.argv[1:] if arg.startswith('--session-id=')), 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa')
def send(value): print(json.dumps(value),flush=True)
for line in sys.stdin:
    frame=json.loads(line)
    trace({'event':'input','type':str(frame.get('type'))[:96],'uuid':str(frame.get('uuid'))[:96]})
    if frame.get('type')=='control_request':
        send({'type':'control_response','response':{'subtype':'success','request_id':frame['request_id'],'response':{}}})
    elif frame.get('type')=='user':
        with (root/'prompts').open('a') as log: log.write('prompt\n')
        received=frame['uuid']
        send({'type':'user','uuid':received,'session_id':session,'parent_tool_use_id':None,'message':{'id':'native-'+received,'role':'user','content':[{'type':'text','text':'One exact slow turn'}]}})
        send({'type':'command_lifecycle','command_uuid':received,'state':'started','session_id':session,'uuid':'lifecycle-'+received})
        for index in range(40):
            send({'type':'assistant','uuid':f'frame-{index}','message':{'id':f'frame-{index}','role':'assistant','content':[{'type':'text','text':f'FRAME_{index:03}|'}]}})
            with (root/'produced').open('a') as log: log.write(str(index)+'\n')
            gate=root/'lease-third-frame-gate'
            if index==2 and gate.exists():
                deadline=time.monotonic()+45
                while gate.exists() and not (root/'lease-third-frame-release').exists():
                    if time.monotonic()>=deadline:
                        trace({'event':'third-frame-gate-timeout'})
                        raise RuntimeError('private fixture third-frame gate timed out')
                    time.sleep(0.02)
            time.sleep(0.04)
        send({'type':'result','uuid':'final','subtype':'success','is_error':False,'session_id':session,'usage':{'input_tokens':7,'output_tokens':40}})
        (root/'provider-done').write_text('done')
"#).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

/// This boundary kills a separate host owner process with SIGKILL. Unlike a
/// lease-rotation-only fixture, no destructor or old proxy can help recovery.
#[test]
fn slow_turn_survives_sigkill_of_the_host_owner_process() {
    let _helper_executable = retained_helper_executable();
    const EXACT: &str =
        "host::chat::retained_actor_tests::slow_turn_survives_sigkill_of_the_host_owner_process";
    const ROLE: &str = "CAGENTS_RETAINED_HOST_PROCESS_ROLE";
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Receipt {
        identity: chat::runtime::ActorIdentity,
        agent: crate::model::AgentId,
        run: crate::model::RunId,
        session: TerminalId,
    }
    fn child(root: &Path, role: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", EXACT, "--nocapture"])
            .env_clear()
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_helper_executable(),
            )
            .env(ROLE, role)
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root)
            .env("HOME", root.join("home"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            );
        command
    }
    if std::env::var_os(ROLE).is_none() {
        use std::os::unix::process::ExitStatusExt;
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        for path in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "project",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        install_provider(&root.join("bin/claude"));
        // Cleanup uses a separate private host lease; the parallel test runner
        // never changes its own HOME/runtime or acquires the production lease.
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = child(&self.0, "cleanup").output();
            }
        }
        let _cleanup = Cleanup(root.to_owned());
        struct SourceOwner(std::process::Child);
        impl Drop for SourceOwner {
            fn drop(&mut self) {
                if self.0.try_wait().is_ok_and(|status| status.is_none()) {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let mut owner = SourceOwner(
            child(root, "source")
                .stdout(fs::File::create(root.join("source.stdout")).unwrap())
                .stderr(fs::File::create(root.join("source.stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
        let owner_identity =
            crate::daemon::holder_registry::Process::read(owner.0.id() as i32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.join("owner-receipt.json").exists() {
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "source host failed: {}",
                fs::read_to_string(root.join("source.stderr")).unwrap_or_default()
            );
            assert!(
                Instant::now() < deadline,
                "source host never published its exact owner receipt"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(owner_identity.is_current());
        owner.0.kill().unwrap();
        assert_eq!(owner.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(
            !owner_identity.is_current(),
            "the old host owner must actually be gone"
        );
        wait_until(
            || root.join("provider-done").exists(),
            "provider did not finish while no host owner existed",
        );
        let output = child(root, "successor").output().unwrap();
        assert!(
            output.status.success(),
            "successor failed (coordinator_pid={}, source_pid={}):\n{}\n{}",
            std::process::id(),
            owner.0.id(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    let role = std::env::var(ROLE).unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("sigkill-host-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(&role),
    };
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    if role == "cleanup" {
        // Only exact holder records inside this fixture's private namespace.
        let directory =
            crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Native)
                .unwrap();
        if let Ok(records) = crate::daemon::holder_registry::discover(
            &directory,
            &proof.boot_id,
            crate::daemon::holder_registry::Kind::Native,
        ) {
            for record in records {
                if let Ok(inspection) = Handle::inspect_remote(&record, proof.clone())
                    && let Ok(handle) =
                        Handle::adopt_remote(&record, inspection.source, proof.clone(), |_| {})
                    && handle.close().is_ok()
                    && handle.finish_retained_close().is_ok()
                {
                    continue;
                }
                // On a failing assertion there may be uncommitted outcomes;
                // never misreport that as semantic retirement. Kill only the
                // exact fixture-created process incarnations for test hygiene.
                for process in [&record.root, &record.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        return;
    }
    let path = root.join("registry.sqlite3");
    let mut registry = super::super::model::Registry::at(&path).unwrap();
    let accepted = if role == "source" {
        registry
            .apply(&ModelEdit::OpenProject {
                repository: root.join("project"),
                name: "Killed host fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        Some(
            registry
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: Some(crate::account::Account::system(Provider::Claude).id()),
                            target: ExecutionTarget {
                                repository: root.join("project"),
                                directory: root.join("project"),
                                placement: None,
                            },
                            surface: Surface::Chat,
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "One exact killed-host turn".into(),
                        idempotency_key: "one-killed-host-turn".into(),
                    },
                    1,
                )
                .unwrap(),
        )
    } else {
        None
    };
    let mut state = HostState::new(proof);
    state.install_model(registry);
    let host = Arc::new(HostLock::new(state));
    let _notifications = super::super::notifications::Broker::start(
        &host,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    host.lock().unwrap().notifications = Some(_notifications.handle());
    if let Some(accepted) = accepted {
        let session = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.join("project"),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        let id = session.id;
        start_execution(
            &host,
            0,
            session,
            Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
        )
        .unwrap();
        let (source, handle) = {
            let state = host.lock().unwrap();
            (
                state.chats[&id].session.clone(),
                state.chats[&id].handle.clone(),
            )
        };
        super::super::agent_execution::registry(&host)
            .unwrap()
            .lock()
            .unwrap()
            .bind_agent(
                accepted.agent.id,
                None,
                crate::agent_execution::HostedSession::Chat(source),
            )
            .unwrap();
        handle
            .command_for_run(
                accepted.run.id,
                chat::Command::Prompt {
                    text: "One exact killed-host turn".into(),
                    attachments: Vec::new(),
                    mode: chat::PromptMode::Send,
                },
            )
            .unwrap();
        wait_until(
            || {
                fs::read_to_string(root.join("produced"))
                    .unwrap_or_default()
                    .lines()
                    .count()
                    >= 3
            },
            "provider never began",
        );
        let receipt = Receipt {
            identity: handle.actor_identity().unwrap(),
            agent: accepted.agent.id,
            run: accepted.run.id,
            session: id,
        };
        let temporary = root.join("owner-receipt.pending");
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .unwrap();
        serde_json::to_writer(&file, &receipt).unwrap();
        file.sync_all().unwrap();
        fs::rename(temporary, root.join("owner-receipt.json")).unwrap();
        loop {
            thread::park();
        }
    }
    let receipt: Receipt =
        serde_json::from_slice(&fs::read(root.join("owner-receipt.json")).unwrap()).unwrap();
    assert!(
        receipt.identity.record.root.is_current(),
        "the original provider must survive the host SIGKILL"
    );
    restore_orphaned_native(&host, &[]).unwrap();
    let handle = host.lock().unwrap().chats[&receipt.session].handle.clone();
    assert_eq!(handle.actor_identity(), Some(receipt.identity.clone()));
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let retained = retained_actor::retained_runs(&host);
    registry
        .lock()
        .unwrap()
        .interrupt_unproven_agent_runs_except(2, &retained)
        .unwrap();
    wait_until(
        || {
            registry
                .lock()
                .unwrap()
                .agent_run(receipt.run)
                .unwrap()
                .state
                == State::Succeeded
        },
        "exact retained result never committed",
    );
    let run = registry.lock().unwrap().agent_run(receipt.run).unwrap();
    handle.set_visible(true);
    wait_until(
        || {
            handle
                .snapshot(None)
                .is_some_and(|snapshot| snapshot.items.len() >= 40)
        },
        "transcript did not hydrate after host SIGKILL",
    );
    let snapshot = handle.snapshot(None).unwrap();
    let text = snapshot
        .items
        .iter()
        .filter_map(|item| match &item.content {
            chat::ItemContent::Text {
                text,
                thinking: false,
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    for index in 0..40 {
        let marker = format!("FRAME_{index:03}|");
        assert_eq!(run.result.text.matches(&marker).count(), 1);
        assert_eq!(text.matches(&marker).count(), 1);
    }
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    super::super::agent_execution::process(
        &host,
        Authority::default(),
        crate::agent_execution::Operation::Close(receipt.agent),
    )
    .unwrap_or_else(|error| {
        let current_proof = host.lock().unwrap().proof().clone();
        panic!(
            "{error}; {}",
            sigkill_retirement_diagnostic(&receipt.identity, &current_proof)
        );
    });
    wait_until(
        || {
            !receipt.identity.record.holder.is_current()
                && !receipt.identity.record.root.is_current()
        },
        "explicit close did not retire exact native owners",
    );
}

/// No prompt is written in this fixture: an initialized actor still lacks a
/// provider-confirmed thread while its exact ownership proof rotates.
#[test]
fn unnamed_actor_adopts_then_commits_exact_pane_before_first_provider_result() {
    let _helper_executable = retained_helper_executable();
    const MODE: &str = "CAGENTS_UNNAMED_ACTOR_PERSISTENCE_TEST";
    if std::env::var_os(MODE).is_none() {
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        for path in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "project",
        ] {
            fs::create_dir_all(root.path().join(path)).unwrap();
        }
        let test = thread::current().name().unwrap().to_owned();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env_clear()
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_helper_executable(),
            )
            .env(MODE, "1")
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root.path())
            .env("HOME", root.path().join("home"))
            .env("CLAUDE_CONFIG_DIR", root.path().join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.path().join("run"))
            .env("XDG_DATA_HOME", root.path().join("data"))
            .env("XDG_CONFIG_HOME", root.path().join("config"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.path().join("bin").display()),
            )
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "unnamed actor fixture failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    install_provider(&root.join("bin/claude"));
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |name| HostSessionProof {
        host_session_id: "unnamed-actor".into(),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(name),
    };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let registry_path = root.join("registry.sqlite3");
    let mut state = HostState::new(first);
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let host = Arc::new(HostLock::new(state));
    let launch = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = launch.id;
    start_execution(
        &host,
        0,
        launch,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle.actor_identity().unwrap();
    struct Cleanup(crate::daemon::holder_registry::Record);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for process in [&self.0.root, &self.0.holder] {
                if process.is_current() {
                    unsafe {
                        libc::kill(process.pid, libc::SIGKILL);
                    }
                }
            }
        }
    }
    let _cleanup = Cleanup(identity.record.clone());
    wait_until(
        || handle.attachment_snapshot().ready,
        "actor never initialized",
    );
    assert!(
        matches!(
            host.lock().unwrap().chats[&id].session.native_continuity,
            chat::NativeContinuity::RetainedClaudeActor { protocol: 2 }
        ),
        "registered source must equal published actor capability before first input"
    );
    let registry = super::super::agent_execution::registry(&host).unwrap();
    let authority = registry
        .lock()
        .unwrap()
        .retained_agent_authority_source(id)
        .unwrap()
        .unwrap();
    assert_eq!(
        authority,
        crate::agent_execution::HostedSession::Chat(
            host.lock().unwrap().chats[&id].session.clone()
        )
    );
    let source = host.lock().unwrap().chats[&id].session.clone();
    assert!(source.launch.session_id.is_none());
    assert!(source.has_valid_native_continuity());
    let pane = crate::model::CardId::new();
    let workspace = crate::model::WorkspaceState {
        cards: vec![crate::model::CardState {
            id: pane,
            generation: 0,
            title: "Pending conversation".into(),
            width_fraction: 1.0,
            existence: crate::model::Existence::Open,
            exposure: crate::model::Exposure::Visible,
            residency: crate::model::Residency::Hot,
            kind: crate::model::CardKind::Chat(source.clone()),
        }],
        focused: Some(pane),
        ..Default::default()
    };
    let storage_path = root.join("pane.sqlite3");
    let storage = crate::persistence::StateStore::open(
        &storage_path,
        source.proof.as_ref().unwrap().boot_id.clone(),
    )
    .unwrap();
    storage
        .save_workspace(
            &workspace,
            crate::persistence::SnapshotBase {
                workspace: 0,
                cards: &BTreeMap::new(),
            },
        )
        .unwrap();
    drop(handle);
    drop(host);
    drop(lease.take());
    let next = proof("successor");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
    let mut state = HostState::new(next);
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    restore_orphaned_native(&successor, &[]).unwrap();
    let handle = successor.lock().unwrap().chats[&id].handle.clone();
    assert_eq!(handle.actor_identity(), Some(identity.clone()));
    let replacement = successor.lock().unwrap().chats[&id].session.clone();
    assert!(replacement.launch.session_id.is_none());
    assert_ne!(source.proof, replacement.proof);
    // The UI-only real client socket contract is tested separately. This
    // writer receipt is backed by authenticated execed-helper adoption above.
    assert!(chat::is_upgrade_recovery(&source, &replacement));
    storage
        .rebind_chat_upgrade(pane, &source, &replacement)
        .unwrap();
    drop(storage);
    let reopened = crate::persistence::StateStore::open(
        &storage_path,
        replacement.proof.as_ref().unwrap().boot_id.clone(),
    )
    .unwrap();
    assert_eq!(
        reopened.load_card(pane, None).unwrap().unwrap().kind,
        crate::model::CardKind::Chat(replacement.clone())
    );
    assert!(
        !root.join("prompts").exists(),
        "no accepted input may be replayed or synthesized"
    );
    // A later explicit first prompt names the thread without another Ready
    // edge. Its durable hosted descriptor must learn that ID even though no
    // UI ever hydrated a transcript or opened this conversation.
    let notifications = super::super::notifications::Broker::start(
        &successor,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    successor.lock().unwrap().notifications = Some(notifications.handle());
    handle
        .command(chat::Command::Prompt {
            text: "One explicit prompt after unnamed adoption".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait_until(
        || root.join("provider-done").exists(),
        "provider never finished its explicit first prompt",
    );
    wait_until(
        || {
            successor.lock().unwrap().chats[&id]
                .session
                .launch
                .session_id
                .is_some()
        },
        "unopened hosted descriptor did not retain late confirmed identity",
    );
    assert_eq!(
        successor.lock().unwrap().chats[&id]
            .session
            .launch
            .session_id,
        handle.confirmed_session_id()
    );
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    handle.close().unwrap();
    retained_actor::finish_close(&successor, &handle, &replacement).unwrap();
    assert!(!identity.record.root.is_current() && !identity.record.holder.is_current());
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    let retirement = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(retirement.owner(), &identity.record);
    assert_eq!(
        retirement.confirmed_session(),
        handle.confirmed_session_id().as_deref()
    );
    assert_eq!(
        retirement.scope().unwrap(),
        handle.launch_account_scope().unwrap()
    );
    drop(lease);
}

/// Kill the actual host after the helper has durably retired but before the
/// host has published the registry receipt. A successor must finish that exact
/// commit using the roster and private receipt, without recreating an actor.
#[test]
fn retired_helper_receipt_completes_after_host_sigkill_before_registry_commit() {
    let _helper_executable = retained_helper_executable();
    const EXACT: &str = "host::chat::retained_actor_tests::retired_helper_receipt_completes_after_host_sigkill_before_registry_commit";
    const ROLE: &str = "CAGENTS_RETIRED_HOST_PROCESS_ROLE";
    #[derive(serde::Serialize, serde::Deserialize)]
    struct Receipt {
        identity: chat::runtime::ActorIdentity,
        source: Session,
        launch_account_scope: crate::account::scope::LaunchAccountScope,
    }
    fn child(root: &Path, role: &str) -> std::process::Command {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", EXACT, "--nocapture"])
            .env_clear()
            .env(
                "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                retained_helper_executable(),
            )
            .env(ROLE, role)
            .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
            .env("CAGENTS_HOLDER_ROOT", root)
            .env("HOME", root.join("home"))
            .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            );
        command
    }
    if std::env::var_os(ROLE).is_none() {
        use std::os::unix::process::ExitStatusExt;
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path();
        for path in [
            "home/.claude",
            "run",
            "data",
            "config",
            "cache",
            "bin",
            "project",
        ] {
            fs::create_dir_all(root.join(path)).unwrap();
        }
        install_provider(&root.join("bin/claude"));
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = child(&self.0, "cleanup").output();
            }
        }
        let _cleanup = Cleanup(root.to_owned());
        struct Owner(std::process::Child);
        impl Drop for Owner {
            fn drop(&mut self) {
                if self.0.try_wait().is_ok_and(|status| status.is_none()) {
                    let _ = self.0.kill();
                }
                let _ = self.0.wait();
            }
        }
        let mut owner = Owner(
            child(root, "source")
                .stdout(fs::File::create(root.join("source.stdout")).unwrap())
                .stderr(fs::File::create(root.join("source.stderr")).unwrap())
                .spawn()
                .unwrap(),
        );
        let identity = crate::daemon::holder_registry::Process::read(owner.0.id() as i32).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !root.join("retired-before-sql.json").exists() {
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "source host failed: {}",
                fs::read_to_string(root.join("source.stderr")).unwrap_or_default()
            );
            assert!(
                Instant::now() < deadline,
                "host never reached the retirement/SQL boundary"
            );
            thread::sleep(Duration::from_millis(20));
        }
        assert!(identity.is_current());
        owner.0.kill().unwrap();
        assert_eq!(owner.0.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(!identity.is_current());
        let output = child(root, "successor").output().unwrap();
        assert!(
            output.status.success(),
            "successor failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    let role = std::env::var(ROLE).unwrap();
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("retired-host-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(&role),
    };
    let _lease = crate::daemon::HostLease::acquire(&paths, &proof).unwrap();
    if role == "cleanup" {
        let directory =
            crate::daemon::holder_registry::directory(crate::daemon::holder_registry::Kind::Native)
                .unwrap();
        if let Ok(records) = crate::daemon::holder_registry::discover(
            &directory,
            &proof.boot_id,
            crate::daemon::holder_registry::Kind::Native,
        ) {
            for record in records {
                // Only records from this fixture's private holder namespace.
                for process in [&record.root, &record.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        return;
    }
    let path = root.join("registry.sqlite3");
    let mut state = HostState::new(proof.clone());
    state.install_model(super::super::model::Registry::at(&path).unwrap());
    let host = Arc::new(HostLock::new(state));
    if role == "source" {
        let source = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            root.join("project"),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        let id = source.id;
        start_execution(
            &host,
            0,
            source,
            Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
        )
        .unwrap();
        let handle = host.lock().unwrap().chats[&id].handle.clone();
        let identity = handle.actor_identity().unwrap();
        struct Cleanup(crate::daemon::holder_registry::Record);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for process in [&self.0.root, &self.0.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
        let _cleanup = Cleanup(identity.record.clone());
        wait_until(
            || handle.attachment_snapshot().ready,
            "actor never initialized",
        );
        let source = host.lock().unwrap().chats[&id].session.clone();
        assert!(source.launch.session_id.is_none());
        assert!(handle.uncommitted_outcomes().unwrap().is_empty());
        let registry = super::super::agent_execution::registry(&host).unwrap();
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_none()
        );
        handle.close().unwrap();
        // The real helper's retirement RPC persists its private receipt, checks
        // semantic ACKs and completes discovery teardown. Deliberately stop at
        // the next instruction boundary before the host's SQL receipt commit.
        handle.finish_retained_close().unwrap();
        assert!(!identity.record.holder.is_current() && !identity.record.root.is_current());
        assert!(
            registry
                .lock()
                .unwrap()
                .native_actor_retirement(identity.instance)
                .unwrap()
                .is_none()
        );
        let receipt = Receipt {
            identity,
            source,
            launch_account_scope: handle
                .launch_account_scope()
                .expect("fixture captured its existing private Claude account home")
                .clone(),
        };
        let temporary = root.join("retired-before-sql.pending");
        use std::os::unix::fs::OpenOptionsExt;
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .unwrap();
        serde_json::to_writer(&file, &receipt).unwrap();
        file.sync_all().unwrap();
        fs::rename(temporary, root.join("retired-before-sql.json")).unwrap();
        fs::File::open(&root).unwrap().sync_all().unwrap();
        loop {
            thread::park();
        }
    }
    assert_eq!(role, "successor");
    let receipt: Receipt =
        serde_json::from_slice(&fs::read(root.join("retired-before-sql.json")).unwrap()).unwrap();
    assert!(
        !receipt.identity.record.holder.is_current() && !receipt.identity.record.root.is_current()
    );
    let registry = super::super::agent_execution::registry(&host).unwrap();
    assert!(
        registry
            .lock()
            .unwrap()
            .native_actor_retirement(receipt.identity.instance)
            .unwrap()
            .is_none()
    );
    assert!(host.lock().unwrap().chats.is_empty());
    retained_actor::reconcile_retired_actors(&host).unwrap();
    let committed = registry
        .lock()
        .unwrap()
        .native_actor_retirement(receipt.identity.instance)
        .unwrap()
        .expect("startup must commit the surviving exact retirement proof");
    assert_eq!(committed.actor(), receipt.identity.instance);
    assert_eq!(committed.owner(), &receipt.identity.record);
    assert_eq!(committed.source(), &receipt.source);
    assert_eq!(committed.confirmed_session(), None);
    assert_eq!(
        committed.scope(),
        Ok(&receipt.launch_account_scope),
        "cold reconciliation must preserve the exact captured physical account tuple",
    );
    retained_actor::reconcile_retired_actors(&host).unwrap();
    let replay = registry
        .lock()
        .unwrap()
        .native_actor_retirement(receipt.identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(replay, committed);
    assert!(
        host.lock().unwrap().chats.is_empty(),
        "receipt recovery must not create a provider handle"
    );
    assert!(
        !root.join("prompts").exists(),
        "retirement recovery must not synthesize provider input"
    );
    drop(registry);
    drop(host);
    let reopened = super::super::model::Registry::at(&path).unwrap();
    assert_eq!(
        reopened
            .native_actor_retirement(receipt.identity.instance)
            .unwrap(),
        Some(committed)
    );
}

/// Host-admission contract only: this cleanup owner is deterministic and does
/// not model an execed helper's pre-discovery lifecycle or process teardown.
#[test]
fn pending_startup_keeps_exact_cleanup_owner_until_retry_proves_teardown() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    struct Cleanup {
        proved: AtomicBool,
        observations: AtomicUsize,
    }
    impl chat::protocol::LaunchCleanup for Cleanup {
        fn cleanup(&self) -> Result<chat::protocol::LaunchOutcome, String> {
            self.observations.fetch_add(1, Ordering::SeqCst);
            Ok(if self.proved.load(Ordering::SeqCst) {
                chat::protocol::LaunchOutcome::TeardownProved
            } else {
                chat::protocol::LaunchOutcome::Uncertain
            })
        }
    }
    let root = tempfile::tempdir_in("/var/tmp").unwrap();
    let provider = root.path().join("provider");
    fs::write(&provider, "#!/bin/sh\nprintf spawned > \"$0.spawned\"\n").unwrap();
    fs::set_permissions(&provider, fs::Permissions::from_mode(0o700)).unwrap();
    let spawned = provider.with_extension("spawned");
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("pending-startup-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("current"),
    };
    let source = Session {
        id: TerminalId::new(),
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: crate::model::LaunchRecipe {
                program: provider.to_string_lossy().into_owned(),
                arguments: Vec::new(),
                working_directory: root.path().to_owned(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    let host = Arc::new(HostLock::new(HostState::new(proof)));
    let original = Arc::new(chat::protocol::ExecutionContext::capture(root.path()).unwrap());
    let cleanup = Arc::new(Cleanup {
        proved: AtomicBool::new(false),
        observations: AtomicUsize::new(0),
    });
    original.claim_owner(cleanup.clone());
    host.lock().unwrap().native_actor_startups.insert(
        source.id,
        retained_actor::Startup {
            source: source.clone(),
            requested: source.clone(),
            context: original.clone(),
        },
    );
    for _ in 0..2 {
        let retry = Arc::new(chat::protocol::ExecutionContext::capture(root.path()).unwrap());
        assert!(
            start_with_execution(
                &host,
                0,
                source.clone(),
                None,
                false,
                None,
                Default::default(),
                Some(retry),
                None
            )
            .is_err_and(|error| error
                .message
                .contains("exact retained startup has not proved cleanup"))
        );
        let state = host.lock().unwrap();
        let pending = state.native_actor_startups.get(&source.id).unwrap();
        assert!(Arc::ptr_eq(&pending.context, &original));
        assert_eq!(pending.source, source);
        assert!(state.chat_spawning.is_empty());
        assert!(state.chats.is_empty());
        assert!(
            !spawned.exists(),
            "retry may not start a provider before original cleanup proof"
        );
    }
    assert_eq!(cleanup.observations.load(Ordering::SeqCst), 2);
    cleanup.proved.store(true, Ordering::SeqCst);
    assert!(retained_actor::reconcile_startup(&host, &source).unwrap());
    assert!(host.lock().unwrap().native_actor_startups.is_empty());
    assert_eq!(cleanup.observations.load(Ordering::SeqCst), 3);
    assert_eq!(
        original.outcome(),
        chat::protocol::LaunchOutcome::TeardownProved
    );
    assert!(!spawned.exists());
}

#[test]
fn unattached_claude_cleanup_authenticates_old_alias_and_closes_original_pair() {
    let _helper_executable = retained_helper_executable();
    const MODE: &str = "CAGENTS_UNATTACHED_CLAUDE_CLOSE_TEST";
    if std::env::var_os(MODE).is_none() {
        for mode in ["unpaired", "paired", "removed-home", "replaced-home"] {
            let directory = tempfile::tempdir_in("/var/tmp").unwrap();
            let root = directory.path();
            for path in [
                "home/.claude",
                "run",
                "data",
                "config",
                "cache",
                "bin",
                "project",
            ] {
                fs::create_dir_all(root.join(path)).unwrap();
            }
            let test = thread::current().name().unwrap().to_owned();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", &test, "--nocapture"])
                .env_clear()
                .env(
                    "CAGENTS_TEST_RETAINED_ACTOR_HELPER",
                    retained_helper_executable(),
                )
                .env(MODE, mode)
                .env("CAGENTS_RETAINED_ACTOR_FIXTURE", "1")
                .env("CAGENTS_HOLDER_ROOT", root)
                .env("HOME", root.join("home"))
                .env("CLAUDE_CONFIG_DIR", root.join("home/.claude"))
                .env("XDG_RUNTIME_DIR", root.join("run"))
                .env("XDG_DATA_HOME", root.join("data"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", root.join("bin").display()),
                )
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "unattached cleanup mode={mode} failed:\n{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("CAGENTS_HOLDER_ROOT").unwrap());
    let mode = std::env::var(MODE).unwrap();
    let paired = mode == "paired";
    install_provider(&root.join("bin/claude"));
    let paths = crate::daemon::RuntimePaths::discover().unwrap();
    let proof = |incarnation| HostSessionProof {
        host_session_id: HostSessionId::new("unattached-close-fixture"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new(incarnation),
    };
    let first = proof("first");
    let mut lease = Some(crate::daemon::HostLease::acquire(&paths, &first).unwrap());
    let registry_path = root.join("registry.sqlite3");
    let mut state = HostState::new(first);
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let host = Arc::new(HostLock::new(state));
    let launch = chat::fresh_session(
        Provider::Claude,
        TerminalId::new(),
        root.join("project"),
        &crate::account::Account::system(Provider::Claude),
    )
    .unwrap();
    let id = launch.id;
    start_execution(
        &host,
        0,
        launch,
        Arc::new(chat::protocol::ExecutionContext::capture(&root.join("project")).unwrap()),
    )
    .unwrap();
    let handle = host.lock().unwrap().chats[&id].handle.clone();
    let identity = handle.actor_identity().unwrap();
    struct Cleanup(Vec<crate::daemon::holder_registry::Record>);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for owner in &self.0 {
                for process in [&owner.root, &owner.holder] {
                    if process.is_current() {
                        unsafe {
                            libc::kill(process.pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
    let mut cleanup = Cleanup(vec![identity.record.clone()]);
    wait_until(
        || handle.attachment_snapshot().ready,
        "actor never initialized",
    );
    // This alias predates pair publication and the successor daemon's proof.
    let requested = host.lock().unwrap().chats[&id].session.clone();
    let original_scope = handle.launch_account_scope().unwrap().clone();
    // Obtain a real provider-confirmed identity from one explicit offline turn.
    // Neither the issued allocation nor an input correlation UUID proves it.
    handle
        .command(chat::Command::Prompt {
            text: "One explicit turn before cleanup and resume qualification".into(),
            attachments: Vec::new(),
            mode: chat::PromptMode::Send,
        })
        .unwrap();
    wait_until(
        || root.join("provider-done").exists() && handle.confirmed_session_id().is_some(),
        "fixture provider never confirmed its completed conversation",
    );
    wait_until(
        || {
            host.lock().unwrap().chats[&id]
                .session
                .launch
                .session_id
                .is_some()
        },
        "host never retained its provider-confirmed descriptor",
    );
    let confirmed_source = host.lock().unwrap().chats[&id].session.clone();
    assert_eq!(
        confirmed_source.launch.session_id,
        handle.confirmed_session_id()
    );
    let ServerResponse::ChatResumePrepared { session: live } = process_inner(
        &host,
        17,
        ClientRequest::ChatPrepareResume {
            source: confirmed_source.clone(),
        },
    )
    .unwrap() else {
        panic!("a live owner must route to its existing pane");
    };
    assert_eq!(
        live, confirmed_source,
        "live-owner routing must not allocate a new pane or launch"
    );
    assert!(identity.record.holder.is_current() && identity.record.root.is_current());
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let pair = if paired {
        let recipe = crate::model::LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: root.join("project"),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        host.lock()
            .unwrap()
            .chats
            .get_mut(&id)
            .unwrap()
            .console_recipe = Some(recipe.clone());
        let terminal =
            crate::terminal_ipc::TerminalDescriptor::relaunchable(id, recipe.clone(), None).session;
        let (descriptor, console) = super::super::state::spawn_terminal_view_owned(
            &host,
            0,
            terminal,
            Default::default(),
            None,
            true,
        )
        .unwrap();
        let owner = console.holder_identity().unwrap().clone();
        cleanup.0.push(owner.clone());
        let mut source = confirmed_source.clone();
        source.terminal = Some(descriptor.session);
        handle
            .update_native_source(source.clone(), Some(recipe), Some(owner.clone()))
            .unwrap();
        {
            let mut state = host.lock().unwrap();
            let record = state.chats.get_mut(&id).unwrap();
            record.session = source.clone();
            record.paired_terminal = Some(console.clone());
        }
        retained_actor::register_ownership(&host, &handle, &source).unwrap();
        console.preserve_for_restart().unwrap();
        Some(owner)
    } else {
        None
    };
    drop(handle);
    drop(host);
    drop(lease.take());
    if mode == "removed-home" || mode == "replaced-home" {
        fs::rename(
            root.join("home/.claude"),
            root.join("home/.claude-original"),
        )
        .unwrap();
        if mode == "replaced-home" {
            fs::create_dir(root.join("home/.claude")).unwrap();
        }
        assert!(original_scope.revalidate().is_err());
    }
    assert!(identity.record.holder.is_current() && identity.record.root.is_current());
    let next = proof("successor");
    lease = Some(crate::daemon::HostLease::acquire(&paths, &next).unwrap());
    let mut state = HostState::new(next.clone());
    state.enable_holder_recovery();
    state.install_model(super::super::model::Registry::at(&registry_path).unwrap());
    let successor = Arc::new(HostLock::new(state));
    let _notifications = super::super::notifications::Broker::start(
        &successor,
        crate::notifications::Application::Cagents,
    )
    .unwrap();
    successor.lock().unwrap().notifications = Some(_notifications.handle());
    if paired {
        super::super::state::restore_orphaned_holders(&successor).unwrap();
    }
    assert!(successor.lock().unwrap().chats.is_empty());
    if mode == "removed-home" || mode == "replaced-home" {
        assert!(
            retained_actor::restore(&successor, 0, &requested, &next, identity.record.clone())
                .is_err(),
            "normal attachment must retain physical-account admission"
        );
        assert!(successor.lock().unwrap().chats.is_empty());
        assert!(identity.record.holder.is_current() && identity.record.root.is_current());
    }
    let mut changed_account = requested.clone();
    changed_account.launch.recipe.environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        root.join("other-account").to_string_lossy().into_owned(),
    );
    assert!(super::native_cleanup::close_retained(&successor, &changed_account).is_err());
    assert!(identity.record.holder.is_current() && identity.record.root.is_current());
    assert!(successor.lock().unwrap().chat_spawning.is_empty());
    if paired {
        let fault = rusqlite::Connection::open(&registry_path).unwrap();
        fault.execute_batch("CREATE TRIGGER reject_unattached_retirement BEFORE INSERT ON native_actor_retirements BEGIN SELECT RAISE(ABORT, 'fixture unattached retirement commit failure'); END;").unwrap();
        assert!(
            super::native_cleanup::close_retained(&successor, &requested)
                .is_err_and(|error| error.contains("fixture unattached retirement commit failure"))
        );
        assert!(!identity.record.holder.is_current() && !identity.record.root.is_current());
        let state = successor.lock().unwrap();
        assert!(
            state.chats.contains_key(&id),
            "failed durable retirement must retain exact cleanup handle"
        );
        assert!(state.terminals[&id].handle.is_completed());
        assert!(state.chats[&id].handle.is_close_only());
        assert!(state.chat_spawning.is_empty());
        let closed_source = state.chats[&id].session.clone();
        drop(state);
        assert!(
            process_inner(
                &successor,
                17,
                ClientRequest::ChatAttach {
                    session: closed_source
                }
            )
            .is_err(),
            "a cleanup-only owner must not be published as an attached conversation"
        );
        fault
            .execute_batch("DROP TRIGGER reject_unattached_retirement;")
            .unwrap();
        drop(fault);
    }
    super::native_cleanup::close_retained(&successor, &requested).unwrap();
    assert!(!identity.record.holder.is_current() && !identity.record.root.is_current());
    if let Some(pair) = pair {
        assert!(!pair.holder.is_current() && !pair.root.is_current());
    }
    let state = successor.lock().unwrap();
    assert!(!state.chats.contains_key(&id));
    assert!(!state.terminals.contains_key(&id));
    assert!(state.chat_spawning.is_empty());
    drop(state);
    let registry = super::super::agent_execution::registry(&successor).unwrap();
    let retirement = registry
        .lock()
        .unwrap()
        .native_actor_retirement(identity.instance)
        .unwrap()
        .unwrap();
    assert_eq!(retirement.owner(), &identity.record);
    assert_eq!(
        retirement.scope(),
        Ok(&original_scope),
        "cleanup retains the originally captured account tuple"
    );
    let canonical = retirement.source().clone();
    let confirmed = retirement
        .confirmed_session()
        .expect("actual completed fixture turn must confirm a conversation");
    assert_eq!(
        canonical.launch.session_id.as_deref(),
        Some(confirmed),
        "resume must use the actual canonical retirement descriptor"
    );
    assert!(
        process_inner(
            &successor,
            17,
            ClientRequest::ChatPrepareResume {
                source: requested.clone()
            }
        )
        .is_err(),
        "an actually saved unnamed alias cannot acquire a fabricated confirmed identity"
    );
    let mut wrong_recipe = canonical.clone();
    wrong_recipe
        .launch
        .recipe
        .arguments
        .push("--not-the-issued-recipe".into());
    assert!(
        process_inner(
            &successor,
            17,
            ClientRequest::ChatPrepareResume {
                source: wrong_recipe
            }
        )
        .is_err()
    );
    let mut wrong_protocol = canonical.clone();
    wrong_protocol.native_continuity = chat::NativeContinuity::RetainedClaudeActor { protocol: 1 };
    assert!(
        process_inner(
            &successor,
            17,
            ClientRequest::ChatPrepareResume {
                source: wrong_protocol
            }
        )
        .is_err()
    );
    let prepared = process_inner(
        &successor,
        17,
        ClientRequest::ChatPrepareResume {
            source: canonical.clone(),
        },
    );
    if mode == "removed-home" || mode == "replaced-home" {
        assert!(
            prepared.is_err(),
            "close-only authority must not authorize resume through a changed account home"
        );
    } else {
        let ServerResponse::ChatResumePrepared { session } = prepared.unwrap() else {
            panic!("qualified retirement must prepare the exact saved conversation");
        };
        assert_ne!(session.id, canonical.id);
        assert_eq!(session.launch, canonical.launch);
        assert_eq!(session.launch.session_id.as_deref(), Some(confirmed));
        assert_eq!(session.proof, None);
        assert_eq!(session.terminal, None);
        assert_eq!(session.owned_codex_listener, None);
        assert_eq!(
            session.native_continuity,
            chat::NativeContinuity::ReconnectOnly
        );
        let registry = registry.lock().unwrap();
        let predecessor = registry
            .session_lineage(canonical.id)
            .unwrap()
            .unwrap_or(crate::agent_execution::Lineage(canonical.id));
        assert_eq!(
            registry.session_lineage(session.id).unwrap(),
            Some(predecessor)
        );
    }
    assert!(
        successor.lock().unwrap().chats.is_empty(),
        "resume preparation must not launch a provider"
    );
    assert!(!identity.record.holder.is_current() && !identity.record.root.is_current());
    assert_eq!(
        fs::read_to_string(root.join("prompts"))
            .unwrap()
            .lines()
            .count(),
        1,
        "cleanup and resume preparation must never replay the one explicit fixture prompt"
    );
    drop(lease);
}

#[test]
fn unattached_claude_cleanup_refuses_unsupported_protocol_before_reservation() {
    let proof = HostSessionProof {
        host_session_id: HostSessionId::new("unsupported-claude-close"),
        boot_id: crate::daemon::read_boot_id().unwrap(),
        host_incarnation: HostIncarnation::new("current"),
    };
    let source = Session {
        id: TerminalId::new(),
        launch: chat::Launch {
            provider: Provider::Claude,
            recipe: crate::model::LaunchRecipe {
                program: "/not-a-real-provider".into(),
                arguments: Vec::new(),
                working_directory: "/var/tmp".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            session_id: None,
        },
        proof: Some(proof.clone()),
        terminal: None,
        owned_codex_listener: None,
        native_continuity: chat::NativeContinuity::RetainedClaudeActor { protocol: 1 },
    };
    let host = Arc::new(HostLock::new(HostState::new(proof)));
    assert!(
        super::native_cleanup::close_retained(&host, &source)
            .is_err_and(|error| error.contains("protocol is unsupported"))
    );
    let state = host.lock().unwrap();
    assert!(state.chat_spawning.is_empty());
    assert!(state.chats.is_empty());
    assert!(state.native_closing.is_empty());
}
