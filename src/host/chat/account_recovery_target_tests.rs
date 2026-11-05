//! Replacement admission must precede destructive automatic account handoff.
//! These are isolated local protocol fixtures, not provider/native validation.
use super::*;
use crate::{
    account::{home::Homes, registry::Store},
    chat::{
        Command, Event,
        protocol::{Adapter, ProcessSpec, Step},
    },
    model::LaunchRecipe,
    repository::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::Path};

const CHILD: &str = "CAGENTS_RECOVERY_TARGET_ADMISSION_TEST";
const CONVERSATION: &str = "12345678-1234-4234-8234-123456789abc";

#[derive(Clone, Copy)]
enum Target {
    Removed,
    Unreadable,
    Available,
}

#[test]
fn removed_target_preserves_the_original_actor_and_history() {
    run(Target::Removed);
}
#[test]
fn unreadable_target_registry_preserves_the_original_actor_and_history() {
    run(Target::Unreadable);
}
#[test]
fn available_managed_target_recovers_the_same_conversation() {
    run(Target::Available);
}

fn run(target: Target) {
    let exact = thread::current()
        .name()
        .expect("named owning test")
        .to_owned();
    if std::env::var(CHILD).as_deref() != Ok(exact.as_str()) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for name in ["home", "run", "data", "config", "cache", "tmp"] {
            crate::secure_file::prepare_private_directory(&root.join(name)).unwrap();
        }
        let mut command = CommandSpec::new(std::env::current_exe().unwrap());
        for (name, _) in std::env::vars_os() {
            command = command.env_remove(name);
        }
        let command = command
            .args(["--exact", exact.as_str(), "--nocapture", "--test-threads=1"])
            .env(CHILD, &exact)
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TMPDIR", root.join("tmp"))
            .env("PATH", "/usr/bin:/bin")
            .timeout(Duration::from_secs(45))
            .output_limits(128 * 1024, 128 * 1024);
        let output = SystemProcessExecutor
            .execute(&command, &CancellationToken::new())
            .unwrap();
        let output = output.require_success(&command.program).unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout
                .lines()
                .any(|line| line == format!("test {exact} ... ok"))
                && stdout
                    .lines()
                    .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;")),
            "the exact child contract must execute: {stdout}"
        );
        return;
    }
    handoff(target);
}

struct Idle;
impl Adapter for Idle {
    fn initialize(&mut self) -> Result<Step, String> {
        Ok(Step::default())
    }
    fn command(&mut self, _: Command) -> Result<Step, String> {
        Err("This fixture issues no work".into())
    }
    fn receive(&mut self, _: Value) -> Result<Step, String> {
        Ok(Step {
            events: vec![
                Event::Ready(true),
                Event::Session {
                    id: CONVERSATION.into(),
                    title: None,
                },
            ],
            ..Step::default()
        })
    }
}

// Close the actor even if a source-survival assertion fails.
struct CloseOnDrop(Handle);
impl Drop for CloseOnDrop {
    fn drop(&mut self) {
        let _ = self.0.close();
    }
}

