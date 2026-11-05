use std::{fs, process::Command};

use super::*;

struct Fixture {
    _directory: tempfile::TempDir,
    host: crate::host::state::SharedHost,
    homes: Homes,
    store: Store,
    candidate: Candidate,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let homes = Homes::at(directory.path().join("accounts"));
        let candidate = Candidate {
            provider: Provider::Claude,
            key: uuid::Uuid::from_u128(1),
        };
        let home = homes.path(candidate.provider, candidate.key);
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("history.fixture"),
            b"original synthetic history\n",
        )
        .unwrap();
        let store = Store::at(directory.path().join("registry"));
        Self {
            _directory: directory,
            host: Reservation::test_host(),
            homes,
            store,
            candidate,
        }
    }

    fn recover(&self) -> Recovery {
        Recovery::test_install(
            &self.host,
            self.store.clone(),
            &self.homes,
            self.candidate.clone(),
        )
        .unwrap()
    }

    fn home(&self) -> PathBuf {
        self.homes.path(self.candidate.provider, self.candidate.key)
    }
}

fn status_command(permit: &StatusPermit) -> Command {
    // This command is inspected only; no executable is launched in these tests.
    let mut command = Command::new(permit.program());
    command.args(["auth", "status", "--json"]);
    command.env(
        "CLAUDE_CONFIG_DIR",
        permit.account().resolved_home().unwrap(),
    );
    for name in permit.account().credential_overrides() {
        command.env_remove(name);
    }
    command
}

#[test]
fn restoration_permission_is_bound_to_the_reserved_managed_provider_key_and_home() {
    let fixture = Fixture::new();
    let recovery = fixture.recover();
    assert_eq!(recovery.candidate(), &fixture.candidate);
    assert_eq!(
        recovery.account(),
        Account::managed(Provider::Claude, fixture.candidate.key, fixture.home())
    );
    assert!(recovery.system().is_none());
    for account in [
        Account::managed(Provider::Codex, fixture.candidate.key, fixture.home()),
        Account::managed(Provider::Claude, uuid::Uuid::from_u128(2), fixture.home()),
        Account::managed(
            Provider::Claude,
            fixture.candidate.key,
            fixture.home().join("other"),
        ),
        Account::linked(Provider::Claude, fixture.candidate.key, fixture.home()),
        Account::system(Provider::Claude),
    ] {
        assert!(
            recovery
                .status_permit(account, "/bin/false".into())
                .is_err()
        );
    }
    assert!(
        recovery
            .status_permit(recovery.account(), "relative-client".into())
            .is_err()
    );
    assert!(
        recovery
            .status_permit(recovery.account(), "/bin/../bin/false".into())
            .is_err()
    );
    assert!(
        recovery
            .status_permit(recovery.account(), "/bin/false".into())
            .is_ok()
    );
    assert_eq!(
        fs::read(fixture.home().join("history.fixture")).unwrap(),
        b"original synthetic history\n"
    );
}

#[test]
fn an_unrelated_reservation_cannot_be_reused_to_restore_an_orphan() {
    let fixture = Fixture::new();
    let other_key = uuid::Uuid::from_u128(2);
    let other = fixture.homes.path(Provider::Claude, other_key);
    fs::create_dir(&other).unwrap();
    let accounts = [
        Account::managed(Provider::Claude, other_key, other),
        Account {
            provider: Provider::Claude,
            home: AccountHome::SystemAt {
                path: fixture.home().with_file_name("private-system"),
            },
        },
    ];
    let owner = Reservation::test_install(
        &fixture.host,
        &accounts,
        fixture.store.clone(),
        &fixture.homes,
    )
    .unwrap();
    assert!(
        Recovery::new(
            Arc::new(owner),
            fixture.homes.clone(),
            fixture.candidate.clone()
        )
        .is_err()
    );
    assert!(super::super::ensure_paths(&fixture.host, [fixture.home()]).is_ok());
}

#[test]
fn status_command_admission_refuses_redirected_program_args_home_and_credentials() {
    let fixture = Fixture::new();
    let recovery = fixture.recover();
    let permit = recovery
        .status_permit(recovery.account(), "/bin/false".into())
        .unwrap();
    let mut redirected = status_command(&permit);
    redirected.env(
        "CLAUDE_CONFIG_DIR",
        fixture.home().with_file_name("other-home"),
    );
    assert!(permit.acquire(&redirected).is_err());
    let mut logout = Command::new(permit.program());
    logout.args(["auth", "logout"]);
    assert!(permit.acquire(&logout).is_err());
    let mut program = Command::new("/bin/true");
    program.args(["auth", "status", "--json"]);
    assert!(permit.acquire(&program).is_err());
    let mut credentials = status_command(&permit);
    credentials.env("ANTHROPIC_API_KEY", "synthetic-never-sent");
    assert!(permit.acquire(&credentials).is_err());
    assert!(
        !fixture
            .home()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".lifecycle.lock")
            .exists()
    );
}

