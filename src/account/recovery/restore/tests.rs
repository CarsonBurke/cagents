use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    time::Duration,
};

use super::*;
use crate::{
    account::{Account, AccountHome, registry::Store},
    agent::Provider,
};

struct Fixture {
    _directory: tempfile::TempDir,
    host: crate::host::AccountTestHost,
    store: Store,
    homes: Homes,
    candidate: Candidate,
    program: PathBuf,
    ready: PathBuf,
    release: PathBuf,
}

fn quote(path: &std::path::Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

impl Fixture {
    fn new(candidate_status: &str, system_status: Option<&str>, wait: bool) -> Self {
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
            b"unchanged original conversation\n",
        )
        .unwrap();
        let system = directory.path().join("system-fixture");
        if system_status.is_some() {
            fs::create_dir(&system).unwrap();
        }
        let program = directory.path().join("fake-provider");
        let ready = directory.path().join("status-ready");
        let release = directory.path().join("status-release");
        let pause = if wait {
            format!(
                ": > {}\nwhile [ ! -e {} ]; do /usr/bin/sleep 0.01; done\n",
                quote(&ready),
                quote(&release)
            )
        } else {
            String::new()
        };
        let script = format!(
            "#!/bin/sh\nset -eu\n[ \"$#\" = 3 ] && [ \"$1\" = auth ] && [ \"$2\" = status ] && [ \"$3\" = --json ] || exit 21\nif [ \"${{CLAUDE_CONFIG_DIR-}}\" = {} ]; then\n{pause}{candidate_status}\nelif [ \"${{CLAUDE_CONFIG_DIR-}}\" = {} ]; then\n{}\nelse\nexit 22\nfi\n",
            quote(&home),
            quote(&system),
            system_status.unwrap_or("exit 23")
        );
        fs::write(&program, script).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let store = Store::at(directory.path().join("registry"));
        Self {
            _directory: directory,
            host: AccountReservation::test_host(),
            store,
            homes,
            candidate,
            program,
            ready,
            release,
        }
    }

    fn home(&self) -> PathBuf {
        self.homes.path(self.candidate.provider, self.candidate.key)
    }

    fn recovery(&self) -> AccountRecovery {
        let system = self._directory.path().join("system-fixture");
        let accounts = [
            Account::managed(Provider::Claude, self.candidate.key, self.home()),
            Account {
                provider: Provider::Claude,
                home: AccountHome::SystemAt { path: system },
            },
        ];
        let owner = AccountReservation::test_install(
            &self.host,
            &accounts,
            self.store.clone(),
            &self.homes,
        )
        .unwrap();
        AccountRecovery::new(Arc::new(owner), self.homes.clone(), self.candidate.clone()).unwrap()
    }
}

const SIGNED_IN: &str =
    "printf '%s\\n' '{\"loggedIn\":true,\"email\":\"restored@example.invalid\"}'; exit 0";
const SIGNED_OUT: &str = "printf '%s\\n' '{\"loggedIn\":false}'; exit 1";

#[tokio::test]
async fn a_confirmed_surviving_home_is_registered_with_its_original_key_and_files() {
    let fixture = Fixture::new(SIGNED_IN, Some(SIGNED_OUT), false);
    let inode = fs::metadata(fixture.home()).unwrap().ino();
    let settled = restore_with_program(fixture.recovery(), fixture.program.clone())
        .await
        .unwrap();
    let Settled::Registered {
        registry,
        identity: Some(identity),
        snapshot: None,
    } = settled
    else {
        panic!("not restored")
    };
    assert_eq!(identity.email.as_deref(), Some("restored@example.invalid"));
    assert_eq!(registry.registered().len(), 1);
    assert_eq!(
        registry.find(fixture.candidate.key).unwrap().email,
        identity.email
    );
    assert!(!registry.is_archived(fixture.candidate.key));
    assert_eq!(registry.revision(), 1);
    assert_eq!(fixture.store.load().unwrap(), registry);
    assert_eq!(fs::metadata(fixture.home()).unwrap().ino(), inode);
    assert_eq!(
        fs::read(fixture.home().join("history.fixture")).unwrap(),
        b"unchanged original conversation\n"
    );
}

