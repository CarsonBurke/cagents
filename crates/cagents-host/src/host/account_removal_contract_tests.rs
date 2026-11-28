#![cfg(target_os = "linux")]

use crate::{
    account::{
        Account, AccountId,
        home::Homes,
        registry::{Registered, Store},
        remote::{Archived, Operation as Lifecycle, RemovalPlan},
    },
    agent::Provider,
    chat::{Launch, Session},
    device_service::{Operation, Reply},
    host::{SharedHost, account_lifecycle, client::process_request, model::Registry},
    model::{CardId, CardKind, ChannelId, LaunchRecipe, ProjectKind, TerminalId},
    terminal::TerminalSpawnOptions,
    terminal_ipc::{
        ChatExecutionEnvironment, ClientRequest, ProtocolError, ServerResponse, TerminalDescriptor,
    },
    workspace_model::{
        ModelEdit,
        views::{Change, Pane, Scope},
    },
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

const FIXTURE: &str = "CAGENTS_ACCOUNT_REMOVAL_FIXTURE";

struct FixtureClient(SharedHost);

impl FixtureClient {
    fn request(&self, request: ClientRequest) -> Result<ServerResponse, ProtocolError> {
        process_request(&self.0, 1, request).map(|(response, _, _)| response)
    }

    fn edit(&self, edit: ModelEdit) {
        self.request(ClientRequest::ApplyWorkspaceEdit { edit })
            .unwrap();
    }

    fn lifecycle(&self, operation: Lifecycle) -> Result<serde_json::Value, ProtocolError> {
        match self.request(ClientRequest::DeviceOperation {
            operation: Operation::AccountLifecycle { operation },
        })? {
            ServerResponse::DeviceOperation {
                reply: Reply::AccountLifecycle(value),
            } => Ok(value),
            reply => panic!("unexpected lifecycle reply: {reply:?}"),
        }
    }
}

fn register(store: &Store, homes: &Homes) -> (AccountId, PathBuf, Uuid) {
    let key = Uuid::new_v4();
    let home = homes.path(Provider::Codex, key);
    fs::create_dir_all(&home).unwrap();
    store
        .update(|registry| {
            registry.register(
                Registered {
                    key,
                    provider: Provider::Codex,
                    linked_home: None,
                    label: None,
                    email: None,
                    organization: None,
                    added_at: 1,
                },
                &[],
            )
        })
        .unwrap()
        .1
        .unwrap();
    (
        Account::managed(Provider::Codex, key, home.clone()).id(),
        home,
        key,
    )
}

#[test]
fn removal_counts_copies_resumes_and_archives_through_the_selected_daemon() {
    if crate::retained_fixture::run_isolated() {
        return;
    }
    let Some(root) = std::env::var_os(FIXTURE).map(PathBuf::from) else {
        let private = crate::fixture_tempdir().unwrap();
        let root = private.path();
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join(".cagents-fixture-root"), []).unwrap();
        let mut child = Command::new(std::env::current_exe().unwrap());
        child.args([
            "--exact",
            "host::account_removal_contract_tests::removal_counts_copies_resumes_and_archives_through_the_selected_daemon",
            "--nocapture",
        ])
        .env(FIXTURE, root)
        .env("TMPDIR", root)
        .env("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display()))
        .env("SHELL", "/bin/sh")
        .env(crate::account::probe::login_path::ADOPTED_ENV, "1");
        for (name, relative) in [
            ("HOME", "home"),
            ("XDG_RUNTIME_DIR", "runtime"),
            ("XDG_CONFIG_HOME", "config"),
            ("XDG_DATA_HOME", "data"),
            ("XDG_STATE_HOME", "state"),
            ("XDG_CACHE_HOME", "cache"),
            ("CLAUDE_CONFIG_DIR", "claude"),
            ("CODEX_HOME", "codex"),
        ] {
            let path = root.join(relative);
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            child.env(name, path);
        }
        assert!(child.status().unwrap().success());
        return;
    };
    let root = root.as_path();
    let namespace = crate::instance::STORAGE_NAME;
    let store = Store::at(root.join("config").join(namespace));
    let homes = Homes::at(root.join("data").join(namespace).join("accounts"));
    let (source_id, source_home, source_key) = register(&store, &homes);
    let (target_id, target_home, _) = register(&store, &homes);
    let (archive_id, archive_home, archive_key) = register(&store, &homes);
    let thread_id = Uuid::new_v4().to_string();
    let relative = format!("sessions/2026/11/27/rollout-2026-11-27T10-00-00-{thread_id}.jsonl");
    let source_transcript = source_home.join(&relative);
    let target_transcript = target_home.join(&relative);
    fs::create_dir_all(source_transcript.parent().unwrap()).unwrap();
    let transcript = serde_json::json!({
        "timestamp": "2026-11-27T10:00:00Z",
        "type": "session_meta",
        "payload": {"id": thread_id, "cwd": root, "source": "cli", "model_provider": "openai"}
    });
    fs::write(&source_transcript, format!("{transcript}\n")).unwrap();
    fs::write(source_home.join("fixture-login"), "fixture-only login").unwrap();
    fs::write(archive_home.join("keep"), "kept").unwrap();

    let bin = root.join("bin");
    fs::create_dir(&bin).unwrap();
    let codex = bin.join("codex");
    let peer =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/codex-tool-rows.py");
    fs::write(
        &codex,
        format!(
            r#"#!/bin/sh
set -eu
fixture=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)
case "$CODEX_HOME/" in "$fixture/"*) ;; *) exit 70 ;; esac
if [ "${{1:-}}" = logout ]; then
    test ! -f "$CODEX_HOME/refuse-logout" || exit 72
    : > "$CODEX_HOME/logged-out"
    rm -f -- "$CODEX_HOME/fixture-login"
    exit 0
fi
export CAGENTS_TOOL_ROWS_FIXTURE_ROOT="$fixture"
exec /usr/bin/python3 -I "{}" "$@" 2>>"$fixture/codex.stderr"
"#,
            peer.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&codex, fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    // Exercise the production dispatcher using the harness's synthetic host
    // and client record. The process keeps its inherited authority.
    let host = account_lifecycle::tests::host();
    assert!(
        host.lock()
            .unwrap()
            .install_model(Registry::open().unwrap())
    );
    let client = FixtureClient(host);
    let plan = Lifecycle::RemovalPlan {
        id: source_id.clone(),
    };
    let move_and_archive = Lifecycle::Archive {
        id: source_id.clone(),
        move_to: Some(target_id.clone()),
    };
    let command = |program: &str| LaunchRecipe {
        program: program.into(),
        arguments: Vec::new(),
        working_directory: root.to_owned(),
        environment: [
            ("CODEX_HOME".into(), source_home.display().to_string()),
            (
                "CLAUDE_CONFIG_DIR".into(),
                root.join("claude").display().to_string(),
            ),
            ("HOME".into(), root.join("home").display().to_string()),
            ("PATH".into(), path.clone()),
            (
                "XDG_RUNTIME_DIR".into(),
                root.join("runtime").display().to_string(),
            ),
        ]
        .into(),
        unset_environment: Default::default(),
    };
    let terminal_id = TerminalId::new();
    client
        .request(ClientRequest::Spawn {
            session: TerminalDescriptor::relaunchable(terminal_id, command("/bin/cat"), None)
                .session,
            options: TerminalSpawnOptions::default(),
        })
        .unwrap();
    let counted: RemovalPlan = serde_json::from_value(client.lifecycle(plan).unwrap()).unwrap();
    assert_eq!(counted.conversations, 1);
    let held = client.lifecycle(move_and_archive.clone()).unwrap_err();
    assert!(
        held.retryable && held.message.contains("account home"),
        "{held:?}"
    );
    assert!(!target_transcript.exists());
    assert!(!source_home.join("logged-out").exists());
    client
        .request(ClientRequest::ShutdownTerminal { terminal_id })
        .unwrap();

    let mut recipe = command(codex.to_str().unwrap());
    recipe.arguments = vec![
        "resume".into(),
        thread_id.clone(),
        "--remote".into(),
        "unix://".into(),
    ];
    let source = Session {
        id: TerminalId::new(),
        launch: Launch {
            provider: Provider::Codex,
            recipe,
            session_id: Some(thread_id.clone()),
            draft_model: None,
        },
        proof: None,
        terminal: None,
        owned_codex_listener: None,
        native_continuity: Default::default(),
    };
    client.edit(ModelEdit::OpenProject {
        repository: root.to_owned(),
        name: "Private fixture".into(),
        kind: ProjectKind::Folder,
    });
    let channel = ChannelId::new();
    client.edit(ModelEdit::CreateChannel {
        id: channel,
        repository: root.to_owned(),
        title: "Saved conversation".into(),
        title_source: Default::default(),
        checkout: None,
    });
    let scope = Scope::Channel(channel);
    let pane = CardId::new();
    client.edit(ModelEdit::SeedView {
        scope: scope.clone(),
        panes: vec![Pane {
            id: pane,
            title: "Saved chat".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 1.0,
            kind: CardKind::Chat(source.clone()),
            scope: None,
        }],
        closed: Vec::new(),
    });
    let refused = client.lifecycle(move_and_archive.clone()).unwrap_err();
    assert!(refused.message.contains("open pane"), "{refused:?}");
    let ServerResponse::WorkspaceView {
        view: Some(view), ..
    } = client
        .request(ClientRequest::ReadWorkspaceView {
            scope: scope.clone(),
        })
        .unwrap()
    else {
        panic!("fixture view is missing")
    };
    client.edit(ModelEdit::ChangeView {
        scope,
        expected_revision: view.revision,
        operation: Uuid::new_v4(),
        change: Change::Remove { id: pane },
    });
    assert!(!source_home.join("logged-out").exists());

    fs::create_dir_all(target_transcript.parent().unwrap()).unwrap();
    fs::write(&target_transcript, "divergent\n").unwrap();
    assert!(client.lifecycle(move_and_archive.clone()).is_err());
    assert_eq!(
        fs::read_to_string(&target_transcript).unwrap(),
        "divergent\n"
    );
    assert!(!source_home.join("logged-out").exists());
    fs::remove_file(&target_transcript).unwrap();
    let moved: Archived =
        serde_json::from_value(client.lifecycle(move_and_archive).unwrap()).unwrap();
    assert_eq!(moved.moved, 1);
    assert_eq!(
        fs::read(&source_transcript).unwrap(),
        fs::read(&target_transcript).unwrap()
    );
    assert!(source_home.join("logged-out").exists());
    assert!(!source_home.join("fixture-login").exists());
    assert!(!target_home.join("fixture-login").exists());
    assert!(store.load().unwrap().is_archived(source_key));

    let mut replacement = source.clone();
    replacement.id = TerminalId::new();
    replacement
        .launch
        .recipe
        .environment
        .insert("CODEX_HOME".into(), target_home.display().to_string());
    let resumed = client
        .request(ClientRequest::ChatRecoverAccount {
            source,
            replacement: replacement.clone(),
            idle_generation: None,
            execution_environment: ChatExecutionEnvironment::default(),
        })
        .unwrap_or_else(|error| {
            let stderr = fs::read_to_string(root.join("codex.stderr")).unwrap_or_default();
            panic!("fixture failed to resume on the target account: {error:?}\n{stderr}");
        });
    let ServerResponse::ChatAttached { session, .. } = resumed else {
        panic!("unexpected recovery response {resumed:?}")
    };
    assert_eq!(
        session.launch.session_id.as_deref(),
        Some(thread_id.as_str())
    );
    assert_eq!(
        session.launch.recipe.environment["CODEX_HOME"],
        target_home.display().to_string()
    );
    client
        .request(ClientRequest::ChatClose {
            chat_id: replacement.id,
        })
        .unwrap();

    let archive = Lifecycle::Archive {
        id: archive_id.clone(),
        move_to: None,
    };
    fs::write(archive_home.join("refuse-logout"), []).unwrap();
    assert!(
        client
            .lifecycle(archive.clone())
            .unwrap_err()
            .message
            .contains("sign-out")
    );
    let hidden = store.load().unwrap();
    assert!(hidden.is_archived(archive_key));
    assert!(!hidden.is_signed_out(archive_key));
    fs::remove_file(archive_home.join("refuse-logout")).unwrap();
    client.lifecycle(archive).unwrap();
    let registry = store.load().unwrap();
    assert!(registry.is_archived(archive_key));
    assert!(registry.is_signed_out(archive_key));
    assert!(
        registry
            .all_accounts(Some(&homes))
            .iter()
            .any(|account| account.id() == archive_id)
    );
    assert!(
        !registry
            .accounts(Some(&homes))
            .iter()
            .any(|account| account.id() == archive_id || account.id() == source_id)
    );
    assert!(
        registry
            .accounts(Some(&homes))
            .iter()
            .any(|account| account.id() == target_id)
    );
    assert_eq!(
        fs::read_to_string(archive_home.join("keep")).unwrap(),
        "kept"
    );
}