#[test]
fn exact_status_lease_holds_both_locks_only_until_the_spawn_guard_is_dropped() {
    let fixture = Fixture::new();
    let recovery = fixture.recover();
    let permit = recovery
        .status_permit(recovery.account(), "/bin/false".into())
        .unwrap();
    let guard = permit.acquire(&status_command(&permit)).unwrap();
    assert!(
        matches!(fixture.homes.try_lifecycle_lock(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    );
    assert!(
        matches!(fixture.store.try_load(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    );
    drop(guard);
    assert!(fixture.homes.try_lifecycle_lock().is_ok());
    assert!(fixture.store.try_load().is_ok());
    assert!(super::super::ensure_paths(&fixture.host, [fixture.home()]).is_err());
}

#[test]
fn parked_or_replaced_home_refuses_a_previously_minted_status_permit() {
    let fixture = Fixture::new();
    let recovery = fixture.recover();
    let permit = recovery
        .status_permit(recovery.account(), "/bin/false".into())
        .unwrap();
    let parked = fixture
        .home()
        .with_file_name(format!(".removing-{}", fixture.candidate.key));
    fs::rename(fixture.home(), &parked).unwrap();
    fs::create_dir(fixture.home()).unwrap();
    fs::write(
        fixture.home().join("replacement.fixture"),
        b"replacement untouched",
    )
    .unwrap();
    assert!(permit.acquire(&status_command(&permit)).is_err());
    assert_eq!(
        fs::read(parked.join("history.fixture")).unwrap(),
        b"original synthetic history\n"
    );
    assert_eq!(
        fs::read(fixture.home().join("replacement.fixture")).unwrap(),
        b"replacement untouched"
    );
    assert!(
        fixture
            .store
            .load()
            .unwrap()
            .find(fixture.candidate.key)
            .is_none()
    );
}

#[test]
fn newly_ambiguous_key_and_new_system_home_both_invalidate_a_status_permit() {
    for change_system in [false, true] {
        let fixture = Fixture::new();
        let recovery = fixture.recover();
        let permit = recovery
            .status_permit(recovery.account(), "/bin/false".into())
            .unwrap();
        let changed = if change_system {
            fixture.home().with_file_name(".recovery-system-fixture")
        } else {
            fixture.homes.path(Provider::Codex, fixture.candidate.key)
        };
        fs::create_dir_all(&changed).unwrap();
        assert!(permit.acquire(&status_command(&permit)).is_err());
        assert!(
            fixture
                .store
                .load()
                .unwrap()
                .find(fixture.candidate.key)
                .is_none()
        );
    }
}

#[test]
fn relevant_registry_races_refuse_but_unrelated_changes_preserve_the_permission() {
    for pending_same_key in [false, true] {
        let fixture = Fixture::new();
        let recovery = fixture.recover();
        let permit = recovery
            .status_permit(recovery.account(), "/bin/false".into())
            .unwrap();
        fixture
            .store
            .update(|registry| registry.begin(Provider::Codex, uuid::Uuid::from_u128(2), 1))
            .unwrap();
        let before = fixture.store.load().unwrap();
        assert!(permit.acquire(&status_command(&permit)).is_ok());
        assert_eq!(fixture.store.load().unwrap(), before);
        fixture
            .store
            .update(|registry| {
                if pending_same_key {
                    registry.begin(Provider::Claude, fixture.candidate.key, 2);
                } else {
                    registry
                        .register(
                            crate::account::registry::Registered {
                                key: fixture.candidate.key,
                                provider: Provider::Codex,
                                linked_home: None,
                                label: None,
                                email: Some("other-provider@example.invalid".into()),
                                organization: None,
                                added_at: 2,
                            },
                            &[],
                        )
                        .unwrap();
                }
            })
            .unwrap();
        let raced = fixture.store.load().unwrap();
        assert!(permit.acquire(&status_command(&permit)).is_err());
        assert_eq!(fixture.store.load().unwrap(), raced);
    }
}

#[test]
fn cloned_permits_retain_one_reservation_without_an_ownership_cycle() {
    let fixture = Fixture::new();
    let recovery = fixture.recover();
    let weak = Arc::downgrade(&recovery.owner);
    let permit = recovery
        .status_permit(recovery.account(), "/bin/false".into())
        .unwrap();
    let queued = permit.clone();
    drop(recovery);
    drop(permit);
    assert!(weak.upgrade().is_some());
    assert!(super::super::ensure_paths(&fixture.host, [fixture.home()]).is_err());
    drop(queued);
    assert!(weak.upgrade().is_none());
    assert!(super::super::ensure_paths(&fixture.host, [fixture.home()]).is_ok());
}