#[tokio::test]
async fn a_duplicate_system_login_keeps_the_restored_key_as_an_archived_registration() {
    let fixture = Fixture::new(SIGNED_IN, Some(SIGNED_IN), false);
    let Settled::Registered { registry, .. } =
        restore_with_program(fixture.recovery(), fixture.program.clone())
            .await
            .unwrap()
    else {
        panic!("not restored")
    };
    assert!(registry.find(fixture.candidate.key).is_some());
    assert!(registry.is_archived(fixture.candidate.key));
    assert!(
        registry
            .all_accounts(Some(&fixture.homes))
            .iter()
            .any(|account| account.registry_key() == Some(fixture.candidate.key))
    );
    assert!(
        !registry
            .accounts(Some(&fixture.homes))
            .iter()
            .any(|account| account.registry_key() == Some(fixture.candidate.key))
    );
    assert_eq!(registry.revision(), 1);
    assert_eq!(
        fs::read(fixture.home().join("history.fixture")).unwrap(),
        b"unchanged original conversation\n"
    );
}

#[tokio::test]
async fn no_login_or_an_unverified_system_answer_never_publishes_a_registry_row() {
    for (candidate, system) in [
        (SIGNED_OUT, Some(SIGNED_IN)),
        (SIGNED_IN, Some("printf '%s\\n' '{}'; exit 0")),
        (
            "printf '%s\\n' '{\"loggedIn\":true,\"email\":\"restored@example.invalid\"}'; exit 7",
            None,
        ),
    ] {
        let fixture = Fixture::new(candidate, system, false);
        let before = fixture.store.load().unwrap();
        assert!(
            restore_with_program(fixture.recovery(), fixture.program.clone())
                .await
                .is_err()
        );
        assert_eq!(fixture.store.load().unwrap(), before);
        assert!(fixture.store.load().unwrap().pending().is_empty());
        assert_eq!(
            fs::read(fixture.home().join("history.fixture")).unwrap(),
            b"unchanged original conversation\n"
        );
    }
}

async fn wait_for(path: &std::path::Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn provider_io_holds_no_storage_lease_and_checked_commit_preserves_unrelated_changes() {
    let fixture = Fixture::new(SIGNED_IN, None, true);
    let running = tokio::spawn(restore_with_program(
        fixture.recovery(),
        fixture.program.clone(),
    ));
    wait_for(&fixture.ready).await;
    assert!(fixture.homes.try_lifecycle_lock().is_ok());
    let (concurrent, ()) = fixture
        .store
        .update(|registry| registry.begin(Provider::Codex, uuid::Uuid::from_u128(2), 9))
        .unwrap();
    fs::write(&fixture.release, b"go").unwrap();
    let Settled::Registered { registry, .. } = running.await.unwrap().unwrap() else {
        panic!("not restored")
    };
    assert_eq!(registry.pending(), concurrent.pending());
    assert_eq!(registry.revision(), concurrent.revision() + 1);
    assert!(registry.find(fixture.candidate.key).is_some());
}

#[tokio::test]
async fn relevant_registry_change_during_status_refuses_commit_without_overwriting_it() {
    let fixture = Fixture::new(SIGNED_IN, None, true);
    let running = tokio::spawn(restore_with_program(
        fixture.recovery(),
        fixture.program.clone(),
    ));
    wait_for(&fixture.ready).await;
    let (concurrent, ()) = fixture
        .store
        .update(|registry| registry.begin(Provider::Codex, fixture.candidate.key, 9))
        .unwrap();
    fs::write(&fixture.release, b"go").unwrap();
    assert!(running.await.unwrap().is_err());
    assert_eq!(fixture.store.load().unwrap(), concurrent);
    assert_eq!(
        fs::read(fixture.home().join("history.fixture")).unwrap(),
        b"unchanged original conversation\n"
    );
}
