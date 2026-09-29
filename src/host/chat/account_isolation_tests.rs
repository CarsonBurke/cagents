//! Regression for #520: every native start runs its provider in the home of
//! the account it was issued for, whatever home the daemon itself inherited.

use super::*;
use crate::account::{Account, home::Homes, registry::Store};
use crate::model::{HostIncarnation, HostSessionId};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const CHILD: &str = "CAGENTS_ACCOUNT_ISOLATION_TEST";

/// A stand-in for the provider CLI: it records the home it was pointed at and
/// the account selection it was told of, then does what a CLI refreshing its
/// login does - rewrites the login kept in that home. A start on the wrong
/// home therefore shows twice: in the record, and as a login carried into a
/// home it does not belong to.
fn install_provider(program: &Path, record: &Path) {
    std::fs::write(
        program,
        format!(
            "#!/bin/sh\n\
             printf '%s\\n' \"${{CODEX_HOME-unset}}\" >> '{record}'\n\
             home=\"${{CODEX_HOME:-$HOME/.codex}}\"\n\
             printf '%s' \"${{CAGENTS_ACCOUNT_HOMES-}}\" > \"$home/selected\"\n\
             read -r login < \"$home/login\" || exit 3\n\
             printf '%s+renewed\\n' \"$login\" > \"$home/login\"\n",
            record = record.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(program, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn sign_in(home: &Path, login: &str) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(home.join("login"), format!("{login}\n")).unwrap();
}

fn login(home: &Path) -> String {
    std::fs::read_to_string(home.join("login"))
        .unwrap()
        .trim()
        .to_owned()
}

/// Waits until the provider has been started on `home` `count` times, and
/// on no home but `expected` ones.
fn started(record: &Path, home: &Path, count: usize, expected: &[PathBuf]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let homes: Vec<PathBuf> = std::fs::read_to_string(record)
            .unwrap_or_default()
            .lines()
            .map(PathBuf::from)
            .collect();
        if let Some(stranger) = homes.iter().find(|started| !expected.contains(started)) {
            panic!("a provider ran on {}", stranger.display());
        }
        if homes.iter().filter(|started| *started == home).count() >= count {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the provider was not started on {}",
            home.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn native_codex_starts_keep_each_account_on_its_own_home() {
    if std::env::var_os(CHILD).is_none() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        // The daemon was started by `cagents`, run by an agent whose shell
        // had another installation's managed account selected - the
        // environment every pre-fix daemon passed on to the system account.
        let foreign = root
            .join("other-install/accounts/codex")
            .join(uuid::Uuid::new_v4().to_string());
        sign_in(&foreign, "login-foreign");
        sign_in(&root.join("home/.codex"), "login-system");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        install_provider(&root.join("bin/codex"), &root.join("started"));
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::chat::account_isolation_tests::native_codex_starts_keep_each_account_on_its_own_home",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, root)
            .env("HOME", root.join("home"))
            .env("CODEX_HOME", &foreign)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_RUNTIME_DIR", root)
            .env("TMPDIR", root)
            .env("PATH", format!("{}:/usr/bin:/bin", root.join("bin").display()))
            .status()
            .unwrap();
        assert!(status.success(), "account isolation fixture failed");
        assert_eq!(
            login(&foreign),
            "login-foreign",
            "no start touched the home the daemon inherited"
        );
        return;
    }
    let root = PathBuf::from(std::env::var_os(CHILD).unwrap());
    let record = root.join("started");
    let system_home = root.join("home/.codex");
    let homes = Homes::installation().unwrap();
    let store = Store::installation().unwrap();
    let mut accounts = vec![(Account::system(Provider::Codex), system_home.clone())];
    for name in ["login-a", "login-b"] {
        let (key, home) = store
            .create_pending(&homes, Provider::Codex, 1, || {
                homes.create(Provider::Codex, None)
            })
            .unwrap();
        sign_in(&home, name);
        accounts.push((Account::managed(Provider::Codex, key, home.clone()), home));
    }
    let linked = root.join("linked-codex");
    sign_in(&linked, "login-linked");
    let key = uuid::Uuid::new_v4();
    store
        .update(|registry| registry.link(Provider::Codex, key, linked.clone(), None, None, 1))
        .unwrap()
        .1
        .unwrap();
    accounts.push((
        Account::linked(Provider::Codex, key, linked.clone()),
        linked,
    ));
    let expected: Vec<PathBuf> = accounts.iter().map(|(_, home)| home.clone()).collect();
    let project = root.join("home");
    // Each conversation is issued through the same policy windows and
    // `cagents agent` use, then started - and started again from its saved
    // recipe, as a restart or recovery does - in the daemon.
    let sessions: Vec<Session> = accounts
        .iter()
        .map(|(account, _)| {
            chat::fresh_session(Provider::Codex, TerminalId::new(), project.clone(), account)
                .unwrap()
        })
        .collect();
    for round in 0..2 {
        // The second round is a restarted daemon, whose conversations start
        // again from the recipes they were issued with.
        let host = Arc::new(Mutex::new(HostState::new(HostSessionProof {
            host_session_id: HostSessionId::new(format!("account-isolation-{round}")),
            boot_id: crate::daemon::read_boot_id().unwrap(),
            host_incarnation: HostIncarnation::new("incarnation"),
        })));
        for (session, (_, home)) in sessions.iter().zip(&accounts) {
            // The provider stand-in exits without answering; only where it
            // ran matters here.
            let result = start(
                &host,
                1,
                session.clone(),
                None,
                false,
                None,
                Default::default(),
            );
            assert!(result.is_ok(), "round {round}: {:?}", result.err());
            started(&record, home, round + 1, &expected);
        }
    }
    // Each home holds only its own login, renewed by the starts on it, and
    // each provider was told which home cagents selected for it - except
    // on the system account, whose home is the reader's own.
    let deadline = Instant::now() + Duration::from_secs(10);
    for ((account, home), original) in
        accounts
            .iter()
            .zip(["login-system", "login-a", "login-b", "login-linked"])
    {
        let renewed = format!("{original}+renewed+renewed");
        while login(home) != renewed && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(login(home).starts_with(&renewed), "{}", login(home));
        let selected = std::fs::read_to_string(home.join("selected")).unwrap();
        if account.is_system() {
            assert_eq!(selected, "");
        } else {
            assert_eq!(selected, format!("CODEX_HOME={}", home.display()));
        }
    }
}
