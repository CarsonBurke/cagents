//! Fresh native launches resolve execution policy on the selected daemon.
use super::*;
use crate::{
    account::{Account, home::Homes, probe::Cli, registry::Store},
    chat::ConfiguredChatStart,
};
use std::{ffi::OsStr, path::Path};

pub(super) fn start(
    host: &SharedHost,
    client: ClientId,
    intent: ConfiguredChatStart,
) -> Result<ServerResponse, ProtocolError> {
    intent.validate().map_err(error)?;
    require_fresh(host, intent.id)?;
    // The outer request still holds the existing shared control admission.
    // Account mutation installs under its writer, so this refusal protects
    // factory account/project default-mode reads as well as the later spawn.
    super::super::account_lifecycle::ensure_idle(host).map_err(|_| ProtocolError::new(
        ProtocolErrorCode::TerminalUnavailable,
        "An account operation is still using credential directories. Wait for it to finish before starting a new conversation.",
        true,
    ))?;
    // Own the ID and both budgets before any configuration/account/PATH I/O.
    // Error, cancellation and unwinding retain exact nonce-scoped cleanup.
    let admission = capacity::Reservation::fresh(host, intent.id)?;
    // These paths and settings belong to this daemon. No path to a settings
    // file, executable, account home or environment comes from the request.
    // Keep all configuration and filesystem reads outside HostLock.
    let settings =
        crate::settings::default_path().map_err(|failure| unavailable(failure.to_string()))?;
    let store =
        Store::installation().ok_or_else(|| unavailable("Account registry is unavailable"))?;
    let homes =
        Homes::installation().ok_or_else(|| unavailable("Account homes are unavailable"))?;
    let configuration = capture_configuration(intent.provider, &settings).map_err(error)?;
    let search = if configuration.environment.contains_key("PATH") {
        None
    } else {
        // The daemon may have been started by a desktop launcher. Initialize
        // the same bounded shell PATH capture used by account discovery, rather
        // than depending on another client having opened Accounts first.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|failure| unavailable(failure.to_string()))?;
        runtime.block_on(crate::account::probe::search_path())
    };
    let session = prepare_configuration(&intent, configuration, &store, &homes, search.as_deref())?;
    // Use the existing owning launch worker, including its final reservation,
    // account lifecycle authorization, process factory and issued descriptor.
    // Recovery, preview tokens and preserved source environments never enter
    // through this fresh-only ingress.
    super::start_with_execution_options(
        host,
        client,
        session,
        None,
        false,
        None,
        Default::default(),
        None,
        None,
        Some(admission),
    )
}

fn require_fresh(host: &SharedHost, id: TerminalId) -> Result<(), ProtocolError> {
    let state = host.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if state.shutting_down {
        return Err(unavailable("Daemon is shutting down"));
    }
    require_fresh_in(&state, id)
}

pub(super) fn require_fresh_in(state: &HostState, id: TerminalId) -> Result<(), ProtocolError> {
    if state.contains_session_identity(id) {
        return Err(error(
            "This conversation identifier is already owned; attach or explicitly recover it",
        ));
    }
    Ok(())
}

/// Production supplies installation-owned paths. Explicit arguments keep the
/// same real file readers testable without changing global process state.
#[cfg(test)]
fn prepare_at(
    intent: &ConfiguredChatStart,
    settings_path: &Path,
    store: &Store,
    homes: &Homes,
    search_path: Option<&OsStr>,
) -> Result<Session, ProtocolError> {
    prepare_configuration(
        intent,
        capture_configuration(intent.provider, settings_path).map_err(error)?,
        store,
        homes,
        search_path,
    )
}

fn capture_configuration(
    provider: Provider,
    settings_path: &Path,
) -> Result<crate::settings::AgentLaunchSettings, String> {
    let settings = crate::settings::load(settings_path).map_err(|failure| failure.to_string())?;
    let mut configuration = settings
        .agents
        .get(crate::settings::agents::provider_key(provider))
        .cloned()
        .unwrap_or_default();
    configuration.model = None;
    configuration.effort = None;
    configuration.permission_mode = None;
    Ok(configuration)
}