fn handoff(target: Target) {
    let directory = tempfile::tempdir().unwrap();
    let cwd = directory.path().canonicalize().unwrap();
    let provider = directory.path().join("claude");
    std::fs::write(&provider, format!(r#"#!/bin/sh
printf '%s\n' '{{"type":"system","subtype":"init","session_id":"{CONVERSATION}"}}'
while IFS= read -r frame; do
    request=$(printf '%s' "$frame" | /usr/bin/sed -n 's/.*"request_id":"\([^" ]*\)".*/\1/p')
    if [ -n "$request" ]; then
        printf '{{"type":"control_response","response":{{"subtype":"success","request_id":"%s","response":{{"commands":[],"models":[]}}}}}}\n' "$request"
    fi
done
"#)).unwrap();
    std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o700)).unwrap();
    let original_home = directory.path().join(".claude");
    let homes = Homes::installation().unwrap();
    let store = Store::installation().unwrap();
    let (key, alternate_home) = store
        .create_pending(&homes, Provider::Claude, 1, || {
            homes.create(Provider::Claude, None)
        })
        .unwrap();
    let project: String = cwd
        .to_str()
        .unwrap()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let relative = Path::new("projects")
        .join(project)
        .join(format!("{CONVERSATION}.jsonl"));
    let transcript = original_home.join(&relative);
    let destination = alternate_home.join(&relative);
    std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
    std::fs::write(&transcript, "{}\n").unwrap();
    let proof = HostSessionProof {
        host_session_id: "target-admission".into(),
        boot_id: "boot".into(),
        host_incarnation: "incarnation".into(),
    };
    let host = Arc::new(HostLock::new(HostState::new(proof.clone())));
    let source = Session {
        id: TerminalId::new(),
        owned_codex_listener: None,
        native_continuity: Default::default(),
        proof: Some(proof),
        terminal: None,
        launch: chat::Launch {
            provider: Provider::Claude,
            session_id: Some(CONVERSATION.into()),
            recipe: LaunchRecipe {
                program: provider.to_str().unwrap().into(),
                arguments: Vec::new(),
                working_directory: cwd,
                environment: BTreeMap::from([
                    ("HOME".into(), directory.path().to_str().unwrap().into()),
                    (
                        "CLAUDE_CONFIG_DIR".into(),
                        original_home.to_str().unwrap().into(),
                    ),
                    (crate::account::routing::AUTOMATIC_MARKER.into(), "1".into()),
                ]),
                unset_environment: Default::default(),
            },
        },
    };
    let mut recipe = source.launch.recipe.clone();
    recipe.program = "/bin/cat".into();
    let spec = ProcessSpec::from(&recipe);
    let mut initial = Step {
        writes: vec![json!({"ready":true})],
        events: vec![
            Event::Ready(true),
            Event::Session {
                id: CONVERSATION.into(),
                title: None,
            },
        ],
        ..Step::default()
    };
    let event_store = chat::runtime::preflight(&spec, &mut initial).unwrap();
    let handle = Handle::start(
        spec,
        Box::new(Idle),
        initial,
        event_store,
        Some(CONVERSATION.into()),
        |_| {},
    )
    .unwrap();
    let _cleanup = CloseOnDrop(handle.clone());
    {
        let mut state = host.lock().unwrap();
        state.register_client(1);
        state.chats.insert(
            source.id,
            Record {
                execution_instance: uuid::Uuid::nil(),
                console_recipe: None,
                paired_terminal: None,
                session: source.clone(),
                handle: handle.clone(),
                source_process: None,
                subscribers: BTreeMap::from([(1, false)]),
                observations: BTreeMap::new(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
    }
    let mut replacement = source.clone();
    replacement.id = TerminalId::new();
    replacement.proof = None;
    replacement.launch.recipe.environment.insert(
        "CLAUDE_CONFIG_DIR".into(),
        alternate_home.to_str().unwrap().into(),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    while handle.confirmed_session_id().is_none() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    let original = handle.provider_process().unwrap();
    let generation = handle.snapshot(None).unwrap().generation;
    let registry_path = crate::instance::project_dirs()
        .unwrap()
        .config_dir()
        .join("accounts.toml");
    match target {
        Target::Removed => {
            store
                .remove_home(&homes, Provider::Claude, key, false)
                .unwrap();
            assert!(!alternate_home.exists());
        }
        Target::Unreadable => {
            std::fs::write(&registry_path, "not valid TOML = [").unwrap();
        }
        Target::Available => {}
    }
    // This genuine system-home source never reads the damaged managed
    // registry. Both negative cases must reach replacement admission, rather
    // than accidentally passing via an earlier source-account refusal.
    check_launch_account(&source.launch).unwrap();
    let registry_before = std::fs::read(&registry_path).unwrap();
    let result = recover_account(
        &host,
        1,
        source.clone(),
        replacement,
        Some(generation),
        Default::default(),
    );
    if matches!(target, Target::Available) {
        let ServerResponse::ChatAttached { session, snapshot } = result.unwrap() else {
            panic!("expected confirmed replacement")
        };
        assert_eq!(session.launch.session_id.as_deref(), Some(CONVERSATION));
        assert_eq!(snapshot.session_id.as_deref(), Some(CONVERSATION));
        assert_eq!(std::fs::read(&destination).unwrap(), b"{}\n");
        assert!(handle.is_completed());
        assert!(!original.is_current());
        assert!(host.lock().unwrap().chat_spawning.is_empty());
        process(
            &host,
            1,
            ClientRequest::ChatClose {
                chat_id: session.id,
            },
        )
        .unwrap();
    } else {
        let failure = result.unwrap_err();
        assert_eq!(
            failure.code,
            if matches!(target, Target::Removed) {
                ProtocolErrorCode::AccountRemoved
            } else {
                ProtocolErrorCode::TerminalUnavailable
            }
        );
        assert!(!failure.retryable);
        assert!(!handle.is_completed());
        assert!(
            original.is_current(),
            "the original process must survive target refusal"
        );
        assert!(
            !destination.exists(),
            "no history was copied to an inadmissible target"
        );
        assert_eq!(
            alternate_home.exists(),
            matches!(target, Target::Unreadable),
            "a removed target must not be recreated"
        );
        {
            let state = host.lock().unwrap();
            assert!(state.chat_spawning.is_empty());
            assert_eq!(state.chats.len(), 1);
            assert_eq!(state.chats[&source.id].session, source);
            assert!(state.chats[&source.id].handle.same_actor(&handle));
        }
        process(&host, 1, ClientRequest::ChatClose { chat_id: source.id }).unwrap();
        assert!(handle.is_completed());
        assert!(!original.is_current());
    }
    assert_eq!(std::fs::read(&transcript).unwrap(), b"{}\n");
    assert_eq!(
        std::fs::read(&registry_path).unwrap(),
        registry_before,
        "admission does not rewrite the account registry"
    );
}