fn prepare_configuration(
    intent: &ConfiguredChatStart,
    mut configuration: crate::settings::AgentLaunchSettings,
    store: &Store,
    homes: &Homes,
    search_path: Option<&OsStr>,
) -> Result<Session, ProtocolError> {
    intent.validate().map_err(error)?;
    if !intent.working_directory.is_dir() {
        return Err(error(
            "The conversation directory is unavailable on the selected device",
        ));
    }
    // The UI has already selected its effective conversation controls. None
    // means clear, not an invitation to replace them with another window's
    // application defaults. Saved extra CLI arguments remain execution policy.
    // The same captured search path must also reach #!/usr/bin/env provider
    // interpreters. An explicit saved PATH always takes precedence.
    if let Some(path) = search_path {
        let path = path
            .to_str()
            .ok_or_else(|| error("The provider search path is not UTF-8"))?;
        configuration
            .environment
            .entry("PATH".into())
            .or_insert_with(|| path.into());
    }
    let registry = store.load().map_err(|failure| error(failure.to_string()))?;
    if registry.is_newer() {
        return Err(error("Account registry needs a newer cagents version"));
    }
    let system = Account::system(intent.provider);
    let account = if intent.account == system.id() {
        system
    } else {
        registry
            .accounts(Some(homes))
            .into_iter()
            .find(|account| account.provider == intent.provider && account.id() == intent.account)
            .ok_or_else(|| {
                ProtocolError::new(
                    ProtocolErrorCode::AccountRemoved,
                    "The selected provider account was removed on this device",
                    false,
                )
            })?
    };
    if account.registry_key().is_some() && account.resolved_home().is_none_or(|home| !home.is_dir())
    {
        return Err(error(
            "The selected account home is unavailable on this device",
        ));
    }
    let cli = Cli::configured(intent.provider, search_path, configuration).map_err(|failure| {
        match failure {
            crate::account::probe::CliFailure::Unavailable(reason)
            | crate::account::probe::CliFailure::Invalid(reason) => error(reason),
        }
    })?;
    let mut captured = cli.configuration;
    captured.executable = Some(
        cli.program
            .into_os_string()
            .into_string()
            .map_err(|_| error("The provider executable is not a UTF-8 path"))?,
    );
    let mut session = chat::fresh_session_configured(
        intent.provider,
        intent.id,
        intent.working_directory.clone(),
        &account,
        &captured,
        crate::settings::agents::LaunchDefaults {
            model: intent.model.as_deref(),
            effort: intent.effort.as_deref(),
            permission_mode: intent.permission_mode.as_deref(),
        },
    )
    .map_err(error)?;
    if intent.automatic {
        session
            .launch
            .recipe
            .environment
            .insert(crate::account::routing::AUTOMATIC_MARKER.into(), "1".into());
    }
    Ok(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{FileTypeExt, PermissionsExt},
    };

    struct Fixture {
        directory: tempfile::TempDir,
        settings: std::path::PathBuf,
        executable: std::path::PathBuf,
        store: Store,
        homes: Homes,
        intent: ConfiguredChatStart,
    }
    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let executable = directory.path().join("configured-provider");
            fs::write(&executable, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
            let settings = directory.path().join("config/settings.toml");
            let mut preferences = crate::settings::AppSettings::default();
            preferences.agents.insert(
                "codex".into(),
                crate::settings::AgentLaunchSettings {
                    executable: Some(executable.to_str().unwrap().into()),
                    arguments: vec!["--server-extra".into()],
                    environment: BTreeMap::from([(
                        "SERVER_LAUNCH_POLICY".into(),
                        "daemon-value".into(),
                    )]),
                    model: Some("daemon-model".into()),
                    effort: Some("low".into()),
                    permission_mode: Some("workspaceWrite".into()),
                    ..Default::default()
                },
            );
            crate::settings::save(&settings, &preferences).unwrap();
            let store = Store::at(directory.path().join("accounts-config"));
            let homes = Homes::at(directory.path().join("accounts-data"));
            let intent = ConfiguredChatStart {
                id: TerminalId::new(),
                provider: Provider::Codex,
                working_directory: directory.path().to_owned(),
                account: Account::system(Provider::Codex).id(),
                model: None,
                effort: None,
                permission_mode: None,
                automatic: false,
            };
            Self {
                directory,
                settings,
                executable,
                store,
                homes,
                intent,
            }
        }
        fn prepare(&self) -> Result<Session, ProtocolError> {
            prepare_at(&self.intent, &self.settings, &self.store, &self.homes, None)
        }
    }

    #[test]
    fn configured_ingress_refuses_account_mutation_before_factory_reads_or_start_reservation() {
        let fixture = Fixture::new();
        let account_home = fixture.directory.path().join("reserved-home");
        fs::create_dir(&account_home).unwrap();
        let account = Account::linked(Provider::Codex, uuid::Uuid::new_v4(), account_home);
        let host = crate::host::AccountReservation::test_host();
        let _reservation = crate::host::AccountReservation::test_install(
            &host,
            &[account],
            fixture.store.clone(),
            &fixture.homes,
        )
        .unwrap();
        // Exercise the actual ingress. It must refuse before looking up the
        // installation's settings/registry or touching any provider endpoint.
        let failure = start(&host, 1, fixture.intent.clone()).unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::TerminalUnavailable);
        assert!(failure.message.contains("account operation"), "{failure:?}");
        assert!(failure.retryable);
        let state = host.lock().unwrap();
        assert!(state.chat_spawning.is_empty());
        assert!(state.chats.is_empty());
        assert!(!state.contains_session_identity(fixture.intent.id));
    }

    #[test]
    fn configured_ingress_shutdown_refusal_stays_nonretryable_without_reserving_identity() {
        let fixture = Fixture::new();
        let host = crate::host::AccountReservation::test_host();
        host.lock().unwrap().shutting_down = true;
        let failure = start(&host, 1, fixture.intent.clone()).unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::TerminalUnavailable);
        assert_eq!(failure.message, "Daemon is shutting down");
        assert!(!failure.retryable);
        let state = host.lock().unwrap();
        assert!(state.chat_spawning.is_empty());
        assert!(state.chats.is_empty());
        assert!(!state.contains_session_identity(fixture.intent.id));
    }

    #[test]
    fn configured_factory_failure_releases_only_its_exact_preparation_slot() {
        let fixture = Fixture::new();
        let host = crate::host::AccountReservation::test_host();
        let survivor = TerminalId::new();
        let survivor_owner = capacity::Reservation::fresh(&host, survivor).unwrap();
        fs::remove_file(&fixture.executable).unwrap();
        let result = (|| -> Result<Session, ProtocolError> {
            let _owner = capacity::Reservation::fresh(&host, fixture.intent.id)?;
            // The same saved-policy factory used by ingress fails after the
            // reservation, before any provider process can be constructed.
            fixture.prepare()
        })();
        assert!(result.is_err());
        let state = host.lock().unwrap();
        assert!(!state.contains_session_identity(fixture.intent.id));
        assert!(survivor_owner.holds(&state, survivor));
        assert_eq!(state.chat_preparing.len(), 1);
        assert!(state.chat_spawning.is_empty());
        assert_eq!(capacity::usage(&state), 1);
        drop(state);
        drop(survivor_owner);
    }

    #[test]
    fn configured_removed_account_keeps_typed_refusal_and_exact_factory_cleanup() {
        let mut fixture = Fixture::new();
        let key = uuid::Uuid::new_v4();
        fs::create_dir_all(fixture.homes.path(Provider::Codex, key)).unwrap();
        fixture
            .store
            .update(|registry| {
                registry.register(
                    crate::account::registry::Registered {
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
        fixture.intent.account = Account::managed(
            Provider::Codex,
            key,
            fixture.homes.path(Provider::Codex, key),
        )
        .id();
        assert!(fixture.prepare().unwrap().is_unstarted());
        assert!(
            fixture
                .store
                .remove_home(&fixture.homes, Provider::Codex, key, true)
                .unwrap()
                .1
                .is_none()
        );

        let host = crate::host::AccountReservation::test_host();
        let survivor = TerminalId::new();
        let survivor_owner = capacity::Reservation::fresh(&host, survivor).unwrap();
        let failure = (|| -> Result<Session, ProtocolError> {
            let _owner = capacity::Reservation::fresh(&host, fixture.intent.id)?;
            fixture.prepare()
        })()
        .unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::AccountRemoved);
        assert!(!failure.retryable);
        assert_eq!(
            failure.message,
            "The selected provider account was removed on this device"
        );
        // The existing client typed-error contract maps AccountRemoved to
        // Recovery::Account. No provider or connection is constructed here.
        let state = host.lock().unwrap();
        assert!(!state.contains_session_identity(fixture.intent.id));
        assert!(survivor_owner.holds(&state, survivor));
        assert_eq!(state.chat_preparing.len(), 1);
        assert!(state.chat_spawning.is_empty());
        assert!(state.chats.is_empty());
        assert_eq!(capacity::usage(&state), 1);
        drop(state);
        drop(survivor_owner);
    }

    #[test]
    fn configured_invalid_execution_policy_keeps_nonretryable_invalid_request() {
        let fixture = Fixture::new();
        let mut configuration = capture_configuration(Provider::Codex, &fixture.settings).unwrap();
        configuration.environment.insert(
            "CODEX_HOME".into(),
            fixture.directory.path().to_string_lossy().into_owned(),
        );
        let failure = prepare_configuration(
            &fixture.intent,
            configuration,
            &fixture.store,
            &fixture.homes,
            None,
        )
        .unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::InvalidRequest);
        assert!(!failure.retryable);
        assert!(failure.message.contains("reserved for account isolation"));
    }

    #[test]
    fn owning_factory_uses_saved_execution_policy_and_preserves_explicit_clear() {
        let fixture = Fixture::new();
        let session = fixture.prepare().unwrap();
        let recipe = &session.launch.recipe;
        assert_eq!(recipe.program, fixture.executable.to_str().unwrap());
        assert_eq!(
            recipe
                .environment
                .get("SERVER_LAUNCH_POLICY")
                .map(String::as_str),
            Some("daemon-value")
        );
        assert_eq!(recipe.arguments, ["--server-extra", "--remote", "unix://"]);
        assert_eq!(session.id, fixture.intent.id);
        assert!(session.is_unstarted());
    }

    #[test]
    fn explicit_choices_and_automatic_routing_survive_host_resolution() {
        let mut fixture = Fixture::new();
        fixture.intent.model = Some("chosen-model".into());
        fixture.intent.effort = Some("high".into());
        fixture.intent.permission_mode = Some("readOnly".into());
        fixture.intent.automatic = true;
        let session = fixture.prepare().unwrap();
        let args = &session.launch.recipe.arguments;
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--model", "chosen-model"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--config", "model_reasoning_effort=high"])
        );
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--sandbox", "read-only"])
        );
        assert_eq!(
            session
                .launch
                .recipe
                .environment
                .get(crate::account::routing::AUTOMATIC_MARKER)
                .map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn missing_executable_account_and_invalid_intent_refuse_without_fallback() {
        let mut fixture = Fixture::new();
        fs::remove_file(&fixture.executable).unwrap();
        let failure = fixture.prepare().unwrap_err();
        assert_eq!(failure.code, ProtocolErrorCode::InvalidRequest);
        assert!(!failure.retryable);
        fixture.intent.account = Account::managed(
            Provider::Codex,
            uuid::Uuid::new_v4(),
            fixture.directory.path().join("not-enrolled"),
        )
        .id();
        assert!(fixture.prepare().unwrap_err().message.contains("removed"));
        fixture.intent.account = Account::system(Provider::Claude).id();
        assert!(fixture.prepare().is_err());
        fixture.intent.id = TerminalId::from_uuid(uuid::Uuid::nil());
        assert!(fixture.prepare().is_err());
    }

    #[test]
    fn registered_account_home_is_resolved_by_the_daemon_registry() {
        let mut fixture = Fixture::new();
        let key = uuid::Uuid::new_v4();
        fixture
            .store
            .update(|registry| {
                registry.register(
                    crate::account::registry::Registered {
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
        fixture.intent.account = Account::managed(
            Provider::Codex,
            key,
            fixture.directory.path().join("untrusted-ui-home"),
        )
        .id();
        assert!(
            fixture
                .prepare()
                .unwrap_err()
                .message
                .contains("account home")
        );
        fs::create_dir_all(fixture.homes.path(Provider::Codex, key)).unwrap();
        let session = fixture.prepare().unwrap();
        assert_eq!(
            session.launch.recipe.environment.get("CODEX_HOME"),
            Some(
                &fixture
                    .homes
                    .path(Provider::Codex, key)
                    .to_string_lossy()
                    .into_owned()
            )
        );
        assert!(
            session
                .launch
                .recipe
                .unset_environment
                .contains("OPENAI_API_KEY")
        );
        assert!(
            !session
                .launch
                .recipe
                .environment
                .contains_key("OPENAI_API_KEY")
        );
    }

    #[test]
    fn fresh_ingress_refuses_an_original_terminal_or_chat_reservation_before_settings_io() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};
        let fixture = Fixture::new();
        let host = Arc::new(HostLock::new(HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("configured-fresh"),
            boot_id: BootId::new("test-boot"),
            host_incarnation: HostIncarnation::new("test-incarnation"),
        })));
        host.lock()
            .unwrap()
            .reserve_spawn(fixture.intent.id)
            .unwrap();
        let failure = start(&host, 1, fixture.intent.clone()).unwrap_err();
        assert!(failure.message.contains("already owned"));
        let pending = fixture.prepare().unwrap();
        {
            let mut state = host.lock().unwrap();
            assert!(state.chats.is_empty());
            assert!(state.chat_spawning.is_empty());
            state.cancel_spawn(fixture.intent.id);
            state.chat_spawning.insert(fixture.intent.id, pending);
        }
        assert!(
            start(&host, 2, fixture.intent.clone())
                .unwrap_err()
                .message
                .contains("already owned")
        );
        let state = host.lock().unwrap();
        assert_eq!(state.chat_spawning.len(), 1);
        assert!(state.chats.is_empty());
    }

    #[test]
    fn captured_execution_policy_is_immutable_and_later_starts_read_actual_saved_changes() {
        let fixture = Fixture::new();
        let first = fixture.prepare().unwrap();
        let mut saved = crate::settings::load(&fixture.settings).unwrap();
        saved
            .agents
            .get_mut("codex")
            .unwrap()
            .environment
            .insert("SERVER_LAUNCH_POLICY".into(), "next-value".into());
        crate::settings::save(&fixture.settings, &saved).unwrap();
        let next = fixture.prepare().unwrap();
        assert_eq!(
            first.launch.recipe.environment["SERVER_LAUNCH_POLICY"],
            "daemon-value"
        );
        assert_eq!(
            next.launch.recipe.environment["SERVER_LAUNCH_POLICY"],
            "next-value"
        );
    }

    #[test]
    fn original_preparation_serializes_all_start_routes_and_preserves_newer_pending_rows() {
        use crate::model::{BootId, HostIncarnation, HostSessionId};
        use std::os::unix::net::UnixListener;
        let fixture = Fixture::new();
        let session = fixture.prepare().unwrap();
        let host = Arc::new(HostLock::new(HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new("configured-preparation"),
            boot_id: BootId::new("test-boot"),
            host_incarnation: HostIncarnation::new("test-incarnation"),
        })));
        let endpoint = fixture.directory.path().join("control.sock");
        drop(UnixListener::bind(&endpoint).unwrap());
        let reservation = capacity::Reservation::fresh(&host, session.id).unwrap();
        assert!(capacity::Reservation::fresh(&host, session.id).is_err());
        assert!(capacity::Reservation::new(&host, &session).is_err());
        assert!(endpoint.symlink_metadata().unwrap().file_type().is_socket());
        drop(reservation);
        assert!(!host.lock().unwrap().contains_session_identity(session.id));
        let mut reservation = Some(capacity::Reservation::fresh(&host, session.id).unwrap());
        {
            let mut state = host.lock().unwrap();
            capacity::Reservation::install(&mut reservation, &host, &mut state, &session).unwrap();
            let mut issued = session.clone();
            issued.proof = Some(state.proof().clone());
            state.chat_spawning.insert(session.id, issued);
        }
        drop(reservation);
        assert!(
            host.lock().unwrap().chat_spawning[&session.id]
                .proof
                .is_some()
        );
    }

    #[test]
    fn configured_path_and_pin_override_initialized_shell_search_and_launch_uses_same_path() {
        let fixture = Fixture::new();
        let bin = fixture.directory.path().join("shell-bin");
        fs::create_dir(&bin).unwrap();
        let program = bin.join("codex");
        fs::copy(&fixture.executable, &program).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let mut configuration = capture_configuration(Provider::Codex, &fixture.settings).unwrap();
        configuration.executable = None;
        let session = prepare_configuration(
            &fixture.intent,
            configuration.clone(),
            &fixture.store,
            &fixture.homes,
            Some(bin.as_os_str()),
        )
        .unwrap();
        assert_eq!(session.launch.recipe.program, program.to_str().unwrap());
        assert_eq!(
            session.launch.recipe.environment["PATH"],
            bin.to_str().unwrap()
        );
        configuration.environment.insert(
            "PATH".into(),
            fixture
                .directory
                .path()
                .join("missing-bin")
                .to_str()
                .unwrap()
                .into(),
        );
        assert!(
            prepare_configuration(
                &fixture.intent,
                configuration.clone(),
                &fixture.store,
                &fixture.homes,
                Some(bin.as_os_str())
            )
            .is_err()
        );
        configuration.executable = Some(fixture.executable.to_str().unwrap().into());
        let pinned = prepare_configuration(
            &fixture.intent,
            configuration,
            &fixture.store,
            &fixture.homes,
            Some(bin.as_os_str()),
        )
        .unwrap();
        assert_eq!(
            pinned.launch.recipe.program,
            fixture.executable.to_str().unwrap()
        );
        assert_ne!(
            pinned.launch.recipe.environment["PATH"],
            bin.to_str().unwrap()
        );
    }
}
